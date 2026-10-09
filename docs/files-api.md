# Files API

The gateway owns `/v1/files`. Files are stored in the gateway's own encrypted [file store](file-storage.md), never at a provider, and every id a client sees is a gateway id (`file-<32 hex>`). The API is OpenAI-compatible: the official SDKs work unchanged against `https://<gateway>/v1`.

What you can use a file for **today**:

| OpenAI purpose | Stored as | Usable today |
| --- | --- | --- |
| `batch` | `batch_input` | Yes: as `input_file_id` of a batch ([async jobs](async-jobs.md)). |
| `user_data`, `vision`, `assistants`, `evals` | `user_file` | Stored, listed and downloadable only. Referencing a `file_id` in Chat Completions or Responses is phase 2 and is not implemented yet. |
| `batch_output` | `batch_output` | Written by the batch engine (results and errors). Listed and downloadable; clients can't upload it. |

`fine-tune` and anything else is refused (`400 unsupported_capability` for `fine-tune`, otherwise `400 invalid_request_error`).

## Authentication and ownership

- **Credential:** an inference API key (`Authorization: Bearer …`). The workspace comes from the key only, never from the request.
- **Visibility:** a file belongs to its workspace. Every key of that workspace can list, read and delete it; another workspace's id returns `404 not_found`. Personal workspaces stay owner-private, because only the owner holds their keys.
- **Recorded:** the creating key and its human owner (if any) are stored with the file.
- **Contents:** never written to logs, audit or metrics. Metadata (sizes, purposes, timestamps) is all the gateway reports.

## Endpoints

| Route | Behavior |
| --- | --- |
| `POST /v1/files` | Multipart: `file`, `purpose`, optional `expires_after[anchor]=created_at` and `expires_after[seconds]` (3600–2592000). Returns the File object. |
| `GET /v1/files` | `purpose`, `limit` (1–10000, default 10000), `after` (a file id), `order` (`asc` or `desc`, default `desc`, by creation time). Returns `{object:"list", data, first_id, last_id, has_more}`. Unknown query parameters are refused. |
| `GET /v1/files/{id}` | The File object. |
| `GET /v1/files/{id}/content` | The decrypted contents, streamed, as `application/octet-stream` with `Content-Disposition: attachment` and `X-Content-Type-Options: nosniff`. |
| `DELETE /v1/files/{id}` | `{id, object:"file", deleted:true}`. Frees the quota at once. A batch that hasn't read the file yet can't read it afterwards. |

The File object:

```json
{"id":"file-0f1e…","object":"file","bytes":120000,"created_at":1760000000,"expires_at":1760604800,
 "filename":"requests.jsonl","purpose":"batch","status":"processed","status_details":null}
```

`expires_at` is the effective expiry: the sooner of the client's `expires_after` and the purpose's retention in Admin › Settings › Data & privacy › Storage (batch files 7 days and user files 30 days by default). An expired file is unreadable at once; the sweeper deletes it shortly after.

## Uploads

The upload streams straight into the store. The gateway holds one network chunk plus the first 4 KiB of the file (for type checks) in memory, never the whole file.

- **Field order.** OpenAI's Python SDK sends `purpose` before `file`; its Node SDK sends `file` first. Both work:
  - **Purpose first:** the purpose is checked before any byte is stored, and the type check runs while streaming.
  - **File first:** the file is staged under a provisional purpose (batch input for `.jsonl` names, otherwise a user file), the trailing fields are read, and then the file is committed, moved to the right purpose (a streamed re-encryption, never buffered) or discarded. Either way only the final file remains.
- **Size.** At most `GATEWAY_FILES_MAX_BYTES` (default 200 MiB, 1 KiB–8 GiB). Larger uploads return `413 file_too_large`.
- **Filename.** Only the last path segment is kept. Control and bidirectional-override characters are dropped, and the name is bounded to 255 characters.
- **Type checks.** These are basic and only catch obvious mistakes:
  - `batch`: the first bytes must be JSONL text, with no NUL bytes and `{` as the first character.
  - `vision`: the file must start with a PNG, JPEG, GIF or WebP signature.
  - Empty files are refused for every purpose.
  - The batch engine validates every batch line itself when a batch is created.
- **Content type.** A plain client `type/subtype` is kept (for `vision`, only `image/*`). Downloads are always served as `application/octet-stream`.
- **Failures leave nothing behind.** A refused, failed, interrupted or abandoned upload (client disconnect, the one-hour upload deadline) deletes any partial object and marks its metadata row deleted, which also releases its quota reservation.

## Errors

| Status | `error.code` | When |
| --- | --- | --- |
| 503 | `file_storage_not_configured` | `GATEWAY_FILE_STORE=off`. |
| 403 | `file_purpose_disabled` | The purpose's group (Batch files or User files) is turned off in Admin › Settings › Storage. Groups are off by default. |
| 413 | `storage_quota_exceeded` (`type: insufficient_quota`) | The workspace's [storage quota](governance.md#storage) would be exceeded. |
| 413 | `file_too_large` | Above `GATEWAY_FILES_MAX_BYTES`. |
| 400 | `invalid_request_error` | Bad form, purpose, `expires_after` or content, with `param` naming the field. |
| 400 | `unsupported_capability` | `purpose=fine-tune`. |
| 404 | `not_found` | Unknown, expired, deleted or another workspace's file. |
| 503 | `file_storage_unavailable` | The store or database failed. |

Except for 503s, errors carry `x-should-retry: false`, so the official SDKs don't retry them.

## Storage quota

Every byte a workspace keeps counts against its **Storage** limit: live files of every purpose, including batch outputs, plus uploads in progress. The limit is stacked like the other limits: a workspace-type default (1 GiB to start), an optional platform override per workspace and a tighten-only workspace cap. See [governance](governance.md#storage).

- **During the upload.** The quota is checked while the file streams. The upload reserves bytes ahead of what it has written, in steps of up to 8 MiB, bounded by the remaining quota. As soon as the running total would exceed the remaining quota, the upload is aborted, the partial object deleted and `413 storage_quota_exceeded` returned.
- **Concurrent uploads.** Each reservation step runs in a short transaction that takes the catalog advisory lock (shared, the same first lock as admission) and then a per-workspace advisory lock. It counts committed sizes plus the other uploads' reservations, so concurrent uploads to one workspace can never jointly exceed the quota. No I/O happens while the lock is held.
- **What counts.** Expired files stop counting at once. Pending uploads stop counting when they finish or fail, or after the sweeper's one-day cutoff if the gateway crashed mid-upload.
- **Results already paid for.** A writer such as the batch engine may store them with `QuotaMode::CountOnly`: they always count but are never refused.

## Storage usage (not charged)

The maintenance task records storage in append-only hourly rows (`storage_usage_hours`, migration 0020): byte-seconds per workspace and purpose, derived exactly from each file's `committed_at` and `deleted_at`. Each hour is recorded once it has ended, after a 5-minute settle, and recording is replica-safe.

- **Where it shows.** Usage & costs shows it as **GB-days** (1 GB = 2^30 bytes for 24 hours) with the cost state **Not charged**. That's a deliberate state, never "unknown" and never a $0 estimate.
- **Who sees it.** Workspace admins and personal owners see their workspace by purpose. Platform Admins and Auditors see per-workspace totals; personal workspaces show totals only.
- **Future pricing.** An optional installation storage price (micro-USD per GB-day) can be added later as an append-only, effective-dated price. It would apply to hours starting at or after its effective time; earlier hours stay Not charged. Usage rows hold quantities only, so adding a price never rewrites history.

## Dashboard

- **Workspace › Files.** A compact list (name, purpose, size, created, expires, status) with search and a purpose filter, Upload (choose the purpose, then the file), Download and Delete (with confirmation), and the storage bar. Visibility:
  - Workspace admins and personal owners see every file.
  - Other members see files they uploaded themselves, from the dashboard or with their own keys.
  - Platform roles without membership see nothing.
- **Audit.** Dashboard uploads and deletions are audited as `file.uploaded` and `file.deleted`, recording the purpose only.
- **Elsewhere.** The Storage limit and its used/quota bar appear wherever workspace limits are edited. Admin › Settings › Data & privacy › Storage shows bytes stored per purpose group. See [management API](management-api.md#files).

## For the batch engine

The batch engine uses the same `FileStorage` service:

- **Reading input.** It reads a batch input file by gateway id within the caller's workspace (`get`/`open`).
- **Writing output.** It writes `batch_output` files with `create` (quota-counted). Those appear in `GET /v1/files` with purpose `batch_output`.

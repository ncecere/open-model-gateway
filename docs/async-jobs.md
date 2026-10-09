# Async jobs: video generation and batches

The gateway proxies two kinds of long-running provider job:

- **Video generation:** `POST /v1/videos`. **No supported provider at the moment** (see below).
- **Batch Chat Completions:** `POST /v1/files` (`purpose=batch`) and then `POST /v1/batches` (OpenAI).

A job is **one upstream attempt**, with one execution and one durable reservation. It is admitted when it is created and settled when the provider reports a terminal state. Migration `0016_async_jobs.sql` stores metadata only: never prompts, batch lines, outputs, `metadata` values, provider messages or media.

This page describes the current source. It is mock-tested only. No live or paid video or batch request has been made.

> **Video: no supported provider.** OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24, with no replacement ([OpenAI video generation guide](https://developers.openai.com/api/docs/guides/video-generation)). The gateway's video adapter targeted that API, so OpenAI connections no longer offer the `videos` protocol:
>
> - `POST /v1/videos` validates the request, then returns **400 `unsupported_capability`** ("Video generation has no supported provider …") before admission. No job, execution or hold is created, and nothing is sent upstream.
> - Add model shows the **Video** type as disabled ("No supported provider yet") on every connection. Existing video models show readiness **Provider API retired**.
> - Jobs created before the shutdown remain readable from gateway records. Refreshing or downloading them calls the retired API and fails; their holds follow the usual lease rules (unknown, retained).
> - The job infrastructure stays: ids, ownership, the state machine, video-seconds pricing and settlement are unchanged and covered by tests with a fake provider. **An OpenRouter video adapter is planned** (its API uses `duration`, `resolution`, `aspect_ratio` and `polling_url`).

## What is supported

| Route | Behavior |
| --- | --- |
| `POST /v1/videos` | Multipart (as the official SDKs send it) or JSON. Fields: `model`, `prompt` (≤ 32 KiB), `seconds` (`4`, `8` or `12`; default 4) and `size` (`720x1280` (default), `1280x720`, `1024x1792` or `1792x1024`). The gateway always sends `seconds` and `size` upstream, so the billed duration and resolution are known. |
| `GET /v1/videos`, `GET /v1/videos/{id}` | Lists this workspace's jobs; `after`, `limit` 1–100 and `order` are supported. A single read refreshes from the provider until the job is settled. |
| `GET /v1/videos/{id}/content?variant=video\|thumbnail\|spritesheet` | Available for completed jobs only. Streamed through unbuffered, with allowlisted media types. |
| `DELETE /v1/videos/{id}` | Finished jobs only (completed or failed), because the accounting of a running job is still open. |
| `POST /v1/files` | Multipart with `purpose=batch` first, then `file` (JSONL). See [batch input](#batch-input). |
| `GET /v1/files/{id}`, `GET /v1/files/{id}/content` | Batch input, output and error files owned by the workspace. Content is streamed from the provider and never stored. |
| `POST /v1/batches` | `{input_file_id, endpoint:"/v1/chat/completions", completion_window:"24h", metadata?}`. One batch per input file. |
| `GET /v1/batches`, `GET /v1/batches/{id}`, `POST /v1/batches/{id}/cancel` | Gateway records, newest first. Cancel is passed through to the provider, and settlement follows the provider's final state. |

**Not supported** (each returns an explicit `unsupported_capability` or `invalid_request_error`):

- Video: `input_reference`, remix, edits, extensions and characters.
- Batch endpoints other than `/v1/chat/completions`.
- `output_expires_after` and `expires_after`.
- File purposes other than `batch`.
- Batch lines with `stream`, `n > 1`, audio output or modalities, `web_search_options` or `prediction`.
- Video on any provider, for now (see above). OpenRouter video is planned; OpenRouter has no Batch API.

## Identity, ownership and privacy

- Clients see gateway ids only: `video_<32 hex>`, `batch_<32 hex>` and `file-<32 hex>`. Upstream ids never leave the gateway.
- Every lookup is scoped to the calling key's workspace. Another workspace's id returns `404 not_found`, never `403`. Lists come from gateway rows, never the provider account, which spans workspaces.
- Reads, content and cancel need the job's connection to be enabled. Disabling a connection stops client calls and polling for its jobs.
- In Logs, a job is an ordinary request row: its request id is the execution id. The Workspace and Admin Logs filter `workload=jobs` (the **Jobs** toggle) narrows the list to video and batch jobs. Rows carry a `job` summary (`id`, `kind`, `state`, `upstream_status`), and the request page has a **Video job** or **Batch job** card. Visibility rules are those of Logs.

## State machine

Gateway states are `queued`, `in_progress`, `completed`, `failed`, `cancelled` and `expired`. A database trigger only allows forward transitions. Terminal states are final, identity columns are immutable, `settled_at` is written once and rows are never deleted.

| Provider status | Gateway state |
| --- | --- |
| Video `queued`, `in_progress`, `completed`, `failed` | Same name |
| Batch `validating` | `queued` |
| Batch `in_progress`, `finalizing`, `cancelling` | `in_progress` (the provider status is kept as `upstream_status`) |
| Batch `completed`, `failed`, `cancelled`, `expired` | Same name |

An observed regression, such as `in_progress` followed by `queued`, is ignored.

## Reservation and settlement

Both kinds use the ordinary `governance` admission and `finish`. Every budget and budget total (0015) applies unchanged, and settlement is idempotent: an identical replay is a no-op. Rate limits differ from interactive requests (see [job limits](#job-limits)).

**Video.** The workload is `videos`. Pricing v3 adds the meter `output_video_seconds_ms` (`/second`, `/minute` or `/hour`), whose `variant` is the resolution (`720x1280`, …). A line without a variant is the default.

- **Hold:** requested seconds × the highest variant rate, plus a request ceiling of 1. A meter without a price is unbounded, so admission is refused under a budget.
- **Completed:** settles from the provider's `seconds` at the reported size's line.
- **Missing or unparseable duration:** unknown; the hold is retained.
- **Longer than requested:** the evidence is kept and the hold is retained.
- **Failed:** the attempt is failed without usage. It stays unknown unless every line of the price is free or not applicable.

**Batch.** The workload is `batches`; the price is any token price (v1/v2/v3).

- **Hold:** `lines × input_token_limit` input, plus the sum of every line's `max_completion_tokens`/`max_tokens`. Each line's maximum must be within the price's `output_token_limit`. The reservation records its request count (`governance_reservations.request_count`), so settlement checks the aggregated usage against `lines × input ceiling`.
- **Completed with provider `usage`:** exact settlement (cached reads included; this schema has no cache writes).
- **Completed without `usage`:** the background poller streams the output file and sums each line's `response.body.usage`, discarding bodies. Any line without valid usage, a line count that differs from `request_counts.completed`, or more than `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` leaves the usage unknown and the hold retained. A client read never scans the output file while a poller is configured.
- **Failed:** failed without usage (unknown unless the price is entirely free).
- **Cancelled or expired:** any reported usage is recorded as evidence (a known floor), and the hold is retained as unknown for reconciliation, because partial work may be billed.

**Leases.** A job's lease is extended to its poll deadline (video: 6 hours; batch: 26 hours) plus 10 minutes. The extension runs under the installation lock and never shortens a lease. If the deadline passes without a terminal state, lease reconciliation marks the reservation unknown and keeps the hold. A later terminal observation records the job state but leaves that reservation alone.

Known limits:

- Jobs never fail over.
- Tiered (`min_prompt_tokens`) batch prices are applied to aggregated batch input and can over-estimate.

## Job limits

Long-running jobs do not consume interactive limits. Migration `0018_job_limits.sql` adds **Jobs at once** (`concurrent_jobs`) to every policy layer; see [governance](governance.md#jobs-at-once).

| Limit | Video and batch jobs |
| --- | --- |
| Jobs at once | Each active job holds one slot. Type defaults allow 2 per workspace. |
| Requests at once | Held only while the create call runs, then released. |
| Requests and tokens per minute | Not checked, and job reservations never count toward them. |
| Budgets | Fully applied: the conservative ceiling is reserved at admission, as before. |

A slot is released when the job reaches a terminal state (`completed`, `failed`, `cancelled`, `expired`), as soon as cancel is requested, or when the lease expires. A refused job returns `429` with `error.code` `job_limit_exceeded` (`rate_limit_error` type, retryable) and a message naming the scope kind: API key, workspace or installation. Nothing is sent upstream, and a claimed batch input file is released for reuse.

Edit the limit with the other limits: Admin › Settings › Defaults & limits (installation and type defaults), a workspace's platform override, Workspace › Settings › Limits (tighten only) and each key's limits. Effective access shows it per layer.

## Batch input

The gateway reads `POST /v1/files` as a stream. It holds at most one line (≤ 4 MiB) plus one network chunk and writes each validated line to the provider's multipart upload. Nothing is stored or logged.

Each line must be exactly `{custom_id, method:"POST", url:"/v1/chat/completions", body}` with:

- `body.model`: the same gateway API model name on every line, for a model declaring `batches`. It is rewritten to the route's upstream model.
- `body.messages`: an array.
- Exactly one positive integer `max_completion_tokens` or `max_tokens`.

The first line selects the route (no failover). Before the upload is allowed to complete, the whole file's hold is previewed against the route's current price. An unpriced, unbounded or over-ceiling file is refused, and the upstream body is aborted, so the provider never receives a complete form. Limits: `GATEWAY_MAX_BATCH_FILE_BYTES` (default 200 MiB) and 50,000 lines.

## Poller

`serve` starts a background poller on the runtime database role. Every interval it:

1. Claims at most 32 due, unsettled jobs that are still within their poll deadline (`FOR UPDATE SKIP LOCKED`, so multiple gateway processes never poll the same job at once).
2. Refreshes each job from its provider with a bounded deadline.
3. Applies the state machine and settles terminal jobs.

Failures back off per job, up to 32 × the interval. With the poller disabled, client reads still refresh and settle jobs, and then a batch read may scan the output file.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_JOB_POLL_INTERVAL_SECONDS` | `30` | Poller interval, `0`–`3600`; `0` disables the poller. |
| `GATEWAY_MAX_BODY_BYTES_VIDEOS` | 2 MiB | `POST /v1/videos` body cap (1 KiB–64 MiB). |
| `GATEWAY_MAX_BATCH_FILE_BYTES` | 200 MiB | Batch input file cap (1 KiB–512 MiB). |
| `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` | 1 GiB | Largest output file scanned for usage (1 MiB–16 GiB). |

## Model setup and grants

- **Add model:** the **Video** and **Batch** types each have one protocol (`videos`, `batches`). Batch is offered on OpenAI connections. Video is disabled on every connection ("No supported provider yet"). The price editor shows the video-seconds meter with resolution tiers for video models, and token meters for batch models.
- **Runtime grants** (`deploy/staging/runtime-grants.sql`): `SELECT`/`INSERT` on `async_jobs` and `async_job_files`, `UPDATE` of their state columns only, `UPDATE(lease_expires_at)` on reservations and `EXECUTE` on `valid_upstream_job_id`. No `DELETE` or `TRUNCATE`.
- **Probes:** `verify-privileges.sql` and the ignored `runtime_privileges` test check these grants and run a full video and batch lifecycle as the runtime role.

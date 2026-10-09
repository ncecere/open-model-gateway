# File storage

The gateway can store files it owns, such as batch inputs and outputs, exports, the installation logo and video outputs, in an encrypted object store. Files can hold customer prompts and results, so every object is encrypted by the gateway before any backend sees it, whatever the backend.

The store is **off by default**. When it's off, nothing is written, and the purposes that hold customer content can't be turned on.

What's implemented today: the store itself (local disk and S3-compatible), encryption, metadata and retention (`stored_files`, migration 0019), the retention sweeper, the `files verify` and `files sweep` commands, metrics, and Admin › Settings › Data & privacy › Storage. **No feature writes files yet.** Gateway-run batch jobs, the Files API, CSV exports, the Settings logo and video outputs will use the store as they're built (see [Consumers](#consumers)).

## Configuration

Everything comes from the server environment. Invalid configuration stops startup with an error that names the variable, never its value.

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_FILE_STORE` | `off` | `off`, `local` or `s3`. |
| `GATEWAY_FILE_ENCRYPTION_KEYS_ENV` | — | Required when the store is on. The **name** of the variable that holds the master keys (see [Encryption keys](#encryption-keys)). |
| `GATEWAY_FILE_STORE_DIR` | — | `local`: an absolute directory. It's created with mode 0700 if it doesn't exist. If it exists, it must be a real directory (not a symlink), owned by the gateway user and not accessible to group or others. |
| `GATEWAY_S3_BUCKET` | — | `s3`: the bucket. The gateway never creates it. |
| `GATEWAY_S3_PREFIX` | none | Key prefix, made of path segments using `A-Z a-z 0-9 . _ -`. |
| `GATEWAY_S3_REGION` | `us-east-1` | An AWS region code when no endpoint is set. With an endpoint, any short lowercase code (`auto` for R2, `us-central1` for GCS). |
| `GATEWAY_S3_ENDPOINT` | unset (AWS S3) | The exact origin `http(s)://host[:port]` of an S3-compatible store or an AWS VPC endpoint. It must be listed in `GATEWAY_S3_ENDPOINT_ALLOWLIST`. |
| `GATEWAY_S3_ENDPOINT_ALLOWLIST` | empty | Comma-separated exact origins. An `http://` entry is the server-side approval for plaintext HTTP. Use it only on a private network. |
| `GATEWAY_S3_FORCE_PATH_STYLE` | `true` with an endpoint, otherwise `false` | `true` or `false`. MinIO, RustFS and Ceph usually need path style. |
| `GATEWAY_S3_AUTH` | `aws:default` | `aws:default`, `aws:profile:<name>`, `aws:role:<role-arn>`, or `static`. |
| `GATEWAY_S3_ACCESS_KEY_ID_ENV`, `GATEWAY_S3_SECRET_ACCESS_KEY_ENV` | — | `static` only: the **names** of the variables that hold the access key ID and secret. The values are read at startup and never stored or logged. |
| `GATEWAY_AWS_PROFILE_ALLOWLIST` | empty | Shared with [Bedrock](bedrock.md): profiles that `aws:profile:<name>` may use. |
| `GATEWAY_S3_CA_FILE` | none | A PEM bundle of extra trust anchors for a private endpoint's TLS certificate. |

The AWS identity modes are parsed and allowlisted by the same code as Bedrock. See [Bedrock identity modes](bedrock.md#identity-modes). `aws:role:` uses STS AssumeRole with the server's default chain. The S3 client is built directly, so `AWS_ENDPOINT_URL*` and profile endpoint overrides are never inherited.

### Amazon S3

```sh
GATEWAY_FILE_STORE=s3
GATEWAY_S3_BUCKET=acme-gateway-files
GATEWAY_S3_PREFIX=prod
GATEWAY_S3_REGION=eu-west-1
GATEWAY_S3_AUTH=aws:role:arn:aws:iam::123456789012:role/gateway-files
GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS
GATEWAY_FILE_KEYS=k2026a:<base64 32 bytes>
```

The identity needs `s3:PutObject`, `s3:GetObject`, `s3:DeleteObject` and `s3:AbortMultipartUpload` on `arn:aws:s3:::acme-gateway-files/prod/*`. Turn on Block Public Access. Add a lifecycle rule that aborts incomplete multipart uploads after one day. Bucket versioning keeps "deleted" objects as noncurrent versions, so if you use it, expire noncurrent versions quickly. Server-side encryption (SSE-S3 or SSE-KMS) is optional, on top of the gateway's own encryption.

### MinIO

```sh
GATEWAY_FILE_STORE=s3
GATEWAY_S3_BUCKET=gateway-files
GATEWAY_S3_ENDPOINT=https://minio.internal:9000
GATEWAY_S3_ENDPOINT_ALLOWLIST=https://minio.internal:9000
GATEWAY_S3_AUTH=static
GATEWAY_S3_ACCESS_KEY_ID_ENV=MINIO_GATEWAY_KEY_ID
GATEWAY_S3_SECRET_ACCESS_KEY_ENV=MINIO_GATEWAY_SECRET
GATEWAY_S3_CA_FILE=/etc/gateway/minio-ca.pem   # if MinIO uses a private CA
GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS
```

Create a dedicated MinIO user whose policy is limited to the bucket. Don't use the root credentials. Region defaults to `us-east-1`, and path style is on because an endpoint is set.

### RustFS

```sh
GATEWAY_FILE_STORE=s3
GATEWAY_S3_BUCKET=gateway-files
GATEWAY_S3_ENDPOINT=http://rustfs.storage.svc:9000
GATEWAY_S3_ENDPOINT_ALLOWLIST=http://rustfs.storage.svc:9000
GATEWAY_S3_AUTH=static
GATEWAY_S3_ACCESS_KEY_ID_ENV=RUSTFS_GATEWAY_KEY_ID
GATEWAY_S3_SECRET_ACCESS_KEY_ENV=RUSTFS_GATEWAY_SECRET
GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS
```

This example uses plain HTTP inside a cluster network. The allowlist entry is the approval for that, and the objects are still encrypted. Prefer HTTPS when the path leaves the cluster.

### Cloudflare R2

```sh
GATEWAY_FILE_STORE=s3
GATEWAY_S3_BUCKET=gateway-files
GATEWAY_S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
GATEWAY_S3_ENDPOINT_ALLOWLIST=https://<account-id>.r2.cloudflarestorage.com
GATEWAY_S3_REGION=auto
GATEWAY_S3_AUTH=static
GATEWAY_S3_ACCESS_KEY_ID_ENV=R2_KEY_ID
GATEWAY_S3_SECRET_ACCESS_KEY_ENV=R2_SECRET
GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS
```

Use an R2 API token scoped to the bucket with Object Read & Write.

Wasabi (`https://s3.<region>.wasabisys.com`), Ceph RGW and GCS interoperability (`https://storage.googleapis.com` with HMAC keys, region `auto`) are configured the same way: an allowlisted endpoint and `static` keys. These three are not exercised by automated tests.

### Local disk

```sh
GATEWAY_FILE_STORE=local
GATEWAY_FILE_STORE_DIR=/var/lib/open-model-gateway/files
GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS
```

Objects are stored at `<dir>/<purpose>/<workspace-id|installation>/<uuid>`. Directories are 0700 and files 0600. A write goes to a unique temp file (`O_EXCL|O_NOFOLLOW`), is fsynced, renamed into place atomically, and the directory is fsynced. Every directory component is checked with `lstat`, and files are opened with `O_NOFOLLOW`, so no symlink below the root is followed and no path can leave the root. The local backend is a single-host store: every replica would need the same directory. Use S3 for more than one replica. It requires a Unix host.

## Encryption

Every object is encrypted with streaming, chunked AES-256-GCM (RustCrypto `aes-gcm` 0.10.3, a STREAM construction):

- Each object gets a fresh random 256-bit data key. The active master key wraps it with AES-256-GCM.
- The 106-byte header holds a magic value, the version, the chunk size, the master key ID, a random 7-byte nonce prefix and the wrapped data key. The wrap is authenticated together with the version, chunk size, key ID, nonce prefix and the object key, so a header can't be moved to another object.
- Plaintext is sealed in 64 KiB chunks. The nonce is the prefix, a 32-bit chunk counter and a last-chunk flag. Each chunk's associated data binds the object key. Flipping a bit, truncating (even at a chunk boundary), reordering, dropping or appending chunks, or moving an object to another key all fail with `integrity`.
- Reads authenticate each chunk before yielding it. A damaged object ends the stream with an error, never a clean end, so consumers must treat any stream error as failure of the whole object and discard partial output.
- `put` returns the plaintext size and SHA-256. They're recorded in `stored_files`.

Overhead is 106 bytes plus 16 bytes per 64 KiB chunk.

### Encryption keys

`GATEWAY_FILE_ENCRYPTION_KEYS_ENV` names a variable whose value is `kid:base64key[,kid:base64key…]`:

- **Key ID:** 1 to 32 characters of `A-Z a-z 0-9 . _ -`.
- **Key:** standard base64 of exactly 32 random bytes, for example from `openssl rand -base64 32`.
- **Order:** the first key encrypts new objects. The others are decrypt-only, which is how you rotate. At most 16 keys.

Keep the key variable in your secret manager, not in the gateway's database or backups. **Losing a key makes every object encrypted under it unreadable.** See [operations](operations.md#file-store-encryption-keys) for backup and rotation.

## Metadata, purposes and retention

`stored_files` (migration 0019) holds **metadata only**: object key, purpose, owning workspace (NULL for installation scope), the creating user and API key, the sanitized original filename, content type, plaintext size and SHA-256, backend, encryption key ID, `created_at`, `committed_at`, an explicit `expires_at`, `deleted_at`, and the delete-attempt counters. The partial index on live rows per workspace makes "bytes stored by this workspace" cheap, ready for a future storage quota.

| Purpose | Scope | Settings group | Default retention | Allow toggle |
| --- | --- | --- | --- | --- |
| `batch_input`, `batch_output` | workspace | Batch files | 7 days | yes, off by default |
| `video_output` | workspace | Video outputs | 7 days | yes, off by default |
| `user_file` (Files API) | workspace | User files | 30 days | yes, off by default |
| `export` | workspace or installation | Exports | 1 day | follows the backend being on |
| `branding` | installation | Branding | never expires | follows the backend being on |

Retention can be set from 1 to 365 days per group (both batch purposes share one setting). A file expires at its explicit `expires_at` or at `created_at` plus the group's **current** retention, whichever comes first. Shortening retention therefore applies to existing files too. An expired file is unreadable at once, even before the sweeper removes it.

Object keys are generated by the gateway as `<purpose>/<workspace-id|installation>/<uuid>`, using only lowercase letters, digits, `_`, `-` and `/`. They're never derived from client input. Purposes are an extensible enum: add a value with a migration that widens the `stored_files` CHECK constraint.

Rows are never deleted. Deleting a file removes the object, then sets `deleted_at` and clears the filename and content type. The runtime role has no `DELETE` on `stored_files`, identity and ownership columns can't be updated, size and hash are written once, and deleted rows are final (enforced by a trigger and by `deploy/staging/runtime-grants.sql`).

### Write path and sweeper

1. Insert a pending row: identity, owner, purpose and key ID.
2. Stream the object.
3. Record the size and SHA-256 once (`committed_at`).

If the upload fails, the row is marked deleted. If the gateway crashes mid-upload, it leaves a pending row, and the sweeper removes the object after a day.

`serve` sweeps every minute, at most 200 files per run. It claims due rows in a short transaction, where `last_delete_attempt_at` acts as a 5-minute lease, so replicas don't collide and no lock is held across network I/O. It then deletes the objects (idempotently) and marks each row deleted, or increments `delete_attempts` and records `last_delete_error` (`store_off`, `backend_mismatch`, `unavailable`, `denied`, …). Rows recorded under another backend than the one configured are never marked deleted without deleting their object.

```sh
open-model-gateway files sweep --once [--limit 1000]   # one sweep, prints {claimed, deleted, failed}
open-model-gateway files verify [--limit 100000]       # read-only; exits nonzero on problems
```

`files verify` checks every committed, undeleted file. It reports objects that are missing, sizes that don't match, files recorded under another backend, key IDs that are no longer configured, store errors, stale pending uploads, rows with failed deletes, and a per-key-ID count (useful for rotation). It does not list the bucket, so stray objects that have no row aren't found. Use a bucket lifecycle rule or inventory for those.

## Network behavior

- **Attempts and redirects:** one attempt per request (no SDK or reqwest retries), no redirects, and no ambient proxy (`HTTP(S)_PROXY` is ignored).
- **Timeouts:** 10-second connect, 60-second read and 300-second operation timeouts.
- **Small objects:** objects up to 8 MiB of ciphertext use one `PutObject`.
- **Large objects:** larger objects use a multipart upload in 8 MiB parts, which bounds one object to about 78 GiB. Any failure, size-limit breach or cancellation aborts the upload (`AbortMultipartUpload`; on cancellation it's spawned with a 30-second limit).
- **Request bodies:** they're in memory with an exact `Content-Length`, at most one part.
- **Response bodies:** a successful `GetObject` body streams. Every other response body is capped at 1 MiB.
- **Errors:** they're fixed categories (`not_found`, `denied`, `unavailable`, `timeout`, `integrity`, `key_unavailable`, `too_large`, `unsafe_path`, …). Bucket names, paths, keys and upstream messages never appear in errors, logs or metrics.

### Compatibility

The SDK is configured to avoid newer AWS-only defaults that older MinIO, RustFS and Ceph releases reject:

- Flexible request checksums are calculated, and response checksums validated, only **when required**. Requests carry no `x-amz-checksum-*`, `x-amz-sdk-checksum-algorithm`, trailers or `aws-chunked` bodies.
- S3 Express session auth and multi-region access points are disabled.
- Path-style addressing is on when an endpoint is set.
- Stalled-stream protection is off; the reqwest read timeout bounds slow streams.

The gateway's own encryption provides integrity, so it doesn't depend on S3 checksums.

Tests:

- **Real stores:** `apps/gateway/tests/filestore_s3.rs` starts **MinIO** (`pgsty/minio:RELEASE.2026-08-04T00-00-00Z`, pinned by digest) and **RustFS** (`rustfs/rustfs:1.0.1`, pinned by digest) on random loopback ports and checks round trips at every chunk and part boundary, multipart uploads, that only ciphertext is stored under the prefix, tamper detection, idempotent deletes, multipart abort on failure and size limit, denial on wrong credentials, and health-probe cleanup. Upstream MinIO stopped publishing community images in October 2025, so the MinIO image is the pgsty community build of the upstream source. Override the images with `FILESTORE_MINIO_IMAGE` and `FILESTORE_RUSTFS_IMAGE`.
- **Docker:** the tests skip with a message when Docker is unavailable. CI sets `FILESTORE_REQUIRE_DOCKER=1`, so a missing Docker fails the run.
- **Real AWS:** an opt-in test runs only with `FILESTORE_AWS_TEST_BUCKET` set (optionally `FILESTORE_AWS_TEST_REGION` and `FILESTORE_AWS_TEST_AUTH`). It never runs in CI.
- **Wire behavior:** a loopback mock (`filestore/s3/tests.rs`) asserts the wire behavior: path-style URLs, SigV4, no checksum headers, one attempt on 503, no redirect following, and abort on a failed multipart upload.

### Crate choice

The backend uses **`aws-sdk-s3` (pinned `=1.122.0`)**, not `object_store`:

- **Same identity code as Bedrock:** it shares the gateway's AWS SDK generation (`aws-config`, smithy runtime and SigV4 crates already locked for Bedrock), so `aws:default`, `aws:profile:` and `aws:role:` use exactly the same identity code and allowlists. `object_store` would need a separate credential bridge.
- **Compatibility switches:** it exposes the compatibility switches directly (`request_checksum_calculation`, `response_checksum_validation`, `force_path_style`, S3 Express and MRAP toggles).
- **Transport:** it accepts the same bounded reqwest transport pattern as Bedrock (no redirects, proxy or retries).

1.122.0 is the S3 crate from the same SDK release as the locked STS and SSO crates, so adding it changed no existing AWS dependency (the lockfile only moved `crc` 3.4.0 → 3.3.0, which `crc-fast` requires).

## Admin › Settings › Data & privacy › Storage

The **Storage** card shows:

- **Backend:** Off, Local disk, Amazon S3, or S3-compatible. For S3, the bucket and the endpoint **host** only, marked "(HTTP)" when the endpoint uses plain HTTP.
- **Encryption key:** the active key ID and how many decrypt-only keys are configured.
- **Health:** the last health check, shown only if it was run against the current configuration.

Below that is a compact table, one row per purpose group: retention in days, an Allow switch for groups that hold customer content (batch files, video outputs, user files), and the bytes stored.

Platform Admins can:

- **Test storage:** writes, reads and deletes a 1 KiB random object under the prefix. At most 5 tests per admin per minute and 60 per hour, counted from the audit log.
- **Turn on a customer-content group:** this needs a configured backend and a passing health check, which runs before saving, never under the installation lock.

Auditors see the same values read-only. Changes are audited as `settings.storage_updated` and `settings.storage_test`, recording only counts and the backend kind. See [settings](settings.md#storage) and the [management API](management-api.md#installation-settings).

Turning a group off stops new files of that group. Existing files stay readable until their retention ends.

## Consumers

Consumers use `filestore::files::FileStorage`, which pairs the database with the configured store. It's built from the `FileStoreRuntime` that `serve` creates and passes to request handlers as an axum `Extension`:

```rust
let files = FileStorage::new(store.clone(), runtime.clone());
if files.accepts(Purpose::BatchOutput).await? { /* … */ }
let stored = files.create(NewFile { created_by_api_key_id: Some(key), max_bytes: Some(limit),
    ..NewFile::new(Purpose::BatchOutput, Some(workspace_id)) }, body).await?;
let (meta, stream) = files.open(stored.id, Some(workspace_id)).await?; // exact scope
files.delete(stored.id, Some(workspace_id)).await?;
files.workspace_stored_bytes(workspace_id).await?;                     // future quota
```

The raw `FileStore` trait (`put`, `get`, `delete`, `head`, `health`) is the low-level interface. Tests can use `filestore::memory_store()` or `FileStoreRuntime::memory()`, both encrypted with a random key.

Planned uses, none wired yet:

- **Batch engine:** `batch_input` for uploaded JSONL and `batch_output` for results and errors, scoped to the workspace with the creating key recorded. Both require "Batch files" to be allowed.
- **Files API (`/v1/files`):** `user_file` (OpenAI `user_data`, `vision`, `assistants`, `evals`) and `batch_input` (`batch`), mapped by `Purpose::from_openai`. It keeps the sanitized original filename and content type, honors a client `expires_after` through `NewFile::expires_at`, and can enforce a per-workspace quota from `workspace_stored_bytes`.
- **CSV exports:** `export` in the requesting workspace (or installation scope for platform reports), with an explicit short `expires_at` for download links. Retention defaults to 1 day.
- **Settings logo:** `branding/installation/<uuid>`, which never expires. The general settings will reference the file ID instead of an external URL. Re-encrypting non-expiring objects for key retirement is a planned command (see operations).
- **Video outputs:** `video_output` copies of provider results, scoped to the workspace. Requires "Video outputs" to be allowed.

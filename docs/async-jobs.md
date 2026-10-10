# Async jobs: video generation and batches

The gateway proxies two kinds of long-running provider job:

- **Video generation:** `POST /v1/videos`. **No supported provider at the moment** (see below).
- **Batches:** `POST /v1/batches` from a gateway file (`purpose=batch`, [Files API](files-api.md)), for any model. They run natively on a provider batch API or line by line through the gateway. See [batches](batches.md); this page keeps what batches share with video jobs.

A video job and a native batch are **one upstream attempt**, with one execution and one durable reservation, admitted when created and settled when the provider reports a terminal state. A gateway-run batch's lines are separate attempts with their own reservations ([batches](batches.md#pricing-and-budgets)). Migrations `0016_async_jobs.sql` and `0021_batch_engine.sql` store metadata only: never prompts, batch lines, outputs, `metadata` values, provider messages or media.

This page describes the current source. It is mock-tested. No live video request has been made. Batches had one small capped live check (see [batches](batches.md)).

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
| `/v1/files` | The gateway-owned [Files API](files-api.md): `purpose=batch` uploads are stored (encrypted) in the gateway's file store; batch output and error files appear there as `batch_output`. |
| `/v1/batches` | Create, retrieve, list and cancel for `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings` and `/v1/messages` lines; see [batches](batches.md). |

**Not supported** (each returns an explicit `unsupported_capability` or `invalid_request_error`):

- Video: `input_reference`, remix, edits, extensions and characters.
- Batch `output_expires_after`, and batch lines the endpoint itself rejects (streaming, `n > 1`, unsupported fields): see [batches](batches.md#validation-report).
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

**Batch.** The workload is `batches`. Holds, batch price lists, line transfers and settlement are described in [batches](batches.md#pricing-and-budgets). A native batch settles like a job: from the provider's aggregate usage, else the sum of its decoded lines' usage. Cancelled and expired batches record usage as evidence and keep the hold. Batches created by the 0016 passthrough (`batch_mode` NULL) are still polled and settle only from the provider's aggregate usage (otherwise unknown, hold retained).

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

A slot is released when the job reaches a terminal state (`completed`, `failed`, `cancelled`, `expired`), as soon as cancel is requested, or when the lease expires. A refused job returns `429` with `error.code` `job_limit_exceeded` (`rate_limit_error` type, retryable) and a message naming the scope kind: API key or workspace. Nothing is sent upstream.

Edit the limit with the other limits: Admin › Settings › Defaults & limits (type defaults), a workspace's platform override, Workspace › Settings › Limits (tighten only) and each key's limits. Effective access shows it per layer.

## Batch input

Batch inputs are gateway files ([Files API](files-api.md)). `POST /v1/batches` streams the file once, validates every line and copies the valid lines to the batch's private copy; see [batches](batches.md#validation-report). Limits: 50,000 lines and 4 MiB per line.

## Poller

`serve` starts a background poller on the runtime database role. Every interval it:

1. Claims at most 32 due, unsettled provider jobs (videos and native batches; gateway-run batches belong to the [batch runner](batches.md#execution-of-gateway-run-batches)) that are still within their poll deadline (`FOR UPDATE SKIP LOCKED`, so multiple gateway processes never poll the same job at once).
2. Refreshes each job from its provider with a bounded deadline. A native batch is first submitted (once); when it ends, its results are collected into gateway files.
3. Applies the state machine and settles terminal jobs.

Failures back off per job, up to 32 × the interval. With the poller disabled, video reads still refresh and settle jobs, but native batches are not processed.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_JOB_POLL_INTERVAL_SECONDS` | `30` | Poller interval, `0`–`3600`; `0` disables the poller. |
| `GATEWAY_MAX_BODY_BYTES_VIDEOS` | 2 MiB | `POST /v1/videos` body cap (1 KiB–64 MiB). |
| `GATEWAY_BATCH_WORKERS`, `GATEWAY_BATCH_CONCURRENCY` | `4`, `2` | Gateway-run batch lines at once per process and per batch ([batches](batches.md#configuration)). Each route also has its own batch capacity and gates ([scheduling](batches.md#scheduling-on-self-hosted-models)). |
| `GATEWAY_MAX_BATCH_FILE_BYTES`, `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` | 200 MiB, 1 GiB | Still validated at startup but no longer used: inputs are Files API uploads and native outputs are decoded line by line. |

## Model setup and grants

- **Add model:** the **Video** and **Batch** types each have one protocol (`videos`, `batches`). A Batch model is no longer needed: any chat or embeddings model can be batched, and a Batch model runs Chat Completions natively only. Video is disabled on every connection ("No supported provider yet"). The price editor shows the video-seconds meter with resolution tiers for video models, and token meters for batch models.
- **Runtime grants** (`deploy/staging/runtime-grants.sql`): `SELECT`/`INSERT` on `async_jobs` and `async_job_files`, `UPDATE` of their state columns only, `UPDATE(lease_expires_at)` on reservations and `EXECUTE` on `valid_upstream_job_id`. No `DELETE` or `TRUNCATE`.
- **Batch engine grants (0021):** see [batches](batches.md#storage-and-grants).
- **Probes:** `verify-privileges.sql` and the ignored `runtime_privileges` test check these grants and run a video job, native batches and a gateway-run batch as the runtime role.

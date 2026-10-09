# Batches

`/v1/batches` runs OpenAI-format batches for any model the gateway serves. A batch reads a gateway file (`purpose=batch`, uploaded with the [Files API](files-api.md)) and writes its results to gateway files (`batch_output`). It runs in one of two modes, chosen per batch:

- **Native:** on the provider's batch API (OpenAI Batch or Anthropic Message Batches), when every line routes to one deployment whose adapter supports it. Native lines are charged the route's **batch price list** when one is published.
- **Gateway-run:** line by line through the gateway's normal inference engine, with each line its own attempt and reservation, at **standard prices**. Every other batch runs this way, including batches that mix models.

This page describes the current source (migration `0021_batch_engine.sql`). It is mock-tested only: no live or paid batch request has been made.

## API

| Route | Behavior |
| --- | --- |
| `POST /v1/batches` | `{input_file_id, endpoint, completion_window:"24h", metadata?}`. Validates the whole input file, admits the batch and returns it (`validating`). An invalid file returns 400 with a line-numbered report. |
| `GET /v1/batches` | This workspace's batches, newest first (`after`, `limit` 1–100). |
| `GET /v1/batches/{id}` | One batch, from gateway records. |
| `POST /v1/batches/{id}/cancel` | Stops the batch (see [Cancel](#cancel)). |

- **Endpoints:** `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings` and `/v1/messages` (Anthropic shape). Every line uses the batch's endpoint and is validated exactly like an interactive request to it. Lines are never streamed. Chat Completions lines may name their maximum `max_tokens` (legacy) or `max_completion_tokens`, not both.
- **Lines:** `{custom_id, method:"POST", url, body}`, at most 50,000 per file and 4 MiB each. Blank lines are skipped.
- **Ids:** `batch_<32 hex>` and `file-<32 hex>` gateway ids. Upstream ids and client `custom_id`s are never sent to or exposed from the other side: native lines are sent upstream as `l0`, `l1`, … and mapped back.
- **`metadata`:** at most 16 string values. It is validated but **not stored**, so the batch object returns `metadata: null`. Two keys are gateway options:
  - `omg_mode`: `auto` (default) or `gateway` (always run line by line).
  - `omg_retries`: `0` (default), `1` or `2` (see [Failures and retries](#failures-and-retries)).
- **Not supported:** `output_expires_after` (400 `unsupported_capability`), other completion windows, and file purposes other than `batch` as input.

### Validation report

Creating a batch streams the input file once. Every line is checked: shape, unique `custom_id`, the endpoint's body contract, a model available to the key and serving the endpoint, a price with ceilings (the batch must be reserved for), and an output maximum within the price's output ceiling. An invalid file creates nothing:

```json
{"error": {"type": "invalid_request_error", "code": "invalid_batch_input", "param": "input_file_id", "message": "…"},
 "errors": {"object": "list", "data": [{"code": "duplicate_custom_id", "message": "custom_id must be unique within the file.", "line": 3, "param": null}]}}
```

`line` is the file's physical line number (1-based). At most 20 problems are reported. Messages are fixed strings; they never echo line content. Codes: `invalid_json`, `invalid_line`, `invalid_custom_id`, `duplicate_custom_id`, `invalid_method`, `invalid_url`, `invalid_body`, `unsupported_feature`, `missing_max_tokens`, `model_not_found`, `model_unsupported`, `model_not_priced`, `max_tokens_too_large`, `line_too_large`, `too_many_lines`, `empty_file`.

Valid lines are copied to the batch's **private copy**: an encrypted file without an API purpose, so `/v1/files` never lists it. The client may delete its input file at any time without affecting the batch. Batches need the [file store](file-storage.md) with "Batch files" allowed; otherwise creation returns 400 `batch_files_disabled`.

## Modes

| | Native | Gateway-run |
| --- | --- | --- |
| When | Every line routes to the same deployment, its adapter supports the endpoint natively and can encode every line, and `omg_mode` is not `gateway` | Everything else, including mixed-model batches |
| Upstream | One provider batch (one upstream attempt) | One request per line through the inference engine (routing, deadlines, explicit failover policy) |
| Accounting | One reservation for the batch, pinned to the price version and its batch list | One reservation per line attempt, transferred out of the batch's ceiling |
| Prices | The batch price list when published, else standard prices (shown as "No batch price") | Standard prices |

- **OpenAI** (native): the gateway encodes each line for its upstream model (chat-like endpoints as Chat Completions, embeddings as Embeddings), uploads one streamed JSONL file, creates the batch, and when it ends downloads the output and error files, decodes every line, and then deletes the provider's input, output and error files.
- **Anthropic** (native, chat-like endpoints): one streamed `POST /v1/messages/batches` body, then `GET …/results` when `processing_status` is `ended`, then `DELETE` of the batch.
- **Models set up as "Batch"** (protocol `batches`, from 0016) run Chat Completions natively only.

A native batch is created first and submitted by the [poller](async-jobs.md#poller). The submission is claimed once; one that is interrupted (for example by a restart) is never repeated, because the provider may have received it. That batch fails with `submission_interrupted` and keeps its hold for reconciliation.

## Execution of gateway-run batches

The batch runner (`serve`, one per process) claims runnable batches with a renewable lease, so a crashed process's batches resume elsewhere.

- **Workers:** at most `GATEWAY_BATCH_WORKERS` lines run at once per process (default 4; `0` turns the runner off) and at most `GATEWAY_BATCH_CONCURRENCY` lines of one batch (default 2).
- **Exactly once:** a line is claimed by inserting its `batch_lines` row before it runs, so it can never run twice, even with several runners. Rows hold metadata only: line number, state, attempts, status and error codes, the execution id and the result segment.
- **Restarts:** a line a stopped runner left `running` becomes `interrupted` and is never executed again. Its attempt's hold is reconciled as unknown when its lease expires. Lines that finished but whose results were not yet stored are reported as `result_unavailable`.
- **Results:** finished lines are written to encrypted result segments (internal files) as they complete. When the batch ends they are merged into the output and error files and deleted.
- **Rate limits:** batch lines are exempt from requests and tokens per minute and from requests at once. A provider 429 pauses that batch's dispatch (1 s, doubling up to 60 s); nothing is resent.

### Failures and retries

No line is retried implicitly: a failed line is recorded failed and listed in the error file with the status and body the endpoint returns interactively. With `omg_retries` set, a line that failed with a retryable error (429, upstream unavailable, timeout) runs again up to that many times, after a 2 s then 4 s pause. Every retry is a new attempt with its own reservation.

### Cancel

- **Gateway-run:** no new lines start, running lines finish, and the batch ends `cancelled`. Lines that never ran are listed in the error file with `batch_cancelled`.
- **Native, not yet submitted:** cancelled at once. Nothing reached the provider, so the reservation settles at zero.
- **Native, submitted:** cancelled upstream. The poller collects whatever results the provider returns.

The "Jobs at once" slot is released as soon as cancel is requested.

### Stops

- **Budget:** a line whose admission is refused for budget (`budget_exceeded` or `unresolved_usage`) stops the batch. It ends `failed` with error code `budget_exceeded`. Results of finished lines are kept, and lines that never ran are listed with `budget_exceeded`.
- **Window:** a batch still running 24 hours after creation stops and ends `expired` (`batch_expired` for lines that never ran).

## Results

The output file holds successful lines and the error file holds everything else. Each file exists only when it has at least one line. Both are `batch_output` files of the workspace (listed by `/v1/files`, encrypted, retained like batch files). The order follows completion, not the input.

```json
{"id": "batch_req_…", "custom_id": "your-id", "response": {"status_code": 200, "request_id": "…", "body": {…}}, "error": null}
{"id": "batch_req_…", "custom_id": "other", "response": null, "error": {"code": "batch_cancelled", "message": "…"}}
```

- **Bodies:** the endpoint's normal response shape, with the public model name.
- **Failed lines:** the error status and body. Native provider errors carry a sanitized code and a fixed message, never the provider's text.
- **Counts:** `request_counts` is `{total, completed, failed}`. `failed` counts lines that ran and failed (and interrupted lines). Lines that never ran appear only in the error file.
- **Statuses:** `validating`, `in_progress`, `finalizing`, `cancelling`, then `completed`, `failed`, `cancelled` or `expired`.

Results count toward the workspace's [storage](files-api.md) but never fail on its quota, because the work was already paid for.

## Pricing and budgets

**Batch price lists.** A pricing v3 version can carry `batch_price_lines` beside `price_lines` ([governance API](governance-api.md#batch-price-lists)). It must price exactly the same meters, and it shares the token ceilings and `max_units`. Enter the provider's published batch rates: the gateway never derives them (no computed 50%).

- **Native:** the batch reservation pins the price version and its list. `price_tier` is `batch` when the version publishes batch lines, else `standard`, and the dashboard shows "No batch price". A list published later never reprices an admitted batch.
- **Gateway-run:** lines always use the standard list.

**Admission.** Creating a batch is one admission:

- **Hold:** the sum of every line's own bound at the applicable list, which is the same hold the line would take interactively (input ceiling plus its output maximum; embeddings are input only).
- **Limits:** the batch takes one "Jobs at once" slot ([governance](governance.md#jobs-at-once)) and is exempt from requests and tokens per minute. Every budget applies to the full hold.
- **Refusals:** unpriced or unbounded lines are refused at validation, so a batch is always bounded.

**Gateway-run lines.** Each line attempt is an ordinary execution (`inference_executions.batch_job_id` links it to its batch) with its own reservation at the standard price:

- **Transfer:** its hold is moved out of the batch's reservation (the envelope) under the installation lock, so the budget totals do not change.
- **Budgets:** only the part of a line's hold that the envelope can no longer cover is checked against budgets. A line that overspent (settled above its bound) therefore stops the batch at the budget.
- **Settlement:** each line settles from its own usage. Unknown usage keeps that line's hold.
- **End:** when the batch ends, the envelope settles at zero, which releases what no line used.

**Native batches** settle once from the provider's aggregate usage, or from the sum of the decoded lines' usage when every succeeded line reported it. Otherwise the usage is unknown and the hold is retained. Cancelled and expired batches record any usage as evidence and keep the hold, as before ([async jobs](async-jobs.md#reservation-and-settlement)).

## Monitoring

- **Logs › Batches** (workspace): one row per batch with mode (Native or Gateway, plus "Batch prices" or "No batch price" for native), status, a progress bar of finished lines, cost so far (settled plus on hold; unknown is never zero) and the created time. The row menu has Open, output and error downloads and Cancel. Members see the batches they created, workspace admins see the workspace's, and personal workspaces are visible only to their owner.
- **Batch page:** status and Cancel in the header, a 4-stat summary (progress, completed, failed, cost so far), the files, and line outcomes (counts by state and code).
- **Admin › Logs › Batches:** Team and Project batches across the installation, plus totals only (running, finished, failed) for personal workspaces.
- **Management API:** `GET /api/v1/workspaces/{ws}/batches[?status=active|finished]`, `GET …/batches/{id}`, `POST …/batches/{id}/cancel` (admins, or the creator; audited as `batch.cancelled`) and `GET /api/v1/platform/batches`.
- **Alerts** ([alerts](alerts.md)):
  - `batch_failed`: one incident per batch that ended failed or expired, open for 24 hours.
  - `batch_stalled`: an unfinished batch without progress for `window_minutes` (5–1440); progress or the end resolves it.
  - Workspace rules watch their workspace; installation rules watch Teams and Projects.
- **Metrics** (low cardinality):
  - `gateway_batches{mode,event}`, where the event is created, submitted or the final state.
  - `gateway_batch_lines{mode,provider,outcome}`.
  - `gateway_batch_queue_depth{mode}`.
  - `gateway_batch_workers{state=capacity|busy}`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_BATCH_WORKERS` | `4` | Gateway-run lines executing at once per process (`0`–`256`; `0` disables the runner). |
| `GATEWAY_BATCH_CONCURRENCY` | `2` | Lines of one batch executing at once (`1`–`64`). |
| `GATEWAY_JOB_POLL_INTERVAL_SECONDS` | `30` | Poller interval: native submission, status and collection (`0` disables native processing). |

## Storage and grants

- **Tables:** `batch_lines` (per-line state) and `batch_segments` (result segment file ids). Both are insert-only history: line rows only move `running` → finished, or to a counted explicit retry (trigger), and segments are never updated.
- **Columns:**
  - `async_jobs`: `batch_mode`, the input, private-copy, output and error file ids, `price_tier`, `retry_limit`, runner lease and progress timestamps.
  - `deployment_prices.batch_price_lines`, `governance_reservations.price_tier`, `inference_executions.batch_job_id`.
- **Runtime grants** (`deploy/staging/runtime-grants.sql`): `SELECT, INSERT` on both tables, line state columns, and the new job columns. Identity, mode, files, tier and retry policy are never updatable, and result files are written once (trigger). `verify-privileges.sql` and the ignored `runtime_privileges` test run a native and a gateway-run batch as the runtime role.

## Known limits

- **Metadata:** it is not stored, so it is not echoed.
- **Order:** results follow completion order.
- **Batch output files:** a batch that stops after writing its output files but before recording them leaves those files to retention.
- **Native:** batches never fail over, and native OpenAI chat-like lines always use Chat Completions upstream.
- **Mixed models:** a mixed-model batch always runs gateway-side, even when every model has a native batch API.

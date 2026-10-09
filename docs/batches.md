# Batches

`/v1/batches` runs OpenAI-format batches for any model the gateway serves. A batch reads a gateway file (`purpose=batch`, uploaded with the [Files API](files-api.md)) and writes its results to gateway files (`batch_output`). It runs in one of two modes, chosen per batch:

- **Native:** on the provider's batch API (OpenAI Batch or Anthropic Message Batches), when every line routes to one deployment whose adapter supports it. Native lines are charged the route's **batch price list** when one is published.
- **Gateway-run:** line by line through the gateway's normal inference engine, with each line its own attempt and reservation, at **standard prices**. Every other batch runs this way, including batches that mix models.

Gateway-run lines start only when their route has capacity ([scheduling](#scheduling-on-self-hosted-models), migration `0022_batch_scheduling.sql`). Native batches are unaffected.

This page describes the current source (migrations `0021_batch_engine.sql` and `0022_batch_scheduling.sql`). It is mock-tested, plus one small capped live check (2026-10-09, official `openai` Python SDK): a native Anthropic Message Batch and a gateway-run mixed OpenAI/Anthropic batch completed and settled at the expected integer micro-USD. A native OpenAI batch was submitted and accepted upstream, but it had not finished after 53 minutes, so its collection and settlement were not observed live.

## API

| Route | Behavior |
| --- | --- |
| `POST /v1/batches` | `{input_file_id, endpoint, completion_window, metadata?}`. Validates the whole input file, admits the batch and returns it (`validating`). An invalid file returns 400 with a line-numbered report. |
| `GET /v1/batches` | This workspace's batches, newest first (`after`, `limit` 1–100). |
| `GET /v1/batches/{id}` | One batch, from gateway records. |
| `POST /v1/batches/{id}/cancel` | Stops the batch (see [Cancel](#cancel)). |

- **Endpoints:** `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings` and `/v1/messages` (Anthropic shape). Every line uses the batch's endpoint and is validated exactly like an interactive request to it. Lines are never streamed. Chat Completions lines may name their maximum `max_tokens` (legacy) or `max_completion_tokens`, not both.
- **Lines:** `{custom_id, method:"POST", url, body}`, at most 50,000 per file and 4 MiB each. Blank lines are skipped.
- **Ids:** `batch_<32 hex>` and `file-<32 hex>` gateway ids. Upstream ids and client `custom_id`s are never sent to or exposed from the other side: native lines are sent upstream as `l0`, `l1`, … and mapped back.
- **`metadata`:** at most 16 string values. It is validated but **not stored**, so the batch object returns `metadata: null`. Two keys are gateway options:
  - `omg_mode`: `auto` (default) or `gateway` (always run line by line).
  - `omg_retries`: `0` (default), `1` or `2` (see [Failures and retries](#failures-and-retries)).
- **`completion_window`:** `24h` (the OpenAI value), or `48h`, `72h` or `168h` for routes that only run batch lines in a time window. Any other value is a 400. A window other than `24h` always runs gateway-side, because provider batch APIs only offer 24 hours. The batch object echoes the window and its `expires_at`.
- **Not supported:** `output_expires_after` (400 `unsupported_capability`) and file purposes other than `batch` as input.

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
- **Capacity:** a line starts only when its route's gates are open and the route has room ([scheduling](#scheduling-on-self-hosted-models)). Each route runs at most 2 batch lines at once by default, across all batches and gateway processes.
- **Routes:** the scheduler picks a line's route with the model's routing policy, then pins the line to it. A line never fails over after it starts. Lines of different models progress independently; a batch's models take turns.
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
- **Window:** when the completion window ends, no new line starts. Running lines finish and settle, then the batch ends `expired` with its partial results. Lines that never ran are listed with `batch_expired`, cost nothing, and their share of the batch hold is released. A line with unknown usage keeps its own hold.

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

## Scheduling on self-hosted models

A self-hosted model (for example Gemma on vLLM) is added as a normal route. Clients submit batches with `/v1/batches` as usual. The model server needs no batch endpoint: the gateway starts each line as an ordinary request when the route has capacity, so batch work fills idle time without crowding out interactive users.

These settings apply to gateway-run lines on any route, local or cloud. Native provider batches (OpenAI, Anthropic) are unaffected.

### Route settings

Set them on **Admin › Models › {model} › {route} › Batch scheduling**, or with `PUT /api/v1/platform/deployments/{id}/batch-scheduling` ([management API](management-api.md#batch-scheduling)). A route without settings uses the defaults.

| Setting | Default | A new line starts only while… |
| --- | --- | --- |
| `max_concurrency` | `2` | fewer than this many batch lines run on the route (all batches, all gateway processes; 1–256). |
| `yield_live_threshold` | off | fewer than N live (non-batch) requests to the route are in flight (1–100000). |
| `metrics` | off | the server's load is at or under its limits (see [server load signal](#server-load-signal)). |
| `window` | any time | the local time is inside an allowed window (see [time windows](#time-windows)). |
| `priority` | off | (not a gate) vLLM `priority` sent on the route's batch lines (see [priority hint](#priority-hint)). |

When a gate closes, lines already running finish. Nothing is resent and nothing is retried implicitly.

**Live traffic** is the gateway's own in-flight accounting: started executions on the route, other than batch lines and jobs, whose reservation lease is still running. Every gateway process reads the same count from PostgreSQL. Traffic that reaches the server without going through the gateway is not counted; use the server load signal for that.

### Server load signal

`metrics: {url, max_waiting?, max_running?, max_kv_cache_percent?}` reads vLLM-compatible Prometheus metrics. Set at least one limit; the route pauses while any reading is **above** its limit:

| Limit | Metric (vLLM) |
| --- | --- |
| `max_waiting` (0–100000) | `vllm:num_requests_waiting`, summed over engines. `0` pauses whenever requests queue on the server. |
| `max_running` (0–100000) | `vllm:num_requests_running`, summed over engines (batch lines count too). |
| `max_kv_cache_percent` (1–100) | `vllm:kv_cache_usage_perc` (a 0–1 fraction; older servers export `vllm:gpu_cache_usage_perc`), the highest engine. |

The metric names come from the vLLM source (`vllm/v1/metrics/loggers.py`, main branch on 2026-10-09).

- **Approval:** the URL must be `/metrics` on the origin of an endpoint approved in `GATEWAY_LOCAL_UPSTREAMS`: `{approved base without the final /v1}/metrics`, for example `http://10.0.0.5:8000/metrics` for the approved base `http://10.0.0.5:8000/v1`. It is fetched with that approval's pinned addresses, without redirects, proxies, retries or credentials. Saving any other URL is refused.
- **Bounds:** a 2-second deadline and a 1 MiB body. Each gateway process caches a reading for 5 seconds.
- **Fail closed:** an unreachable server, an error status, an oversized or non-UTF-8 body, or a missing gauge for a configured limit pauses the route as `metrics_unavailable` and shows the error on the route page.

### Time windows

`window: {timezone, days, start, end}` uses an IANA time zone (the tz database is built into the gateway) and local wall-clock `HH:MM` times.

- **Days** (`mon` … `sun`) are the days a window **starts**. An `end` earlier than `start` spans midnight: `mon`–`fri` `19:00`–`07:00` runs Monday evening to Tuesday morning, through Friday evening to Saturday morning. Monday before 07:00 is closed, because no Sunday window starts.
- **`start` equal to `end`** allows the whole day.
- **DST:** windows follow the wall clock, so an overnight window is an hour shorter or longer on the night the clocks change. A window inside the skipped spring hour does not open that night.
- **Outside the window** no new line starts. Use a longer `completion_window` (up to `168h`) for batches that need several nights.

### Priority hint

With `--scheduling-policy priority`, vLLM handles requests with a lower `priority` value first (ties by arrival), and requests without the field use `0` (vLLM `SchedulerConfig.policy` and the `priority` request field). With `priority: N` (1–1000000), the gateway sends `"priority": N` on this route's batch lines only, so batch work always runs after live traffic.

- **Profiles:** `vllm` and `openai_compatible` routes only. Other routes refuse the setting.
- **Scope:** routes without the setting, and interactive requests, never receive the field.
- **Server support:** vLLM rejects a non-zero `priority` unless it runs with the priority policy, so turn the setting on only for such servers.

### Fair sharing

When several batches wait for one route, the next slot goes to the workspace with the fewest batch lines running on it, then the least recently served workspace, then (within a workspace) the batch with the fewest lines running, least recently served. One huge batch therefore cannot starve the others. A batch whose lines target several routes progresses on each independently.

Claims are durable and safe across processes: a line is claimed by inserting its row under a per-route transaction lock that checks the route's capacity and the fair-share order. A line still runs at most once. A runner that stops leaves its running lines `interrupted` (never re-run), and they count toward the route's capacity until the batch resumes elsewhere.

### Example: Gemma on vLLM

The server runs `vllm serve google/gemma-4 --scheduling-policy priority` on `10.0.0.5:8000`. The gateway approves it:

```sh
GATEWAY_LOCAL_UPSTREAMS='[{"endpoint":"http://10.0.0.5:8000/v1","addresses":["10.0.0.5"]}]'
```

Add a `vllm` connection with that endpoint and a route for the model, then set its batch scheduling:

```json
{
  "max_concurrency": 4,
  "yield_live_threshold": 2,
  "metrics": {"url": "http://10.0.0.5:8000/metrics", "max_waiting": 0, "max_kv_cache_percent": 90},
  "priority": 10,
  "window": {"timezone": "America/New_York", "days": ["mon","tue","wed","thu","fri"], "start": "19:00", "end": "07:00"}
}
```

Overnight on weekdays, at most 4 batch lines run at once. They pause while 2 or more interactive requests are in flight, while any request queues on the server, or while its KV cache is over 90% full, and they always queue behind interactive requests inside vLLM. Clients may submit batches with `"completion_window": "72h"` so a weekend does not expire them.

## Monitoring

- **Logs › Batches** (workspace): one row per batch with mode (Native or Gateway, plus "Batch prices" or "No batch price" for native), status, a progress bar of finished lines, cost so far (settled plus on hold; unknown is never zero) and the created time. The row menu has Open, output and error downloads and Cancel. Members see the batches they created, workspace admins see the workspace's, and personal workspaces are visible only to their owner.
- **Batch page:** status and Cancel in the header, a 4-stat summary (progress, completed, failed, cost so far), the files, and line outcomes (counts by state and code).
- **Waiting for capacity:** a gateway-run batch with no line running and lines held back shows **Queued — waiting for capacity**, with the reason (outside its time window, yielding to live traffic, server busy, server metrics unavailable, at its batch limit, other batches' turn, gateway workers busy, rate limited). Its page lists each model's reason and its place in that route's queue. The OpenAI batch object is unchanged (`in_progress`).
- **Admin › route › Batch scheduling:** the route's queued and running batch lines, why it is paused, live requests in flight, the last server metrics reading, and the settings.
- **Admin › Logs › Batches:** Team and Project batches across the installation, plus totals only (running, finished, failed) for personal workspaces.
- **Management API:** `GET /api/v1/workspaces/{ws}/batches[?status=active|finished]`, `GET …/batches/{id}`, `POST …/batches/{id}/cancel` (admins, or the creator; audited as `batch.cancelled`) and `GET /api/v1/platform/batches`.
- **Alerts** ([alerts](alerts.md)):
  - `batch_failed`: one incident per batch that ended failed or expired, open for 24 hours.
  - `batch_stalled`: an unfinished batch without progress for `window_minutes` (5–1440); progress or the end resolves it. A gateway-run batch that is legitimately waiting (its time window, live traffic, server load, concurrency or its turn) is not stalled: its clock starts when the wait ends. Unreadable server metrics are not a legitimate wait.
  - Workspace rules watch their workspace; installation rules watch Teams and Projects.
- **Metrics** (low cardinality):
  - `gateway_batches{mode,event}`, where the event is created, submitted or the final state.
  - `gateway_batch_lines{mode,provider,outcome}`.
  - `gateway_batch_queue_depth{mode}`.
  - `gateway_batch_workers{state=capacity|busy}`.
  - `gateway_batch_route_lines{provider,state=waiting|running}`: gateway-run lines per route provider kind, installation-wide.
  - `gateway_batch_paused_routes{reason}`: routes with waiting lines per pause reason, installation-wide.
  - `gateway_batch_route_pauses_total{reason}`: times this process saw a route's gate close.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_BATCH_WORKERS` | `4` | Gateway-run lines executing at once per process (`0`–`256`; `0` disables the runner). |
| `GATEWAY_BATCH_CONCURRENCY` | `2` | Lines of one batch executing at once (`1`–`64`). |
| `GATEWAY_JOB_POLL_INTERVAL_SECONDS` | `30` | Poller interval: native submission, status and collection (`0` disables native processing). |

## Storage and grants

- **Tables:** `batch_lines` (per-line state) and `batch_segments` (result segment file ids). Both are insert-only history: line rows only move `running` → finished, or to a counted explicit retry (trigger), and segments are never updated.
- **Scheduling (0022):** `deployment_batch_scheduling` (route settings; upserted, never deleted), `deployment_batch_signals` (each route's last gate evaluation and metrics reading) and `batch_route_waits` (each batch's demand per route, heartbeated by its runner; stale rows are ignored and removed when the batch ends). `batch_lines.deployment_id` records a line's route at claim (fixed by trigger). `async_jobs.completion_window_hours` is fixed at creation; `async_jobs.last_waited_at` pauses the stall clock.
- **Columns:**
  - `async_jobs`: `batch_mode`, the input, private-copy, output and error file ids, `price_tier`, `retry_limit`, runner lease and progress timestamps.
  - `deployment_prices.batch_price_lines`, `governance_reservations.price_tier`, `inference_executions.batch_job_id`.
- **Runtime grants** (`deploy/staging/runtime-grants.sql`): `SELECT, INSERT` on both tables, line state columns, and the new job columns. Scheduling adds `SELECT, INSERT` and the value columns of the settings and signal tables (no `DELETE`), `SELECT, INSERT, DELETE` and the state columns of `batch_route_waits`, and `UPDATE(last_waited_at)`. Identity, mode, files, tier and retry policy are never updatable, and result files are written once (trigger). `verify-privileges.sql` and the ignored `runtime_privileges` test run a native and a gateway-run batch as the runtime role.

## Known limits

- **Metadata:** it is not stored, so it is not echoed.
- **Order:** results follow completion order.
- **Batch output files:** a batch that stops after writing its output files but before recording them leaves those files to retention.
- **Native:** batches never fail over, and native OpenAI chat-like lines always use Chat Completions upstream.
- **Gateway-run routes:** a line is pinned to the route it was scheduled on and does not fail over. Lines of one model wait for the route their routing policy picks for them. A capped route never spills to another (possibly paid) route.
- **Server load:** only vLLM-compatible Prometheus gauges are read; other servers can use concurrency, live traffic and time windows.
- **Mixed models:** a mixed-model batch always runs gateway-side, even when every model has a native batch API.

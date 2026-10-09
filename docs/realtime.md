# Realtime audio

`GET /v1/realtime?model=<API model name>` proxies an OpenAI Realtime (GA interface) WebSocket session. A session is admitted, rate-limited and accounted as **one upstream attempt**. Its budget is reserved in bounded per-response windows. Each response is valued from its own usage. Audio, text, instructions and tool arguments are never logged or stored.

Supported upstream: the `openai` adapter (`wss://api.openai.com/v1/realtime`). Other adapters return `unsupported_capability` before admission. This is a narrow, mock-tested subset. It was not validated against the live API in this change, and no paid requests were made.

## Connect

```js
// Node (server side): the inference key in the Authorization header.
const ws = new WebSocket("wss://gateway.example/v1/realtime?model=company%2Fvoice", {
  headers: { Authorization: `Bearer ${process.env.GATEWAY_KEY}` },
});
// Browser: the OpenAI SDK subprotocol form. The key is visible to the page; use a narrowly scoped key.
new WebSocket(url, ["realtime", `openai-insecure-api-key.${key}`]);
```

- **One inference key:** use `Authorization: Bearer <key>` or the subprotocol `openai-insecure-api-key.<key>`, never both. Missing, repeated, ambiguous or invalid credentials (including `x-api-key`) return HTTP 401 before the upgrade. The workspace comes from the key. The key never reaches the provider: the gateway connects with the connection's server-side credential reference. The server echoes only `realtime`, never the key-bearing subprotocol.
- **Query:** `model` only, exactly once. `call_id` and other parameters return 400.
- **Explicitly unsupported (400 `unsupported_capability`):** the beta interface (`OpenAI-Beta: realtime=v1` or `openai-beta.realtime-v1`), other subprotocols, `POST /v1/realtime/client_secrets` (the gateway never mints upstream ephemeral secrets) and `POST /v1/realtime/calls` (WebRTC/SIP).
- **Before the 101 response** the gateway checks live workspace, catalog and key access, admits the session and connects upstream. These failures are ordinary JSON errors, as on the other routes: 404 `model_not_found`, 400 `unsupported_capability`, 429 for budget and rate denials (budget with `x-should-retry: false`), 503 for configuration problems.
- **Labels:** `X-Session-Id` and `X-Title` headers work as on other routes ([client labels](protocol-matrix.md#client-labels-logs-sessions-and-apps)). Browsers cannot send them.

## Session configuration

Before any client event is forwarded, the gateway sends its own `session.update`: server VAD with `create_response: false`, input transcription off, and `max_output_tokens` set to the response window. It then waits for the acknowledging `session.updated`. Upstream `session.created` and `session.updated` events are forwarded with `model` rewritten to the public alias. Every later `session.updated` is checked again. If the effective configuration would let upstream start billable work by itself (automatic or idle-timeout responses, input transcription), the session fails closed.

## Client events

Only JSON text frames are accepted. A binary frame gets an error event and close 1003. Allowed event types:

| Event | Rules |
| --- | --- |
| `session.update` | `session.type` must be `realtime`. Allowed fields: `instructions`, `output_modalities` (one of `text`/`audio`), `max_output_tokens` (at most the window), `audio.input.{format,noise_reduction,turn_detection,transcription:null}`, `audio.output.{format,voice,speed}`, function `tools`, `tool_choice`, `truncation`. Turn detection must be `null`, or VAD with `create_response: false` and no `idle_timeout_ms`. |
| `input_audio_buffer.append`, `.commit`, `.clear`; `output_audio_buffer.clear` | Forwarded. |
| `conversation.item.create` | `message` items (`input_text`, `input_audio`, `output_text`, `output_audio` parts), `function_call` and `function_call_output`. `input_image` is refused because it has no realtime meter. |
| `conversation.item.delete`, `.retrieve`, `.truncate` | Forwarded. |
| `response.create` | Allowed fields: `instructions`, `output_modalities`, `max_output_tokens` (1 to the window; `"inf"` is refused), `metadata` (at most 16 string pairs), function `tools`, `tool_choice`, `audio.output.{format,voice}`, `conversation: "auto"`. `input`, out-of-band `conversation: "none"`, prompts and MCP tools are refused. When the client sends no `max_output_tokens`, the gateway adds one. Only one response may be in flight at a time. A second `response.create` gets `conversation_already_has_active_response` and is not forwarded; the session stays open. |
| `response.cancel` | Forwarded. A cancelled response still reports usage in `response.done`. |

Any other type, unknown field or disallowed value gets an `error` event (`unsupported_event`, `unsupported_capability` or `invalid_request_error`, with `param` and the client's `event_id`) and close 1008. Nothing is silently dropped.

Server events are forwarded as received, with three exceptions:

- `rate_limits.updated` describes the gateway's provider account. It is not forwarded.
- Upstream `error` events are rebuilt with bounded `type`, `code`, `param` and `event_id`, and a generic message. Provider messages can contain private account or model identifiers.
- Input transcription events mean separately billed work happened that the session cannot account for. The session fails closed.

## Limits

| Variable | Default | Range | Effect |
| --- | --- | --- | --- |
| `GATEWAY_REALTIME_MAX_SESSION_SECONDS` | 900 | 10–3600 | Hard session length. An error event is sent, then close 1000. |
| `GATEWAY_REALTIME_IDLE_SECONDS` | 120 | 5–3600 | No frame in either direction for this long ends the session. |
| `GATEWAY_REALTIME_MAX_OUTPUT_TOKENS` | 4096 | 1–4096 | Per-response output window, further lowered to the price's `output_token_limit`. |
| `GATEWAY_REALTIME_MAX_MESSAGE_BYTES` | 1 MiB | 1 KiB–16 MiB | Client message and frame cap (upstream messages: 16 MiB). |
| `GATEWAY_REALTIME_MAX_EVENTS_PER_SECOND` | 50 | 1–1000 | Client event rate, with a burst of twice the rate. Exceeding it sends an error event and close 1008. |

The session holds one process concurrency permit (`GATEWAY_MAX_CONCURRENT_REQUESTS`) for its whole life.

## Budget and accounting

- **Window:** one response. Input (all modalities) is bounded by the price's `input_token_limit`, which must cover the model's context. Output is bounded by the window `W`. The hold is the v3 bound of text input, cached text, output and the three audio-token meters at those ceilings. Meters a realtime response cannot report (cache writes, images, characters, audio seconds, search units) count as impossible, and `requests` is 1 per response.
- **Admission** reserves one window on the session's single reservation. Pricing must be v3 (v1/v2 cannot price audio tokens and return a configuration error). An unpriced deployment can only be used without budgets, and every response is then unknown.
- **Each further `response.create`** extends the hold by one window before it is forwarded. The extension rechecks the key, workspace and model authorization live, and every applicable budget, read from the maintained totals for the windows that contain the session's admission time. Tokens-per-minute limits count every window in the session's admission minute, so one session can never reserve more than the limit. A denial sends an `error` event (`budget_exceeded`, `unresolved_usage`, `rate_limit_error`, …) and closes with 1008. The request is not forwarded.
- **`response.done`:** usage must be complete and consistent; otherwise it is unknown. Known usage is valued with the pinned price, each meter rounded up, and replaces the window. Unknown usage keeps `max(window, known floor)`. A response above its ceilings is recorded as unbounded.
- **Rate limits:** requests per minute and concurrency count the session once.
- **End:** the reservation settles when every response settled. Its actual cost is the sum of the responses, with 15-key cost components. Otherwise it stays `unknown` and retains the held windows; an unused reserved window is released. A forwarded `response.create` that was neither created nor rejected, and a response still open at the end, keep their windows as unknown. A session with no responses settles at a known zero: realtime sessions bill only per response, and input transcription is refused. Manual reconciliation (`resolve_usage`) of realtime sessions returns 400, because the valuation is per response.
- **Termination:** client close, a client disconnect or any gateway-side end drops the upstream socket immediately. A disconnect while a response is in flight records the session as `cancelled`. Upstream closes and errors send an error event and close 1011. They are never presented as a clean end. Graceful shutdown does not wait for upgraded sockets. A session cut off by a process stop may not be finalized; lease reconciliation (`max session + 60 s`) then marks it unknown and its open responses unknown, and the holds are kept.

### Usage normalization (OpenAI)

From `response.done.response.usage`: `input_tokens = text + audio (+ image, which must be 0)`, `cached_tokens = cached text + cached audio`, `output_tokens = text + audio`, and `total_tokens` must match when present. Cache details may be absent only when nothing was cached.

| Meter | Count |
| --- | --- |
| `input_tokens` / `cache_read_tokens` / `output_tokens` | Uncached text input / cached text input / text output |
| `input_audio_tokens` / `cache_read_audio_tokens` / `output_audio_tokens` | Uncached audio input / cached audio input / audio output |
| `requests` | 1 per response |

The audio-token meters exist only for realtime. They are priced per million tokens, with no prompt tiers and no `max_units`; every other workload ignores them. Example `gpt-realtime` list prices (check the provider before publishing):

```json
[{"meter":"input_tokens","microusd_per_batch":"4000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Text input"},
 {"meter":"cache_read_tokens","microusd_per_batch":"400000","batch":1000000,"unit_label":"/M tokens","sku_label":"Cached text"},
 {"meter":"output_tokens","microusd_per_batch":"16000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Text output"},
 {"meter":"input_audio_tokens","microusd_per_batch":"32000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Audio input"},
 {"meter":"cache_read_audio_tokens","microusd_per_batch":"400000","batch":1000000,"unit_label":"/M tokens","sku_label":"Cached audio"},
 {"meter":"output_audio_tokens","microusd_per_batch":"64000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Audio output"},
 {"meter":"cache_write_tokens","not_applicable":true}, …other meters not applicable…,
 {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}]
```

## Storage and Logs

Migration `0017_realtime.sql`:

- Adds the `realtime` protocol and workload kind, and the audio-token price lines and 15-key cost components. The existing validators are composed, not rewritten.
- Adds `realtime_responses`, one row per response: sequence, state, upstream status, window hold, value or floor, per-modality token counts and timestamps. A trigger lets each row settle exactly once, and rows cannot be deleted.

The session's execution row stores all-modality token totals (unknown if any response was unknown), `billing_usage` null, and `meter_usage.requests` = number of responses. Logs list the session as one request whose latency is the session duration. The request detail shows a **Realtime responses** timeline with each response's status, text/audio input and output, cost or retained hold, and duration. Response rows are accounting evidence and are not cleared by detail retention.

## Not supported

The beta interface, ephemeral client secrets, WebRTC/SIP, transcription-only sessions, input transcription, automatic VAD responses, idle-timeout responses, out-of-band or parallel responses, MCP and hosted tools, image input, prompt templates, tracing, and failover after the upgrade. Failover follows the route plan only for admission and connect failures before the upgrade.

# Engine and provider adapters

```text
Client codecs: Chat / Responses / Messages / Embeddings / Rerank / System One
    ↓ typed generation or workload request
Engine: live workspace/key access → capability checks → routing
        per-attempt admission → deadline-bound execution/accounting
    ↓ ProviderAdapter
Provider: validate connection/options → resolve its credentials → wire transport
    ↑ normalized result/events and presence-preserving usage
Client codec: bounded JSON / SSE
```

Providers are startup registry entries, not engine branches or database enums. Model protocol declarations and adapter request-specific checks both apply. A profile is a **narrow tested subset**, not a promise that every server/version/model or SDK option works.

## Registered transports

| Registry ID | Transport/profile |
| --- | --- |
| `openai` | Fixed `https://api.openai.com/v1`; native Chat, Responses, string embeddings, `gpt-image-*` image generation, audio transcription and speech, realtime sessions over `wss://api.openai.com/v1/realtime` (GA interface), and async Chat Completions batches ([async jobs](async-jobs.md)). It no longer offers `videos`: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24. No arbitrary cloud endpoint override. |
| `anthropic` | Fixed `https://api.anthropic.com/v1`; native Messages, representable Chat subset. |
| `bedrock` | Server AWS identity, an allowlisted named profile or an assumed IAM role; explicit region; optional allowlisted VPC endpoint; SigV4 Converse/ConverseStream, not an OpenAI URL. |
| `openai_compatible` | Explicit approved local Chat/embedding subset, not universal compatibility. |
| `vllm`, `sglang` | Separately declared local Chat/embedding profiles. |
| `ollama` | Compatible Chat plus **native `/api/embed`**, derived from approved `/v1` base; not compatible embedding fallback. |
| `openrouter` | Fixed `https://openrouter.ai/api/v1`; Chat (stream/non-stream), string embeddings, rerank, System One, images, audio transcription (needs account credit) and speech. `env:` key reference only. |

See [protocol matrix](protocol-matrix.md), [Bedrock](bedrock.md) and the provider-owned [local profile notes](../apps/gateway/src/providers/local/README.md). Ollama native embedding follow-up is present in source; it has not been freshly validated by this documentation refresh or certified against a live Ollama server. Do not extend earlier isolated-provider counts to that follow-up or whole-stack integration.

Local Chat maps the gateway's maximum to upstream `max_tokens`. All local profiles reject strict tool guarantees; Ollama rejects every explicit tool-choice option; generic compatible rejects required/named choices. Generic compatible/Ollama reject dimension overrides; vLLM/SGLang accept the bounded tested dimension field, subject to actual model support. Local Responses/Messages are not implied.

## OpenRouter

Registry ID `openrouter`; connection profile `openrouter` (fixed base, `env:` key, no `none`). Server configuration, never request fields:

| Variable | Meaning |
| --- | --- |
| `GATEWAY_OPENROUTER_DATA_COLLECTION` | Optional override, `deny` or `allow`; sent as `provider.data_collection` on every request. Unset, Admin › Settings › Data & privacy decides (default `deny`); see [settings](settings.md#data--privacy). |
| `GATEWAY_OPENROUTER_HTTP_REFERER` | Optional absolute http(s) URL sent as `HTTP-Referer` (attribution only). |
| `GATEWAY_OPENROUTER_TITLE` | Optional 1–128 printable ASCII characters sent as `X-Title`. |

Redirects, ambient proxies and implicit retries are disabled. Non-success bodies are never read: OpenRouter error bodies carry the account `user_id` and embedded provider errors. Statuses map to sanitized kinds: 401/402 (bad key, or no credit on the gateway's account) → `provider_configuration_error`; 403 (moderation) and other 4xx → `upstream_rejected`; 408/524 → `timeout_error`; 429 → `rate_limit_error`; 5xx → `upstream_unavailable`; 3xx → `provider_configuration_error`. A 200 body (or stream frame) carrying `error` fails by its code; a non-JSON 200 is invalid.

OpenRouter `:free` variants may train on prompts. Under the default `deny` they have no eligible endpoint and fail as `upstream_rejected` (upstream 404). Using them requires a Platform Admin (or the operator override) to set `allow` installation-wide.

- **Chat:** OpenAI wire with `max_completion_tokens`, `usage:{include:true}` and `stream_options.include_usage`. Responses normalize OpenRouter-only fields. `native_finish_reason` is dropped. Reasoning traces (`reasoning`, `reasoning_details`) are not returned to clients, but their tokens stay in `completion_tokens`. Other unknown content (images, annotations, non-null refusal) is rejected. The final streaming accounting frame (a repeated finish choice with an empty delta plus `usage`) is treated as usage, not as a second terminal. `: OPENROUTER PROCESSING` comments are skipped. Usage follows the research normalization: `prompt_tokens` is inclusive, `cached_tokens` is cache read, and `cache_write_tokens` is the default write category (no TTL split).
- **Embeddings:** float string subset. Catalog embedding models advertise no `dimensions`, so overrides are rejected before admission. The exception is a model's fixed native width (`nvidia/nemotron-3-embed-1b[:free]`: only 2048), and response vectors must match it. The reported `private/...` model is never compared.
- **Rerank:** `POST /rerank` with `top_n` clamped to the document count. `usage.total_tokens` is input-only tokens. `usage.search_units` is observed when reported, otherwise unknown. Echoed documents are discarded.
- **System One:** `POST /systemone` (not the alpha decisions route). Answers are validated strictly against the questions. `usage.input_tokens`/`output_tokens` are required.
- **Images:** `POST /images`. The client `size` must be a tier (`512|768|1K|1.5K|2K|4K`), sent as `resolution`. An omitted size sends the gateway default `1K`, so the billed tier is always known. Pixel sizes and `auto` are unsupported on OpenRouter because the billed tier would be ambiguous. `quality` and `seed` pass through. `output_image_variant` uses the endpoint-pricing spelling (`768`, `1k`, `1.5k`, `2k`, `4k`), so imported price lines match. Usage follows the chat normalization: an observed `prompt_tokens: 0` proves all input partitions are zero. `completion_tokens` (synthetic image tokens for per-image models) is recorded as reported, so price it `"0"` when the model is priced per image. A 402 for missing credit maps to `provider_configuration_error` without reading the body. `provider.data_collection` is sent as on every workload, but the Image API schema does not list it, and live verification is blocked by 402. Its enforcement for images is **unverified**.
- **Audio transcriptions:** `POST /audio/transcriptions` as JSON with `input_audio:{data: raw base64, format}`. The format comes from the validated container (`mp4` is sent as `m4a`). The request also sends `response_format:"json"`, optional `language`/`temperature` and `provider`. OpenRouter documents that it ignores `prompt`, so requests with a prompt are `unsupported_capability` and never sent. `usage.seconds` (exact decimal text, rounded up to ms) is `input_audio_seconds_ms`; `input_tokens`/`output_tokens` are recorded when present (with `total_tokens` checked); `usage.cost` is evidence. Audio needs at least $0.50 of account balance: the 402 maps to `provider_configuration_error` without reading the body. Live behavior beyond the 402 is **unverified**.
- **Speech:** `POST /audio/speech` with `model, input, voice, response_format` (always explicit; `mp3`/`pcm` only), optional `speed`, and `provider`. The raw audio body (live: chunked `audio/mpeg`) carries no usage or cost, so the attempt is valued locally from the exact `input_characters`. A JSON 200 is mapped by its embedded error code, or fails as invalid. Voices are model-specific (for example `en-US-Harper:MAI-Voice-2`).
- **Cost evidence:** `usage.cost` is parsed from the JSON number's source text (no f64) into exact micro-USD, rounded up, and stored as `provider_cost_microusd`. It is evidence only, never the gateway charge.

## Non-generation workloads

`inference::workload` is the shared path for embeddings, rerank, System One and images (`inference::images`; adapters implement `supports_image_request`, which defaults to `false`, and `execute_images`). Audio uses it as well (`inference::audio`): adapters opt in with `supports_transcription_request`/`supports_speech_request` (default `false`) and implement `execute_audio_transcription`/`execute_audio_speech`. Speech is the one streamed workload. It sets `Workload::STREAMED`, so the attempt is recorded as streamed. After validation, `Workload::attach` receives a `StreamSettlement` that settles success only after upstream EOF, failure on error, deadline or output overflow, and cancellation when the body is dropped. There is no failover after the body is returned. A workload request implements `Workload` (`PROTOCOL`, `validate`, `admission`, `supported_by`, `dispatch`, `usage`, `valid_response`). `Engine::execute_workload` owns the steps:

1. Pure validation.
2. Live catalog/key filtering.
3. Model-protocol and adapter capability gating. No candidate is an explicit 4xx.
4. Explicit route plan/failover.
5. Durable per-attempt admission (`InferenceRepository::admit_workload`).
6. Deadline/cancellation.
7. Invalid-body usage evidence.

`WorkloadAdmission` declares the output reservation (`None` input-only, `Requested`, or the price's `PriceCeiling`) and request-derived unit ceilings, for example `requests: 1`. Those ceilings only tighten v3 `max_units`, and `valid_response` enforces them. Adapters report every meter the workload can produce: meters it cannot produce are semantic zeros, `requests` is 1 per upstream request, and unknown observations stay `None`. Per-route body caps are `GATEWAY_MAX_BODY_BYTES_{IMAGES,AUDIO_TRANSCRIPTIONS,AUDIO_SPEECH,RERANK,SYSTEMONE}` (1 KiB–64 MiB). Defaults are 2 MiB, except transcription at 26 MiB. Image responses have their own upstream cap, `GATEWAY_MAX_RESPONSE_BYTES_IMAGES` (default 20 MiB, 1–64 MiB), which does not raise other workloads' 4 MiB provider cap.

### OpenAI images

`POST /v1/images/generations` is supported only for upstream models named `gpt-image-*`. The request sends `model, prompt, n`, plus `size` (`auto|1024x1024|1536x1024|1024x1536`) and `quality` (`auto|low|medium|high`) when given. No `response_format` is sent, because gpt-image always returns base64. `seed` and tiers are rejected before admission. Usage (live probe shape): `input_tokens` is the total input, and `output_tokens` is the image output. Per-modality details must sum to their totals, and `total_tokens` must equal input plus output. The Images API defines no cache categories, so cache reads are `cached_tokens` when reported and otherwise 0, and cache writes are 0. A missing usage object stays unknown. `output_image_variant` is the generated size. Statuses map like Chat: 400/422/other 4xx → `upstream_rejected` (including moderation blocks), 401/403/3xx → `provider_configuration_error`, 429 → `rate_limit_error`, 5xx → `upstream_unavailable`. Error bodies are never read.

### OpenAI audio

- **Transcriptions:** `POST /v1/audio/transcriptions` with a multipart body the gateway builds itself. It has fixed fields (`model`, `response_format=json`, optional `language`/`prompt`/`temperature`) and `file` named `audio.<ext>` with the canonical media type. Client headers, filenames and credentials are never forwarded. The response must be JSON `{text, usage}`; content-bearing extras (segments, words, logprobs) fail rather than being dropped. `usage.type:"duration"` gives whole `seconds` → `input_audio_seconds_ms`. `usage.type:"tokens"` gives `input_tokens`/`output_tokens` (with `total_tokens` checked) plus the gateway-measured duration. Transcript bodies are capped at 4 MiB of text.
- **Speech:** `POST /v1/audio/speech` JSON `model, input, voice, response_format, speed?`. The upstream media type must match the format: `audio/mpeg`, `audio/wav` (or `x-wav`/`wave`), `audio/opus` (or `ogg`), or `audio/pcm` (or `l16`/octet-stream). The body is passed through unbuffered. Non-SSE speech reports no usage, so `gpt-4o-mini-tts` token usage is not observed.
- Statuses map like Chat. Error bodies are never read.

### OpenAI realtime

`ProviderAdapter::connect_realtime` (default `Unsupported`; opt in with `supports_protocol(ApiProtocol::Realtime)`) returns a connected `RealtimeUpstream`: a text sink and a classified event stream. Dropping it closes the socket. The OpenAI adapter:

- **Connection:** validates the connection before resolving the credential. It connects to `wss://api.openai.com/v1/realtime?model=<upstream model>` with `Authorization: Bearer <server secret>`, tokio-tungstenite (`=0.29.0`) and an explicit ring-backed rustls config with webpki roots. There are no proxies, redirects (handshake errors) or retries, and the connect + configuration deadline is 10 s. Handshake statuses map like Chat (401/403/3xx → `provider_configuration_error`, 429 → `rate_limit_error`, 5xx → `upstream_unavailable`); bodies are never read.
- **Configuration:** before returning, it sends its own `session.update` (server VAD with `create_response:false`, transcription off, per-response `max_output_tokens`) and waits for the acknowledging `session.updated`. A rejection returns `upstream_rejected`. An unsafe acknowledgement (automatic or idle responses, transcription) returns `invalid_upstream_response`.
- **Event classification:** `session.*` (with a safety flag), `response.created`/`response.done` (response id, status, normalized usage), `error` (bounded tokens only), `rate_limits.updated` (filtered) and input-transcription events (unaccounted, so the session fails closed). Every other event is forwarded verbatim. Upstream close or binary frames are errors.
- **Usage:** see [realtime](realtime.md#usage-normalization-openai). Missing or inconsistent counts are unknown, never zero.

Contract tests run against a scripted loopback WebSocket mock (`providers/openai/realtime/mock.rs`): the frontend and engine over a real listener and PostgreSQL. No live or paid realtime call was made.

### Async jobs (video, batch)

Job methods on `ProviderAdapter` (`supports_video_request`, `create_video`, `retrieve_video`, `delete_video`, `video_content`, `upload_batch_file`, `create_batch`, `retrieve_batch`, `cancel_batch`, `file_content`, `batch_output_usage`) default to `Unsupported`; an adapter opts in with `supports_protocol(Videos | Batches)`. Each call is one bounded request: no polling loops, no retries, and dropping the future cancels it. The job service (`crate::jobs`) owns admission, ids, ownership, polling and settlement; see [async jobs](async-jobs.md).

OpenAI (shapes from the official OpenAPI spec via openai-python, 2026-10-08; mock-tested only, no paid calls):

- **Videos (retired):** OpenAI shut down this API on 2026-09-24, so the adapter no longer declares `videos` and new jobs are refused before admission. The calls below remain for jobs created earlier and stay mock-tested. `sora-*` upstream models only. `POST /videos` is a gateway-built multipart form with exactly `model, prompt, seconds, size`. `GET`/`DELETE /videos/{id}` and `GET /videos/{id}/content?variant=` are also used. Video `status` must be `queued|in_progress|completed|failed`. `seconds` is decimal text (an unparseable value is unknown). Only the error `code` is kept (sanitized to `[a-z_]`); messages are dropped. Content media types are allowlisted (`video/mp4`, images for thumbnails/spritesheets, generic binary) and streamed through.
- **Files/Batches:** `POST /files` is a chunked multipart stream (`purpose=batch` + `file`). A content error aborts the body, so the provider never receives a complete form. `POST /batches` carries `{input_file_id, endpoint:"/v1/chat/completions", completion_window:"24h", metadata?}`; `GET /batches/{id}` and `POST /batches/{id}/cancel` are also used. The batch `usage` maps to billing usage like Chat (`cached_tokens` is cache read; this schema has no cache writes, so they are zero). Malformed or partial usage is unknown. `GET /files/{id}/content` is streamed through to clients. For settlement it is stream-parsed line by line with bounded lines and a total cap; only each line's `response.body.usage` is read and bodies are dropped.
- Upstream ids are validated (`[A-Za-z0-9._:-]{1,128}`) before use in a path. Statuses map like Chat, and error bodies are never read.
- OpenRouter: video is planned (the next video adapter). Its `POST /api/v1/videos` uses another request shape (`duration`, `resolution`, `aspect_ratio`, `polling_url`). It has no Batch API.

## Approve local endpoints out of band

`GATEWAY_LOCAL_UPSTREAMS` is server configuration, not an inference request field:

```json
[{"endpoint":"http://models.internal:8000/v1","addresses":["10.10.1.20"]}]
```

Approvals bind a canonical exact HTTP(S) base ending in `/v1` to pinned destination IPs. They are bounded to 64 endpoints and 16 addresses each. No DNS lookup occurs during approval loading or dispatch. URL credentials, query/fragment, encoded path escapes and noncanonical URLs are rejected. Redirects, environment proxies and automatic retries are disabled. HTTP destinations must be approved private addresses (loopback only in development); forbidden metadata/link-local/multicast/special-use destinations are rejected. HTTPS retains certificate checks. Enforce independent network egress too; application approval is not a complete network policy.

Bedrock references (`aws:default`, `aws:profile:<name>`, `aws:role:<arn>`) name AWS identities, never keys; `GATEWAY_AWS_PROFILE_ALLOWLIST` and `GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST` gate profiles and endpoint overrides (see [Bedrock](bedrock.md#connection-configuration)).

Local `credential_ref:"none"` explicitly sends no authentication and resolves no secret. Optional `env:NAME` uses an operator allowlist and a separate upstream Bearer value. Inference keys and unrelated cloud credentials are never forwarded. Cloud `none` is prohibited. `GATEWAY_SECRET_ENV_ALLOWLIST` defaults empty; references and values are not returned by management. A reference is not a secret manager or per-tenant vault policy.

Ollama embedding requests derive the same approved origin/prefix's `/api/embed` and send `truncate:false`. The proxy/server must serve that path and honor non-truncation; the adapter cannot detect an old server silently ignoring fields. There is no retry on `/v1/embeddings`, legacy `/api/embeddings`, truncation or a different host. Its `prompt_eval_count` is batch input usage when present; missing/null stays unknown. Operators must verify deployment model/server behavior and hard aggregate input bounds.

## Logs telemetry: served model and reasoning tokens

Each attempt records, for Logs only, the model the provider reports having served (`reported_upstream_model`, migration 0013) next to the configured route id snapshot (`upstream_model`). Logs show the reported model and fall back to the configured id marked "configured". Reasoning tokens are a telemetry breakdown of the inclusive output count: never charged separately, never sent to clients.

| Adapter | Served model | Reasoning tokens |
| --- | --- | --- |
| `openai` Chat, `openrouter` Chat, local profiles | `model` of the body, or of the first stream chunk that carries it (attached to the usage snapshot) | `usage.completion_tokens_details.reasoning_tokens` |
| `openai` Responses | `response.model` (body or terminal snapshot) | `usage.output_tokens_details.reasoning_tokens` |
| `anthropic` | `model` of the body, or `message_start.message.model` | `usage.output_tokens_details.thinking_tokens` (final cumulative `message_delta`; the start event's preliminary value is ignored) |
| `bedrock` | `trace.promptRouter.invokedModelId` (Converse and the ConverseStream metadata event); otherwise not reported | Not reported: `TokenUsage` has no reasoning field |

A served model is 1–256 bytes of printable, non-space ASCII. Anything else (absent, empty, longer, whitespace, non-string) is unknown, never truncated and never an error. A reasoning count that is absent, malformed or above the output count is unknown, not zero. Streams carry both on the usage event: a stream that reports no usage records neither. Non-generation workloads do not record a served model.

## Streams, limits and accounting

Generation uses text/tool typed contracts, not arbitrary provider JSON. Unknown request fields/options fail rather than disappear. Upstream refusal/reasoning/audio content outside the subset is rejected. Adapter support never guarantees every upstream model accepts the request.

Streams contain deltas, one Finish, optional cumulative Usage and one terminal Done; EOF/duplicates/malformed frames fail. Usage observations are snapshots, not additive. Chat emits `[DONE]` only on success; midstream sanitized errors close without success. Responses/Messages frontend buffering is documented in the matrix. Dropping execution/stream owns cancellation: no detached network producer or hidden adapter retry.

Inbound HTTP bodies are 2 MiB. Provider complete bodies are 4 MiB; SSE frames 1 MiB; Chat supports at most 128 tool-call indices. Embeddings allow 128 strings, 1 MiB aggregate input content, dimensions at most 16384, finite uniform vectors and a 4 MiB response. OpenAI-wire vectors require complete unique indices; native Ollama arrays preserve batch order.

The default process concurrency cap is 128; request deadline defaults 120 seconds (1–3600 supported), connection timeout ten seconds. Durable policy/price admission occurs before dispatch. Finalization has a separate bounded storage window; crashes or failures can leave started rows for lease reconciliation. No prompts/tool arguments/output bodies are stored in execution accounting.

Raw usage and normalized cache partitions remain separate. Missing counters/rates never become zero. Input-only embedding output zero is semantic non-applicability. Prices and conservative holds are configured estimates, not vendor billing. See [cache pricing](cache-pricing.md) and [governance](governance.md).

## Add an adapter

1. Implement `ProviderAdapter` under `apps/gateway/src/providers/` with a stable registry ID.
2. Declare protocols and pure request-specific support checks. Embeddings and other non-generation workloads use their `execute_*` methods (default `Unsupported`), not synthetic chat messages.
3. Validate fields/options before credentials/network. Reject unrepresentable features; never add unchecked passthrough.
4. Return typed complete output or a lazy stream that owns transport/cancellation. Never retry internally.
5. Register at the composition root and add shared contracts plus profile-specific wire, usage, framing, cancellation and endpoint tests. `providers::contract::assert_text_chat_contract` takes the mock's expected Logs telemetry (served model, reasoning tokens) and checks it on both the complete and streamed reply; pass `Telemetry::UNKNOWN` when the provider reports neither.

Historical isolated provider work reported **77 provider tests**, not a whole gateway/database/browser integration pass. The current integrated run in [verification](verification.md) included the native Ollama mocked tests; that is still not live-server certification. This refresh ran no paid requests, live certification or fresh acceptance suite; see [verification](verification.md) for explicitly dated checks.

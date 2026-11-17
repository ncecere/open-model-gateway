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

Providers are startup registry entries, not engine branches or database enums. Model protocol declarations and adapter request-specific checks both apply. Which client protocols each profile can carry at all is one table, `providers::capabilities::PROFILES`: adapters' `supports_protocol` reads it, and so do route/model-setup validation (`route_unsupported_capability`), readiness counts and, through a drift test, the dashboard's `protocolProfiles`. A profile is a **narrow tested subset**, not a promise that every server/version/model or SDK option works.

## Registered transports

| Registry ID | Transport/profile |
| --- | --- |
| `openai` | Fixed `https://api.openai.com/v1`; native Chat, Responses, string embeddings, `gpt-image-*` image generation, audio transcription and speech, realtime sessions over `wss://api.openai.com/v1/realtime` (GA interface), and native batches on OpenAI Batch ([batches](batches.md)). It no longer offers `videos`: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24. No arbitrary cloud endpoint override. |
| `anthropic` | Fixed `https://api.anthropic.com/v1`; native Messages, representable Chat subset. |
| `bedrock` | Server AWS identity, an allowlisted named profile or an assumed IAM role; explicit region; optional allowlisted VPC endpoint; SigV4 Converse/ConverseStream, not an OpenAI URL. |
| `openai_compatible` | Explicit approved local Chat/embedding subset, Jina/Cohere-style rerank and TypeSafe System One; not universal compatibility. |
| `vllm`, `sglang` | Separately declared local Chat/embedding profiles plus rerank (vLLM's Jina/Cohere-style `/rerank`; SGLang's native `/v1/rerank`). No System One. |
| `ollama` | Compatible Chat plus **native `/api/embed`**, derived from approved `/v1` base; not compatible embedding fallback. System One on `/v1/systemone` (Ollama v0.35.0+). No rerank. |
| `openrouter` | Fixed `https://openrouter.ai/api/v1`; Chat (stream/non-stream), string embeddings, rerank, System One, images, audio transcription (needs account credit) and speech. `env:` key reference only. |

See [protocol matrix](protocol-matrix.md), [Bedrock](bedrock.md) and the provider-owned [local profile notes](../apps/gateway/src/providers/local/README.md). Ollama native embedding follow-up is present in source; it has not been freshly validated by this documentation refresh or certified against a live Ollama server. Do not extend earlier isolated-provider counts to that follow-up or whole-stack integration.

Local Chat maps the gateway's effective maximum (client `max_completion_tokens`, its legacy alias `max_tokens`, or the admission-pinned price ceiling when omitted) to upstream `max_tokens`. Priced Chat/Responses requests are bounded before adapter dispatch; adapters declare wire maximums via `generation_output_limit` so invalid defaults cannot create reservations. Legacy unpriced omission behavior remains unchanged, including Anthropic's 1024-token fallback. Reasoning parsers (SGLang, vLLM, Ollama) add a `reasoning_content`/`reasoning` string to the message or delta, and servers add `matched_stop`/`stop_reason` (token id or string) to the choice: local profiles accept exactly those scalar shapes, never return the trace to clients (its tokens stay in `completion_tokens`), and read SGLang's top-level `usage.reasoning_tokens` as Logs telemetry. Rerank and System One on local profiles are described [below](#local-rerank-and-system-one); a profile without them fails such routes before admission with `unsupported_capability`. All local profiles reject strict tool guarantees; Ollama rejects every explicit tool-choice option; generic compatible rejects required/named choices. Generic compatible/Ollama reject dimension overrides; vLLM/SGLang accept the bounded tested dimension field, subject to actual model support. Local Responses/Messages are not implied.

## Canonical generation termination

`FinishReason` distinguishes `Refusal` and `ContextWindowExceeded` from `ContentFilter` and `Length`. The Anthropic Messages adapter accepts native `refusal` and `model_context_window_exceeded`; the Messages codec preserves those stop reasons in JSON and SSE. Chat codecs explicitly map them to `content_filter` and `length`, never serialize the canonical enum names as wire reasons. The text-only Responses codec maps them to incomplete `content_filter` and `max_output_tokens`, without typed refusal parts or retrospective relabeling of streamed `output_text`. Interactive Anthropic Responses remains unsupported; native batch Responses lines use that codec. Existing persisted Logs finish labels intentionally remain coarse (`content_filter`/`length`); no schema changes are required. Usage evidence, explicit terminal-event validation, unknown holds and cancellation rules are unchanged. This is terminal fidelity in the bounded Messages subset, not global vendor conformance.

## Local rerank and System One

Both use the connection's approved base, pinned destinations and client (no redirects, proxies or reqwest retries; see [Lost connections](operations.md#lost-connections-483) for the pool), and the same status mapping as local Chat (401/403/3xx → `provider_configuration_error`, 429 → `rate_limit_error`, other 4xx such as OpenJev's 422 → `upstream_rejected`, 5xx including 529 → `upstream_unavailable`). Error bodies are never read. Request support is checked before the connection, and the connection before any credential is resolved.

| Profile | Rerank | System One |
| --- | --- | --- |
| `openai_compatible` | `POST {base}/rerank`, Jina/Cohere wire (live: a Jina `jina-reranker-v3.5` server) | `POST {base}/systemone`, TypeSafe wire (live: OpenJev) |
| `vllm` | `POST {base without /v1}/rerank`, vLLM's canonical route (its `/v1/rerank` is a deprecated alias) | No: vLLM serves no System One route |
| `sglang` | `POST {base}/rerank`, SGLang's native shape (mapped below) | No |
| `ollama` | No: Ollama has no rerank API | `POST {base}/systemone` (Ollama v0.35.0+) |

- **Jina/Cohere rerank** (vLLM `RerankRequest`/`RerankResponse`, `vllm/entrypoints/pooling/scoring/protocol.py`, main 2026-10-09): sends `model, query, documents` and `top_n` (clamped to the document count) only when the client sets it. `return_documents` is never sent; vLLM always echoes documents, and an echoed `document` is discarded. Results are `{index, relevance_score, document?}`; any other result field fails. `usage.total_tokens` and vLLM's `usage.prompt_tokens` are both the input; when both are present they must be equal. `usage.search_units` is observed when reported. Missing counters stay unknown.
- **SGLang rerank** (`V1RerankReqInput`/`RerankResponse`, `python/sglang/srt/entrypoints/openai/{protocol,serving_rerank}.py`, main 2026-10-10): the response is a bare array `[{index, score, document?, meta_info?}]` sorted by score, with per-pair `meta_info.prompt_tokens`. The adapter sends `return_documents:false` and **no `top_n`**: with `top_n`, SGLang drops the other pairs and their token counts. Every document must come back once; the gateway sorts by score (ties by index) and truncates to `top_n`. Input tokens are the sum of every pair's `meta_info.prompt_tokens`, unknown when any pair omits it (SGLang's decoder rerankers report none).
- **System One:** the TypeSafe body `{model, state, questions}` from the shared encoder, with no `provider` or other extras. Answers and `usage.input_tokens`/`output_tokens` (both required) are validated exactly as on OpenRouter (`providers/text_workloads.rs`). Input is uncached input-only. Ollama may add `prompt_eval_cached_count`, a subset of `input_tokens` restored from its cache (`llm/score.go`): it becomes the cache-read count, with no write category reported, so uncached input is derived only when the price marks cache writes not applicable. OpenJev and Ollama extensions (`images`, `steps`, `samples`, `think`, `keep_alive`) are never sent; image state parts are rejected as before.
- **Accounting:** rerank is input-only (semantic output 0) with `requests: 1` and `search_units` as observed. System One meters input and output tokens with `requests: 1` and `search_units: 0`. A body whose results or answers fail validation keeps its valid usage as attempt evidence. Local servers report no cost, so there is no provider cost evidence.

## OpenRouter

Registry ID `openrouter`; connection profile `openrouter` (fixed base, `env:` key, no `none`). Server configuration, never request fields:

| Variable | Meaning |
| --- | --- |
| `GATEWAY_OPENROUTER_DATA_COLLECTION` | Optional override, `deny` or `allow`; sent as `provider.data_collection` on every request. Unset, Admin › Settings › Data & privacy decides (default `deny`); see [settings](settings.md#data--privacy). |
| `GATEWAY_OPENROUTER_HTTP_REFERER` | Optional absolute http(s) URL sent as `HTTP-Referer` (attribution only). |
| `GATEWAY_OPENROUTER_TITLE` | Optional 1–128 printable ASCII characters sent as `X-Title`. |

Redirects, ambient proxies and implicit retries are disabled (reqwest's retry policy only: the connection pool beneath it may re-dispatch a request it never wrote, see [Lost connections](operations.md#lost-connections-483)). Non-success bodies are never read: OpenRouter error bodies carry the account `user_id` and embedded provider errors. Statuses map to sanitized kinds: 401/402 (bad key, or no credit on the gateway's account) → `provider_configuration_error`; 403 (moderation) and other 4xx → `upstream_rejected`; 408/524 → `timeout_error`; 429 → `rate_limit_error`; 5xx → `upstream_unavailable`; 3xx → `provider_configuration_error`. A 200 body (or stream frame) carrying `error` fails by its code; a non-JSON 200 is invalid.

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

- **Connection:** validates the connection before resolving the credential. It connects to `wss://api.openai.com/v1/realtime?model=<upstream model>` with `Authorization: Bearer <server secret>`, tokio-tungstenite (`=0.30.0`; the browser side stays on Axum `=0.8.9`'s tungstenite 0.29, named explicitly for error classification) and an explicit ring-backed rustls config with webpki roots. There are no proxies, redirects (handshake errors) or retries, and the connect + configuration deadline is 10 s. Handshake statuses map like Chat (401/403/3xx → `provider_configuration_error`, 429 → `rate_limit_error`, 5xx → `upstream_unavailable`); bodies are never read.
- **Configuration:** before returning, it sends its own `session.update` (server VAD with `create_response:false`, transcription off, per-response `max_output_tokens`) and waits for the acknowledging `session.updated`. A rejection returns `upstream_rejected`. An unsafe acknowledgement (automatic or idle responses, transcription) returns `invalid_upstream_response`.
- **Event classification:** `session.*` (with a safety flag), `response.created`/`response.done` (response id, status, normalized usage), `error` (bounded tokens only), `rate_limits.updated` (filtered) and input-transcription events (unaccounted, so the session fails closed). Every other event is forwarded verbatim. Upstream close or binary frames are errors.
- **Usage:** see [realtime](realtime.md#usage-normalization-openai). Missing or inconsistent counts are unknown, never zero.

Contract tests run against a scripted loopback WebSocket mock (`providers/openai/realtime/mock.rs`): the frontend and engine over a real listener and PostgreSQL. No live or paid realtime call was made.

### Async jobs (video, batch)

Job methods on `ProviderAdapter` (`supports_video_request`, `create_video`, `retrieve_video`, `delete_video`, `video_content`, and the native batch methods `native_batch`, `encode_native_line`, `submit_native_batch`, `retrieve_batch`, `cancel_batch`, `native_batch_results`, `decode_native_result`, `delete_native_batch`) default to `Unsupported`/false; video opts in with `supports_protocol(Videos)`, native batches per target and endpoint with `native_batch`. The engine never switches on providers: the adapter encodes canonical lines (with the gateway's `l<n>` ids, never client `custom_id`s) and decodes result lines into canonical responses. Each call is one bounded request: no polling loops, no retries, and dropping the future cancels it. The job service (`crate::jobs`) owns admission, ids, ownership, polling and settlement; see [async jobs](async-jobs.md).

OpenAI (shapes from the official OpenAPI spec via openai-python, 2026-10-08; mock-tested only, no paid calls):

- **Videos (retired):** OpenAI shut down this API on 2026-09-24, so the adapter no longer declares `videos` and new jobs are refused before admission. The calls below remain for jobs created earlier and stay mock-tested. `sora-*` upstream models only. `POST /videos` is a gateway-built multipart form with exactly `model, prompt, seconds, size`. `GET`/`DELETE /videos/{id}` and `GET /videos/{id}/content?variant=` are also used. Video `status` must be `queued|in_progress|completed|failed`. `seconds` is decimal text (an unparseable value is unknown). Only the error `code` is kept (sanitized to `[a-z_]`); messages are dropped. Content media types are allowlisted (`video/mp4`, images for thumbnails/spritesheets, generic binary) and streamed through.
- **OpenAI native batches:** chat-like lines are encoded as Chat Completions bodies (`/v1/chat/completions`) and embeddings as Embeddings bodies. `POST /files` is one chunked multipart stream (`purpose=batch`); an error item aborts the body, so the provider never receives a complete form. `POST /batches` carries `{input_file_id, endpoint, completion_window:"24h"}` (no client metadata); a refused create deletes the uploaded file. Results are the output file followed by the error file (`GET /files/{id}/content`); each line is decoded with the Chat/Embeddings decoders, and error lines keep only a sanitized code. Batch `usage` maps like Chat (cache reads only). After collection the input, output and error files are deleted (`DELETE /files/{id}`, 404 is done).
- **Anthropic native batches** (Chat Completions, Responses and Messages lines): one streamed `POST /v1/messages/batches` `{requests:[{custom_id, params}]}` body (params from the interactive encoder, never streaming), `GET`/`POST …/cancel` (`processing_status` `in_progress`/`canceling`/`ended` mapped to batch states; `request_counts` summed), `GET …/results` JSONL (`succeeded` messages decoded like interactive responses; `errored` keeps only the error type), and `DELETE` after collection.
- Upstream ids are validated (`[A-Za-z0-9._:-]{1,128}`) before use in a path. Statuses map like Chat, and error bodies are never read.
- OpenRouter: video is planned (the next video adapter). Its `POST /api/v1/videos` uses another request shape (`duration`, `resolution`, `aspect_ratio`, `polling_url`). It has no Batch API.

## Approve local endpoints out of band

`GATEWAY_LOCAL_UPSTREAMS` is server configuration, not an inference request field:

```json
[{"endpoint":"http://models.internal:8000/v1","addresses":["10.10.1.20"]}]
```

Approvals bind a canonical exact HTTP(S) base ending in `/v1` to pinned destination IPs. They are bounded to 64 endpoints and 16 addresses each. No DNS lookup occurs during approval loading or dispatch. URL credentials, query/fragment, encoded path escapes and noncanonical URLs are rejected. Redirects, environment proxies and automatic retries are disabled. HTTP destinations must be approved private addresses (loopback only in development); unspecified, link-local, multicast, broadcast, carrier-grade NAT and cloud-metadata destinations are always rejected. HTTPS retains certificate checks and accepts public and private addresses. Since v0.9.0 (#203) an approval is also refused, over both schemes, in every range the outbound egress policy (`egress.rs`) refuses for every other client: benchmarking (`198.18.0.0/15`, `2001:2::/48`), IETF protocol assignments (`192.0.0.0/24`), the deprecated 6to4 relay (`192.88.99.0/24`), documentation (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`, `3fff::/20`) and the AWS IPv6 metadata range (`fd00:ec2::/32`, where only the exact `fd00:ec2::254` was refused before, and the rest was also accepted over plain HTTP). One shared table (`special_use.rs`) decides this for both policies. An address that carries IPv4 (mapped, compatible, 6to4, Teredo) is judged by the IPv4 address it carries; NAT64 approvals are refused outright. A refused approval **stops `serve` at startup**, before any database connection, with a message naming each affected approval's host, address and category and `GATEWAY_LOCAL_UPSTREAMS` (never a credential). Commands other than `serve` do not load approvals. Use an ordinary public or RFC 1918 address, or remove the approval (upgrade note in [the release notes](releases/v0.9.0.md#local-upstream-approvals-in-special-use-ranges-are-now-refused-203)). `GET /health/ready` still reports `checks.local_upstream_approvals`, now always `"ok"`, because a gateway that is answering has loaded only permitted approvals ([operations](operations.md#health-endpoints)). Enforce independent network egress too; application approval is not a complete network policy.

**Batch scheduling signals.** A route's batch scheduling may read server load from `{approved base without the final /v1}/metrics` of an approved endpoint (for `http://models.internal:8000/v1`: `http://models.internal:8000/metrics`), with the same pinned client: no redirects, proxies, retries or credentials, a 2-second deadline and a 1 MiB body. No other metrics URL can be saved or fetched. Readings that fail count as busy ([batches](batches.md#server-load-signal)).

**Batch priority hint.** `vllm` and `openai_compatible` routes may opt in to a vLLM `priority` value on gateway-run batch lines (Chat and OpenAI-wire embeddings). The batch runner scopes the value to the one route a line is pinned to; the adapter adds `"priority"` only then, so interactive requests and routes without the setting never receive the field ([priority hint](batches.md#priority-hint)).

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

Streams contain deltas, one Finish, optional cumulative Usage and one terminal Done; EOF/duplicates/malformed frames fail. Usage observations are snapshots, not additive. Chat emits `[DONE]` only on success; midstream sanitized errors close without success. How a failure inside a returned stream is classified is [below](#failures-inside-a-returned-stream). Responses/Messages frontend streaming behavior is documented in the matrix. Dropping execution/stream owns cancellation: no detached network producer or hidden adapter retry.

### Failures inside a returned stream

Once a stream is returned the response is a `200` whatever happens next (#486, #614, #622). It ends in one terminal error event, never a success marker; usage the provider did not report stays unknown and the reservation keeps its hold (on a free price too: a rejection inside a returned stream is not a rejection before processing, so it never settles as a known zero); nothing is retried and nothing fails over. What the failure decides is its **class**, which sets the terminal error each client protocol shows ([matrix](protocol-matrix.md#errors-inside-a-stream)), the recorded execution error code, and the routing-health observation: `rate_limit_error`, `overloaded_error`, `upstream_unavailable` and `timeout_error` count against [routing health](routing.md#passive-observations); `upstream_rejected`, `context_length_exceeded`, `provider_configuration_error` and `invalid_upstream_response` do not.

**A provider error event is classified like the same error as an HTTP response**, for every client protocol. The one rule lives in `providers/stream_error.rs`, and the stream path and the response path share their status tables so they cannot drift (`stream_class_tests` compares them, for every name, against real HTTP responses):

1. The error's documented name (`code`, else `type`; an explicit `code` wins even when unknown) stands for an HTTP status; a numeric `code` is that status (vLLM, llama.cpp and OpenRouter send it so).
2. The adapter's own HTTP table classifies that status: `framing::status_error` (also behind `framing::status`) for Anthropic, OpenAI chat and Responses and the local profiles, OpenRouter's `status_error` for OpenRouter. For a Messages client the adapter's Messages response path applies its refinements too (`framing::generation_error`): 503 and 529 are `Overloaded`; for 400, 413 and 422 the error envelope refines (`context_length_exceeded`, `prompt_too_long` and `too_many_tokens`, and for Anthropic `invalid_request_error` with the documented `prompt is too long` prefix, are `ContextLength`), otherwise `UpstreamRejected`. OpenRouter's Messages path applies the refinement for those three statuses only, so its 529 stays `UpstreamUnavailable`.
3. The message text is never read, except for that one Anthropic prefix. A name that is not documented, a name in another casing, or an error with no readable name is **`UpstreamUnavailable`**: an unknown failure is evidence neither of a client fault or a configuration fault (those classes do not count against routing health, and a rejection could settle as a free known zero) nor of a safe capacity condition. It is never a success. A numeric `code` that is not an error status is `InvalidUpstream`.

| Name (`code` or `type`) | HTTP status | Chat and Responses clients | Messages clients | Routing health |
| --- | --- | --- | --- | --- |
| `rate_limit_error`, `rate_limit_exceeded`, `insufficient_quota` | 429 | `Busy` | `Busy` | counts |
| `api_error`, `server_error` | 500 | `UpstreamUnavailable` | `UpstreamUnavailable` | counts |
| `timeout_error` | 504 | `UpstreamUnavailable` | `UpstreamUnavailable` | counts |
| `overloaded_error`, `overloaded` | 529 | `UpstreamUnavailable` | `Overloaded` (OpenRouter: `UpstreamUnavailable`) | counts |
| `invalid_request_error`, `invalid_prompt`, the `invalid_image*` and other image codes | 400 | `UpstreamRejected` | `UpstreamRejected` | no effect |
| `context_length_exceeded`, `prompt_too_long`, `too_many_tokens` | 400 | `UpstreamRejected` | `ContextLength` | no effect |
| `request_too_large` | 413 | `UpstreamRejected` | `UpstreamRejected` | no effect |
| `not_found_error`, `model_not_found` | 404 | `UpstreamRejected` | `UpstreamRejected` | no effect |
| `billing_error` | 402 | `UpstreamRejected` (OpenRouter: `Configuration`) | same | no effect |
| `authentication_error`, `invalid_api_key` | 401 | `Configuration` | `Configuration` | no effect |
| `permission_error` | 403 | `Configuration` (OpenRouter: `UpstreamRejected`) | same | no effect |
| a numeric `code` | itself | the adapter's status table | with the Messages refinements | per class |
| any other name, or none | none | `UpstreamUnavailable` | `UpstreamUnavailable` | counts |

Where each wire shape is read and where its HTTP equivalent is defined:

| Adapter | In-stream error | Read from | HTTP equivalent (defined in) |
| --- | --- | --- | --- |
| Anthropic | `event: error`, `{"type":"error","error":{"type",...}}` | `error.type` | chat and Responses clients: `framing::status`; Messages clients: `anthropic_generation_status` (`framing::generation_error` with `anthropic_error`) |
| OpenAI chat, local profiles | an SSE frame (or a 200 body) `{"error":{...}}` | `error.code` (string or number), else `error.type` | `check_status` (= `framing::status`); Messages clients: `framing::generation_status` |
| OpenAI Responses | the `error` event (top-level `code`) and `response.failed` (`response.error.code`) | `code` | `framing::status` (the only HTTP table this transport has). `response.incomplete` is not an error: it is a terminal event that finishes as `length` or `content_filter`, unchanged. A `response.completed` that carries an `error` stays `invalid_upstream_response` |
| OpenRouter | a frame (or a 200 body) `{"error":{"code":...}}` | numeric `code`, else the name | OpenRouter's `status_error`; Messages clients: `generation_status` for 400, 413 and 422 only |
| Bedrock | an exception frame | the `:exception-type` header ([Bedrock](bedrock.md#exception-frames)) | `service_error` / `sdk_messages_error` |

**A transport failure inside a returned stream is `upstream_unavailable`** and counts against routing health, as a connection failure before the stream does. A frame that arrived whole and decodes at the transport layer but has the wrong shape or violates the protocol stays `invalid_upstream_response`. The line is where the failure arises: the HTTP body, the SSE decoder and the AWS event-stream decoder are the transport; the adapter's parser is the protocol.

| Adapter | Transport failure: `upstream_unavailable` | Protocol violation: `invalid_upstream_response` |
| --- | --- | --- |
| Anthropic, OpenAI chat, Responses, OpenRouter, local profiles (SSE) | a body error (reset, bad chunked or length framing, cut TLS: `framing::transport`); the body ending before the protocol's terminal frame, with or without a partial frame behind it (`framing::TRUNCATED`: Anthropic `message_stop`, chat `[DONE]`, Responses `response.completed`/`response.incomplete`); an SSE line that is not valid UTF-8 (`framing::UNDECODABLE`) | a frame whose data is not JSON; JSON of the wrong shape; an event not allowed in that state; a second terminal; `[DONE]` before a finish; a frame over the 1 MiB bound |
| Bedrock (event stream) | a body error (`wire::Failure::Body`); the body ending inside a frame (`Truncated`) or before `messageStop` and `metadata` (`stream::State::end`); a prelude length no frame can have, a failed CRC or malformed headers (`Corrupt`); the SDK decoder rejecting the bytes (`RawMessage::Invalid`) | a frame that decodes but means nothing to the SDK (no string `:exception-type`, a `:message-type` `error` frame, a payload that does not match its model), the gateway's usage-envelope guard, duplicate `:event-type` headers, an unknown or out-of-order event, a frame over the 8 MiB wire limit |

Not changed: a stream's idle and total timeouts keep their class. Bedrock's 60 s wait between events and the engine's request deadline are `timeout_error`; a read or total timeout of the HTTP transport inside the body is `timeout_error` too (for Bedrock it used to surface as `invalid_upstream_response`, an accident of how the body error was wrapped). A client that disconnects drops the request future, which drops the upstream transport: the attempt is recorded `cancelled` and neither counts as a provider failure nor touches routing health.

Inbound HTTP bodies are 2 MiB. Provider complete bodies are 4 MiB; SSE frames 1 MiB; Chat supports at most 128 tool-call indices. Embeddings allow 128 strings, 1 MiB aggregate input content, dimensions at most 16384, finite uniform vectors and a 4 MiB response. OpenAI-wire vectors require complete unique indices; native Ollama arrays preserve batch order.

The default process concurrency cap is 128; request deadline defaults 120 seconds (1–3600 supported), connection timeout ten seconds, and a per-client pool idle timeout (3 to 20 seconds, below the upstream's; see [Lost connections](operations.md#lost-connections-483)). Durable policy/price admission occurs before dispatch. Finalization has a separate bounded storage window; crashes or failures can leave started rows for lease reconciliation. No prompts/tool arguments/output bodies are stored in execution accounting.

Raw usage and normalized cache partitions remain separate. Missing counters/rates never become zero. Input-only embedding output zero is semantic non-applicability. Prices and conservative holds are configured estimates, not vendor billing. See [cache pricing](cache-pricing.md) and [governance](governance.md).

## Add an adapter

1. Implement `ProviderAdapter` under `apps/gateway/src/providers/` with a stable registry ID.
2. Declare the profile's protocols in `providers::capabilities::PROFILES` (and the dashboard's `protocolProfiles`; a test compares them), and add pure request-specific support checks. Embeddings and other non-generation workloads use their `execute_*` methods (default `Unsupported`), not synthetic chat messages.
3. Validate fields/options before credentials/network. Reject unrepresentable features; never add unchecked passthrough.
4. Return typed complete output or a lazy stream that owns transport/cancellation. Never retry internally.
5. Register at the composition root and add shared contracts plus profile-specific wire, usage, framing, cancellation and endpoint tests. `providers::contract::assert_text_chat_contract` takes the mock's expected Logs telemetry (served model, reasoning tokens) and checks it on both the complete and streamed reply; pass `Telemetry::UNKNOWN` when the provider reports neither. `assert_rerank_contract` and `assert_systemone_contract` check valid results/answers, input-only metering, one request, and token and search-unit counts exactly as the mock reports them (`None` stays unknown).

Historical isolated provider work reported **77 provider tests**, not a whole gateway/database/browser integration pass. The current integrated run in [verification](verification.md) included the native Ollama mocked tests; that is still not live-server certification. This refresh ran no paid requests, live certification or fresh acceptance suite; see [verification](verification.md) for explicitly dated checks.

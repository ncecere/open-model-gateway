# Protocol support

Current inference routes share key authentication, global public model aliases, live workspace grants, key restrictions, routing and per-attempt accounting. Support means the bounded text/function-tool or string-embedding subset below, **not universal SDK/vendor option compatibility**. Model `supported_protocols` and request-specific profile checks may narrow each row further.

| Adapter | Chat `/v1/chat/completions` | Responses `/v1/responses` | Messages `/v1/messages` | Embeddings `/v1/embeddings` | Rerank `/v1/rerank` | System One `/v1/systemone` |
| --- | --- | --- | --- | --- | --- | --- |
| `openai` | Native Chat | Native Responses | No | Native float/string subset | No | No |
| `anthropic` | Representable subset via Messages | No | Native Messages | No | No | No |
| `bedrock` | Text/tools via Converse | No | Text/tools via Converse | No | No | No |
| `openai_compatible` | Narrow compatible profile | No | No | OpenAI-wire subset; no dimensions | No | No |
| `vllm`, `sglang` | Profile-specific compatible subset | No | No | OpenAI-wire subset, bounded dimensions | No | No |
| `ollama` | Compatible Chat subset | No | No | Native `/api/embed`, no dimensions | No | No |
| `openrouter` | Compatible Chat subset (stream/non-stream) | No | No | Float/string subset; fixed native widths only | Native `/rerank` | Native `/systemone` |

### Speech routes

| Adapter | Transcriptions `/v1/audio/transcriptions` | Speech `/v1/audio/speech` |
| --- | --- | --- |
| `openai` | Native multipart (gateway re-encoded) | Native, `mp3`/`wav`/`opus`/`pcm` |
| `openrouter` | JSON base64 `input_audio`; no `prompt` | Native, `mp3`/`pcm` only |
| Others | No | No |

A model's protocols belong to one workload: Chat/Responses/Messages combine, while embeddings and each other kind stand alone. Images, audio, Rerank and System One routes return **400 `unsupported_capability`** when the model exists but has no deployment that declares the protocol on an adapter supporting it. Chat/Responses/Messages/Embeddings keep 501 for unsupported capabilities.

Unsupported combinations fail explicitly. Local labels do not mean every installed server/model supports all fields. Ollama native embedding follow-up has source and mocked tests, but this documentation refresh does not claim a fresh pass or live-server certification; consult [local profile notes](../apps/gateway/src/providers/local/README.md). [Bedrock](bedrock.md) documents its separate transport.

## Chat Completions

One choice (`n` absent or 1); text-string messages with system/developer/user/assistant/tool roles; function tools and tool-call/results; `temperature`, `max_completion_tokens`, streaming and optional `stream_options.include_usage`. Legacy client `max_tokens`, multimodal arrays, structured output, reasoning, log probabilities and arbitrary extensions are rejected.

Local transports translate the accepted maximum to upstream `max_tokens`. All local profiles reject strict function guarantees. Ollama rejects explicit tool choice; generic compatible rejects required/named choice. Anthropic Chat accepts leading system/developer text but rejects strict schemas, temperature above 1 and non-object tool arguments. Adapter defaults do not substitute for explicit generation bounds when a priced admission requires them.

```json
{"model":"company/smart","messages":[{"role":"user","content":"Hello"}],"max_completion_tokens":32,"stream":false}
```

## Responses

Stateless native OpenAI text/function subset: `model`, string or nonempty item-list `input`, `instructions`, `max_output_tokens`, `temperature`, `stream`, `store:false`, flat function `tools`, supported `tool_choice`, and `text:{"format":{"type":"text"}}`.

Message items support user/assistant/system/developer and text/input-text (assistant output-text also accepted). Function calls use `call_id,name,arguments` (string); results use `call_id,output` (string). Tools are flat Responses definitions, not nested Chat envelopes. OpenAI strict function schemas can be forwarded where supported.

Upstream requests explicitly set `store:false`. Upstream assistant message items may carry `phase: "commentary"` (preamble) or `phase: "final_answer"`. Text from both phases is returned as output text in upstream order. Any other phase value fails as an invalid upstream response rather than being dropped. `store:true`, `previous_response_id`, background/stateful operations, hosted tools, structured output, reasoning items, images/audio and annotations are unsupported. Max-token/content-filter terminal results are incomplete, not completed. This is not a universal chat-to-agent/state mapping.

## Messages

Requires exactly `anthropic-version: 2023-06-01`; beta headers are rejected. Supports `model,messages,system,max_tokens,temperature,stream,tools,tool_choice`. `max_tokens` is positive/required; temperature is 0–1. Text blocks, assistant object-input `tool_use` and user text/text-block `tool_result` are supported. `tool_choice` can be auto/none/any/named tool within adapter constraints. Because Messages streams are buffered (see below), `message_start` is sent after validated completion. Its `usage` has the Anthropic shape: observed `input_tokens` and cache counters, plus `output_tokens: 0` as the count at stream start. `message_delta` then reports the final cumulative output. Unknown counters are omitted, never sent as zero. Errors before completion send only an `error` event.

Thinking, images/audio, cache controls, metadata, error-marked tool results and unknown fields are rejected. Cache-aware usage accounting does not enable request cache controls. `/v1/messages` accepts one Bearer or `x-api-key`, never both; other inference routes use Bearer only. Repeated/conflicting credential headers fail.

## Embeddings

Non-streaming, nonempty string or string batch; optional `encoding_format:"float"` and positive profile-supported `dimensions`. Token-ID arrays, base64, stream and unknown fields are rejected.

```json
{"model":"company/embeddings","input":["First document","Second document"],"encoding_format":"float"}
```

Limits: 128 inputs, 1 MiB aggregate string bytes, maximum 16384 dimensions, overall 2 MiB inbound HTTP and 4 MiB response. Vectors must be finite, nonempty, uniform and match input count/requested dimensions. OpenAI-wire indices must be complete and unique. Client usage is numeric protocol shape when observed, omitted when unknown; financial token values remain strings/null.

Embeddings use input-only budget/valuation and semantic output zero, not chat accounting. Ollama sends native `truncate:false` to the same approved origin/prefix; it never falls back to truncating/compatible embedding endpoints. Operators must verify server non-truncation and hard aggregate ceilings.

## Rerank

Non-streaming Cohere v2 / Jina-style subset, inference keys only, with the same workspace catalog, key restriction and routing rules as Chat:

```json
{"model":"company/rerank","query":"cat","documents":["kitten","airplane","dog"],"top_n":2}
```

The response is `{"object":"list","id","model","results":[{"index","relevance_score"}],"usage":{...}}`. `id` is the gateway request ID. Results keep provider order and are validated to have unique, in-range indices, finite scores, and a count between 1 and `min(top_n, documents)`. Documents are never echoed. `usage` contains only observed counters (`total_tokens`, `search_units`) and never fake zeros. Unknown fields, including `return_documents`, `provider` and object documents, are rejected with 400.

Limits: at most 1000 non-blank documents, 64 KiB per document, 32 KiB per query, and 1 MiB of aggregate content. `top_n` is at least 1. The body cap is `GATEWAY_MAX_BODY_BYTES_RERANK` (default 2 MiB, over the cap returns 413). Rerank is input-only: tokens are uncached input with semantic output 0. Meters are `search_units` when the provider reports them (otherwise unknown, never zero) and `requests: 1`.

## System One

`POST /v1/systemone` implements the TypeSafe System One contract, so the TypeSafe SDK works unchanged with `base_url = https://<gateway>`. Use an inference key.

```json
{"model":"company/clef","state":"hello there","questions":{"is_q":{"type":"noul","instructions":"Is the text a greeting?"}}}
```

- **`state`:** a nonblank string, an object, or a nonempty array. Image parts (`{"type":"image_url"}` array items) are rejected as `unsupported_capability` because no profile declares image input.
- **Questions:** 1–64 keys, each 1–128 bytes. Each question is `noul` (optional `criteria:{true,false}`), `choice` (2–255 options), or `score` (2–10 levels). `instructions` and criteria values may be strings or JSON.
- **Rejected:** client `provider`, `user` and other unknown fields. Validation failures return **422** (TypeSafe-compatible).
- **Response:** `{"id","model","answers","usage":{"input_tokens","output_tokens"}}` with TypeSafe numeric answer shapes. Choice answers carry `probabilities` and `confidence`; score answers carry `legend`, `probabilities` and `confidence`.
- **Answer validation:** answers are checked against the questions: one per key with a matching type, a choice within the options, probability keys equal to the options or levels, values finite and within [0, 1], and sums within rounding tolerance.
- **Billing:** gateway billing stays in the financial APIs and is never added to this body.
- **Admission:** input tokens are metered; output tokens are reserved up to the pinned price's `output_token_limit` (some models report free output tokens), and `requests` is 1. The body cap is `GATEWAY_MAX_BODY_BYTES_SYSTEMONE` (default 2 MiB).

## Images

`POST /v1/images/generations`, an OpenAI-compatible non-streaming subset. Use an inference key.

```json
{"model":"company/image","prompt":"a red dot","n":1,"size":"1024x1024","quality":"low","response_format":"b64_json"}
```

| Adapter | Upstream | `size` | `quality` | `seed` |
| --- | --- | --- | --- | --- |
| `openai` | `POST /v1/images/generations`, `gpt-image-*` models only | `auto`, `1024x1024`, `1536x1024`, `1024x1536` | `auto/low/medium/high` | Unsupported |
| `openrouter` | `POST /images` | Tier `512`, `768`, `1K`, `1.5K`, `2K`, `4K` (sent as `resolution`; omitted means `1K`) | Passed through | Passed through |
| Others | No | | | |

- **Request:** `model` and a nonblank `prompt` (at most 32,000 characters) are required. `n` is 1–4 (default 1). `response_format` may only be `b64_json`. `"url"` returns 400 `unsupported_capability`, because the gateway never hosts files. Unknown fields (`output_format`, `background`, `style`, `user`, `provider`, ...) and unknown sizes/qualities are rejected with 400 `invalid_request_error`. A size, quality or seed that no deployment's adapter supports returns 400 `unsupported_capability` before admission.
- **Response:** `{"created","data":[{"b64_json","revised_prompt"?}],"usage"?}`. `usage` has only observed `input_tokens`, `output_tokens` and `total_tokens`, never fake zeros. It is omitted when the provider reports nothing.
- **Validation:** every image must be strict padded base64 whose bytes start with PNG, JPEG or WebP magic. A declared `media_type`/`output_format` must match the bytes. Exactly `n` images must be returned. URL items, SVG, unknown content fields and count mismatches fail as `invalid_upstream_response`. The attempt keeps its hold, and the provider's usage and returned-image count are recorded as evidence.
- **Limits:** the inbound body cap is `GATEWAY_MAX_BODY_BYTES_IMAGES` (default 2 MiB). The upstream response cap is `GATEWAY_MAX_RESPONSE_BYTES_IMAGES` (default 20 MiB, 1–64 MiB). It applies only to images; other workloads keep their 4 MiB provider cap.
- **Metering:** `output_images` is the count returned, `requests` is 1, and character/audio/search meters are semantic zeros. `output_image_variant` is the price tier: OpenAI's generated pixel size (for example `1024x1024`), or the OpenRouter tier in endpoint-pricing spelling (`768`, `1k`, `1.5k`, `2k`, `4k`). Token meters follow each provider (see [provider adapters](provider-adapters.md)).
- **Admission:** the hold is `n` × the highest priced `output_images` variant, plus input tokens at the price's `input_token_limit` and output tokens at its `output_token_limit` (image tokens cannot be bounded by the request). Character/audio/search ceilings are 0. A possible meter without a price (for example no `output_images` line) is unbounded: budgeted admission is denied with `budget_exceeded`. At settlement, a billed variant without a matching or default line is unresolved.
- Prompts and image bytes are never logged or stored.

## Audio transcriptions

`POST /v1/audio/transcriptions`, an OpenAI-compatible `multipart/form-data` subset. Use an inference key (Bearer).

```sh
curl https://gateway/v1/audio/transcriptions -H "Authorization: Bearer $KEY" \
  -F model=company/whisper -F file=@clip.wav -F response_format=json
```

- **Fields:** exactly one `file` part with a filename, `model`, optional `language` (2–3 lowercase letters), `prompt` (at most 4096 bytes), `response_format` (`json` default, or `text`) and `temperature` (0–1). `stream=false` is accepted. `verbose_json`/`srt`/`vtt`/`diarized_json`, `timestamp_granularities[]`, `include[]`, `chunking_strategy`, known-speaker fields and `stream=true` return 400 `unsupported_capability`. Any other field, a duplicate field, a filename on a text field, or a text field over 4096 bytes is 400 `invalid_request_error`.
- **Multipart parsing is strict:** a `multipart/form-data` boundary is required. There is no preamble and no epilogue beyond a final CRLF. Part headers may be only `Content-Disposition: form-data` (`name`, `filename`/`filename*`) and `Content-Type`, at most 8 KiB, and there are at most 16 parts.
- **File:** the extension of the last path component must be `flac`, `mp3`, `mp4`, `mpeg`, `mpga`, `m4a`, `ogg`, `wav` or `webm`. A part `Content-Type` must be a matching audio type, or `application/octet-stream`/absent (the extension decides). The container magic must match. The client filename is never forwarded, logged or echoed; upstreams receive `audio.<ext>`.
- **Limits:** `GATEWAY_AUDIO_MAX_UPLOAD_BYTES` caps the file part (default 25 MiB, 1 KiB–64 MiB) and returns 413. It is separate from the body cap `GATEWAY_MAX_BODY_BYTES_AUDIO_TRANSCRIPTIONS` (default 26 MiB, covering multipart framing; also 413) and from the 2 MiB JSON cap.
- **Response:** `json` is `{"text","usage"?}`, where `usage` is OpenAI's discriminated shape from observed counters only: `{"type":"tokens","input_tokens","output_tokens","total_tokens"}` or `{"type":"duration","seconds"}`. It is omitted when nothing was observed. `text` is `text/plain; charset=utf-8`. Upstreams are always asked for JSON, so usage is captured either way. Transcripts are never logged or stored.
- **Duration (server side):** the gateway measures the upload from container headers without decoding:
  - **WAV:** PCM/float/A-law/µ-law with a consistent `fmt `. Every byte after `data` counts as audio.
  - **MP3:** every MPEG frame header is walked, with ID3v2/ID3v1/APE tags skipped. Junk between frames makes the duration unknown.
  - **Ogg Vorbis/Opus:** pages are CRC-checked, there must be a single stream ending exactly at EOF, and the duration is the highest granule (Opus minus pre-skip).
  - **FLAC:** the STREAMINFO total samples. This is a declared value: an understated header makes the provider-reported duration exceed the admission ceiling, so the response is withheld (`invalid_upstream_response`) and the observed duration is still recorded against the retained hold.
  - **MP4/M4A and WebM:** not measured.
- **Metering:** `input_audio_seconds_ms` is the provider-billed duration when reported (OpenAI `usage.type:"duration"` whole seconds; OpenRouter `usage.seconds` as exact decimal text, rounded up to ms). For token-billed models (OpenAI `usage.type:"tokens"`) the tokens are recorded, and so is the measured duration when known. Unreported counters stay unknown. `requests` is 1; character/output-audio/image/search meters are semantic zeros.
- **Admission:** with a measured duration, the `input_audio_seconds_ms` ceiling is the duration rounded up to whole seconds, plus one second. A provider observation above it fails as `invalid_upstream_response`, and the usage is still recorded. Without a measurement, only the price's `max_units` bounds the meter. Otherwise the hold is unbounded and budgeted admission is denied (`budget_exceeded`). A measured ceiling above the price's `max_units` is also unbounded rather than under-held. Output tokens are reserved to the price's `output_token_limit`.
- **Pricing:** per-second/minute/hour models use an `input_audio_seconds_ms` line. Token meters a provider never charges should be `not_applicable`; unreported tokens are then semantic zeros, so the attempt can settle. For meters a model reports but does not bill (OpenRouter Whisper may report tokens; OpenAI token models also record measured audio), use explicit `"0"` lines rather than `not_applicable`.

## Speech

`POST /v1/audio/speech` (JSON) returns binary audio. Use an inference key.

```json
{"model":"company/voice","input":"Hello there","voice":"alloy","response_format":"mp3"}
```

- **Request:** `model`, nonblank `input`, `voice` (1–64 of `[A-Za-z0-9._:-]`), optional `response_format` (`mp3` default, `wav`, `opus`, `pcm`) and `speed` (0.25–4). `aac`/`flac`, `instructions` and `stream_format:"sse"` return 400 `unsupported_capability`. Other unknown fields are 400 `invalid_request_error`. A format the deployment's adapter does not produce (OpenRouter: only `mp3`/`pcm`) returns 400 `unsupported_capability` before admission.
- **Characters:** `input` is limited to `GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS` (default 4096, at most 100,000), counted exactly as **Unicode scalar values** (Rust `char`s, not bytes or UTF-16 units) before admission. The same count is the `input_characters` meter. Providers may count differently: OpenRouter's "character" unit is not yet verified for non-ASCII text.
- **Response:** 200 with an explicit gateway `Content-Type` (`audio/mpeg`, `audio/wav`, `audio/opus`, `audio/pcm`) after the upstream media type was checked against the requested format. The body is streamed through as it arrives, without buffering. The first chunk is awaited before headers are sent, so an empty or immediately failing upstream body is a JSON error and eligible for explicit failover. Later failures, a deadline, or more than `GATEWAY_AUDIO_SPEECH_MAX_OUTPUT_BYTES` (default 64 MiB) abort the transfer. The client sees a truncated body, never a clean end. The attempt is settled only after upstream EOF and before the body completes.
- **Cancellation:** dropping the client connection drops the upstream request synchronously. The attempt is recorded as cancelled with its character usage, and the hold is retained (state unknown), because the provider may still charge.
- **Metering:** `input_characters` (exact), `requests` 1, semantic zeros for images/input audio/search. Output audio duration is never reported, so it is unknown: price `output_audio_seconds_ms` as `not_applicable`, or give it `max_units`, or the hold is unbounded. Upstreams return no usage on this route: OpenAI `tts-1`/`gpt-4o-mini-tts` token usage (only on SSE) and OpenRouter cost (only via the delayed generation lookup) are not observed, so token meters stay unknown unless priced `not_applicable`. Value per-character models such as `tts-1` ($15/M) or `microsoft/mai-voice-2-flash` ($15/M) with an `input_characters` line.
- Speech input and generated audio are never logged or stored.

## Streaming and evidence boundaries

Native upstream SSE parsers tolerate fragmented network/UTF-8 boundaries and require explicit terminal events, not EOF. Bodies are limited to 4 MiB, frames to 1 MiB. Redirects, environment proxies and hidden provider retries are disabled.

Chat frontend streams incrementally. Responses/Messages start streaming but buffer at most 4 MiB of normalized content before ordered item/block delivery; they are **not token-by-token frontend streams**. Text is consolidated before tools because the shared generation result does not preserve mixed text/tool positions. Parallel tools are serialized into valid lifecycles. Engine Done alone authorizes success; malformed arguments, errors, EOF and overflow close with sanitized errors rather than success. When an upstream body or terminal snapshot fails structural validation but carries a valid usage object, the attempt still fails and keeps its hold. The validated counts are recorded on the failed execution instead of being lost. This applies to non-stream OpenAI Chat/compatible, Responses and Anthropic Messages bodies, and to the Responses stream terminal snapshot.

Dropping streams/futures requests cancellation without promising zero charges. No failover occurs after stream return. See [routing](routing.md) and [provider adapters](provider-adapters.md).

The earlier “native protocol milestone 3” is historical. Isolated provider tests (including the earlier 77-test result) are not whole-stack, new-native-follow-up, real-server or production-readiness evidence. [Verification](verification.md) records dated checks separately.

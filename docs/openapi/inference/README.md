# Inference OpenAPI contract maintenance

[inference.openapi.json](../inference.openapi.json) is the **incomplete inference contract** for #82: the documented `/v1/*` surface of the gateway. It is generated test-only from the compiled `route_policy::ROUTES` entries under `/v1/` plus the reviewed fragments in this directory, with the #81 fragment format and loader. Nothing here changes runtime behaviour, adds a runtime endpoint or a production dependency, and no CI or workflow file is involved: the gates are ordinary `cargo test` tests.

Status after PR3: **27 operations, 6 incomplete request/response slots**, all reserved for PR2’s Chat, Responses and Messages operations. The falling baseline must reach zero when PR2 lands; no media, file, batch, video or Realtime operation may remain a placeholder.

## Run the gates

```sh
cargo test -p open-model-gateway --lib inference_openapi -- --nocapture
# with a PostgreSQL DATABASE_URL (sqlx creates isolated test databases):
cargo test -p open-model-gateway --lib --all-features inference_openapi
```

Normal tests regenerate in memory and compare exact deterministic bytes with the committed artifact. To write it, after reviewing fragment changes:

```sh
OMG_REGENERATE_OPENAPI=1 cargo test -p open-model-gateway --lib inference_openapi_artifact_matches_regeneration
```

Writing is refused whenever `CI` is set, regeneration validates the document first, and nothing touches the network or a production endpoint. The DB-free gates (generator, drift, vocabulary, envelopes, real-DTO vectors, real-handler refusals, real credential parsers) run without a database; the real-handler fixtures need a disposable PostgreSQL and the `integration-tests` feature.

## What is generated and what is reviewed

Generated from source (a fragment cannot override any of it): paths, methods, path parameters, `operationId`, `x-omg-access: InferenceKey`, `x-omg-token-policy: Denied`, `security`, the shared error vocabulary and envelopes, and the reusable headers and responses.

* **Route parity is three-way**: the `/v1/` entries of `ROUTES`, the independent source scan of every route registration (`authz_matrix::inventory`) and the artifact must name the same method/template pairs. A new route (`count_tokens`, a Wave 7 realtime route) fails until its fragment lands. Omitted, extra, wrong-method, wrong-template, access, scope, token-policy, security, path-parameter and invented-query-parameter drift each fail (`wiring_tests.rs`).
* **Authority** is inference-key only. `security` offers `inferenceKey` (Bearer); `/v1/messages` adds the `x-api-key` alternative and `GET /v1/realtime` the `openai-insecure-api-key.<key>` WebSocket subprotocol, never both with Bearer. The table is proven against the real credential parsers by sending each form to the real router (a well-formed credential in an accepted form reaches key authentication; anything else is a 401 first). Inference keys never authorize management, and admin tokens never authorize inference.
* **Query parameters** are exactly what handlers read (`GET /v1/realtime?model=`); every other query string is ignored by the real handlers, so none may be documented.

Reviewed fragments (`operations.json` for placeholders, `simple-workloads.json` for the completed areas) have the #81 shape `{operations, schemas}` with `deny_unknown_fields`, duplicate-key and duplicate-name rejection. `operation` supplies hand-written OpenAPI fields; a completed operation replaces its generated `<operationId>Request` / `<operationId>Response` placeholder components by exact name.

## Error envelopes (never the management `ErrorEnvelope`)

Four native shapes, distinct schemas, each generated from source:

| Schema | Used by | Shape |
| --- | --- | --- |
| `OpenAiErrorEnvelope` | every OpenAI-shaped route | `{error:{message,type,code,param:null}}`; `code` is a gateway machine code |
| `AnthropicErrorEnvelope` | `/v1/messages` | `{type:"error",error:{type,message}}` with **no gateway machine code** |
| `JobErrorEnvelope` | videos, batches | OpenAI-shaped, types `invalid_request_error` / `permission_error`, job codes |
| `FileErrorEnvelope` | files | OpenAI-shaped, nullable string `param` |

* The code vocabulary is generated from `InferenceError::code` over a compile-time exhaustive variant list (adding a variant breaks the build until it is listed), the types from the real renderers (`protocols::batch_line_error`), and the job codes from a `syn` scan of the `JobError::{Conflict,Invalid,Forbidden}` literals under `src/jobs`. A reviewed table (`errors::OPENAI_TRANSPORT`) adds the transport-only types and codes, each checked to still occur in its source file.
* `/v1/messages` errors follow Anthropic's shape and carry **no gateway machine code** (coordinator decision 5): `unsupported_capability`, `budget_exceeded` and the rest cannot be told apart there except by their message. The outer transport layer also rewrites 405 and 413 on that path into Anthropic's shape (`http.rs` `request_context`, `protocols/messages.rs` `http_error`); both are documented on the operation even though it is still incomplete, and the 413 is proven on the real stack.
* The management `ErrorEnvelope` is refused by name and by reference, and an operation may only use the envelope family of its protocol.
* A declared `Content-Length` over a body cap is refused before any handler by the body-limit layer with the plain text `length limit exceeded` (`text/plain`, 413); a streamed oversized body is refused by the handler with its JSON error. Both are documented on every body-bearing route (except `/v1/messages`, whose 413 is always Anthropic-shaped).
* Headers: `x-request-id` (UUID) and `Cache-Control: no-store` on every response; `WWW-Authenticate: Bearer` on every 401; `x-should-retry: false` on permanent refusals; `Retry-After` when every route of a model is cooling down.
* `POST /v1/embeddings` uses the shared streamed-body 413 envelope: `type: invalid_request_error`, `code: payload_too_large`, message "Request body too large" (#432).

## Wire validation, coverage and vectors

`inference_openapi/responses.rs` captures status, headers and bytes before any JSON decoding and validates them against the exact `METHOD template` and status with the offline Draft 2020-12 validator. It is a copy of the management `validate` shape because the management one requires `error.code` to equal the HTTP status string, which is wrong for machine codes; the validator, schema quality rules, coverage engine and HTTP boundary vectors are the shared #81 code. The statuses of the documented responses are checked against what the real renderers can produce for each route.

Completed operations are executed through the real router, authentication, catalog, routing and admission with a deterministic provider adapter (`fixtures_tests.rs`). The #81 gates apply: every operation needs a validated positive response, array and union-branch coverage, executed HTTP boundary vectors (one run per wire form and per union arm a valid body does not carry), and every declared request location must be walked. Request DTOs are additionally compared with the real serde types (`workload_tests.rs`): the field inventory must match (an invented or dropped field fails), the contract may be stricter than serde only on reviewed fields, never looser. Serde also accepts the positional struct array of each DTO, which the top-level schemas list as their second arm; System One's nested questions are documented in object form only. The two realtime refusals have no 2xx response; their reviewed 400 carries `x-omg-positive-kind: refusal` and counts as their positive observation.

Cross-checks OpenAPI cannot express: the embedding vectors of a response must be in the requested `encoding_format` (float arrays, or padded little-endian float32 base64), and the System One validation status is 422 where every other workload answers 400.

## `x-omg-*` extensions

OpenAPI 3.1 cannot express everything the real surface does; the contract states it with extensions (SDK generation does not cover them): `x-omg-access`, `x-omg-token-policy`, `x-omg-schema-status`, `x-omg-request-schema`, `x-omg-response-schema` (as in the management contract); `x-omg-max-bytes` (a UTF-8 byte limit, where `maxLength` counts characters); `x-omg-positive-kind: refusal`; `x-omg-limits` (below); and the vector-harness exemptions of the #81 README (`x-omg-unsendable-boundary`). PR3 adds `x-omg-websocket`, `x-omg-cancellation`, `x-omg-multipart`, `x-omg-binary-stream`, `x-omg-parsed-number` and refusal tables; PR2 owns `x-omg-sse`. Open JSON is allowed only at reviewed pointers (`inference_openapi::ARBITRARY_JSON`, each with its justification): System One forwards TypeSafe content parts verbatim, and several maps have caller-chosen keys.

## Configurable limits

Limits are runtime configuration; the artifact states the defaults and names the environment variables (`x-omg-limits`), with no parity gate on configured values:

| Operation | Limit | Default | Environment variable |
| --- | --- | --- | --- |
| `POST /v1/embeddings` | request body | 2 MiB, fixed | none (`http.rs`) |
| `POST /v1/rerank` | request body | 2 MiB | `GATEWAY_MAX_BODY_BYTES_RERANK` (1 KiB to 64 MiB) |
| `POST /v1/systemone` | request body | 2 MiB | `GATEWAY_MAX_BODY_BYTES_SYSTEMONE` |
| `POST /v1/images/generations` | request body | 2 MiB | `GATEWAY_MAX_BODY_BYTES_IMAGES` |
| `POST /v1/images/generations` | upstream response | 20 MiB | `GATEWAY_MAX_RESPONSE_BYTES_IMAGES` (1 to 64 MiB) |

Fixed limits that are constants of the handlers (embeddings: 128 inputs, 1 MiB in total, 16384 dimensions; rerank: 1000 documents of 64 KiB, 32 KiB query, 1 MiB in total; System One: 64 questions; images: 4 images, 32000-character prompt, 2^53-1 seed) are part of the schemas or their descriptions. The aggregate caps that no schema keyword can express (total bytes) are documented, not asserted.

## Messages beta headers

The decision text said Messages "rejects beta headers". The handler accepts an open list and refuses only a denylist (`protocols/messages.rs`, `docs/protocol-matrix.md`); the artifact documents the handler. The stale in-repo sentence (`docs/guardrails-design.md`, which also said `cache_control` and thinking were rejected) is corrected in the same PR.

## PR3 media, jobs and WebSocket representation

The falling baseline is **6 incomplete schema slots**, all owned by PR2's
Chat, Responses and Messages operations. Removing those six when PR2 lands
must produce zero; PR3 does not claim those generation protocols are complete.

OpenAPI 3.1 cannot describe the full wire protocols. The reviewed `x-omg-*`
extensions are part of this contract, **not covered by SDK generation**:

- `x-omg-multipart` specifies part headers, duplicates, filenames, ordering,
  byte limits and framing that JSON Schema cannot check. Transcriptions use
  the bounded buffered audio parser; Files use a different streaming parser.
  When purpose precedes file, file must be last and expiry fields must already
  be complete. When file precedes purpose, expiry fields may be before or after
  file; remaining fields are read before the provisional upload is committed,
  moved or discarded.
- `x-omg-binary-stream` requires clean transport EOF; `x-omg-binary-variants` lists the legacy video MIME allowlists per asset variant. A late speech, file-store
  or video-provider failure truncates the body; HTTP 200 and a valid prefix are
  not proof of a successful download. `x-omg-cancellation` names the executed
  drop test; cancellation itself cannot be inferred from response bytes.
- `x-omg-websocket` describes the 101 handshake, mutually exclusive Bearer or
  key-subprotocol authentication, the client-event allowlist and close codes
  1000 (idle/session expiry), 1003 (binary client frames), 1008 (event/policy/rate
  denials), 1009 (transport size cap), and 1011 (fatal upstream/server failure).
  Frames are not HTTP bodies. See [Realtime](../../realtime.md).
- `x-omg-refusals` records status, native envelope, code and trigger. Video
  creation currently refuses after validation; the read/delete routes remain
  for legacy jobs. Batch invalid-input reports are a separate `errors.data[]`
  list with line numbers, not an array in the top-level `error` object.
- `x-omg-limits` records **defaults**, not a parity promise about a configured
  installation. Runtime admission, live workspace/key policy and configured
  caps always remain authoritative.

### Configurable defaults

| Limit | Default | Environment variable |
|---|---:|---|
| Transcription body | 26 MiB | `GATEWAY_MAX_BODY_BYTES_AUDIO_TRANSCRIPTIONS` |
| Speech body | 2 MiB | `GATEWAY_MAX_BODY_BYTES_AUDIO_SPEECH` |
| Transcription file part | 25 MiB | `GATEWAY_AUDIO_MAX_UPLOAD_BYTES` |
| Speech input Unicode scalar values | 4096 | `GATEWAY_AUDIO_SPEECH_MAX_INPUT_CHARS` |
| Speech output | 64 MiB | `GATEWAY_AUDIO_SPEECH_MAX_OUTPUT_BYTES` |
| Files upload | 200 MiB (+64 KiB form overhead) | `GATEWAY_FILES_MAX_BYTES` |
| Video create/list route body | 2 MiB | `GATEWAY_MAX_BODY_BYTES_VIDEOS` |
| Batch input validation file | 200 MiB | `GATEWAY_MAX_BATCH_FILE_BYTES` |
| Batch output scan | 1 GiB | `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` |
| Realtime session duration | 900 seconds | `GATEWAY_REALTIME_MAX_SESSION_SECONDS` |
| Realtime idle timeout | 120 seconds | `GATEWAY_REALTIME_IDLE_SECONDS` |
| Realtime output per response | 4096 tokens | `GATEWAY_REALTIME_MAX_OUTPUT_TOKENS` |
| Realtime message/frame | 1 MiB | `GATEWAY_REALTIME_MAX_MESSAGE_BYTES` |
| Realtime event rate | 50/second | `GATEWAY_REALTIME_MAX_EVENTS_PER_SECOND` |

The root limit for the core Chat/Responses/Messages, embeddings/models group
and Realtime is fixed at 2 MiB (no environment variable). Media/workload
routes use the named caps above; job/file reads also have route-specific
body-limit layers.
A declared Content-Length over the applicable route limit yields plain text
413 `length limit exceeded`, even on GET. A handler reading a streamed body
may instead return its documented native JSON 413. Authentication and the
route-specific layer ordering are not bypassed by this documentation.

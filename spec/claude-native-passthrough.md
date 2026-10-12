# Claude native passthrough (#70, #71)

Status: proposal for approval. Nothing here is implemented yet.

## Problem

Claude Code's gateway contract (<https://code.claude.com/docs/en/llm-gateway-protocol>) asks a gateway to:

- accept `POST /v1/messages?beta=true`;
- forward `anthropic-version` and `anthropic-beta` unchanged, without allowlisting beta values ("the set changes with Claude Code releases");
- pass request bodies through unchanged: `thinking` (`{"type":"adaptive"}` on 4.6+), `context_management`, `output_config`, tool `strict`/`defer_loading`, `cache_control` anywhere (including block-form `system` and mid-conversation system entries), 1-hour cache `ttl`;
- return Anthropic's own errors. For example, "prompt is too long" drives Claude Code's compaction.

Today `/v1/messages` rejects any `anthropic-beta` header and unknown fields (`deny_unknown_fields`). It normalizes the request into the shared `ChatRequest` (text, tools, tool results only), so it converts `system` blocks to a string and drops `cache_control`. The Anthropic adapter rejects `thinking` and other non-text blocks in responses. Upstream 4xx errors become a generic `invalid_request_error`. Claude Code therefore cannot use the gateway beyond basic chat.

## Decision proposed

A **native passthrough mode** for `/v1/messages`, only on deployments whose provider is the native Anthropic adapter. Everything else keeps today's normalized path.

### Request

1. The frontend parses only what the gateway itself needs: `model` (gateway model name, string), `max_tokens` (positive integer), `stream` (bool) and `metadata.user_id` (client label). Everything else stays a raw JSON object. The inbound body cap is unchanged (2 MiB).
2. The engine selects a native deployment as today: same authorization, workspace grants, routing plan, admission, reservation and failover rules. If the routed candidates include no native Anthropic deployment, the request falls back to today's normalized path. If normalization then fails (an unsupported field or beta header), it returns today's explicit `400 invalid_request_error`. Nothing is silently dropped.
3. The adapter sends the original body with only `model` replaced by the deployment's upstream model. It forwards `anthropic-version` (still exactly `2023-06-01`) and `anthropic-beta` verbatim. It never forwards the client's `Authorization`/`x-api-key` or any other header. It adds its own provider credential. Redirects, proxies and implicit retries stay disabled.

### Admission and accounting (#71)

- Holds are unchanged: input is bounded by the price's `input_token_limit`, and output by `max_tokens` (Anthropic counts thinking tokens inside `max_tokens`), capped by the price's output limit. Over-limit requests are refused before any upstream call, as today.
- Settlement uses only counts Anthropic reports: `input_tokens`, `output_tokens` (includes thinking), `cache_read_input_tokens`, `cache_creation_input_tokens` and the `ephemeral_5m`/`ephemeral_1h` split. The existing parser already handles these, as does cache pricing with its TTL-overlap rule (writes are not double charged). Missing counts stay unknown and keep the hold. Missing cache rates make the cost unknown, never zero.
- Server-side tools (web search, code execution) bill extra units that the gateway does not price. **Requests that declare server tools are refused** (`400`) until they are priced. That is a body inspection of `tools[].type`, not a field allowlist.

### Response

- **Non-stream:** the upstream JSON body is returned unchanged, apart from `model`, which is set to the gateway model name. Usage is read from it for settlement.
- **Stream:** upstream SSE events are relayed verbatim as they arrive, so `thinking`, `signature_delta`, `redacted_thinking` and future block types work. `message.model` in `message_start` is the only rewrite. The adapter still parses each event for framing (1 MiB/event, 4 MiB total stays as today, or is raised for thinking; see the open question), for usage (`message_start`, `message_delta`) and for the terminal `message_stop`. EOF without `message_stop` is an error, never a fabricated success. Client disconnect cancels upstream. There is no failover once streaming has started.
- **Errors:** an upstream 4xx/5xx with a well-formed Anthropic error body is returned with its HTTP status, `error.type` and `error.message` (message capped at 1 KiB). Anything else maps to today's sanitized error.

### Explicitly unchanged

Inference keys are never forwarded and never authorize management. Unknown usage is never zero. There is one reservation per attempt. Personal privacy rules are unchanged. Translated providers (OpenAI, Bedrock, OpenRouter, local) do not gain passthrough.

## Tests

- **Frontend:** `?beta=true` and beta headers accepted; unknown fields preserved; normalized-path rejection when no native deployment exists.
- **Adapter (mock HTTP):** the body arrives byte-equivalent except `model`; headers forwarded and credentials not; thinking and cache blocks relayed verbatim (stream and non-stream); usage parsed; upstream error fidelity; EOF and oversize handling; cancellation.
- **Database (PostgreSQL 17):** cache read/write/1h settlement, unknown cache rate keeps the hold, server-tool refusal.
- **SDK contract:** an official Anthropic SDK stream with thinking and cache_control against the mock.
- **#73 (separate, paid):** real Claude Code against a real Anthropic connection under a spending cap.

## Open questions for approval

1. Should the 4 MiB stream bound stay, or rise (for example to 16 MiB) for long thinking sessions?
2. Should server tools be refused for now (proposed), or passed through with cost marked unknown?
3. Should `/v1/messages/count_tokens` be forwarded too? It is optional: Claude Code falls back to estimates without it.

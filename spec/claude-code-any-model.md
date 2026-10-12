# Claude Code through the gateway, with any model (#70, #71)

Status: revised proposal for approval. Nothing here is implemented yet.

## Goal

Claude Code (`ANTHROPIC_BASE_URL` pointed at the gateway) works with **any model the gateway serves**, including Anthropic, OpenAI, Bedrock, OpenRouter and local models. Every request goes through the gateway's normal path: inference-key authentication, workspace grants, routing, admission, per-attempt reservation, settlement, logs and cancellation. No route bypasses it.

## Why it fails today

`/v1/messages` rejects every `anthropic-beta` header and every field outside a small text/tool subset. It turns a block-form `system` prompt into a string and rejects `thinking`, `cache_control`, images and thinking blocks in the history. It also returns a generic error where Claude Code needs Anthropic's own (for example "prompt is too long", which triggers compaction). Claude Code sends all of these on almost every request.

## Design

One Claude-compatible `/v1/messages`, also reached as `/v1/messages?beta=true`. It parses the full Claude Code request into the shared request type. Each provider adapter then translates what its upstream supports. Nothing is passed through blind: every field Claude Code sends has an explicit, documented rule, written as a table in `docs/protocol-matrix.md` and enforced in code.

| Claude Code sends | Anthropic routes | Other routes |
|---|---|---|
| `anthropic-version`, `anthropic-beta` headers | Forwarded (beta values verbatim) | Accepted and not forwarded |
| Block-form `system`, `cache_control` anywhere, 1-hour `ttl` | Preserved exactly | Text kept, cache hints dropped (they only affect caching and cost, not output) |
| `thinking` (`adaptive` or `enabled` with budget), `output_config.effort` | Forwarded | Mapped to the provider's reasoning control where one exists (for example OpenAI `reasoning_effort`); otherwise dropped, since it doesn't change what the model must do |
| `thinking`/`redacted_thinking` blocks in assistant history | Forwarded with signatures | Removed from history (they are only valid for the model that produced them) |
| Images in messages or tool results | Forwarded | Translated where the provider takes images; otherwise a 400 naming the field |
| `output_config.format` (structured output), tool `strict` | Forwarded | Mapped to JSON-schema output/strict tools where supported; otherwise a 400 |
| `context_management`, `defer_loading`, task budget | Forwarded | Dropped; Claude Code then manages context itself |
| Server tools (web search, code execution) | **400 for now** (their extra charges aren't priced yet) | 400 |
| `temperature`, `top_p`, `top_k`, `stop_sequences`, `metadata.user_id` | Forwarded (`user_id` stays a client label) | Mapped where supported |
| Anything not in this table | 400 naming the field (new Claude Code fields get added to the table deliberately) | 400 |

Inference keys are consumed by the gateway and never forwarded. Each provider credential stays the gateway's own.

### Responses

- Streams are incremental, as #23 already does. On Anthropic routes, thinking, signature and redacted-thinking blocks stream back unchanged. Other providers' reasoning text, where returned, streams as `thinking` blocks without a signature.
- **Errors** use Anthropic's shapes and status codes. A provider's context-length error becomes `400 invalid_request_error` "prompt is too long", so Claude Code compacts. Overload becomes 529 `overloaded_error` and rate limits become 429. Messages are capped at 1 KiB and contain no upstream secrets or bodies.
- The output bound rises from 4 MiB to **16 MiB** for Messages (long thinking sessions).

### Accounting (#71)

- Holds are unchanged: input is bounded by the price's input limit, output by `max_tokens` (thinking is counted inside it), within the price's output limit.
- Settlement uses only reported counts: input, output (including thinking/reasoning), and cache read/write with the 5-minute/1-hour split. These use the existing parsers and cache pricing, so cache writes are not double charged. Missing counts or rates stay unknown and keep the hold.

### Models

Claude Code sends whatever name it is configured with (`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, and so on), and the gateway resolves it like any other model name. `docs/claude-code.md` explains the setup. Claude Code's automatic discovery only lists model ids containing "claude", so other models are configured by name.

## Delivery (one PR each)

1. Frontend: accept the full request and the table above for every provider; error mapping; 16 MiB bound; server tools refused.
2. Anthropic route fidelity: preserved system/cache blocks, `thinking` with signatures, beta headers forwarded, thinking streamed back.
3. Reasoning and structured-output mapping for OpenAI-style providers.
4. Accounting checks for thinking and cache on PostgreSQL 17 (#71).
5. `/v1/messages/count_tokens` (see the open question).
6. #73: real Claude Code acceptance with paid calls, under your spending cap.

## Open question

For `count_tokens` you answered "see 2", and I'm not sure what you meant. My reading is that it should also go through the gateway: Anthropic routes ask Anthropic, and other models get a gateway estimate. Without the endpoint, Claude Code just estimates context usage itself. Please confirm or correct.

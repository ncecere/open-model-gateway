# Native protocol support (milestone 3)

All three inference routes share gateway authentication, model aliases, routing, key/model policy, process-local concurrency admission, and usage accounting. These endpoints expose a deliberately restricted stateless text/function-tool subset, not drop-in support for every provider feature.

| Adapter | Chat Completions frontend | Responses frontend | Messages frontend | Actual upstream |
| --- | --- | --- | --- | --- |
| `openai` | Yes | Yes | No | Native `/v1/chat/completions` or native `/v1/responses`, respectively |
| `anthropic` | Yes, subset below | No | Yes | Native `/v1/messages` |
| `bedrock` | Yes, text/tools | No | Yes, text/tools | AWS Converse / ConverseStream with SigV4 |

See [Bedrock configuration and limitations](bedrock.md). No other adapters are registered by the service. Unsupported protocol/adapter combinations fail rather than changing upstream protocol silently.

## Responses (`POST /v1/responses`)

Accepts `model`, `input` (text or a nonempty list of message, function-call, and function-call-output items), `instructions`, `max_output_tokens`, `temperature`, `stream`, `store:false`, function `tools`, `tool_choice`, and `text:{"format":{"type":"text"}}`.

Message items accept user, assistant, system, or developer roles and text strings or input-text blocks (assistant output-text blocks are also accepted). Function calls use `call_id`, `name`, and string `arguments`; function outputs use `call_id` and string `output`. Tools use native flat Responses function definitions, not Chat Completions' nested `function` envelope. Tool choices are auto, none, required, or a named function. Optional strict function schemas are forwarded to OpenAI.

Every OpenAI Responses upstream request explicitly sets `store:false`. `store:true`, `previous_response_id`, `background`, unknown fields, reasoning options/output, images/audio, hosted tools, structured output formats, annotations, and stateful retrieval/deletion are unsupported. The API does not implement server-side conversation state. A max-token or content-filter terminal result is `incomplete`, never `completed`.

## Messages (`POST /v1/messages`)

Requires exactly `anthropic-version: 2023-06-01`; beta headers are rejected. Accepts `model`, `messages`, text/text-block `system`, required positive `max_tokens`, `temperature` (0–1), `stream`, `tools`, and `tool_choice` (auto, none, any, named tool). User/assistant blocks may contain text, assistant `tool_use` with object input, and user `tool_result` with text/text-block content. Unknown fields, `thinking`, images, audio, cache controls, metadata, error-marked tool results, and other extensions are rejected. Unknown usage stays unknown; token counts are never fabricated as zero.

The Anthropic adapter only connects to `https://api.anthropic.com/v1`, uses `x-api-key` and the pinned version header, and rejects deployment endpoint overrides and nonempty regions. For Chat Completions requests it accepts leading system/developer text, text conversations and function tools. Temperature above 1 and strict function schemas are unsupported; when Chat omits an output limit it uses an explicit 1024-token default. Non-object tool arguments cannot be represented in Anthropic Messages and fail. Anthropic Responses cross-protocol routing is intentionally disabled.

## Streaming, limits, and limitations

Native upstream adapters parse SSE incrementally across arbitrary network and UTF-8 boundaries. The Responses streaming subset permits one output-text content part per message item; multiple message items and function-call items are supported, with terminal output checked against accumulated deltas. JSON responses are limited to 4 MiB and SSE frames (including ignored fields/comments) to 1 MiB. HTTP redirects, environment proxies, and automatic retries are disabled. Futures/streams own the HTTP response; dropping them cancels work without detached producer tasks. Upstream response bodies, credentials, private model identifiers, and errors are not copied into public errors.

Frontend native streaming starts immediately but buffers at most 4 MiB of normalized output before emitting content. This deliberately serializes parallel tool calls into valid native block/item lifecycles. It is not token-by-token frontend streaming. Text is consolidated before tool blocks because the shared response type does not preserve mixed text/tool block positions. Empty content and unknown usage are not invented. Responses events have increasing `sequence_number`, output item/content/argument lifecycle events, and a final response snapshot. Messages events have message/block start/delta/stop lifecycle events. Only engine `Done` authorizes a terminal success event; EOF, adapter errors, invalid arguments, and buffer overflow produce sanitized errors without successful terminal events. Responses incomplete results use `response.incomplete`.

The native upstream adapters require explicit `message_stop` / `response.completed` or `response.incomplete`, not EOF. Accounting and cancellation behavior remain governed by the common engine. No live-provider requests are required by the local mock test suites.

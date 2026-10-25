# Claude Code through the gateway

The Messages frontend accepts Claude Code's request grammar on `POST /v1/messages`, including `?beta=true`. It uses ordinary inference-key authentication, workspace model grants, routing, admission, per-attempt reservations and settlement. It is not an unmetered proxy.

**This is delivery 1 of #70, not full Claude Code acceptance.** Native Anthropic cache/thinking/beta fidelity and OpenAI-style reasoning/structured-output mapping are not delivered yet. Native Anthropic routes refuse those fields with a named 400 instead of silently dropping them. Images, structured output, strict tools and server tools are also refused. See the [per-field matrix](protocol-matrix.md#claude-code-field-rules-delivery-1).

## Configure models by name

1. Have an administrator configure the provider connection and deployment, publish pricing, enable `messages` in the model's supported protocols, and grant the model to your workspace/key.
2. Create a workspace inference key. This is **not** an upstream provider key or dashboard credential. Keep it in your local secret manager; never commit it.
3. Set the gateway origin (without `/v1`) and the gateway's public model names:

```sh
export ANTHROPIC_BASE_URL="https://gateway.example.com"
# Set ANTHROPIC_AUTH_TOKEN from your secret manager to the workspace inference key.
export ANTHROPIC_MODEL="company/main-model"
export ANTHROPIC_DEFAULT_SONNET_MODEL="company/main-model"
export ANTHROPIC_DEFAULT_OPUS_MODEL="company/main-model"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="company/fast-model"
claude
```

Names are resolved exactly as for other gateway requests. They do not need to contain `claude` or identify an Anthropic model. Claude Code's automatic model discovery only lists ids containing `claude`, so configure other models by name rather than relying on discovery. Changing a name does not grant model access or change which provider a deployment uses.

Send one inference credential, not both Bearer and `x-api-key`. The gateway consumes it; upstream requests use the provider connection's own credential. `anthropic-version: 2023-06-01` and beta headers are accepted on translated routes and never forwarded there. Beta headers currently produce a named 400 on native Anthropic routes until delivery 2 can forward their exact values.

## Current behavior and limits

- Text, ordinary function tools, sampling controls and `metadata.user_id` follow the matrix. On non-Anthropic routes, cache hints, context-management hints, deferred-loading hints and task budgets are dropped deliberately; Claude Code remains responsible for its context. Foreign thinking history is removed. The current adapters expose no Messages reasoning control, so thinking/effort hints are dropped; reasoning mapping follows in delivery 3.
- No current generation adapter declares image input. An image in a message or tool result returns 400 naming `image`; no client image URL is fetched.
- Server tools such as web search and code execution return 400 naming `tools.type`, even on native Anthropic routes: their additional charges are not priced yet.
- Context-length errors become `400 invalid_request_error` with `prompt is too long`, enabling client compaction. Overload is `529 overloaded_error`; rate limits remain 429. Error messages are bounded to 1 KiB and never return upstream diagnostic bodies or secrets.
- Messages output is bounded to 16 MiB; individual upstream SSE frames remain bounded to 1 MiB. The inbound JSON body remains bounded to 2 MiB.
- `max_tokens` remains required and includes thinking; it is the unchanged output reservation bound. Missing usage/rates remain unknown and keep holds. Client hints never reset or bypass limits.
- `/v1/messages/count_tokens` is not implemented in this delivery (#72). Real Claude Code acceptance, native thinking streams and paid-provider verification are separate deliveries (#73); this page does not claim they have passed.

# Claude Code through the gateway

The Messages frontend accepts Claude Code's request grammar on `POST /v1/messages`, including `?beta=true`. It uses ordinary inference-key authentication, workspace model grants, routing, admission, per-attempt reservations and settlement. It is not an unmetered proxy.

**Claude Code now works on native Anthropic routes within the documented subset (#70, deliveries 1–2).** System/cache blocks, beta values, thinking controls and signed history, images, context-management hints, deferred tools and task budgets are preserved. Thinking/signature/redacted-thinking blocks stream back without reconstruction. Structured output, strict tools and server tools remain refused; translated reasoning mapping follows in delivery 3. See the [per-field matrix](protocol-matrix.md#claude-code-field-rules). This is protocol support, not a claim of paid real-client acceptance (#73).

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

Send one inference credential, not both Bearer and `x-api-key`. The gateway consumes it; upstream requests use the provider connection's own credential. `anthropic-version: 2023-06-01` is required. Native Anthropic routes forward that version and the client's beta values verbatim except for the denylist below; the gateway never adds beta features. Translated Messages routes apply the same denylist and bounds without forwarding the headers. No caller field may choose an upstream host, path or arbitrary header, and `metadata.user_id` stays a gateway client label, never upstream metadata.

## Beta headers and cache pricing

Beta names are an open list, as Anthropic's [gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol) requires. New Claude Code releases need not wait for a gateway allowlist update. `claude-code-20250219`, `extended-cache-ttl-2025-04-11`, tool-search, effort, OAuth and structured-output betas are forwarded verbatim. Body-field validation and cache-price admission still apply independently.

The gateway denies only tokens beginning with these reviewed prefixes (case-insensitive):

- `context-1m-`: unmodeled long-context premium pricing.
- `code-execution-`, `web-search-`, `web-fetch-`, `computer-use-`: unsupported hosted capabilities and additional charges.
- `files-api-`, `mcp-client-`: unsupported upstream files and remote-server capabilities.

Every comma-separated token across repeated headers is checked. Bounds remain 16 header values (1024 header-safe ASCII bytes each, space/tab allowed) and 32 nonempty tokens total. Accepted values are not rewritten. Denied tokens return 400 `invalid_request_error` beginning `Unexpected value(s)` and listing only the rejected names, matching Anthropic's wording; Claude Code can automatically retry without those betas. For example:

```text
Unexpected value(s) `context-1m-2025-08-07` for the `anthropic-beta` header. Please consult our documentation at docs.anthropic.com or try again without the header.
```

Operators may set `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` on clients to disable experimental capabilities instead. To request a denylist change, open a GitHub issue with the capability/pricing evidence and a mock wire/refusal contract. See the [matrix](protocol-matrix.md#claude-code-field-rules) for the complete policy.

For native cache hints, ask the administrator to publish a compatible immutable v2/v3 deployment price. Cache reads must always be explicitly priced. The 5-minute cache-write category must be priced for absent/5-minute TTL, and the 1-hour category for `ttl:1h`. Mixed requests need both write categories; the conservative monetary hold must also be finite. Missing prices, v1 single-rate prices and unknown/not-applicable required categories refuse with 400 naming `cache_control` before any reservation or upstream request. The `extended-cache-ttl-*` beta is allowed; its paired `cache_control.ttl:1h` is priced or refused by this cache gate, not the beta filter. Anthropic's usual write prices are 1.25× base input for 5 minutes and 2× for 1 hour; publish the applicable model's rates. Holds use the existing conservative full-input bound for each priced cache category plus `max_tokens`; settlement charges observed disjoint counts without double charging aggregate writes. Cache-enabled batch lines use gateway-run per-line admission rather than native Message Batches.

## Current behavior and limits

- Text, ordinary function tools, sampling controls and `metadata.user_id` follow the matrix. On non-Anthropic routes, cache hints, context-management hints, deferred-loading hints and task budgets are dropped deliberately; Claude Code remains responsible for its context. Foreign thinking history is removed. Those translated adapters expose no Messages reasoning control yet, so thinking/effort hints are dropped; reasoning mapping follows in delivery 3.
- Native Anthropic accepts base64 PNG/JPEG/GIF/WebP images in messages and tool results: strict padded base64, matching format magic, at most 5 MiB decoded per image and 20 images per request. The 2 MiB HTTP body cap still applies. URL sources are refused before dispatch; neither the gateway nor the provider fetches client URLs. Translated routes still return 400 naming `image`.
- Native Anthropic preserves `tool_result.is_error` exactly, including explicit false; translated routes still return 501 for true rather than misrepresenting an error as success.
- Server tools such as web search and code execution return 400 naming `tools.type`, even on native Anthropic routes: their additional charges are not priced yet.
- Context-length errors become `400 invalid_request_error` with `prompt is too long`, enabling client compaction. Overload is `529 overloaded_error`; rate limits remain 429. Ordinary error messages are bounded to 1 KiB and never return upstream diagnostic bodies or secrets. Beta rejection messages echo only bounded client token names (at most 17 KiB of text) so Claude Code can remove their exact names; those names are not logged.
- Messages output is bounded to 16 MiB; individual upstream SSE frames remain bounded to 1 MiB. The inbound JSON body remains bounded to 2 MiB.
- `max_tokens` remains required and includes thinking; it is the output-token reservation bound. Cache writes use the separate conservative monetary hold described above. Missing usage/rates remain unknown and keep holds. Client hints never reset or bypass limits.
- `/v1/messages/count_tokens` is not implemented in this delivery (#72). Real Claude Code acceptance and paid-provider verification remain separate (#73); this page does not claim they have passed.

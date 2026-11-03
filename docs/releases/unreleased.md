# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.10.0](v0.10.0.md). They become the next release's notes.

## Documentation

- Workspace-wide object visibility also shows ordinary members the key names on other members' batch rows; the keys list stays filtered. This is the intended behaviour, now documented in the management API, batches guide and contract (#403).

## Fixes

- **Chat Completions and Responses** answer an oversized streamed request body (no `Content-Length`) with the shared OpenAI 413 envelope, code `payload_too_large`, like the embeddings, rerank, image, System One, audio, video and batch routes; they used code `invalid_request_error` with the generic invalid-request message (#438).
- **Authentication storage failures are 503, not 500** (#439). When the database fails while a management request is authenticated (browser session or admin token), while signing in or while signing out, the gateway answers the retryable `503` with `{"error":"Authentication service unavailable"}`, like every other management storage failure. A failed OS random-number generator during sign-in stays `500`. A storage failure during the sign-in callback, after the one-use login attempt was consumed, needs a fresh sign-in (reloading the callback answers 400). The management contract documents the 503 on every authenticated route.
- **Current Anthropic SDKs work with `/v1/messages`** (#430). The gateway accepted only `anthropic-version: 2023-06-01`, so every client on a current SDK major (TypeScript `@anthropic-ai/sdk` 1.x, Python `anthropic` 2.x and the other SDKs that send `2026-10-21` by default) was refused with 400. Both versions are now accepted. Under `2026-10-21` the gateway applies that version's rules: `temperature`, `top_p` and `top_k` are refused with 400 naming the field and the version, even at a default value or `null`, instead of being dropped. Documents and citations remain outside the supported grammar under both versions. Native Anthropic routes keep sending `2023-06-01` upstream, since an accepted request means the same under either version; `/v1/batches` lines for `/v1/messages` carry no version header and keep the `2023-06-01` rules. The SDK contract tests now run against `@anthropic-ai/sdk` 1.4.0 (supersedes Dependabot #422).

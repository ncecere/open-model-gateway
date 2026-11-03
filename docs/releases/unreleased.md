# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.10.0](v0.10.0.md). They become the next release's notes.

## Fixes

- **Chat Completions and Responses** answer an oversized streamed request body (no `Content-Length`) with the shared OpenAI 413 envelope, code `payload_too_large`, like every other OpenAI-shaped route; they used code `invalid_request_error` with the generic invalid-request message (#438).
- **Current Anthropic SDKs work with `/v1/messages`** (#430). The gateway accepted only `anthropic-version: 2023-06-01`, so every client on a current SDK major (TypeScript `@anthropic-ai/sdk` 1.x, Python `anthropic` 2.x and the other SDKs that send `2026-10-21` by default) was refused with 400. Both versions are now accepted. Under `2026-10-21` the gateway applies that version's rules: `temperature`, `top_p` and `top_k` are refused with 400 naming the field and the version, even at a default value or `null`, instead of being dropped. Documents and citations remain outside the supported grammar under both versions. Native Anthropic routes keep sending `2023-06-01` upstream, since an accepted request means the same under either version; `/v1/batches` lines for `/v1/messages` carry no version header and keep the `2023-06-01` rules. The SDK contract tests now run against `@anthropic-ai/sdk` 1.4.0 (supersedes Dependabot #422).

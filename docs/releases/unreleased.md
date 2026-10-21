# Open Model Gateway: unreleased

Notes for changes on `main` since v0.7.0. They become the next release's notes.

## Fixes

- **Optional generation maxima (#215).** Priced Chat Completions and Responses requests may omit the maximum, including streaming requests. Each attempt uses its admission-pinned price ceiling for both reservation and upstream dispatch; explicit maxima remain unchanged, never clamped.
- Batch lines follow the same optionality. Initial admission rejects stale planning prices before accounting writes; admitted work retains its price/cap while queued. The original uploaded file is unchanged.

Native Anthropic Messages still requires `max_tokens`. Unpriced/unknown accounting behavior is preserved. No schema or environment changes. Other client-conformance work remains tracked in #222; this is not a universal compatibility claim.

## Embedding encoding (#216)

`/v1/embeddings` honors `float` and standard padded base64 of little-endian float32 vectors, including the ordinary OpenAI SDK encoding default. Raw HTTP omission/null remains float. Validation and client response-size checks precede successful settlement; failed responses retain valid token/cost evidence and unresolved holds.

**Remaining limitation:** batch embedding lines remain float-only and explicitly reject base64. Other unsupported fields remain unsupported.

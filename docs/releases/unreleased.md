# Open Model Gateway: unreleased

Notes for changes on `main` since v0.7.0. They become the next release's notes.

## Fixes

- **Optional generation maxima (#215).** Priced Chat Completions and Responses requests may omit the maximum, including streaming requests. Each attempt uses its admission-pinned price ceiling for both reservation and upstream dispatch; explicit maxima remain unchanged, never clamped.
- Batch lines follow the same optionality. Initial admission rejects stale planning prices before accounting writes; admitted work retains its price/cap while queued. The original uploaded file is unchanged.

Native Anthropic Messages still requires `max_tokens`. Unpriced/unknown accounting behavior is preserved. No schema or environment changes. Other client-conformance work remains tracked in #222; this is not a universal compatibility claim.

## Embedding encoding (#216)

`/v1/embeddings` honors `float` and standard padded base64 of little-endian float32 vectors, including the ordinary OpenAI SDK encoding default. Raw HTTP omission/null remains float. Validation and client response-size checks precede successful settlement; failed responses retain valid token/cost evidence and unresolved holds.

**Remaining limitation:** batch embedding lines remain float-only and explicitly reject base64. Other unsupported fields remain unsupported.

## Stateless Responses followups (#217)

Clients can reuse supported text/function-call `output` items from JSON or terminal streaming responses as followup `input`. Optional read-only item IDs/statuses and omitted/empty output-text annotations are validated without becoming state or authority; tool correlation still uses `call_id`. Function-call-output metadata retains its nullable semantics. Nonempty annotations/citations, malformed metadata, stateful references and other unsupported content remain explicitly rejected. No schema or caller-side maximum/store workaround is required.

## Permanent capability refusals (#218)

`unsupported_capability` errors now carry `x-should-retry: false`, so ordinary OpenAI and Anthropic SDK clients do not repeat permanent refusals, including HTTP 501. Existing statuses and error bodies are unchanged. Transient errors remain eligible for client retries; each dispatched client retry requires a separate durable admission, and unknown usage keeps its hold. Gateway upstream retries/failover policy is unchanged. This does not expand supported capabilities or resolve the remaining compatibility gaps in #222.

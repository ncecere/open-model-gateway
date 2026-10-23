# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.1](v0.7.1.md). They become the next release's notes.

## Nullable OpenAI defaults (#221)

Chat Completions and Responses accept omitted/null `stream` as non-streaming. Responses accepts omitted/null/false `store` as stateless operation under the gateway privacy subset; explicit `store:true` still fails before admission and every native upstream Responses request still sends `store:false`. Existing nullable supported scalar/options keep omission-equivalence and pricing defaults. This is not blanket null acceptance: OpenAI SDK 7.28 `tools` collections are optional, not nullable; malformed nested values, nonnullable fields and unknown/unsupported options remain rejected. Public Chat assistant `refusal:null` means no refusal and is accepted without forwarding; non-null refusal content, other-role refusal fields, empty messages and unknown fields such as `parsed:null` remain rejected. This does not add typed refusal support. Native Anthropic 0.131 `stream`/`tools` and required `max_tokens` are unchanged. No schema or environment changes.

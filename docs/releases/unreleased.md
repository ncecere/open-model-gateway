# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.1](v0.7.1.md). They become the next release's notes.

<<<<<<< HEAD
## Fixes

- **Chat Completions API guide (#225).** The model-page guide now correctly lists `max_tokens` as an accepted alias of `max_completion_tokens`; when both are supplied, values must be equal (otherwise 400). Optional maxima and existing unsupported-capability disclosures are unchanged; no runtime behavior changes.
=======
## No-argument tools and empty successful results (#220)

Chat function definitions may omit `parameters`, meaning no arguments. Only omission becomes the closed empty-object schema (`type:object`, `properties:{}`, `additionalProperties:false`); explicit object schemas are unchanged and null/nonobject parameters remain invalid. Omitted `strict` stays absent, with existing per-adapter strict capability rules unchanged.

Native Messages accepts omitted `tool_result.content` as empty text and explicit `is_error:false` as the default success. Call IDs and content bounds remain intact. Error-marked results (`is_error:true`) are explicitly unsupported, not silently downgraded; null content, malformed flags, missing IDs and unknown fields remain invalid. Messages still requires `max_tokens`. No database/runtime-grant changes, hosted/beta/thinking expansion or general nullable-field compatibility is included.
>>>>>>> origin/main

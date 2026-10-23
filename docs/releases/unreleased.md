# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.1](v0.7.1.md). They become the next release's notes.

## Fixes

- **Chat Completions API guide (#225).** The model-page guide now correctly lists `max_tokens` as an accepted alias of `max_completion_tokens`; when both are supplied, values must be equal (otherwise 400). Optional maxima and existing unsupported-capability disclosures are unchanged; no runtime behavior changes.

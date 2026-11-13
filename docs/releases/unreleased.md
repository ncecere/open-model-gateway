# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.16.0](v0.16.0.md). They become the next release's notes.

## Internal restructuring

For contributors; no gateway behaviour changes.

- **`Engine::execute_protocol` is split** (#539). The 386-line inference entry point keeps the request setup, routing plan and attempt loop; route eligibility, the all-refused error, per-attempt admission, the upstream call, the settlement of each outcome and the streamed-response tracker move into named helpers in `inference/attempt.rs`. The statements and their order are unchanged. `inference/mod.rs` drops from 1,208 to 875 production lines and the `#499` marker count by one.
- **`Metrics::new` is split** (#537). The 510-line constructor now builds the families in `unregistered` and registers them in six helpers, in the same order: the same families, help text, buckets and pre-created series. The `#499` marker count drops from 57 to 56.

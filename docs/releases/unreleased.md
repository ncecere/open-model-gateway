# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.16.0](v0.16.0.md). They become the next release's notes.

## Internal restructuring

For contributors; no gateway behaviour changes.

- **`Metrics::new` is split** (#537). The 510-line constructor now builds the families in `unregistered` and registers them in six helpers, in the same order: the same families, help text, buckets and pre-created series. The `#499` marker count drops from 57 to 56.
- **`Engine::execute_protocol` is split** (#539). The inference entry point (386 lines by Clippy's count) keeps the request setup, routing plan and attempt loop; route eligibility, the all-refused error, per-attempt admission, the upstream call, the settlement of each outcome and the streamed-response tracker move into named helpers in `inference/attempt.rs`. The statements and their order are unchanged. `inference/mod.rs` drops from 1,208 to 875 production lines and the `#499` marker count by one.
- **Provider-cost `snapshot` is split** (#541). The 327-line comparison in `provider_costs/matching.rs` now runs as named steps (load the frozen observations, the personal workspace set, the imported actuals, the claims, the observation summary, the per-row binding match, the estimate-only rows and the category coverage), in the same order: the same SQL, report JSON and errors. The `#499` marker count drops from 56 to 55.

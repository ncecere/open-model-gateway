# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.16.0](v0.16.0.md). They become the next release's notes.

## Internal restructuring

For contributors; no gateway behaviour changes.

- **`Metrics::new` is split** (#537). The 510-line constructor now builds the families in `unregistered` and registers them in six helpers, in the same order: the same families, help text, buckets and pre-created series. The `#499` marker count drops from 57 to 56.

# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Dependencies and CI

- **`rustix` 1.1.6** (#491), for RUSTSEC-2026-0486 (the extended-attribute functions could mark unwritten bytes as initialized). `rustix` reaches the build only through `tempfile`, a test dependency, so it is not in the gateway binary; the update clears the advisory.
- **CI runs on an exact runner image** (#491). `ci.yml` used `ubuntu-latest`, which moved to Ubuntu 26.04 on 2026-11-11 and broke package installation in three jobs. Every workflow now names `ubuntu-24.04` (or `ubuntu-24.04-arm`), and the supply-chain test refuses a floating or computed runner. See [verification](../verification.md#supply-chain).

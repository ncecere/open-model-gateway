# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Dependencies and CI

- **`rustix` 1.1.6** (#491), for RUSTSEC-2026-0486 (the extended-attribute functions could mark unwritten bytes as initialized). `rustix` reaches the build only through `tempfile`, a test dependency, so it is not in the gateway binary; the update clears the advisory.
- **CI runs on an exact runner image** (#491). `ci.yml` used the floating label `ubuntu-latest`, which changes the operating system under a job without a commit; it now names `ubuntu-24.04` like every other workflow. The supply-chain test refuses a floating, self-hosted or computed runner and a reusable workflow from outside the checkout. See [verification](../verification.md#supply-chain).
- **CI no longer installs operating-system packages at run time** (#491). `age` for the backup tests is the pinned v1.3.2 release, downloaded through `fetch-verified.sh` with its SHA-256; Playwright installs its browser without `--with-deps`. `apt-get` against the Ubuntu mirrors was unpinned, and on 2026-11-11 the runners could not resolve the mirrors, which failed three jobs on every pull request. The supply-chain test refuses a package manager in any workflow `run:` step; it does not read scripts those steps call.

# Changelog

Notable changes to Open Model Gateway, one entry per release. Each release's notes (highlights, migrations, new environment variables, behaviour changes, upgrade steps and known limitations) are in [`docs/releases/`](docs/releases/).

The project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, minor releases may include breaking changes; the release notes say how to adapt.

## [Unreleased]

## [0.3.1] - 2026-10-09

Distroless runtime image (`gcr.io/distroless/cc-debian12`, pinned by digest; no shell, curl or package manager), the binary as entrypoint with the former shell entrypoint's `_FILE` secret import ported to Rust, and `open-model-gateway healthcheck` for the image `HEALTHCHECK`. Supply-chain scanning: cargo-deny (`deny.toml`), an npm production audit gate, CodeQL for JavaScript/TypeScript and Rust, and weekly grouped Dependabot updates; the Bedrock SDK drops its legacy hyper 0.14/rustls 0.21 connector. Rerank on the `openai_compatible`, `vllm` and `sglang` local profiles and System One on `openai_compatible` and `ollama`, with Add model offering those types only where the profile serves them. Fixes from the v0.3.0 screenshot pass: cooling-down routes answer a retryable 503 with `Retry-After` instead of 404, rounded money display with exact tooltips, aligned stat-tile charts, "Unpriced" unit-priced models, audit labels for every event, "No access" users, per-owner personal workspace rows, Logs that fit at 1440, consistent storage units, and clearer batch waits and lower-bound batch costs. No migrations. Notes: [`docs/releases/v0.3.1.md`](docs/releases/v0.3.1.md).

## [0.3.0] - 2026-10-09

Encrypted file store, gateway-owned Files API, batches for any model with capacity-aware scheduling for self-hosted servers, a "Jobs at once" limit, the video provider retired, and the first signed multi-arch release images. Migrations `0018` to `0022`. Notes: [`docs/releases/v0.3.0.md`](docs/releases/v0.3.0.md).

## [0.2.0] - 2026-10-09

Alerts, key safety, model compare, SCIM, JWKS refresh, metrics and backup tooling, async video and batch jobs, realtime audio, and constant-time budget admission. Migrations `0011` and `0013` to `0017`. Notes: [`docs/releases/v0.2.0.md`](docs/releases/v0.2.0.md).

## [0.1.0] - 2026-10-08

The single-enterprise rebuild: Team, Project and personal workspaces, OIDC platform roles, catalogs, multimodal workloads across five provider families, immutable micro-USD accounting, logs and Admin settings. Migrations `0001` to `0010`. Notes: [`docs/releases/v0.1.0.md`](docs/releases/v0.1.0.md).

[Unreleased]: https://github.com/ncecere/open-model-gateway/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ncecere/open-model-gateway/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ncecere/open-model-gateway/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ncecere/open-model-gateway/releases/tag/v0.1.0

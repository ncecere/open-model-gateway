# Changelog

Notable changes to Open Model Gateway, one entry per release. Each release's notes (highlights, migrations, new environment variables, behaviour changes, upgrade steps and known limitations) are in [`docs/releases/`](docs/releases/).

The project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, minor releases may include breaking changes; the release notes say how to adapt.

## [Unreleased]

Scale work, phase P0 (measurement): `gateway_admission_seconds` and `gateway_settlement_seconds` histograms by phase and outcome (including lock waits); `tools/mock-upstream`, `tools/loadgen` (open-loop runs across replicas, and a seeder that only targets `omg_loadtest*` databases); a laptop load-test stack (`deploy/loadtest/compose.yaml`: PostgreSQL 17, PgBouncer, three replicas) driven by `scripts/loadtest.py`; a capacity baseline, PgBouncer guidance and a `pg_stat_statements` guide in [operations](docs/operations.md). Phase P1: reports, usage, logs, `/me` and `/v1/models` read from REPEATABLE READ READ ONLY snapshots without the installation or catalog lock (authorization inside the snapshot), with an optional reporting replica (`GATEWAY_REPORTING_DATABASE_URL`, `GATEWAY_REPORTING_MAX_LAG_SECONDS`). Phase P2: maintained per-minute and in-flight counters for workspace and key scopes (migration 0024), the `held_unknown` split in budget totals (0025), price and bounds resolved before the installation lock (about 14 → 6 round trips under it), budget alerts and the metrics gauge on totals, change-only routing health writes and batched `SKIP LOCKED` lease reconciliation; laptop ceiling about 80 → 205 ok/s. Fix: a request cancelled just after `BEGIN` could return its pooled connection still inside a transaction (sqlx); transactions now begin in a detached task (`db.rs`, direct `Pool::begin` is a clippy error), and 503 storage errors log the request id and SQLSTATE.

## [0.3.2] - 2026-10-09

Fixes three gaps found in live DGX Spark acceptance. Routes whose connection profile can't serve the model (System One on `vllm`, rerank on `ollama`) are refused with 400 `route_unsupported_capability` at route creation, enabling, model setup and model protocol changes, using the one capability table the inference path uses; existing ones read "Not serving: <reason>". Pricing-v3 publications must state every meter the route's workload can use (priced, `not_applicable` or the new explicit `unknown`), else 400 `price_meters_incomplete`; budgeted admission on a price that cannot bound the hold now returns 503 `price_unbounded` (non-retryable) instead of a misleading `budget_exceeded`, and older prices stay unchanged. Both new management errors carry their machine code in `error.reason`, like every management error. Chat Completions accepts legacy `max_tokens` as an alias of `max_completion_tokens`. CI and the release pipeline no longer pull from Docker Hub (anonymous rate limits and an outage failed builds): the `Dockerfile`'s node and rust stages, PostgreSQL/Caddy test and staging-rehearsal images, BuildKit and the SBOM generator come from `mirror.gcr.io` (same digests), RustFS from `ghcr.io`, MinIO via the mirror with a retried pull, cargo-deny as a release binary, and the `Dockerfile` drops its `# syntax=` line; the runtime image and `deploy/staging/compose.yaml`'s defaults are unchanged (`STAGING_POSTGRES_IMAGE`/`STAGING_CADDY_IMAGE` override them). No migrations. Also five dashboard fixes (batch outcome labels, rounded batch cost hint, sub-cent trailing zeros, Explore title casing, truncated user emails) and reviewed Dependabot updates (minor/patch groups; base64 0.23, tower-http 0.7, webpki-roots 1.0). Notes: [`docs/releases/v0.3.2.md`](docs/releases/v0.3.2.md).

## [0.3.1] - 2026-10-09

Distroless runtime image (`gcr.io/distroless/cc-debian12`, pinned by digest; no shell, curl or package manager), the binary as entrypoint with the former shell entrypoint's `_FILE` secret import ported to Rust, and `open-model-gateway healthcheck` for the image `HEALTHCHECK`. Supply-chain scanning: cargo-deny (`deny.toml`), an npm production audit gate, CodeQL for JavaScript/TypeScript and Rust, and weekly grouped Dependabot updates; the Bedrock SDK drops its legacy hyper 0.14/rustls 0.21 connector. Rerank on the `openai_compatible`, `vllm` and `sglang` local profiles and System One on `openai_compatible` and `ollama`, with Add model offering those types only where the profile serves them. Fixes from the v0.3.0 screenshot pass: cooling-down routes answer a retryable 503 with `Retry-After` instead of 404, rounded money display with exact tooltips, aligned stat-tile charts, "Unpriced" unit-priced models, audit labels for every event, "No access" users, per-owner personal workspace rows, Logs that fit at 1440, consistent storage units, and clearer batch waits and lower-bound batch costs. No migrations. Notes: [`docs/releases/v0.3.1.md`](docs/releases/v0.3.1.md).

## [0.3.0] - 2026-10-09

Encrypted file store, gateway-owned Files API, batches for any model with capacity-aware scheduling for self-hosted servers, a "Jobs at once" limit, the video provider retired, and the first signed multi-arch release images. Migrations `0018` to `0022`. Notes: [`docs/releases/v0.3.0.md`](docs/releases/v0.3.0.md).

## [0.2.0] - 2026-10-09

Alerts, key safety, model compare, SCIM, JWKS refresh, metrics and backup tooling, async video and batch jobs, realtime audio, and constant-time budget admission. Migrations `0011` and `0013` to `0017`. Notes: [`docs/releases/v0.2.0.md`](docs/releases/v0.2.0.md).

## [0.1.0] - 2026-10-08

The single-enterprise rebuild: Team, Project and personal workspaces, OIDC platform roles, catalogs, multimodal workloads across five provider families, immutable micro-USD accounting, logs and Admin settings. Migrations `0001` to `0010`. Notes: [`docs/releases/v0.1.0.md`](docs/releases/v0.1.0.md).

[Unreleased]: https://github.com/ncecere/open-model-gateway/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/ncecere/open-model-gateway/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ncecere/open-model-gateway/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ncecere/open-model-gateway/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ncecere/open-model-gateway/releases/tag/v0.1.0

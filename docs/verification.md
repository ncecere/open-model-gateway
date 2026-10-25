# Verification

## Test commands

Run from the repository root. None of them make paid provider calls or touch the local demo (PostgreSQL 54349, port 3000).

| Command | Covers | Needs |
| --- | --- | --- |
| `cargo fmt --all -- --check` | Formatting | Rust |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Lints | Rust |
| `cargo test --workspace --all-features` | Unit, mock-provider contract, SDK and real-PostgreSQL integration tests (SQLx creates disposable databases) | `DATABASE_URL` to the disposable cluster, e.g. `postgres://gateway:gateway@127.0.0.1:54339/gateway`; `npm ci` for the SDK tests |
| `cargo test -p open-model-gateway --all-features --test runtime_privileges -- --ignored` | Runtime-role ACL rollback probe | Disposable cluster only |
| `cargo test -p open-model-gateway --all-features --lib providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence -- --ignored --exact` | Anthropic terminal fidelity, ordinary SDKs, runtime-role settlement/unknown holds and cancellation | Disposable cluster only; changes maintenance-database ACLs. Run sequentially after the runtime ACL probe, never via an unrestricted ignored-test sweep; requires `npm ci` |
| `npm run typecheck:web` / `npm run test:web` / `npm run build:web` | SPA types, Vitest unit/component tests, production build | Node 22 |
| `npm run test:browser` | Playwright end-to-end journey and axe accessibility scans (below) | Disposable PostgreSQL, Rust, Chromium (`npx playwright install chromium`) |
| `npm run test:demo` | Local demo issuer and runtime helper | Node, Python 3 |
| `npm run test:container` | Runtime image contract (`tests/container-image.test.mjs`): distroless (no shell, package manager, curl, perl, Node or Cargo), UID 10001, read-only root filesystem, `_FILE` secret import and refusal, `serve` never migrates, explicit `migrate`, exec-form `healthcheck`, graceful `SIGTERM` as PID 1 | Docker, Node; builds the image unless `OMG_CONTAINER_IMAGE` names one; uses a throwaway `mirror.gcr.io/library/postgres:17-alpine` on an internal network |
| `npm run test:staging` | Staging (including the runtime-image checks of `verify`), provider-acceptance and backup helpers | Python 3 |
| `cargo deny check` | Supply chain: RustSec advisories (vulnerable, unmaintained, unsound, yanked), licence allow-list, duplicate-version warnings, crates.io-only sources; policy in `deny.toml` | [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) 0.20 (`cargo install cargo-deny --locked` or the release binary); fetches the advisory database |
| `node .github/scripts/vendored-advisories.mjs` | Additional advisory-only upstream-base coverage for explicitly registered vendored packages; empty registrations pass with an explicit message | Node 22, Cargo, the same pinned cargo-deny 0.20.2; uses `deny.toml` |
| `node --test tests/vendored-advisories.test.mjs` | Offline synthetic-advisory acceptance and fail-closed registration/graph controls | Node 22, Cargo, Git; cargo-deny 0.20.2 on PATH or `CARGO_DENY=/absolute/path/to/cargo-deny` (absence skips scanner tests locally, fails CI) |
| `npm audit --omit=dev --audit-level=high` | Supply chain: high and critical advisories in production npm dependencies (the SPA bundle) | Node 22; reads `package-lock.json` |

CI (`.github/workflows/ci.yml`) runs all of these; the `browser` job uses a PostgreSQL service container, and `test:container` runs in the `staging-image` job (and on each pushed digest in `image.yml`). The binary's startup secret handling and `healthcheck` are also covered by `cargo test` (`apps/gateway/src/startup.rs`, `apps/gateway/src/healthcheck.rs`, `apps/gateway/tests/startup.rs`).

### Supply chain

CI's `cargo-deny` job runs `cargo deny --all-features check` (cargo-deny 0.20.2's release binary, installed and checksum-verified by the SHA-pinned `taiki-e/install-action`), and the `npm-audit` job gates on `npm audit --omit=dev --audit-level=high` and writes the full, non-blocking `npm audit` (dev tooling included) to the job summary. `.github/workflows/codeql.yml` runs CodeQL for `javascript-typescript` and `rust` (build mode `none`) on pushes to `main`, pull requests and weekly; results appear on the Security tab. `.github/dependabot.yml` opens weekly grouped updates for Cargo, npm (root workspace), GitHub Actions (SHA pins) and the `Dockerfile` base images. The release image is additionally scanned by Trivy in `image.yml`.

**Vendored upstream-base advisories (#214).** The normal `cargo deny --all-features check` stays unchanged on the real graph. Cargo marks path packages with `source: null`, which cargo-deny 0.20.2 omits from advisory matching. A separately labelled step in that same read-only job runs `node .github/scripts/vendored-advisories.mjs`: it reads `cargo metadata --locked --all-features --format-version 1`, validates registrations and graph references, copies the metadata into a private temporary directory, and invokes the same pinned tool with `--all-features --metadata-path <temporary-file> check advisories`. It removes the temporary input afterwards. With no registrations (today), it reports that fact and scans unchanged identities.

Register a reviewed crates.io upstream base in the **root** `Cargo.toml` only when adding a vendored dependency, for example (not a current registration):

```toml
[workspace.metadata.vendored.example]
name = "example"
version = "1.2.3"
manifest = "vendor/example/Cargo.toml"
```

Only those three fields are accepted; crates.io is the fixed registry identity. The name/version must exactly match the actual path package, and the manifest must be a canonical, existing `vendor/.../Cargo.toml` within the repository (no traversal or symlinks). Registered packages must be explicitly excluded from workspace membership. Cargo's automatic workspace membership does not establish first-party ownership: only canonical manifest directories explicitly listed in the root `[workspace] members` array are exempt, and never packages under `vendor/`. To avoid ambiguous TOML interpretation without a second general-purpose parser, the root manifest must begin with `[workspace]` followed by `members = [...]` (blank lines/comments allowed); entries must be literal single- or double-quoted paths without escapes, globs, traversal, symlinks or duplicates. Unsupported layouts fail closed. All other path packages require registration, including implicit workspace members and explicitly listed vendor packages. Unknown path packages, stale/duplicate registrations, name/version mismatches, unexpected metadata shapes, missing/inconsistent graph references and projected ID collisions fail closed. First-party workspace packages are never projected. Before accepting a successful scan with registrations, the helper requires exactly one strictly parsed `[INFO] gathered N crates in Nms` diagnostic and strictly parsed `[DEBUG] filtered <name> <version>` lines. Each filtered identity must uniquely match one projected metadata package and appear only once; the gathered count plus filtered package count must equal the total projected metadata package count. Missing/altered filter diagnostics that leave a package unaccounted for, malformed/duplicate graph diagnostics, unknown/ambiguous filtered identities, count mismatches, or any filtered registered package refuse coverage even if cargo-deny itself exits green. This guards the pinned diagnostic/count contract; it does not independently attest the scanner's advisory-matching internals. It does not invent roots/edges or override the real policy to force inclusion. Only the registered package source/ID and corresponding graph references change in the copy; actual dependency declarations, `Cargo.lock`, the build graph, normal license/ban/source checks, SBOMs and provenance are untouched. No blanket mapping or advisory ignores are added. This means **upstream-base advisory coverage**, not proof that patched code is unmodified or independently certified.

The job also runs `node --test tests/vendored-advisories.test.mjs` with the installed pinned scanner. A temporary fixture named/versioned `smallvec 1.10.0` and an isolated local Git advisory database with a **synthetic**, non-production `RUSTSEC-2026-9999` demonstrate the unprojected miss and projected detection, plus unchanged-registry/no-vendor and refusal controls, including registered packages filtered by `cfg(any())` or `[graph].exclude`. Its `db-path` is temporary and `db-urls` selects only that cached database; cargo-deny's `--offline` disables fetching (0.20.2 has no `--disable-fetch` flag), and `disable-yank-checking = true` prevents index access. The fixture uses no network or credentials and does not build Rust. `--offline` is also accepted by the helper for local runs with a previously cached real advisory database.

**Third-party code never shares a job with a write token (#194).** A job whose `GITHUB_TOKEN` has any `write` permission may use only these actions, all pinned by SHA: `actions/checkout`, `actions/download-artifact`, `docker/setup-buildx-action`, `docker/login-action`, `docker/build-push-action` and `github/codeql-action/upload-sarif`. "Has a write permission" means any scope set to anything but `read` or `none`, set in the job or at the workflow level, inherited from the caller of a reusable workflow (`ci.yml`'s `image` job grants `contents`, `packages` and `id-token` write to `image.yml`), or implied by declaring nothing, because the repository's default token may write. Scanners, linters and setup steps run in jobs with a read-only token.

- **Image pipeline.** In `image.yml`, `build` pushes the per-architecture images by digest and nothing else. `verify` (`contents: read`, `packages: read`) pulls the digest, drops its registry login and runs the version check, the staging rehearsal, the runtime contract test and the Trivy scan. `publish` (which holds `id-token: write` for cosign) starts only after every `verify` leg passes. Tags and annotations come from a read-only `meta` job. The digests move between jobs as job outputs.
- **Buildx.** `docker/setup-buildx-action` downloads the latest Buildx without a check when the runner has none, when `version` is set, or for the cloud driver. Every job that can write first runs `docker buildx version` (it fails if the runner has no Buildx), and no workflow sets `version` or the cloud driver. The BuildKit image the action starts is `mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:…`, pinned by digest; the test requires a digest wherever the action is used.
- **CodeQL.** `init` and `analyze` download the CodeQL bundle, so `codeql.yml` runs them in a read-only `analyze` job (`upload: never`, SARIF written to a file and passed on as an artifact). A separate `upload` job holds `security-events: write` and runs only `github/codeql-action/upload-sarif` after checkout and `download-artifact`.

`tests/supply-chain.test.mjs` (`npm run test:supply-chain`) reads every workflow and local action with the `yaml` package in strict mode (a parse error, a duplicate key or an alias bomb fails it; aliases and merge keys are resolved), opens local composite actions and local reusable workflows, and fails when a job that can write uses any other action (including `docker://` images and external reusable workflows), when an allowlist entry is unused, when `setup-buildx-action` could download Buildx, when a workflow uses an installer action that downloads without a checksum (`actions/setup-node`, `azure/setup-helm`, `aquasecurity/trivy-action`, `aquasecurity/setup-trivy`, `sigstore/cosign-installer`), or when an action is not pinned by SHA (with a version comment) or `docker://` image by digest. It runs the checks against adversarial in-memory workflows (flow-style YAML, aliases, merge keys, absent and `write-all` permissions, a hidden action in a composite or in a called workflow) to show they fail.

**Run-time downloads go through one script.** `.github/scripts/fetch-verified.sh <https-url> <sha256> <dest>` accepts only a plain `https://` URL (redirects too: `--proto =https --proto-redir =https`) and a 64-digit lowercase SHA-256, downloads to a temporary file beside the destination, compares the SHA-256 and only then moves the file into place. A mismatch, a failed download, a bad argument or an existing destination exits non-zero and leaves nothing behind. It has no test or insecure mode; its tests put a stand-in `curl` on `PATH`. The test fails on any `curl`, `wget`, `gh release download`, PowerShell download, inline Python or Node HTTP client, or `go`/`cargo`/`pip` tool install in a `run:` step, and on a call of the script whose URL or checksum is not a literal (the one exception is the staging rehearsal's readiness probe of `https://localhost:18443`; `gh api` is allowed only in `prune-dev-images.yml`, which deletes old dev package versions).

| Tool | Where | Pin |
| --- | --- | --- |
| Trivy | `image.yml` `verify` | v0.70.0 release tarball per architecture, SHA-256 from the release's `trivy_0.70.0_checksums.txt` |
| cosign | `image.yml` `publish` | v3.0.6 `cosign-linux-amd64`, SHA-256 from `cosign_checksums.txt` |
| helm | `ci.yml` `helm` | v3.18.4 from get.helm.sh, SHA-256 from its `.sha256sum` |
| kubeconform | `ci.yml` `helm` | v0.7.0 release tarball, SHA-256 from the release `CHECKSUMS` |
| Node.js | `.github/actions/setup-node` (all `ci.yml` jobs) | 22.23.3 (the `Dockerfile`'s version) from nodejs.org, SHA-256 from `SHASUMS256.txt` |
| cargo-deny | `ci.yml` `cargo-deny` | `taiki-e/install-action` at a pinned SHA with `cargo-deny@0.20.2`; it verifies the checksum in its bundled manifest, and the test requires an exact version and the checksum left on |

To bump one, change the version in the URL and the checksum together and take the checksum from the vendor's published checksum file.

**Residual risk.** The write-job allowlist is trusted code that holds the job token, and so is what it runs: the Buildx and Docker engine that come with the runner image, BuildKit, and GitHub's own actions (`actions/*`, `github/codeql-action`), pinned by SHA but not audited by us. The runner image itself is GitHub's. The image build (`docker/build-push-action`) fetches Rust crates (locked by `Cargo.lock`) and npm packages (`package-lock.json` integrity hashes) inside the Dockerfile's build stages, and reads the BuildKit `type=gha` layer cache that earlier builds of the same ref wrote; a compromised dependency reaches the image, which is why the image is scanned, attested and tested but not rebuilt from source by anyone else. Not covered by a checksum of ours, and run only in read-only jobs: the Rust toolchain (`dtolnay/rust-toolchain` runs rustup, which verifies component hashes from the channel manifest over TLS; the version it installs is pinned to an exact release by `rust-toolchain.toml`, not the floating `stable` channel — see [Rust toolchain](#rust-toolchain)), Playwright's Chromium (version fixed by `package-lock.json`), apt packages (`age`, signed by the distribution), CodeQL's bundle and Trivy's vulnerability database. The SARIF file passes from the read-only analysis job to the upload job as data.

### Rust toolchain

`rust-toolchain.toml` pins an exact channel version (currently 1.99.1) with the `rustfmt` and `clippy` components. `rustup` reads it automatically, so a plain `cargo fmt`/`cargo clippy`/`cargo build`/`cargo test` on a machine with `rustup` installs and uses that exact compiler without any extra flag — **local gates must run on the pinned version, not whatever toolchain happened to be default or active before**. Both `ci.yml` jobs that install Rust (`gateway`, `browser`) pass the same version explicitly to the SHA-pinned `dtolnay/rust-toolchain` action's `toolchain:` input, and every Rust build stage in every Dockerfile in the repository — the root `Dockerfile` and `deploy/loadtest/Dockerfile` — uses `rust:1.99.1-bookworm` at the identical digest, pinned by digest. `tests/supply-chain.test.mjs` parses `rust-toolchain.toml` with a small strict TOML-subset parser, finds every Dockerfile by walking the repository (not a fixed list) and checks every `FROM rust:...` stage in each, and fails if any of them disagree; if the channel is not an exact `x.y.z` version; if two Dockerfiles pin the same version at different digests; if any `dtolnay/rust-toolchain` step omits `toolchain:`, sets a floating channel (`stable`, `beta`, `nightly`, with or without a date), or names a different exact version; or if any workflow or local composite action overrides the pin at run time (`RUSTUP_TOOLCHAIN`, `rustup default`/`override set`/`toolchain install`, or `cargo +<toolchain>`).

`rust-version` fields in `Cargo.toml` (`apps/gateway`, `tools/loadgen`, `tools/mock-upstream`; currently `1.88`) are each crate's **minimum supported Rust version** for crates.io consumers, not the build pin: they are allowed to sit well below 1.99.1 and are not checked against `rust-toolchain.toml`.

This exists because CI and local development can otherwise drift onto different compilers: PR #348 passed every local gate on rustc 1.98.1, then failed CI's `-D warnings` build because the newer floating `stable` channel had deprecated an API the code used (#354).

**Bumping the pinned version is a single reviewed PR, never a side effect of another change:**

1. Read the new version's release notes for behavior changes, new lints and deprecations.
2. Change the version in `rust-toolchain.toml`, both `ci.yml` steps' `toolchain:` input and the `Dockerfile`'s `rust:<version>-bookworm` tag and digest together, in the same commit.
3. Fix any lint or warning the new compiler introduces (CI turns warnings into errors).
4. Run the full local gates in [Test commands](#test-commands) above on the newly pinned version before opening the PR; `tests/supply-chain.test.mjs` also runs as part of `npm run test:supply-chain`.

An optional non-blocking scheduled job on the floating `stable` channel would give early warning of an upcoming deprecation, but it would need its own job with a read-only token and no secrets, distinct from the pinned jobs above; none exists yet (#354 leaves it for a follow-up rather than adding a job that is not yet verified to run safely).

### No Docker Hub pulls in CI

GitHub's shared runners hit Docker Hub's anonymous pull limit, and a Docker Hub outage (`auth.docker.io` 504) failed builds, so CI and the release pipeline pull nothing from Docker Hub:

| Image or tool | Source |
| --- | --- |
| `Dockerfile` build stages (`node`, `rust`) | `mirror.gcr.io/library/...`, Google's Docker Hub mirror (the same image digests). No `# syntax=` line: BuildKit's built-in Dockerfile frontend. The runtime base was already `gcr.io/distroless`. |
| PostgreSQL service containers (`gateway`, `browser` jobs) and `test:container`'s database | `mirror.gcr.io/library/postgres:17-alpine` |
| Staging rehearsal PostgreSQL and Caddy | `STAGING_POSTGRES_IMAGE` / `STAGING_CADDY_IMAGE` set to `mirror.gcr.io/library/postgres:17-bookworm` and `mirror.gcr.io/library/caddy:2-alpine` by `ci.yml` and `image.yml`. `deploy/staging/compose.yaml` still defaults to Docker Hub for operators. |
| BuildKit (`docker/setup-buildx-action`) | `driver-opts: image=mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:…` |
| SBOM generator | `attests: type=sbom,generator=mirror.gcr.io/docker/buildkit-syft-scanner:stable-1@sha256:…` (replaces `sbom: true`) |
| MinIO / RustFS file-store tests | `mirror.gcr.io/pgsty/minio@sha256:…` (pgsty publishes only to Docker Hub; the pull is retried with backoff) and `ghcr.io/rustfs/rustfs@sha256:…` (RustFS's own registry). Same digests as on Docker Hub. |
| cargo-deny | Release binary via `taiki-e/install-action` (not the Docker-based `EmbarkStudios/cargo-deny-action`) |
| Trivy | The pinned release binary (GitHub Releases, SHA-256-checked); its vulnerability database already defaults to `mirror.gcr.io/aquasec`, then `ghcr.io` |

The mirror serves only images it has cached: popular official images are, but an image or tag nobody has pulled through it is "not found". Before changing a mirror reference, check that it resolves and matches Docker Hub, for example `docker buildx imagetools inspect mirror.gcr.io/library/rust:1.99.1-bookworm` and the same for `rust:1.99.1-bookworm`. Dependabot's `docker` updates still track the `Dockerfile`'s mirrored build stages (it queries `mirror.gcr.io`, whose tag list holds only cached tags, so a new version can appear there later than on Docker Hub; bump by hand if it lags). The mirror references in workflows, tests and the staging overrides are not tracked by Dependabot and are updated by hand. Local developer stacks (`compose.yaml`, `deploy/demo/compose.yaml`) are not used by CI and still pull from Docker Hub.

`deny.toml` ignores two advisories, each with its reason: RUSTSEC-2023-0071 (`rsa` via `openidconnect`; no fixed release, and the gateway only verifies ID-token signatures with public keys) and RUSTSEC-2026-0253 (`lru` via the pinned `aws-sdk-s3`; unsound only when a key's `Drop` panics, fixed with the next AWS SDK generation). No licence exceptions are needed; the allow-list is MIT, MIT-0, Apache-2.0 (with or without LLVM-exception), BSD-2/3-Clause, ISC, 0BSD, Zlib, Unicode-3.0 and CDLA-Permissive-2.0 (`webpki-roots`).

### Browser suite

`npm run test:browser` (Chromium only, about 2 minutes after builds) starts an isolated stack from `tests/browser/stack.mjs`:

- creates a new database `omg_browser_<id>` on `OMG_BROWSER_ADMIN_DATABASE_URL` (default the disposable cluster `postgres://gateway:gateway@127.0.0.1:54339/gateway`; port 54349 is refused), runs `migrate` and `provision-user --platform-admin`, and drops the database afterwards (`OMG_BROWSER_KEEP_DB=1` keeps it);
- builds the SPA into `target/browser-tests/web-dist` and the debug gateway binary, then serves them with `GATEWAY_WEB_DIR` on 127.0.0.1:18291 (`OMG_BROWSER_PORT_BASE` moves the ports);
- runs the passwordless local demo issuer (`createDemoIssuer` from `scripts/demo-oidc.mjs`, test ports only) and a deterministic OpenAI-compatible mock upstream approved through `GATEWAY_LOCAL_UPSTREAMS`.

The journey signs in each persona through real signed OIDC: the unentitled user is denied; the Platform Admin creates a Team and Project, maps SSO groups, adds a mock connection and a priced model and assigns it; the Auditor sees Admin without mutation controls and gets 403 from mutations; the team admin and member get their workspace roles; the member creates a key, calls `/v1/chat/completions` through the mock, sees the request in Logs (with a known cost) and Usage, checks key pages at 390px, revokes the key and gets 401. The accessibility pass is described in [accessibility](accessibility.md).

`OMG_BROWSER_SKIP_BUILD=1` reuses existing builds; `OMG_BROWSER_GATEWAY_BIN` / `OMG_BROWSER_WEB_DIR` select other builds. Failures leave traces and screenshots of the disposable stack in `target/browser-tests/` and the gateway log in `target/browser-tests/gateway.log`.

## Single-enterprise rebuild — current evidence

The organization-free rebuild uses fresh enterprise migrations. Existing installations and the legacy demo were not reset, migrated or reseeded. The current local demo is `gateway_enterprise_demo` on loopback PostgreSQL 54349, served by an explicitly verified restricted runtime role; 54339 is the separate disposable regression cluster.

Current integrated validation passed formatting, strict workspace/all-target/all-feature Clippy, Rust build, **291 Rust checks including the separately invoked runtime ACL rollback probe**, frontend typecheck, **477 frontend tests**, isolated SPA build, container/staging helpers and the guarded runtime-provisioning tests. The served SPA is that verified 477-test build, promoted only after the gateway was stopped; prior builds are preserved privately.

Fresh signed-OIDC browser passes exercised Admin, Auditor, Alex, Blair and unentitled accounts. Checks covered actual versus global membership, private credential boundaries, direct user details, own key rotation/revocation, service-account disable/non-resurrection, six-decimal policy values, zero-data CSV and desktop/390px navigation. Browser review found and drove fixes for pristine empty replacement creation, corrected-input validation retries, and unentitled callback UX. Parent retests confirmed price retry/publication with immutable prior versions, direct empty catalog replacement, readable deployment labels and friendly denied login with `/me` remaining 401. Pristine settings now refresh after a server-side reset without overwriting edited drafts (confirmed in the browser). A freshly issued inherited key returned 200 from `/v1/models`, 401 against `/api/v1/me`, and 401 after revocation; a browser session alone returned 401 for inference. The test-only Acceptance Project was disabled afterwards; its configuration endpoints then return 404. The all-blank replacement-policy path is covered by component and governance tests; its browser attempt was inconclusive because automation did not clear the numeric inputs.

No paid/provider inference, production IdP changes, fabricated usage/ledger data, remote CI, container deployment, accessibility certification or production-readiness claim is implied. Browser evidence remains private under `.local/enterprise-rebuild/`.

## Multimodal increment and OpenRouter (2026-10-08)

Added Grounded-style model setup (Connections \u2192 Models with routes, readiness and pill-tab record pages), People/Records restyle, OpenRouter provider, OpenRouter-style v3 price lines with public-catalog import, and new workloads: `/v1/images/generations`, `/v1/audio/transcriptions`, `/v1/audio/speech`, `/v1/rerank`, `/v1/systemone`. Migrations 0002 and 0003 were applied explicitly to the fresh demo database only; runtime grants were reapplied and verified.

Integrated gates: fmt, strict Clippy, **458 Rust checks** (disposable PostgreSQL, including the runtime ACL rollback probe), **572 frontend tests**, typecheck and isolated SPA build. Live, user-authorized, capped tests used cheap models through real adapters and the running gateway: OpenAI `gpt-6-luna` (Chat, Responses incl. opaque reasoning items), `gpt-image-1-mini`, `whisper-1`, `tts-1`; Anthropic `claude-haiku-5-5`; OpenRouter GLM 5.3 Flash, Nemotron embed/rerank, Clef Flash (System One), MAI-Voice-2-Flash. Recorded costs matched usage \u00d7 configured rates exactly; total live spend across runs was well under $0.05. Reports: `.local/enterprise-rebuild/{live-acceptance,openrouter-live,images-live,audio-live,multimodal-acceptance}-report*.md`.

Known limits: OpenRouter images and transcription are mock-verified only (account has no purchased credit, upstream 402); OpenRouter `:free` models are blocked while the server data-collection policy is `deny`; provider-reported cost is stored as evidence, never the charge; video, realtime audio and asynchronous jobs remain unimplemented.

## Archived pre-rebuild verification

**Everything below describes the superseded multi-organization implementation and historical test runs. It is not current rebuild acceptance, and its old demo/migration reproduction commands must not be used as enterprise upgrade instructions.**

## Organization-settings sidebar regression

Fixed Organization settings dropping the workspace navigation. Sidebar links and jump search now retain the selected workspace within the same organization; the main settings route, breadcrumbs, permissions, and data remain organization-scoped. Workspace context is revalidated against fresh session inventory, isolated by user/organization, and cleared on logout. Missing explicit scopes still do not fall back.

Frontend typecheck, **295 tests**, and the production build passed. Read-only browser checks as the existing demo Organization Admin verified Product → Organization settings retains all six sidebar links, reload preserves Product, Members tab navigation retains focus/sidebar links, and API keys returns to Product. No demo configuration, membership, keys, or provider traffic was changed.

## Grounded-inspired dashboard refresh

Passed formatting, strict all-target/all-feature Clippy, **239 Rust tests** with disposable SQLx databases, and the separately invoked **runtime ACL rollback probe**. Frontend typecheck, **287 unit/source-render tests**, the production Vite build, Rust build, demo issuer tests, container-entrypoint tests, staging/helper tests, and diff checks passed. Catalog tests include redacted direct lookups, live operator revocation, literal search beyond the first 200 records, and filtering before pagination. Session tests cover actual versus inherited membership, human-key eligibility, personal privacy, and existing policy authority. No new runtime grants or migrations were necessary; explicit catalog/session read probes exercise the existing ACLs. Bitop component sources and the CLI lock file are unchanged.

Browser acceptance used the compiled SPA, actual PostgreSQL/session middleware, the opt-in signed test OIDC issuer, and a disposable copy of the demo database. One fixture identity was exercised through controlled platform-operator, organization-admin, shared-workspace-admin, and member role changes. Checks covered:

- Platform-only Admin and contextual organization/workspace settings; inherited authority versus direct membership; service-only issuance for an operator without membership, and successful human-key issuance for a direct shared member.
- Model/provider/deployment detail links, direct missing-record handling without writable child panels, catalog search/status URLs, model/provider-prefilled deployment forms, and disabled-by-default creation.
- URL-backed resource and nested policy tabs, selected-tab focus, read-only member limits, legacy scoped-admin bookmarks, unauthorized Admin, and unknown paths.
- Dirty-form Keep editing/Discard behavior. The browser pass found and fixed asynchronous deployment creation losing its opener; dismissing now returns focus to Create deployment.
- One-time key display without printing/copying the token, followed by browser Back removing the dialog and secret. The disposable key selected No models; no inference was sent.
- Owner-personal creation/visibility and absence of sharing controls; search Escape focus restoration; 390px settings/catalog layouts without horizontal document overflow.

The browser session, issuer, gateway, and disposable database were removed. The original demo retained **4 users, 6 workspaces, 8 keys, 0 revoked keys, 4 price versions, 0 executions, and 0 ledger rows**. A private local backup remains outside the build context. No provider calls, staging deployment, enterprise-SSO acceptance, or new axe scan were performed. These manual browser checks are not a committed automated end-to-end gate; full paginated session inventory and on-demand selectors remain separate roadmap work.

## Prior Bitop source refresh

Refreshed the changed Button and CommandPalette sources and preserved the checked search-contrast composition patch. The source now supplies an MIT notice; it is copied verbatim and recorded with the component hashes. Verified all **45** source/copied manifest entries (44 component/support files plus LICENSE). TypeScript, **135 frontend tests**, and the production build passed.

Signed-OIDC browser checks verified that a search navigation focuses the destination heading despite the scope-keyed shell remount, while Escape restores the search opener. Scope-keyed action-dialog teardown remains intact. Governance still uses pill tabs and opens the demo budget as `5.00` USD with a decimal keyboard hint. Sampled axe WCAG2 A/AA checks reported zero violations and the existing keyboard-glyph manual contrast flag. No policy, price, or provider configuration was changed during verification.

## Prior pill tabs and US-dollar inputs

Passed TypeScript, **131 frontend tests**, and the production Vite build. Governance now uses the vendored Bitop pill tabs for scope and detail selection instead of stacked policy panels. Budget and price forms accept USD decimal strings, convert exactly with BigInt, and preserve the existing micro-USD API contract. Tests cover prefills, exact roundtrips, ceiling comparisons, int64 input/reservation boundaries, meaningful subcent precision, and read-only aggregate totals beyond one int64 ledger entry. Verified all **44** vendored file hashes against the provenance manifest.

Signed-OIDC browser checks confirmed workspace-first governance, arrow-key focus plus Enter activation, one visible policy panel, organization/platform/local detail switching, and API-key policy selection. Organization Admin's budget opens as `50.00`; `50.000001` was rejected against the $50 inherited ceiling and seven-plus decimal places were rejected without rounding or saving. Platform pricing forms show USD per million tokens. Alex sees only Workspace/API keys scope tabs, not organization administration. At 390px there was no horizontal document overflow or browser local/session storage. Sampled axe WCAG2 A/AA checks reported zero violations and the existing keyboard-glyph manual contrast flag. No budgets or prices were changed/published during this browser pass.

## Prior admin selector and organization-limit clarity

Passed TypeScript, **123 frontend tests**, production Vite build, **12 governance API database tests**, and the HTTP inference regression enforcing one platform ceiling across team/project/personal traffic. No backend behavior or persisted limits changed in this UI refinement.

Signed-OIDC browser checks verified Platform → organization → Platform round trips with child URL parameters cleared, and returning to Platform from Workspace mode. Alex's Admin entry now opens Product directly; the Admin selector contains only managed Product/Research scopes, and the sidebar no longer presents an Organization administration section. Switching to Research opens project administration. Organization Admin sees the renamed Organization limits screen with platform maximums, additional local limits, and effective caps; no workspace-selection prompt. A local 121-attempt/minute value was rejected against the platform's 120 maximum without saving. At 390px there was no document overflow or local/session storage; sampled axe WCAG2 A/AA checks had zero violations and the existing keyboard-glyph manual contrast flag. No demo records were changed by this browser run.

## Prior platform-owned catalog, delegated limits, and Projects

Passed: **219 Rust tests** (196 library, 2 CLI, 9 database, 8 inference, 3 catalog-migration integration, 1 SDK), strict all-target/all-feature Clippy, formatting and Rust build; **115 frontend tests**, TypeScript and production Vite build; **4 demo issuer tests**. New tests cover shared global deployments with distinct organization aliases/entitlements, preservation of immutable history during migration, live grant/credential revalidation, aggregate platform ceilings across team/project/personal scopes, separate platform/local policy authority, project parity, and legacy infrastructure-route rejection.

A separate read-only review found that a delegated admin could erase an explicit local cap when a tighter parent temporarily masked it. The corrected check compares durable local values, not current effective values. A regression exercises workspace/key caps across parent lowering and later raising. Personal model-grant listings now include owner-only individual grants, identify their source, and never imply that deleting a workspace grant also deletes an individual grant.

The existing `gateway_demo` was backed up before applying migrations 0007/0008. Migration retained its four users, eight keys, five original workspaces, four price versions and accounting history. No reseeding/reset was performed. Browser verification then created the new **Research** project through the Organization Admin UI, added Alex as project admin and Blair as member, delegated the existing `demo/fast` entitlement, and saved a 10-attempt/minute cap which Alex tightened to 8. Existing Product/personal memberships, permissions and limits were not changed.

Real signed OIDC browser checks verified:

- Platform model/provider/deployment configuration and pricing/routing navigation are available without an organization URL/context; organization assignment/ceiling management uses an explicit consumer selection. Providers omit credential references. Platform sidebar remains flat, without a separate Organization section.
- Organization Admin sees assigned models, Teams/Projects, memberships and local policy controls, not infrastructure setup. Global catalog/project/ceiling endpoints and old organization provider endpoints return 403; assigned catalog returns 200.
- A new project's unset limits display shared parent allowance and inherited platform/org ceilings. A 121-attempt limit was rejected against the 120 parent; saving 10 worked. Alex could read project membership and tighten to 8, but could not clear the explicit cap (403), read org members, or use global project administration.
- Blair has project membership, read-only governance and granted-model visibility, but no Admin link, policy-edit control or project-member administration. Only Blair's own personal workspace appears in the session.
- Local/session storage remained empty; no real provider traffic or fabricated usage was generated. The sidebar now collapses when resizing into a narrow viewport; both fresh mobile load and desktop-to-390px resize had no document overflow. Sampled 390px project governance axe WCAG2 A/AA scan had zero violations, with a keyboard glyph flagged for manual contrast review. These are bounded checks, not accessibility certification.

The demo remains on http://127.0.0.1:3000 with disabled upstream connections/deployments. Production cloud/IdP acceptance, load testing and financial invoicing remain outside this verification.

## Prior demo personas, platform navigation, and jump search

Passed: **191 Rust tests** (172 library, 2 CLI, 9 database, 7 inference, 1 SDK), strict all-target/all-feature Clippy, formatting, Rust build, **100 frontend tests**, typecheck/production Vite build, and **4 demo issuer tests**. The Bitop refresh tracks 42 files, including a reproducible popup className composition hook for scoped search contrast.

The existing local `gateway_demo` was upgraded with `bootstrap-demo --add-missing-personas`, adding only the missing Organization Admin identity/memberships and its new fixtures. Existing account privileges, edits, revoked keys, catalog and history were not reset. The issuer and gateway were restarted together. Tests cover fresh four-persona roles, guarded additive upgrades, existing-email collisions, concurrency, rollback, and no revival/adoption of existing users.

Browser verification used real signed OIDC login/logout for all four personas. Platform Admin sees Organizations / Teams / Users under Platform with no separate Organization section. The cross-organization Teams directory, organization filter and creation form's parent-organization selection worked; no extra live demo team was created. Organization Admin could read organization/team membership but received 403 for both platform directories. Alex could read team membership but received 403 for organization membership and both platform directories. Blair had no Admin portal and received 403 for all four administrative endpoints. Each non-platform persona's session contained only one personal workspace.

Header search was checked by click and ⌘K, filtering, Enter navigation to Users/Product, an unauthorized Users search with no results as Alex, Escape, focus trapping and return to the trigger. ARIA controls/active-descendant IDs resolved to real elements. The full-width footer was absent, storage remained empty, and the 390px layout had no document overflow. Overflowing mobile tables became labelled, keyboard-focusable scroll regions. Settled-dialog axe WCAG2 A/AA checks had zero violations after contrast correction; remaining manual-review flags concerned Base UI focus guards/ARIA relationships, keyboard glyphs and clipped/off-screen table content. This is sampled verification, not comprehensive accessibility certification.

## Prior hierarchy administration checks

Formatting, strict all-target/all-feature Clippy, Rust build, **181 Rust tests**, frontend typecheck, **92 frontend tests**, and the production Vite build passed.

New coverage includes platform-user directory authorization/projections, scoped organization/team pagination, personal-workspace exclusion, audited renames, concurrent privilege changes, and rechecking organization/team creation authority. Frontend coverage verifies hierarchy pages, permission-specific creation actions, an existing-resources-only selector, platform views without implicit organization context, and upward navigation clearing lower scopes.

A real signed-OIDC browser run against disposable `gateway_hierarchy_smoke` verified an operator with no organizations entering Admin → Organizations, creating an organization and team from page buttons, reaching team members, renaming both resources, opening the invitation entry point, and viewing platform users. The selector showed only existing organization/personal/team entries. Both smoke processes were stopped and that database was removed; no demo/user data was modified by those creation tests.

The live demo was separately checked as Alex: Admin opened the scoped Teams page, team creation and platform Users controls were absent, and the Users API returned 403. A final Operator check verified platform navigation lists Organizations/Users only until entering an organization. Desktop Teams/Users axe WCAG2 A/AA checks had zero violations/incomplete checks at the sampled full viewport. Mobile Users had zero violations, with off-screen table timestamp columns flagged for manual contrast review; horizontal table scrolling remains intentional. The sampled 390px layout had no document overflow, and local/session storage were empty.

## Prior governance, Bitop dashboard, and local-demo checks

The integrated checkout passed formatting, strict all-target/all-feature Clippy, Rust build, and **170 Rust tests** (152 library, 1 CLI guard, 9 database, 7 inference integration, 1 official-client SDK integration). Frontend typecheck, **80 frontend tests**, production Vite build, and **4 local-demo issuer tests** passed. All **38 vendored Bitop file hashes** match their provenance manifest. Vite reports a non-fatal approximately 582 kB JavaScript chunk-size warning.

New regression coverage includes distributed quota races, hierarchical limits, pinned prices, unknown/partial usage holds, legacy unpriced history, UTC cycles, reconciliation authorization under lock, retention preserving accounting, key-rotation policy/consumption lineage, cached-target drift at admission, per-attempt failover accounting, required-residency filtering, and deadline cancellation/capacity release without poisoning passive health.

Fresh browser checks used the compiled SPA and actual PostgreSQL/session middleware:

- An isolated `gateway_governance_smoke` database exercised signed OIDC login, organization creation, and saving a workspace quota/budget policy. Its gateway/issuer were stopped and its database removed.
- The explicitly requested local demo on **127.0.0.1:3000** uses the separate **gateway_demo** database. The account picker, Operator → Alex → Blair sign-ins and logout transitions were exercised through real PKCE/nonce-bound signed OIDC callbacks.
- Operator pricing versions and price form, model/deployment routing controls, saving a default routing policy, unknown passive health, and empty cost summaries were inspected. Alex could select Product and see team administration, without the organization Admin portal; Blair likewise had no Admin portal.
- Sampled picker, governance page, price dialog, desktop Costs and mobile overview returned **zero axe WCAG2 A/AA violations**. Mobile document width matched the 390px viewport. Local/session storage and lingering secret-field counts were zero at the sampled signed-in state.
- Demo provider connections/deployments remain disabled. No provider requests, fabricated usage/costs, or real enterprise credentials were used. The demo is deliberately left running for exploration; see [local-demo.md](local-demo.md).

These are bounded smoke checks, not exhaustive browser coverage. The demo issuer's browser flow additionally checks its cross-origin callback CSP allowance and Origin-preserving referrer policy; unit tests cover host/redirect/origin/account rejection, signatures, PKCE, and code reuse.

## Prior milestones 1–3 baseline

### Automated checks

The integrated checkout passed:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features` with PostgreSQL: **114 tests** (99 library, 9 database, 5 inference integration, 1 official-client SDK integration).
- `npm run typecheck:web`, `npm run test:web`: **56 tests**, and `npm run build:web`.
- `cargo build -p open-model-gateway`.

The SDK test runs current locked OpenAI/Anthropic JavaScript clients against an actual gateway HTTP server and a deterministic fixture adapter. It checks Chat, Responses, and Messages JSON responses and streaming helpers (`finalResponse`, `finalMessage`). It does not contact providers.

Database tests exercise real migrations, tenant-scoped constraints, session/CSRF rules, invitations, owner protections, human/service-account keys, and concurrent lifecycle changes. Regression tests cover key-creation/removal races, concurrent ownership changes and invitation acceptance, stale ownership insertion, and personal model policy across existing/future workspaces.

A separate read-only code review found the stale workspace-owner insertion race; ownership insertion now uses the same parent-lock ordering and authority recheck as role updates.

## Browser smoke

A separate gateway used an isolated `gateway_browser_smoke` PostgreSQL database and the compiled SPA on loopback port 18081. The test-only issuer at 18082 generated an RSA key, signed nonce-bound ID tokens, and validated PKCE. The smoke exercised the real callback/session middleware, not an injected browser session or mocked management API.

Verified through browser interactions:

1. OIDC login as an explicitly CLI-provisioned fixture operator.
2. Create organization and team workspace; navigate between management screens.
3. Create model alias and enable organization-wide personal access.
4. Create, rotate and revoke a key; dismiss one-time disclosures without logging/copying their values.
5. Create a team service account.
6. Missing CSRF returns 403; no session plus an inference-style Bearer header returns 401.
7. Sign out; the next `/me` request returns 401.
8. No local/session storage entries; no token field remains after disclosure closes.
9. Desktop/mobile rendering; document width equals the 390px mobile viewport. Sampled signed-in and signed-out pages had zero axe WCAG2 A/AA violations.

This is a smoke test, not exhaustive browser coverage. The fixture processes were stopped and the isolated database removed. No live SSO, cloud credential chain, paid inference, or production data was used.

### Reproduce the local fixture

`tests/oidc-smoke-issuer.mjs` is deliberately insecure **test-only** authentication for a single synthetic user. It refuses to run without `OMG_ENABLE_TEST_ISSUER=1`, binds only loopback, and accepts only client `gateway-browser-smoke` with callback `http://127.0.0.1:18081/api/v1/auth/callback`. Never register this issuer against a real deployment.

Use an isolated disposable database. Build the SPA/gateway, migrate that database, and run `provision-user --email browser-smoke@example.invalid --platform-admin` against it. Start the issuer with its opt-in flag. Configure the separate gateway with:

```text
GATEWAY_ENV=development
GATEWAY_LISTEN=127.0.0.1:18081
GATEWAY_WEB_DIR=apps/web/dist
GATEWAY_PUBLIC_URL=http://127.0.0.1:18081
GATEWAY_OIDC_ISSUER=http://127.0.0.1:18082
GATEWAY_OIDC_CLIENT_ID=gateway-browser-smoke
```

Do not set a client secret. Start the gateway, browse to port 18081, and use the sign-in link. Stop both fixtures and remove only their disposable database afterward.

## Single-host staging rehearsal

Verified locally after the Bitop refresh:

- 219 Rust tests, 135 frontend tests, four demo-issuer tests, 33 container-entrypoint cases and nine Python deployment/acceptance-helper tests passed. Frontend typecheck and Rust formatting passed.
- Built the locked multi-stage Linux/arm64 image with Rust 1.90/Node 22; runtime runs as UID 10001, contains the built SPA and Bitop license, and has no Node/Cargo executable.
- Booted an independent production-mode Postgres/Caddy/gateway Compose stack at `https://localhost:18443`; CA-verified HTTPS readiness succeeded, OIDC reports disabled, and unauthenticated `/api/v1/me` returns 401.
- Runtime grants prohibit DDL/schema ownership, maintenance-database connections, platform-user provisioning, migration writes and immutable ledger/price/audit mutation. Rollback-only assertions exercised membership/key triggers and lock permissions under the actual runtime role.
- Repeated the complete fresh-cluster initialization/migration/privilege/HTTPS/restore sequence on a disposable `omg-staging-cold` project with separate ports/state, then removed only that test project's containers and volumes.
- Rehearsed private custom-format backups and disposable restoration using a direct migrator-role connection, with schema/history checks. This is a same-host rehearsal, **not encrypted/off-host disaster recovery or measured RTO/RPO acceptance**.
- Confirmed the retained staging database has zero users, organizations, providers, executions and ledger rows. The existing demo was not reset. The provider helper was tested with mocks only; no paid inference was sent.

CI now includes a Linux image/isolated-stack smoke; the optional manual GHCR publication workflow tests its exact image before pushing. Remote CI/publication is not implied by these local results. See [staging](staging.md) and [live acceptance](live-acceptance.md).

## Per-key model restrictions — completed first release

- Full validation passed: **232 Rust tests** (including 13 dedicated restriction tests), **212 frontend tests**, four demo-issuer tests, 33 container-entrypoint cases and nine Python deployment/helper tests; Rust formatting/strict Clippy, frontend typecheck and builds passed.
- Covers omitted/null inheritance, explicit deny-all, true subset denial, 200 distinct selections/raw-input bounds, disabled-but-granted preconfiguration, tenant/composite FK isolation, owner-personal individual grants, team/project/service keys, private list visibility, repeated rotation and budget continuity, entitlement removal/regrant, cached-target denial and creation/admission races. Corrected the invalid assignment fixture and advanced the legacy-backfill test through migrations 0008/0009; its conservative unknown-cost assertions remain intact.
- Added reproducible Bitop checkbox primitives; all 47 copied-file hashes have regression coverage, and the source/copied hashes and regeneration were checked against the read-only upstream. The incidental upstream StatCard refresh remains compatible with existing unlinked tiles.
- Isolated real-OIDC browser acceptance on ports 18081/18082 exercised selected/inherited/deny-all creation, stale hidden selections, rotation, invalid-selection focus, keyboard checkbox interaction and a delayed save with duplicate submission suppressed. One-time credentials were not printed, persisted to browser storage or included in screenshots; closing the dialog removed them. All controls disabled and the form reported busy during the held save.
- At 390px, the dialog/page had no horizontal overflow. Sampled dialog axe checks reported zero violations, with three contrast nodes requiring manual review because of overlapping-background detection; this is not an accessibility certification. Screenshots were kept outside Git at `/tmp/omg-key-model-selection.png` and `/tmp/omg-key-model-mobile.png`.
- Rebuilt the Linux/arm64 runtime image, backed up staging, applied migration 0009 and the explicit runtime grants, and passed readiness/CA-verified HTTPS, rollback-only privilege/cascade checks and disposable restore. Runtime can insert/read restriction rows but cannot directly update/delete them; parent-entitlement FK cascades remove selections while preserving headers.
- Backed up and migrated the existing demo from eight to nine migrations without reseeding. Prior users/workspaces/keys/revocations/prices/execution/ledger counts were unchanged. Restarted the demo with matching binary/SPA and confirmed readiness. Staging OIDC remains disabled; no paid inference was sent. The isolated browser fixture services and database were removed afterward.

Selections are fixed at issuance in this release. Rotation retains them; post-creation editing and all other unselected roadmap items remain out of scope. Remote CI results are separate from these local checks.

## Remaining acceptance work

Real enterprise IdP interoperability, live upstream/model permissions, workload credential refresh, production load/security testing, public-host ingress/certificates, provider invoices and customer billing remain unverified or unimplemented. Distributed quotas/budget reservations now have database/concurrency coverage, but have not been production-load validated. Native Responses/Messages stream text incrementally but deliver tool calls whole; see [protocol matrix](protocol-matrix.md). These milestones do not make the service ready for public launch.

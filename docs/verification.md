# Verification

## Test commands

Run from the repository root. None of them make paid provider calls or touch the local demo (PostgreSQL 54349, port 3000).

| Command | Covers | Needs |
| --- | --- | --- |
| `cargo fmt --all -- --check` | Formatting | Rust |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Lints | Rust |
| `cargo test --workspace --all-features` | Unit, mock-provider contract, SDK and real-PostgreSQL integration tests (SQLx creates disposable databases) | `DATABASE_URL` to the disposable cluster, e.g. `postgres://gateway:gateway@127.0.0.1:54339/gateway`; `npm ci` for the SDK tests |
| `cargo test -p open-model-gateway --all-features --test runtime_privileges -- --ignored` | Runtime-role ACL rollback probe | Disposable cluster only |
| `npm run typecheck:web` / `npm run test:web` / `npm run build:web` | SPA types, Vitest unit/component tests, production build | Node 22 |
| `npm run test:browser` | Playwright end-to-end journey and axe accessibility scans (below) | Disposable PostgreSQL, Rust, Chromium (`npx playwright install chromium`) |
| `npm run test:demo` | Local demo issuer and runtime helper | Node, Python 3 |
| `npm run test:container` | Runtime image contract (`tests/container-image.test.mjs`): distroless (no shell, package manager, curl, perl, Node or Cargo), UID 10001, read-only root filesystem, `_FILE` secret import and refusal, `serve` never migrates, explicit `migrate`, exec-form `healthcheck`, graceful `SIGTERM` as PID 1 | Docker, Node; builds the image unless `OMG_CONTAINER_IMAGE` names one; uses a throwaway `mirror.gcr.io/library/postgres:17-alpine` on an internal network |
| `npm run test:staging` | Staging (including the runtime-image checks of `verify`), provider-acceptance and backup helpers | Python 3 |
| `cargo deny check` | Supply chain: RustSec advisories (vulnerable, unmaintained, unsound, yanked), licence allow-list, duplicate-version warnings, crates.io-only sources; policy in `deny.toml` | [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) 0.20 (`cargo install cargo-deny --locked` or the release binary); fetches the advisory database |
| `npm audit --omit=dev --audit-level=high` | Supply chain: high and critical advisories in production npm dependencies (the SPA bundle) | Node 22; reads `package-lock.json` |

CI (`.github/workflows/ci.yml`) runs all of these; the `browser` job uses a PostgreSQL service container, and `test:container` runs in the `staging-image` job (and on each pushed digest in `image.yml`). The binary's startup secret handling and `healthcheck` are also covered by `cargo test` (`apps/gateway/src/startup.rs`, `apps/gateway/src/healthcheck.rs`, `apps/gateway/tests/startup.rs`).

### Supply chain

CI's `cargo-deny` job runs `cargo deny --all-features check` (cargo-deny 0.20.2's release binary, installed and checksum-verified by the SHA-pinned `taiki-e/install-action`), and the `npm-audit` job gates on `npm audit --omit=dev --audit-level=high` and writes the full, non-blocking `npm audit` (dev tooling included) to the job summary. `.github/workflows/codeql.yml` runs CodeQL for `javascript-typescript` and `rust` (build mode `none`) on pushes to `main`, pull requests and weekly; results appear on the Security tab. `.github/dependabot.yml` opens weekly grouped updates for Cargo, npm (root workspace), GitHub Actions (SHA pins) and the `Dockerfile` base images. The release image is additionally scanned by Trivy in `image.yml`.

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
| Trivy | Its vulnerability database already defaults to `mirror.gcr.io/aquasec`, then `ghcr.io` |

The mirror serves only images it has cached: popular official images are, but an image or tag nobody has pulled through it is "not found". Before changing a mirror reference, check that it resolves and matches Docker Hub, for example `docker buildx imagetools inspect mirror.gcr.io/library/rust:1.99.0-bookworm` and the same for `rust:1.99.0-bookworm`. Dependabot's `docker` updates still track the `Dockerfile`'s mirrored build stages (it queries `mirror.gcr.io`, whose tag list holds only cached tags, so a new version can appear there later than on Docker Hub; bump by hand if it lags). The mirror references in workflows, tests and the staging overrides are not tracked by Dependabot and are updated by hand. Local developer stacks (`compose.yaml`, `deploy/demo/compose.yaml`) are not used by CI and still pull from Docker Hub.

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

Real enterprise IdP interoperability, live upstream/model permissions, workload credential refresh, production load/security testing, public-host ingress/certificates, provider invoices and customer billing remain unverified or unimplemented. Distributed quotas/budget reservations now have database/concurrency coverage, but have not been production-load validated. Native Responses/Messages content is bounded-buffered rather than token-by-token; see [protocol matrix](protocol-matrix.md). These milestones do not make the service ready for public launch.

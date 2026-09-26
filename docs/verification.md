# Verification

## Current pill tabs and US-dollar inputs

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

## Remaining acceptance work

Real enterprise IdP interoperability, live upstream/model permissions, workload credential refresh, production load/security testing, ingress/TLS, provider invoices and customer billing remain unverified or unimplemented. Distributed quotas/budget reservations now have database/concurrency coverage, but have not been production-load validated. Native Responses/Messages content is bounded-buffered rather than token-by-token; see [protocol matrix](protocol-matrix.md). These milestones do not make the service ready for public launch.

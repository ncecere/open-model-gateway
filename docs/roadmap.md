# Implementation roadmap

Implemented here means code plus local automated coverage—not proof of production readiness or live cloud/SSO certification. The initial foundation and provider-independent inference engine are in place.

## 1. Identity, workspaces, and access — implemented

- [x] OIDC authorization-code login with PKCE/state/nonce, explicit account linking, verified email.
- [x] Hashed twelve-hour browser sessions, logout, exact-Origin and CSRF enforcement.
- [x] Separate platform operator, organization owner/admin/member, and workspace roles.
- [x] Private personal spaces, sibling shared team/project workspaces, workspace switching.
- [x] Email-bound, expiring, single-use invitations; membership/owner protections.
- [x] Team/project service accounts, independent of an employee's key lifecycle.
- [x] Trusted `provision-user` CLI for initial operator/linking approval; no public dev login.

Operational follow-up: live IdP acceptance, automatic JWKS refresh, session/attempt cleanup, login abuse limits, upstream single logout and revoke-all-device UX. See [identity](identity.md).

## 2. Management API and dashboard — implemented

- [x] Session-only Rust API under `/api/v1`; inference keys cannot administer resources.
- [x] Functional React/Vite dashboard using real management endpoints.
- [x] Reference-only provider credentials; operator-only connection configuration.
- [x] Platform-owned catalog/providers/deployments/routing/pricing, explicit organization entitlements, and delegated workspace/individual-personal model grants.
- [x] Platform global configuration without an organization context; org admins consume assigned models rather than managing infrastructure.
- [x] Key creation, one-time disclosure, bounded expiry, atomic rotation, revocation.
- [x] Service-account lifecycle; disabling a member/account permanently revokes affected keys.
- [x] Execution history, thirty-day known-token totals and explicit unknown-usage counts.
- [x] Transactional, sanitized mutation audit records; personal audit privacy.
- [x] Real PostgreSQL isolation and concurrent lifecycle tests.

Usage and configured-rate cost accounting are implemented, but neither is provider-invoice/customer billing. Lists are bounded/paginated; global `/me` workspace inventory is not yet paginated. See [management API](management-api.md).

## 3. Native protocols and provider breadth — implemented bounded subset

- [x] OpenAI Chat Completions text/function-tool JSON and incremental SSE.
- [x] Native OpenAI Responses transport and frontend: stateless text/function tools.
- [x] Native Anthropic Messages transport and frontend: text/tool blocks.
- [x] AWS Bedrock Converse/ConverseStream through official SDK/workload identity.
- [x] Explicit protocol capability selection, no silent unsupported fallback.
- [x] Shared/local provider contracts; native event ordering, bounded frames, cancellation.
- [x] Real OpenAI/Anthropic client SDK JSON and streaming-helper contract tests.
- [x] Published supported/unsupported matrix and Bedrock setup.

Deliberate limits: native Responses/Messages frontend content is currently buffered (at most 4 MiB), not delivered token-by-token; upstreams are parsed incrementally. No multimodal content, hosted tools, reasoning, persisted Responses state, or full vendor-option passthrough. Live provider/model/IAM validation remains an operator acceptance step. Additional providers (Azure, Vertex, OpenAI-compatible), finer model capabilities, expanded canonical content, and native extensions are future increments. See [protocol matrix](protocol-matrix.md).

## 4. Governance, accounting, and routing — implemented bounded scope

- [x] PostgreSQL-backed org/workspace/key attempt, token and leased concurrency limits across replicas.
- [x] Separately-owned platform organization ceilings; optional child restrictions share parent allowance and cannot remove or exceed effective parent limits.
- [x] UTC monthly USD budgets with serialized reservations; unknown usage retains holds.
- [x] Immutable deployment price versions and append-only integer-micro cost ledger.
- [x] Pinned-rate settlement, crash/expiry reconciliation worker, evidence-backed manual usage resolution.
- [x] Scoped known/held/unknown cost views and bounded CSV export.
- [x] Optional settled-execution detail compaction without deleting financial history.
- [x] Priority/weighted routing, observed failure thresholds/cooldowns, explicit failover policies.
- [x] Required-residency labels for all selection; same-label fallback only, operator-controlled labels.
- [x] Gateway-owned stream deadline watchdog, including unpolled streams.
- [x] Governance/pricing/routing/cost management screens with copied Bitop UI primitives and a RAG-inspired shell.
- [ ] Per-key model allowlists, egress enforcement and secret-manager integrations.
- [ ] Provider invoices, billing adjustments/credits, customer billing and financial archival.
- [ ] Exclusive half-open probes, active health checks and OpenTelemetry metrics/traces.

Limits count attempts, including opt-in fallbacks. Reservations require accurate configured HARD provider input ceilings and explicit output limits; prices are estimates, not invoice guarantees. Unknown unpriced history blocks newly enabled budgets rather than being erased. Cooldowns reopen by elapsed time, not a single-probe gate. Labels are operator assertions, not verified geography. See [governance](governance.md) and [routing](routing.md).

## 5. Production operations — next

- [ ] Production images, TLS ingress, least-privilege runtime database roles.
- [ ] Independent security review, load tests, RLS assessment and abuse controls.
- [ ] Backups/restores, on-call dashboards, alerts, migration/runbook exercises.
- [ ] Live IdP, credential refresh, upstream provider and IAM acceptance.
- [ ] Tenant onboarding policies, quotas and financial billing if required.

Production serves the built SPA from Rust using `GATEWAY_WEB_DIR`; Node is unnecessary at runtime. Do not launch publicly until isolation, credential protection, distributed limits, spending controls, and operations are verified. Marketplace/provider settlement remains out of initial scope.

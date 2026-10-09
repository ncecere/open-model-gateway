# Application roadmap — for review

## Approved enterprise rebuild

The approved next direction is a **single-enterprise installation with sibling Teams, Projects and private personal workspaces**, multiple scoped catalogs, OIDC platform entitlement/group mappings, and optional cost-center allocation. The first rebuild also recreates richer cost reporting and cache-aware accounting and adds embeddings and local provider profiles. See [the confirmed decisions and eight-milestone plan](enterprise-rebuild.md).

**Status (2026-10-09):** the rebuild is implemented and released as v0.1.0, with later rounds adding logs, Admin settings, Bedrock access modes, alerts, key safety, model compare, JWKS refresh and SCIM. The implemented inventory is in [Implemented baseline](#implemented-baseline-and-existing-limitations) below; legacy organization wording in the R-items describes the pre-rebuild review.

- **Implemented workloads:** Chat Completions, Responses, Messages, embeddings, image generation (base64), audio transcription and speech, realtime audio sessions (OpenAI GA WebSocket, [realtime](realtime.md)), Chat Completions batches (OpenAI, [async jobs](async-jobs.md)), rerank and System One, across OpenAI, Anthropic, AWS Bedrock, OpenRouter and approved local profiles (OpenAI-compatible, vLLM, SGLang, Ollama). The async video job infrastructure exists but has **no supported provider**: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24, so `/v1/videos` returns `unsupported_capability`.
- **File store (infrastructure only):** encrypted object storage (local disk or S3-compatible: AWS S3, MinIO, RustFS, R2; [file storage](file-storage.md)) with per-purpose retention, a sweeper, `files verify`/`files sweep` and Admin › Settings › Storage. No feature writes files yet; gateway-run batches, the gateway Files API, CSV exports, the uploaded logo and video outputs are planned consumers.
- **Planned, not implemented:** an OpenRouter video adapter (video has no supported provider since OpenAI's Videos API shutdown on 2026-09-24), video remix/edits/extensions and reference inputs, batches for endpoints other than `/v1/chat/completions`, realtime beyond the bounded OpenAI WebSocket subset (WebRTC/SIP, ephemeral client secrets, input transcription, automatic VAD responses), image edits and vision input, token-by-token Responses/Messages streaming, active health probes, key-expiry/hold-age notifications and webhooks, provider-invoice reconciliation.
- **Quality gates:** CI runs Rust/PostgreSQL integration tests, Vitest and a Playwright + axe browser suite ([verification](verification.md#browser-suite), [accessibility](accessibility.md)). Production/security acceptance (live IdP and providers, load, off-host recovery, independent review) remains open.

## Historical proposal and restored baseline

**Decision status: R07's first release is complete. The Grounded-style dashboard/navigation refresh was separately approved; the remaining full proposals below are not selected.** Choose remaining items by ID (for example, `R06, R11, R12`) or check the boxes below. Priorities are recommendations, not a delivery commitment. Previously requested ideas are included so they can be reviewed alongside new suggestions.

This is a source-level product and engineering review of the Rust gateway, React dashboard, documented contracts, tests and deployment tooling. It is not a new penetration test, live-provider certification or comprehensive browser audit. The original review baseline was staging milestone `dff83d7`. The user subsequently approved completing per-key restrictions (R07); that first release is now verified. The subsequent authorized dashboard refresh adds contextual settings, resource routes, explicit session membership/capabilities, direct catalog detail reads, and server-side catalog search. It does not authorize new roles, changes to inherited access, paid inference, or implementation of every proposal below.

## Assessment

The application already has the difficult foundation: tenant isolation, private personal workspaces, separate platform/organization/workspace authority, real provider adapters, distributed spending controls, immutable estimated-cost accounting, and a usable administration dashboard. Rebuilding those pieces would add little value.

The highest-value next work is making that foundation **easier to operate, easier to integrate with, and safe at larger scale**. In particular:

- A locally tested staging image is not yet a real-SSO/live-provider pilot or a disaster-recovery plan.
- Tables paginate, but session inventory and selection controls still load broad resource lists.
- Users can configure models and keys, but there is no integrated developer quickstart or capability-oriented model catalog.
- Costs are honestly labelled estimates, but proactive notifications and richer reporting would make governance more useful before a request is blocked.
- Native Responses/Messages support and passive routing health work within documented limits; incremental native delivery and controlled recovery are substantive improvements, not cosmetic changes.

## Suggested starting points

Choose based on the immediate goal rather than approving everything:

| Goal | Suggested selections | Why |
| --- | --- | --- |
| Run a controlled real pilot | R01, R02, R03, R04, R05 | Validate the actual environment and make failures, recovery and regressions manageable. These are not sufficient by themselves to certify a public launch. |
| Improve everyday usability | R11, R12, R13 | Help people make their first request, understand spending and act before limits block work. |
| Support larger organizations | R06, then R14/R18 if needed; R07 is complete | Avoid oversized inventories, narrow credential access and reduce manual access administration. |
| Support self-hosted models | R09, R10, then R08 | Add the requested servers with explicit compatibility, safe origins and controlled recovery. |

**My remaining product-focused shortlist:** R06 (paginated inventory), R11 (developer quickstart), and R12 (notifications). R07 (key restrictions) is complete. If real users are about to depend on the gateway, prioritize the pilot/operations items first.

Priority guide: **P0** = confidence needed before dependable real-world use; **P1** = strong next-product value; **P2** = demand-dependent expansion. These are not vulnerability severity ratings.

Effort is relative: **S** = narrow change; **M** = several components plus tests; **L** = cross-layer design/migration/protocol work. Ranges reflect unresolved scope, not calendar estimates. “Done” below means a proposed acceptance criterion, not a result already achieved.

## A. Pilot and operational confidence

### R01 — Real OIDC and first-provider pilot · P0 · S–M

- [ ] Select R01
- **Value:** establish that the actual identity provider, TLS origin, credentials and provider account work—not only fixtures.
- **Scope / done:** execute the existing [live acceptance checklist](live-acceptance.md), including role/privacy denials, one explicitly approved bounded provider request, cancellation, revocation and accounting checks; record sanitized evidence.
- **Depends on:** issuer/client details, approved initial administrator, protected secret delivery, target hostname and explicit approval for any paid requests. Do not build a second demo or automatically enable a provider.

### R02 — Identity lifecycle and login hardening · P0 · M

- [ ] Select R02
- **Value:** avoid signing-key rotation outages and make lost sessions and authentication abuse manageable.
- **Progress (2026-10-09):** bounded JWKS refresh is implemented (Cache-Control TTL clamped 5 min to 24 h, rate-limited single-flight refetch on unknown key IDs, 6-hour outage grace, asymmetric algorithms only); see [identity](identity.md#signing-keys-jwks). The rest of R02 remains open.
- **Scope / done:** bounded JWKS refresh on rotation/unknown key IDs; login/callback abuse controls; expired session/attempt cleanup; view and revoke the caller's sessions. Test key rollover without weakening issuer/audience/nonce checks, concurrent revocation, stale cookies and cleanup boundaries.
- **Decision:** choose session lifetime/idle policy and whether IdP logout integration is necessary. Ordinary org administration must not become global identity administration.

### R03 — Operational telemetry and safe request troubleshooting · P0 · M–L

- [ ] Select R03
- **Value:** answer “why did this fail?” without inspecting customer prompts or guessing from aggregate totals.
- **Scope / done:** metrics/traces for first-content and completion latency, errors, admission denials, pool saturation, reconciliation lag, concurrency, fallback attempts and unresolved holds; correlate sanitized request/root-attempt IDs with a permission-scoped detail view. Provide dashboards and actionable alerts with bounded metric cardinality.
- **Useful first slice:** distinguish temporary rate/concurrency denial from exhausted budgets and unresolved-accounting blockers; provide retry guidance only when meaningful. These currently share a generic rate-limit error. Add an authorized aged-hold review queue without promising automatic refunds.
- **Guardrails:** no bodies, credentials or private-workspace metadata in global telemetry. Distinguish gateway pressure, provider failure and unknown observations. Existing execution history and request IDs should be extended, not replaced.

### R04 — Recovery and release safety · P0 · M–L

- [ ] Select R04
- **Value:** make the service recoverable beyond the existing same-host restore rehearsal.
- **Scope / done:** encrypted off-host backups, retention/integrity checks and a clean-host restore/cutover exercise with measured recovery objectives. Add image dependency scanning/SBOM and digest-based promotion; rehearse schema-compatible rollback or roll-forward rather than assuming an old binary can undo a migration.
- **Depends on:** hosting, backup destination, key custody and agreed RPO/RTO. Separately load-test multiple replicas, hot tenants, slow stream consumers, database interruptions and shutdown/reconnect storms; document supported capacity before tuning connection pools or queues. Obtain an independent security review before public launch.

### R05 — Repeatable browser and accessibility regression gates · P0/P1 · M

- [ ] Select R05
- **Value:** protect the role-sensitive UI and one-time-secret flows as features expand.
- **Scope / done:** automate real browser journeys for all four personas against an isolated fixture: login/logout, scope changes, deep links, key lifecycle, delegated ceilings, search focus, keyboard interaction and mobile layouts. Exercise authorization with direct negative API tests as well as hidden controls.
- **Guardrails:** synthetic identities only; redact screenshots; no paid upstreams. Preserve pill tabs, USD decimal inputs and context-change secret teardown. Sampled accessibility checks are not certification.
- **Progress (2026-10-09):** `npm run test:browser` (CI job `browser`) runs a Playwright journey for all five personas against the real gateway, a disposable database, the local signed-OIDC issuer and a mock upstream: team/project creation, SSO mappings, priced mock model, key issue/inference/Logs/Usage/revoke, auditor read-only and API denials, 390px key pages. axe scans (WCAG 2.2 AA + best practice; serious/critical fail) cover the main pages per persona at 1440/390 plus dialogs, skip link, focus trap and reduced motion ([accessibility](accessibility.md)). Deep links, delegated ceilings and rotation are not yet in the suite.

## B. Scale and inference capabilities

### R06 — Paginated workspace inventory and server-side search · P1 · L

- [ ] Select the remaining R06 scope
- **Partial delivery through the dashboard refresh:** additive session action capabilities and actual membership, direct platform catalog detail reads, and server-side catalog search/filtering. `/me` inventory and most selectors are still eager; cursor-based inventory/search is not complete.
- **Value:** keep sign-in, context switching, model delegation and search responsive with thousands of resources.
- **Scope / done:** compact session capabilities, caller-scoped cursor search, direct selected-resource resolution, and on-demand selector pages. Replace eager `useChoices` loading where large directories feed selectors; do not fetch all pages in the background.
- **Acceptance:** administration eligibility remains correct when the only managed workspace is off-page; deep links work beyond page one; revocation clears stale scope data; personal resources remain owner-private. Test bounded request counts and cancelled searches with large fixtures.

### R07 — Per-key model restrictions · P1 · M

- [x] R07 selected and completed — issuance-time restrictions
- **Value:** give an application credential only the models it needs, without changing the whole workspace's access.
- **Status:** first release complete: backend, migration/runtime ACLs, dashboard controls, regression tests and isolated browser acceptance. The failing assignment fixture and legacy migration test have been corrected; see [verification](verification.md).
- **Scope / done:** inherit / explicit deny-all / selected-model modes; enforce the same intersection in listing, routing and final admission. Rotation must retain restrictions and budget lineage; entitlement removal/reassignment must not resurrect removed selections. Cover shared/service/personal keys and revocation races.
- **Delivered scope:** restrictions are fixed at issuance and preserved by rotation. Post-creation editing is **not included** and would need a separate authority decision. Do not assume an administrator may modify another person's credential merely because they may revoke it. Per-key restrictions are not a substitute for parent entitlements or aggregate budgets.

### R08 — Truly incremental Responses and Messages streaming · P1 · L

- [ ] Select R08
- **Value:** reduce perceived latency and make streaming clients behave as expected on long responses.
- **Scope / done:** replace bounded frontend buffering with incremental text/tool events while preserving native ordering, tool-call identity, usage and terminal semantics. Test fragmented UTF-8, interleaved tools, cancellation, backpressure, incomplete results and malformed EOF using real SDK helpers.
- **Guardrails:** no fabricated completion, post-stream retry, unbounded buffering or refund based on missing usage. Existing incremental Chat streaming is already implemented.

### R09 — OpenAI-compatible, vLLM and SGLang providers · P1 · L, staged

- [ ] Select R09
- **Value:** support the additional hosted and self-hosted backends already requested.
- **Slices:** (a) explicitly configured OpenAI-compatible Chat/Responses transport; (b) vLLM profile and fixtures; (c) SGLang profile and fixtures. Each needs its own tested subset and documented server/version/configuration requirements.
- **Acceptance:** server-controlled exact-origin allowlisting, redirect/credential-leak protection, explicit protocol capabilities, bounded parsing and normal accounting/cancellation. Unknown or unsupported capabilities fail clearly; “OpenAI-compatible” must not imply universal Responses/tool support.
- **Decision:** HTTPS-only versus explicitly approved private HTTP origins, supported server versions, and required authentication. Use adapters/registry, not provider switches in the engine.
- **Research caveat:** inspected upstream main snapshots contain server-specific response metadata and terminal-event differences that need explicit fixtures. Minimum compatible released versions have not been established; merely changing the current OpenAI adapter's URL is not enough.

### R10 — Active health checks and controlled recovery probes · P1 · M–L

- [ ] Select R10
- **Value:** avoid sending a burst of traffic to a deployment as soon as its cooldown expires.
- **Scope / done:** distributed, leased half-open probe ownership; stale/unknown health presentation; bounded active checks with backoff; operator-visible drain/maintenance state if needed. Concurrent replicas must not all become the recovery probe.
- **Guardrails:** a successful health endpoint is not proof that model inference or IAM works. Some checks themselves generate: the inspected SGLang Python server's `/health` can generate a token by default, whereas `/ready` does not. Qualify paths per server/version. Any inference probe needs explicit cost/accounting policy; do not generate unapproved background inference. Preserve residency and no-post-stream-failover rules.

## C. User and administrator experience

### R11 — Developer quickstart and capability-oriented model catalog · P1 · S–M

- [ ] Select the remaining R11 scope
- **Partial delivery through the dashboard refresh:** role-aware setup links, connected catalog configuration, and explicitly non-inference record-existence checks. This is not a capability catalog, SDK quickstart, or comprehensive configuration preflight.
- **Value:** shorten the path from receiving access to making a correct first request.
- **Scope / done:** a permission-scoped “Use this model” view showing alias, supported protocol/subset, relevant limits and configured USD prices where authorized; copyable curl/Python/TypeScript examples with placeholders; an API contract/reference and onboarding checklist linking existing setup screens.
- **Operator companion:** a no-inference configuration preflight for missing credential references, pricing/bounds, entitlements, unsupported capabilities and routing exclusions. Keep standard vendor-compatible model listing stable; expose richer metadata through an authorized extension.
- **Guardrails:** no secret retrieval, automatic paid tests or invented capabilities. Keep creation on dedicated pages—not inside the context switcher. A full chat playground is a separate decision.

### R12 — Budget, key-expiry and operational notifications · P1 · M

- [ ] Select R12
- **Value:** warn users before spending limits or expired credentials interrupt work.
- **Scope / done:** configurable estimated-budget thresholds, upcoming key expiry and unresolved-hold age notifications; choose an initial channel (in-app, email or signed webhook). Use durable delivery, deduplication, retries, recipient authorization and delivery status.
- **Guardrails:** separate settled estimates, holds and unpriced unknowns; thresholds must not replace enforcement. Prevent alert storms and disclosure of personal activity to org-wide recipients. Webhook destinations need egress controls.
- **Status:** budget-threshold, spend-spike, error-rate and failing-connection alerts are implemented ([alerts](alerts.md), 2026-10-08): in-app notifications plus email through the SMTP relay, idempotent incidents and recorded delivery outcomes. Key-expiry and hold-age notifications, webhooks and email retries are not implemented.

### R13 — Cost trends, filters and internal allocation reports · P1 · M

- [ ] Select R13
- **Value:** answer which authorized workloads are driving spend and how the current period compares with the previous one.
- **Scope / done:** explicit date ranges, model/workspace/service-account breakdowns, trend charts, budget headroom and consistent exports. Optional cost-center tags should not become authorization inputs. Label any forecast as an estimate and surface incomplete usage.
- **Guardrails:** current UTC-month costs and bounded CSV already exist. Extend them without inventing zero-cost rows, changing pinned prices or exposing private workspace details. Decide aggregate personal-use reporting separately before implementing it.

### R14 — Self-service access requests and onboarding templates · P2 · M–L

- [ ] Select R14
- **Value:** replace “ask an admin” and manual UUID handoffs with an accountable approval process.
- **Scope / done:** request access to an eligible shared workspace or model; show the request's status and reason; authorized approvers accept/reject with audit evidence. Optional reviewed templates can apply initial shared-workspace grants and limits.
- **Guardrails:** approval must recheck current entitlements, membership and authority atomically. Never list private workspaces, auto-promote approvers or let a template override a platform ceiling. Decide whether invitation email delivery belongs in this slice.

### R15 — Safer administrative changes and impact previews · P2 · M–L

- [ ] Select R15
- **Value:** reduce surprises when revoking assignments, changing limits or moving traffic.
- **Scope / done:** permission-scoped impact previews, readable configuration diffs/history and guarded bulk actions with explicit targets and partial-failure reporting. Prefer dry-run previews before expanding bulk mutation surface.
- **Guardrails:** previews must not reveal private resources. Recheck at commit time; a preview is not authorization. Rollback publishes a newly authorized configuration—it never edits old ledger entries, rewrites price versions or revives revoked credentials.

### R16 — Management automation and declarative configuration · P2 · L

- [ ] Select R16
- **Value:** make repeatable deployment/model/entitlement setup possible without scripting browser sessions.
- **Scope / done:** versioned management contract, scoped machine-management credentials, idempotent operations and dry-run configuration diffs. Start with a narrow operator CLI/import-export path before committing to a full Terraform provider.
- **Depends on:** an explicit machine-authorization/audit model. Inference keys must never gain management access; exported configuration must contain references, not secrets or private activity.

## D. Enterprise and demand-driven extensions

### R17 — Managed secrets and credential-refresh acceptance · P2 · M–L

- [ ] Select R17
- **Value:** move beyond host-managed environment/file secrets when operations require centralized rotation.
- **Scope / done:** add one selected secret-manager backend with constrained reference namespaces, safe caching/refresh and rotation tests; validate actual AWS workload-credential renewal. Revoked or unavailable credentials fail safely without exposing their values.
- **Decision:** choose the hosting platform and secret manager first. Existing validated environment references/file mounts remain a supported simple deployment path.

### R18 — Enterprise provisioning and access reviews · P2 · L

- [ ] Select R18
- **Value:** reduce manual membership maintenance and identify stale access as organizations grow.
- **Progress (2026-10-09):** SCIM 2.0 provisioning (Okta/Entra subset) is implemented: deactivation suspends and revokes credentials, pushed groups feed the existing group mappings with group provenance, and Admin › Settings › Sign-in shows read-only status ([SCIM](scim.md), migration 0014). Dry-run diffs and access reviews remain open; real Okta/Entra acceptance is pending.
- **Scope / done:** choose explicit IdP-group mapping or SCIM, with a dry-run membership diff, deprovisioning rules and periodic review of shared memberships/service credentials. Surface key expiry/last-use metadata only to authorized viewers.
- **Guardrails:** external claims must not automatically grant platform administration; removing one organization's membership must not disable an unrelated organization's user. Preserve service-account independence, last-owner protections and personal privacy.

### R19 — Provider-invoice reconciliation · Decision first · M–L

- [ ] Select R19
- **Value:** explain differences between configured-rate estimates and what upstream vendors actually charge.
- **Scope / done:** versioned import for one provider's usage/invoice report, duplicate detection, evidence references and matched/unmatched reconciliation views. Explicitly handle additional usage categories, discounts and credits rather than forcing them into the current two-rate calculation.
- **Guardrails:** actual vendor charges belong in a separate auditable representation; never overwrite immutable execution estimates or fabricate attribution for unmatched totals. Resolve reporting/privacy and currency requirements first. This is **not customer invoicing**.

### R20 — Additional inference capabilities, selected by use case · P2 · L per slice

- [ ] Select R20 and specify a slice
- **Options:** structured JSON output for application integrations; embeddings for retrieval workloads; image input for vision use cases. Rerank (`/v1/rerank`) and TypeSafe System One (`/v1/systemone`) are implemented through the OpenRouter adapter on the shared non-generation workload path. Base64-only image generation (`/v1/images/generations`) is implemented for OpenAI `gpt-image-*` (one capped live check) and OpenRouter `/images`. OpenRouter images are mock-tested only, because live calls return 402 without account credit. Speech is implemented on the same path. Realtime audio (`GET /v1/realtime`, OpenAI GA WebSocket) is a separate session path: one attempt and reservation per session, budget windows sized from the session's actual context (previous response usage plus new client input, capped at the context window), reserved per `response.create` and settled per `response.done` with realtime-only audio-token meters. It is mock-tested, plus one capped live check against `gpt-realtime-mini`; see [realtime](realtime.md). `/v1/audio/transcriptions` (multipart, server-measured WAV/MP3/Ogg/FLAC duration) uses OpenAI and OpenRouter; OpenRouter is mock-tested only, because live calls return 402. `/v1/audio/speech` (streamed binary, exact character metering) uses OpenAI and OpenRouter. One capped live check covered `whisper-1`, `tts-1` and `mai-voice-2-flash`. Async jobs ([async jobs](async-jobs.md)) cover Chat Completions batches (`/v1/files` + `/v1/batches`) and the video job path (`/v1/videos`). Video has no supported provider: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24, so `/v1/videos` returns `unsupported_capability` and an OpenRouter video adapter is planned. Each job is one attempt and reservation, polled in the background (`GATEWAY_JOB_POLL_INTERVAL_SECONDS`) and settled from actual video seconds or batch usage. The batch input file is validated while it streams through and is never stored. These are mock-tested only; no live or paid video/batch call was made. Other batch endpoints, video remix/edits/references and the OpenRouter video adapter remain unimplemented.
- **Acceptance:** explicit typed protocol and adapter support, capability metadata, payload/output bounds, usage/pricing semantics and SDK fixtures for the chosen slice. Preserve unsupported-feature errors; do not silently drop options or label every provider compatible.
- **Depends on:** a named consuming application, required models and realistic acceptance examples. Avoid expanding all protocols simultaneously.

## Decisions to make before selecting larger work

1. **Deployment target:** private pilot or public service? Single host or managed cloud? Expected tenants, peak concurrency and recovery objectives determine operational scope.
2. **Financial purpose:** internal visibility/allocation (R13), provider-invoice matching (R19), or external customer billing? Customer invoices/payment collection are a separate **XL** product requiring billing accounts, credits/refunds, tax/currency policy, payment integration and reconciliation. Do not turn current estimates directly into invoices.
3. **Privacy and retention:** what aggregate reporting is permitted for personal consumption, who receives notifications, and what must be retained? Keep existing owner-private boundaries unless an explicit reviewed policy says otherwise.
4. **Automation trust:** who may approve access, edit key restrictions and issue machine-management credentials? These are different powers, not synonyms for “admin.”
5. **Consumer demand:** which provider/server versions and new API features are needed by real applications? That decides R09/R20 sequencing.

## Defer unless a concrete need emerges

- Cross-tenant response/semantic caching: isolation, consent, invalidation and billing semantics need their own design.
- Persisted chat/playground histories, prompt capture, DLP or moderation pipelines: new sensitive-data processing and retention obligations; not a default gateway feature.
- Automatic “cheapest/best model” routing, shadow traffic or broad retry policies: quality evaluation, capability/residency checks and duplicate-spend controls must come first.
- Marketplace settlement, Kubernetes/multi-region active-active, or a database-RLS rewrite merely for completeness: justify them with deployment requirements and measured risks.
- General-purpose hard deletion or personal-workspace impersonation: conflicts with immutable accounting and current privacy guarantees. A future shared-workspace archival/ownership-transfer workflow should be separately scoped.

## Review evidence and boundaries

The recommendations extend the existing implementation rather than assuming these features are absent:

| Area reviewed | Evidence | Roadmap implications |
| --- | --- | --- |
| Session inventory, scope and search | [`management/mod.rs`](../apps/gateway/src/management/mod.rs), [`ui.tsx`](../apps/web/src/components/ui.tsx), [`search.ts`](../apps/web/src/lib/search.ts) | `/me` returns full accessible inventories; selectors eagerly collect pages with a safety cap; jump search is local. R06 is a coordinated backend/frontend change. |
| Catalog and access administration | [`catalog.tsx`](../apps/web/src/pages/catalog.tsx), [`model-access.tsx`](../apps/web/src/pages/model-access.tsx), [`workspace.tsx`](../apps/web/src/pages/workspace.tsx) | CRUD and delegation already exist. R11/R14/R15 improve discovery, onboarding and change safety rather than rebuilding CRUD. |
| Usage and costs | [`overview.tsx`](../apps/web/src/pages/overview.tsx), [`governance.tsx`](../apps/web/src/pages/governance.tsx), [governance contract](governance.md) | Existing totals, unknown states, immutable estimates and bounded exports are the base for R03/R12/R13/R19. |
| Protocols and routing | [protocol matrix](protocol-matrix.md), [routing contract](routing.md), [`providers/contract.rs`](../apps/gateway/src/providers/contract.rs) | Restricted native support, buffering and passive cooldowns are documented limits, not evidence of full vendor compatibility. |
| Identity, denials and capacity | [`identity.rs`](../apps/gateway/src/identity.rs), [`inference/error.rs`](../apps/gateway/src/inference/error.rs), [`governance.rs`](../apps/gateway/src/governance.rs), [`config.rs`](../apps/gateway/src/config.rs) | Startup identity discovery, generic `Busy` denials and a fixed connection-pool setting motivate lifecycle, diagnostics and measured capacity work—not a claim of a proven security defect. |
| Requested server compatibility | [vLLM source snapshot](https://github.com/vllm-project/vllm/tree/77871126f9b69cff9feffff390d1171a60dbd7e2/vllm/entrypoints/openai), [SGLang health/readiness implementation](https://github.com/sgl-project/sglang/blob/fc9e1c8d296216ff1e216dfbe7286ef392448d28/python/sglang/srt/entrypoints/http_server.py#L692-L778), [generation-check default](https://github.com/sgl-project/sglang/blob/fc9e1c8d296216ff1e216dfbe7286ef392448d28/python/sglang/srt/environ.py#L347) | Research used official main snapshots, not release certification. R09/R10 need version-specific protocol and probe qualification. |
| Operations and acceptance | [staging](staging.md), [live acceptance](live-acceptance.md), [verification](verification.md) | Packaging/local rehearsal exists; real integration, off-host recovery and production-load acceptance remain separate. |

No unselected recommendation authorizes paid traffic, production changes, new data capture or automatic implementation.

---

## Implemented baseline and existing limitations

Implemented here means code plus local automated coverage—not proof of production readiness or live cloud/SSO certification. The following inventory preserves the existing milestones and includes the completed R07 first release.

## 1. Identity, workspaces, and access — implemented

- [x] OIDC authorization-code login with PKCE/state/nonce, explicit account linking, verified email.
- [x] Hashed twelve-hour browser sessions, logout, exact-Origin and CSRF enforcement.
- [x] Platform Admin/Auditor/User entitlements, separate from SSO, and Team/Project owner/admin/member grants with manual and group provenance.
- [x] Private personal spaces, sibling shared team/project workspaces, workspace switching.
- [x] Email-bound, expiring, single-use invitations; membership/owner protections.
- [x] Team/project service accounts, independent of an employee's key lifecycle.
- [x] Trusted `provision-user` CLI for initial operator/linking approval; no public dev login.
- [x] Issuer JWKS cache with bounded TTL, rate-limited single-flight refresh on unknown key IDs and bounded outage grace; `none`/HMAC ID tokens rejected (2026-10-09).
- [x] SCIM 2.0 provisioning (`/scim/v2`, `GATEWAY_SCIM_TOKEN_ENV`): users, groups, deactivation/reactivation without key resurrection, group-provenance grants; read-only status in Admin › Settings › Sign-in ([SCIM](scim.md), 2026-10-09).

Operational follow-up: live IdP and SCIM acceptance (Okta, Entra), session/attempt cleanup, login abuse limits, upstream single logout and revoke-all-device UX. See [identity](identity.md).

## 2. Management API and dashboard — implemented

- [x] Session-only Rust API under `/api/v1`; inference keys cannot administer resources.
- [x] Functional React/Vite dashboard using real management endpoints.
- [x] Reference-only provider credentials; operator-only connection configuration.
- [x] Platform-owned connections/models/routes/pricing, multiple catalogs with live workspace-type defaults and per-workspace replacements, workspace selections and direct assignments.
- [x] Workspace admins consume available models rather than managing infrastructure.
- [x] Key creation, one-time disclosure, bounded expiry, atomic rotation, revocation.
- [x] Service-account lifecycle; disabling a member/account permanently revokes affected keys.
- [x] Execution history, thirty-day known-token totals and explicit unknown-usage counts.
- [x] Logs (2026-10-08): per-attempt telemetry (finish reason, time to first token, generation time, reasoning tokens, upstream model snapshot) and optional client session/app labels; request, generation and session views with summary metrics in each workspace and on Admin (Team/Project only, never personal rows). Provider-reported served model (migration 0013, alongside the configured snapshot) and reasoning tokens from OpenAI/OpenRouter/compatible usage details and Anthropic `output_tokens_details.thinking_tokens` (2026-10-09); Bedrock reports the served model only through prompt-router traces and no reasoning breakdown, so those stay unknown. See [provider adapters](provider-adapters.md#logs-telemetry-served-model-and-reasoning-tokens).
- [x] Backend APIs for the UX program (2026-10-08): own-scope Home summary and keys, request logs with attempt timelines, key statistics and reversible key disablement, usage overview/explore analytics, effective-access layers with per-model reasons, member picker and catalog filters. Browser UI for them is in progress.
- [x] Key safety audit and model compare (2026-10-08): read-only findings per active key (no or overlong expiry, no effective budget or cap, holder left, unused or never used, all-models access, old secret) for workspaces and for Admin (Team/Project only, personal keys as counts), with fixes that reuse the existing key actions ([key safety](key-safety.md)). Side-by-side comparison of 2–4 models: exact price lines, ceilings, serving state and 30-day observed metrics scoped like Logs ([management API](management-api.md#model-compare)). The audit raises no notifications; alerts are separate.
- [x] Alerts (2026-10-08): installation and Team/Project rules for stacked-budget thresholds, spend spikes, error rates and failing connections, plus owner-only built-in personal budget alerts; a bounded, idempotent background evaluator (`GATEWAY_ALERT_INTERVAL_SECONDS`, `alerts evaluate --once`); in-app notifications with a top-bar bell and per-user read state; email via the SMTP relay with recorded outcomes ([alerts](alerts.md)).
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

Deliberate limits: native Responses/Messages frontend content is currently buffered (at most 4 MiB), not delivered token-by-token; upstreams are parsed incrementally. No multimodal chat content (vision), hosted tools, persisted Responses state, or full vendor-option passthrough; images, audio, rerank and System One use their own bounded endpoints. Live provider/model/IAM validation remains an operator acceptance step. Local OpenAI-compatible, vLLM, SGLang and Ollama profiles cover Chat and embeddings only (no local Responses/Messages). Finer model capabilities, expanded canonical content, and native extensions remain future increments. See [protocol matrix](protocol-matrix.md).

## 4. Governance, accounting, and routing — implemented bounded scope

- [x] PostgreSQL-backed installation/workspace/key attempt, token and leased concurrency limits across replicas.
- [x] Live workspace-type defaults and platform overrides; tighten-only workspace/key restrictions share parent allowance and cannot remove or exceed effective parent limits.
- [x] Stacked UTC daily/weekly (ISO)/monthly/lifetime USD budgets (one per period at every policy layer, all enforced) with serialized reservations; unknown usage retains holds; budget changes never reset consumption.
- [x] Immutable deployment price versions and append-only integer-micro cost ledger.
- [x] Pinned-rate settlement, crash/expiry reconciliation worker, evidence-backed manual usage resolution.
- [x] Scoped known/held/unknown cost views and bounded CSV export.
- [x] Optional settled-execution detail compaction without deleting financial history.
- [x] Priority/weighted routing, observed failure thresholds/cooldowns, explicit failover policies.
- [x] Required-residency labels for all selection; same-label fallback only, operator-controlled labels.
- [x] Gateway-owned stream deadline watchdog, including unpolled streams.
- [x] Governance/pricing/routing/cost management screens with copied Bitop UI primitives and a RAG-inspired shell.
- [x] Issuance-time per-key model allowlists, explicit deny-all and inherited access; live enforcement and rotation lineage retention.
- [ ] Egress enforcement and secret-manager integrations.
- [ ] Provider invoices, billing adjustments/credits, customer billing and financial archival.
- [ ] Exclusive half-open probes, active health checks and OpenTelemetry metrics/traces.

Limits count attempts, including opt-in fallbacks. Reservations require accurate configured HARD provider input ceilings and explicit output limits; prices are estimates, not invoice guarantees. Unknown unpriced history blocks newly enabled budgets rather than being erased. Cooldowns reopen by elapsed time, not a single-probe gate. Labels are operator assertions, not verified geography. See [governance](governance.md) and [routing](routing.md).

## 5. Production operations — next

- [x] Multi-stage non-root image, HTTPS ingress and separate schema-owner/runtime database roles: single-host staging baseline, not public-launch certification.
- [x] Private generated file secrets, explicit migration/grant jobs, bounded runtime permissions and rollback-only privilege smoke checks.
- [x] Local custom-format backup/disposable restore rehearsal and release/recovery runbooks.
- [x] OIDC/provider configuration overlays and an explicit-opt-in bounded live-provider acceptance helper; no real credentials or paid traffic in automation.
- [ ] Independent security review, load tests, RLS assessment and abuse controls.
- [ ] Off-host encrypted backup/restore and actual recovery-time/cutover exercises.
- [ ] On-call dashboards, OpenTelemetry metrics/traces and alerts.
- [ ] Live IdP, automatic signing-key/credential refresh, upstream provider and IAM acceptance.
- [ ] Tenant onboarding policies, quotas and financial billing if required.

See [staging](staging.md) and [live acceptance](live-acceptance.md). OIDC remains disabled in the default rehearsal until the actual issuer/client information is supplied. Per-key restrictions are complete within the issuance-time scope above. Paginated workspace inventory, native streaming, active health/recovery probes and additional provider adapters remain unselected pending phases. Billing scope (internal allocation versus customer invoices/payment collection) still needs a product decision.

Production serves the built SPA from Rust using `GATEWAY_WEB_DIR`; Node is unnecessary at runtime. Do not launch publicly until isolation, credential protection, distributed limits, spending controls, and operations are verified. Marketplace/provider settlement remains out of initial scope.

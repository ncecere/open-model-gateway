# Single-enterprise gateway rebuild

## Status and scope

This is the user-approved product direction and phased rebuild plan. Milestones 1–4 are now implemented and locally verified in the uncommitted working tree (organization-free schema, catalogs, cache-aware pricing, reporting); see [verification](verification.md). Milestones 5 onward remain planned; the scoped next increment is recorded under "Approved next increment" below.

The gateway is intended to become a broad enterprise inference proxy. The first rebuild covers milestones 1–4 below; images, speech, video and other capability expansion follow in separately tested increments. Broad support does not mean blind provider-option passthrough or universal vendor compatibility.

Development may start with a fresh schema and installation. This does not authorize deleting, migrating, reseeding or otherwise modifying any existing database. Real identity-provider changes are not authorized. Local changes are not automatically committed or pushed.

Paid provider requests: since 2026-10-07 the user authorizes small live tests with keys held in the local, git-ignored `ai.env` (OpenAI, Anthropic, OpenRouter), restricted to the cheap models listed below, tiny inputs and explicit per-run spend caps. Keys are referenced by environment-variable name only and are never printed, logged, committed or sent to the browser. Automated CI remains mock-only.

## Confirmed decisions

### Installation and workspaces

One installation serves one enterprise. The enterprise is the installation identity and administration boundary, not a selectable organization tenant. Users do not switch organizations.

Teams, Projects and personal workspaces exist directly under the installation:

```text
Enterprise installation
├── Users, OIDC, platform roles and settings
├── Providers, models, deployments and pricing
├── Catalogs, policy defaults and financial reporting
└── Workspaces
    ├── Teams
    ├── Projects
    └── Private personal workspaces
```

Teams and Projects have equivalent capabilities and are siblings. A Project can include people from different Teams; it does not belong to a Team. Project access must not be inferred simply from Team membership. Teams and Projects can be created only by Platform Admins. An entitled user's personal workspace is created automatically on their first successful platform sign-in.

Personal workspaces remain owner-private for keys and request details. Platform Admins can see personal-workspace usage and cost totals. Platform Auditors receive the platform-wide read-only reporting access described below. Within shared workspaces, ordinary members see only their own usage and costs; workspace administrators see workspace-wide usage and costs. No default prompt or response-body logging is introduced.

### Shared Grounded UI and interaction patterns

The gateway and Grounded are sibling products built by the same owner and should look and behave consistently. This is a requirement for the **same UI library and shared visual language**, not merely loose inspiration.

Use Grounded's current UI library, layout and composition patterns as the reference: workspace versus Admin areas, workspace switching, navigation/sidebar structure, headers, breadcrumbs, resource lists/details, tabs, forms, dialogs, tables, spacing, typography, colors, and responsive behavior. Gateway-specific content and permissions differ, but equivalent interactions should look and behave the same. Verify the current Grounded source before selecting or refreshing library components; do not infer it from old screenshots.

Keep reference checkouts read-only and do not introduce local cross-repository runtime imports. Reuse the library through its supported distribution/registry workflow, retain its license/provenance, and keep gateway behavior outside shared primitives. UI consistency does not mean copying Grounded's RAG-specific entities, authorization assumptions or sensitive-content features.

### Platform entitlement and identity

SSO authentication and platform entitlement are different. A successful OIDC authentication without platform entitlement must not create a personal workspace or allow platform access.

Platform roles:

- **Platform User:** ordinary platform access and an automatic personal workspace; shared-workspace access still requires membership.
- **Platform Auditor:** Platform User access plus platform-wide read-only configuration, usage/cost and audit access. This does not grant another user's private keys or request details.
- **Platform Admin:** Platform User access plus installation administration, user/workspace administration, catalogs, infrastructure, policies and cost-center assignments. Personal usage/cost totals are available without private key or request-detail access.

Platform Admin and Auditor include Platform User entitlement; a separate User grant is not required. Generic OIDC remains provider-neutral, with configurable group claims and explicit mappings to platform roles and Team/Project memberships and roles. Authentication alone must never infer a platform role.

For the first release, mappings synchronize at sign-in. Group-granted access is removed when its mapping no longer matches, while separately granted manual membership is preserved. Distinct membership sources must therefore be represented rather than overwritten. Sign-in-only synchronization cannot detect group removal for a user who never signs in again. Immediate manual suspension is required; background provisioning/synchronization is a later production capability, not something generic OIDC guarantees.

When synchronization detects loss of all platform-role entitlement:

1. Disable platform access and revoke sessions and user-owned API keys.
2. Retain the account and its access/history information for a 30-day grace period.
3. Leave workspace service-account credentials unaffected by the user's departure.
4. If entitlement returns during the grace period, reactivate the account using current access rules. Previously revoked keys stay revoked; the user must issue fresh credentials.
5. After the grace period, automatically clean up the inactive account without deleting immutable accounting or audit history.

Physical cleanup versus retained identity tombstones, personal-workspace handling after cleanup, and treatment of independently assigned roles must be specified and tested before implementing destructive cleanup. Historical attribution must remain valid; this lifecycle must not cascade-delete financial records. Explicit bootstrap/recovery and last-administrator handling also need implementation safeguards.

### Catalogs and model access

Platform Admins manage multiple catalogs of approved models. Model presence in the platform catalog is not itself permission to use or self-assign that model.

Examples—not hardcoded defaults:

- Local: datacenter-hosted models.
- Approved Cloud: selected cloud models.
- Advanced Cloud: additional restricted cloud models.

Catalog availability has live defaults by workspace type (Personal, Team, Project) and optional per-workspace overrides. An override replaces the type-default catalog list rather than only adding to it. Workspaces without an override follow changes to the defaults automatically.

Personal owners and Team/Project administrators can add models from their workspace's available catalogs. Models outside those catalogs require a direct Platform Admin assignment. Workspace-level restrictions may narrow authorized models further; they cannot bypass platform authorization or key restrictions.

Losing a catalog revokes access to models added through it unless another available catalog or a direct platform assignment still authorizes the model. Authorization provenance must distinguish catalog-based and directly assigned access, and inference must check live access. Already-admitted request handling and revoked-key/model-selection non-resurrection require explicit contract tests rather than claims of retroactive cancellation.

### Policies and budgets

Platform Admins configure default budgets and rate limits by workspace type, plus optional per-workspace overrides. Defaults update existing workspaces that have no override. Increasing an allowance never resets recorded consumption.

Team/Project administrators may tighten local budgets and limits within platform-set ceilings, but cannot raise or bypass those ceilings. Applicable limits compose; an absent child limit does not reserve capacity or remove a parent restriction.

Monthly budgets block new inference when the available allowance is exhausted. An optional installation-wide monthly budget provides an additional shared ceiling across all workspaces. Reporting or denial messages for ordinary users must not expose other users' personal consumption through installation-wide headroom.

Preserve durable per-attempt reservations, immutable pinned prices, conservative unknown holds, rotation lineage, explicit failover, and exact integer monetary arithmetic. A partial known monetary floor is not proof of a finite upper bound.

### Pricing and allocation

Local models use configured token costs just like cloud models. GPU, electricity and infrastructure metering are not part of the first rebuild. Explicit configured zero rates differ from unknown rates. Prices remain configured estimates, not provider invoices.

Recreate cache-aware accounting across provider parsing, normalized usage, price versions, reservation bounds, settlement, reconciliation, reporting, exports and UI—not merely additional pricing form fields. Preserve provider-specific inclusive/exclusive input semantics and disjoint cache categories. Cache-write totals overlap their TTL allocations and must never be charged twice. Missing usage/rates are not zero. Configuring cache rates does not enable request-side caching or unsupported cache controls.

The metering design must accommodate embeddings and later non-token units without forcing image, audio or video charges into token-only fields. Monetary API values remain exact integer micro-USD strings.

Cost-center assignment is optional on each workspace and controlled only by Platform Admins. Multiple workspaces can share a cost center. Unassigned usage remains fully tracked and is reported as Unallocated. Cost centers do not authorize access.

Attribution is recorded at execution admission. Cost-center changes affect only future usage; historical activity retains its original cost center or Unallocated attribution. Cost-center renames/deletion and retained labels must not silently rewrite financial history.

### Provider and protocol scope

Initial local targets: vLLM, SGLang, Ollama and explicitly configured OpenAI-compatible servers. Each profile needs its own tested capability subset; an OpenAI-compatible label does not promise all OpenAI endpoints or options.

Local connections may use HTTP only to explicitly platform-approved internal endpoints. Cloud connections require HTTPS. Endpoint approval, redirect behavior, credential isolation and network egress protections must be designed and tested; client requests cannot choose upstream URLs.

Retain the supported text/function-tool Chat, Responses and Messages subsets and add embeddings in the first rebuild. Continue to reject unsupported capabilities explicitly. Reuse the provider-independent engine/registry and cancellation machinery where their contracts fit; do not force every modality through a chat-shaped representation.

## Milestones and acceptance boundaries

### 1. Enterprise foundation

Fresh single-installation schema; global platform roles and OIDC entitlement; sibling shared/private workspaces; explicit membership provenance; key/service-account lifecycle; platform/workspace navigation and permissions.

Acceptance: real disposable-database authorization tests, entitlement denials, personal privacy with permitted financial totals, group/manual lifecycle behavior, 30-day cleanup and reactivation, last-admin/owner safeguards, session-only management, and reviewed runtime table/column ACLs. Define installation-wide versus workspace-scoped locking deliberately; deleting organization rows is not a concurrency design.

### 2. Catalogs and inference

Multiple catalogs; live type defaults and replacement overrides; self-service model additions and direct assignments; local provider profiles; current protocol subsets plus embeddings.

Acceptance: catalog removal/live authorization tests, clear provider/model capabilities, per-key restrictions and rotation continuity, local endpoint/secret validation, embedding-specific usage semantics, shared/provider-specific mock contracts, fragmented streams and cancellation. Unsupported provider/protocol combinations fail explicitly. No paid calls.

### 3. Pricing and enforcement

Cache-aware versioned rates, provider-normalized usage, immutable accounting, conservative per-attempt bounds, reconciliation, live policy defaults and overrides, hard monthly workspace limits and optional shared installation budget.

Acceptance: exact arithmetic, missing/zero distinctions, TTL allocation ambiguity, no duplicate cache-write charges, unknown/unbounded budget denial before dispatch, non-shrinking unresolved holds, idempotency/evidence refinement, quota races, fallback attempt accounting, price pinning and runtime privilege probes. Embeddings are input-only workloads and require explicit valuation/bounds semantics.

### 4. Reporting and allocation

Platform and workspace cost reports with date ranges, trends, comparisons, workspace/model/provider/optional cost-center breakdowns, bounded CSV and accounting-health indicators.

Acceptance: authorization before aggregation/dimension discovery; ordinary members see own activity; admins/auditors see authorized personal totals without private details; immutable cost-center attribution; exact amount/count strings; explicit unknown coverage and overlapping health indicators; UTC periods and strict filters; bounded pagination/exports and cancellation. Live reports are not frozen financial statements or provider invoices. Measure report/admission contention rather than equating output limits with bounded scans.

### 5. Images and vision

Selected image-input, generation and editing contracts with model capabilities, bounded uploads and appropriate pricing. Specify providers and API subsets before implementation.

### 6. Speech and audio

Transcription, translation, speech generation and streaming; later realtime connections. Define duration/character/token usage and rates, cancellation, transport and privacy per selected capability.

### 7. Video and asynchronous workloads

Selected video input/generation, batch and long-running jobs. Define durable job lifecycle, provider status, cancellation, accounting, storage, retention and download authorization before implementing endpoints.

### 8. Enterprise operational maturity

Background identity provisioning where supported, JWKS lifecycle, telemetry/alerts, off-host recovery, production-load testing, automated compatibility/browser gates and release controls. Develop relevant safeguards alongside milestones 1–4; this is not permission to defer security until broad modality support exists.

## Implementation discipline

Keep current implemented-state documentation honest while the new model is built. Update repository guidance when code transitions to the approved architecture; do not silently assume planned tenancy rules already exist.

Reuse tested engine, provider, key, accounting and packaging primitives. Replace ownership/authorization contracts coherently across database, API and UI. Fresh-schema permission does not justify abandoning immutable-history or secret-handling invariants.

## Approved next increment (2026-10-07)

User-selected cheap models for live validation:

| Capability | Model (provider) | Notes |
|---|---|---|
| Chat / Responses | `gpt-6-luna` (OpenAI direct; also `openai/gpt-6-luna` on OpenRouter) | text |
| Messages | `claude-haiku-5-5` (Anthropic direct; `anthropic/claude-haiku-5.5` on OpenRouter) | text |
| Chat | `z-ai/glm-5.3-flash` (OpenRouter) | text |
| Images | `black-forest-labs/flux-3-image` (OpenRouter) | milestone 5 |
| Decisions (System One) | `cloudflare/clef-flash` (OpenRouter); TypeSafe System One API | new "decisions" endpoint |
| Embeddings | `nvidia/nemotron-3-embed-1b:free` (OpenRouter) | free |
| Rerank | `nvidia/llama-nemotron-rerank-vl-1b-v2:free` (OpenRouter) | free; new rerank endpoint |
| Speech to text | `openai/whisper-large-v3-turbo` (OpenRouter) | milestone 6 |
| Text to speech | `microsoft/mai-voice-2-flash` (OpenRouter) | milestone 6 |

Scope, in order:

1. **Live acceptance** of existing Chat/Responses/Messages/embeddings adapters against the direct providers and fixes for what it finds.
2. **OpenRouter cloud provider profile** (HTTPS only, no redirects, server-held key) with captured upstream usage and reported cost as evidence.
3. **OpenRouter-style pricing.** Prices read like OpenRouter's catalog: per-unit rates shown as "$X / M input tokens", "$Y / M output tokens", cache read/write, "$Z / image", "$W / audio minute", per-request, with optional prompt-size tiers. Exact integer micro-USD storage, immutability and pinning are unchanged; unknown rates stay unknown. For OpenRouter routes an admin may import the current published rates (public catalog read, no key) and review them before publishing a version.
4. **New workload kinds**, each with its own client API, unit-based metering and pricing, admission bounds and mock + live tests: images (generation), speech-to-text, text-to-speech, rerank, decisions (System One). Exact client/upstream contracts are recorded in `.local/enterprise-rebuild/provider-research.md` before implementation.

Video, realtime audio and asynchronous jobs remain later milestones.

Validate each milestone before expanding scope. Use disposable databases, synthetic OIDC identities and mock upstreams. Isolate browser/build artifacts; do not overwrite a live demo build or modify existing services without an explicit promotion step. Tests present in source or historical verification logs are not fresh execution evidence.

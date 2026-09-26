# Architecture

## Product boundary

A multi-user gateway service operated by platform engineers. Users work in personal or shared team/project spaces. Platform administrators own infrastructure; organization administrators delegate assigned model access and impose stricter policies. See [platform administration](platform-administration.md). Administrators configure upstream access, expose a governed model catalog, and set scoped quotas/budgets, versioned prices, and explicit routing policies.

Start as a modular monolith, with a separately built React + Vite SPA served by Rust in production. Separate the control and data planes logically before separating deployments.

## Services

```text
                       TLS ingress
                            |
                      Rust / Axum
                  /         |          \
           SPA + assets   /api/*     /v1/*, /health/*
                            |
                PostgreSQL / provider APIs
```

- **React + Vite SPA:** TypeScript, TanStack Router + Query, and copied Bitop UI primitives/tokens for browser rendering and interaction. The dashboard shell follows the user's open-rag-system reference; source projects are not runtime dependencies. No direct database access or provider credentials. Rust serves the built files; no Node.js runtime or frontend inference proxy runs in production.
- **Rust management API:** implemented OIDC sessions, membership and role authorization, configuration, key lifecycle, audit records, reporting. A workspace inference key must never become an administrative credential.
- **Rust data plane:** API-key authorization, capability-aware routing, provider execution, streaming, usage events. Text/tool subsets of Chat Completions, Responses, and Messages use the provider-independent engine and registered OpenAI, Anthropic, and Bedrock adapters. See the explicit protocol matrix; broader content remains future work. Distributed admission/accounting and opt-in routing policies live outside provider adapters.
- **PostgreSQL:** ownership, authorization, configuration, distributed admission leases, immutable pricing versions and monetary events. Organization-row locks serialize each tenant's admissions/settlements/configuration.
- **Redis:** not required by current distributed controls; PostgreSQL is their source of truth. Consider a distributed fast path only after measurement, without weakening durable spend reservations.

Reqwest handles OpenAI/Anthropic HTTP. Bedrock uses the official AWS SDK, workload credentials, SigV4, Converse and binary event streams; it is not another OpenAI-compatible URL.

### Web serving and development

`GATEWAY_WEB_DIR` optionally points to a built `apps/web/dist` directory. Unset, Rust runs API-only; an invalid configured distribution directory must fail startup. Axum owns `/api/*`, `/v1/*`, and `/health/*` regardless of static serving. SPA fallback is for browser navigation only and must not swallow unknown API paths or missing assets.

During development, Vite listens at `127.0.0.1:3000` and proxies `/api/*`, `/v1/*`, and `/health/*` to `GATEWAY_INTERNAL_URL` (default `http://127.0.0.1:8080`). This server-only setting is loaded from `apps/web/.env.local`. Browser requests use same-origin relative URLs, never absolute API URLs. `VITE_*` variables are public bundle configuration and must never contain secrets. The public readiness request exposes only coarse status. Private dashboard calls require Rust-owned browser sessions; mutations require CSRF and the exact configured public origin.

Build with `npm run build:web`, then preview with `GATEWAY_WEB_DIR=apps/web/dist cargo run -p open-model-gateway -- serve`; the UI and APIs share the gateway port. Apart from optional static serving, the Rust backend and its security boundaries are unchanged.

## Ownership and tenancy

Users are global identities. Organizations are tenant boundaries. Organization memberships determine whether a user can act within a tenant. Platform-wide operator roles are distinct from organization roles; only explicitly provisioned operators configure provider connections.

A workspace owns keys and model grants. Service accounts, execution attribution, and scoped quota/budget policies attach here.

- **Personal:** exactly one owner from that organization; one personal space per user per organization.
- **Team / Project:** sibling shared workspace kinds under an organization, with explicit memberships. Projects do not require a parent team.
- A user can have memberships in multiple organizations, teams and projects.
- Each API key belongs to exactly one organization and workspace.
- Keys have exactly one user or service-account owner. User keys require live membership; service-account keys require an active account and a team/project workspace, not a live employee. Membership/account removal revokes affected keys; re-enabling does not resurrect them.

Tenant-owned foreign keys include `organization_id`; PostgreSQL rejects cross-tenant workspace ownership, key associations and delegated grants outside organization entitlements. Deployments are global platform infrastructure rather than tenant-owned resources. Queries derive tenant/workspace IDs only from authenticated server-side state.

This is not PostgreSQL row-level security. Correct query scoping is still required; cross-tenant isolation tests must accompany every new repository operation. Review RLS and a restricted application database role before general availability. The local Compose role has migration/test privileges and is not a production runtime role.

## Authorization

Tokens contain a random UUID lookup identifier and a cryptographically random 256-bit secret. Store only SHA-256 of the complete token, compare the digest in constant time, and display the full token once. These are random API credentials, not human passwords; password hashing is a separate concern if passwords are ever introduced.

Each request checks:

1. The key exists, is unrevoked, and has not expired.
2. Its organization and workspace are active.
3. For user keys, user and organization membership are active and the user owns the personal space or has active team/project membership. For service keys, the workspace is a team/project and the account is active.
4. For model visibility and inference deployment resolution: a platform-assigned organization entitlement, appropriate workspace or own-personal individual grant, and enabled global model/deployment/provider exist.

No authorization cache yet. Already-admitted work may complete after concurrent revocation. Routing plans are request snapshots, but every attempt revalidates current credentials, memberships and entitlements before admission against all applicable platform and local limits. Stream watchdogs drop transport at the request deadline even when the downstream is not polling.

Requests cannot override their workspace with headers or query parameters. Conflicting or repeated credential headers are rejected. `/v1/messages` accepts `x-api-key` or Bearer, never both together. Other current routes accept Bearer only.

## Models, providers, and protocols

Keep these independent:

- **Model:** a platform catalog resource with a canonical name; organization entitlements provide the stable client-facing alias, such as `company/smart`.
- **Provider connection:** provider type, endpoint/region, and a credential reference. No plaintext upstream credentials in the schema.
- **Deployment:** an upstream model identifier attached to a provider connection and a public alias.
- **Client protocol:** Chat Completions, Responses, or Messages.
- **Capabilities:** the adapter contract currently distinguishes text chat, streaming, and function tools. Model-specific metadata, images, structured output, reasoning, and state need further design.

An allowlisted `env:` secret resolver is implemented; it is not a secret manager or a per-tenant vault policy. OpenAI/Anthropic adapters accept only fixed vendor HTTPS bases, disable redirects/proxies, and reject regions. Management validates references and endpoint URLs; credential references are never returned in list responses. Bedrock requires `aws:default` and an explicit region and rejects production endpoint overrides. Prefer workload identity for cloud providers. Platform administrators provision shared global connections and assign models explicitly to organizations. Nullable legacy catalog organization IDs are provenance only: organization entitlements and per-consumer grants, not those IDs, authorize inference.

The engine now uses narrow typed chat/message/tool/usage/event contracts. Client protocol codecs and provider transports are separate. Adapters are registered at startup; adding a provider does not require changing engine routing or a provider enum in the database. Do not stretch the chat contract into a universal Responses/agent representation: extract shared concepts only where semantics genuinely match. Provider-native extensions must be explicit, not silently dropped or blindly passed through. See [provider adapters](provider-adapters.md).

## Compatibility promises

The implemented bounded frontends are:

- OpenAI Chat Completions: `/v1/chat/completions`
- OpenAI Responses: `/v1/responses`
- Anthropic Messages: `/v1/messages`

All support documented text/function-tool subsets and execution accounting. Unknown or unsupported request fields are rejected. Responses is stateless and Messages pins the Anthropic version header. Native Responses/Messages frontend content is buffered with a 4 MiB bound before ordered SSE delivery; Chat streams incrementally. `/v1/models` returns only the OpenAI list shape. See [protocol matrix](protocol-matrix.md).

Responses support must explicitly define stateful operations, previous-response references, hosted tools, reasoning items, and streaming event sequences. A Chat-to-Responses field mapping is not sufficient. Publish supported/unsupported capability matrices per frontend and upstream.

Never silently discard unsupported content. Reject or route according to explicit policy. Cross-provider fallbacks are opt-in and require capability and data-residency checks. Do not restart a response after emitting client-visible content. Even pre-response retries can duplicate upstream charges.

## Operations and security

- Migrate as an explicit deployment step, not on every replica startup.
- Refuse startup and readiness on missing or mismatched migration state.
- Liveness is independent of the database; readiness has a two-second deadline.
- Structured request logs contain generated request IDs, method, status, and elapsed time—not tokens, bodies, query strings, or prompts. Avoid enabling verbose dependency logs in production without reviewing their contents.
- Current request body limit is 2 MiB. Provider adapters independently bound complete bodies and stream frames.
- No CORS is enabled. Production dashboard and management requests should share one origin.
- Browser auth: Rust-owned OIDC/session validation, Secure/HttpOnly cookies, CSRF protection, and logout invalidation. JWKS refresh currently requires restart; session cleanup is operational follow-up. Do not store inference credentials as dashboard login tokens.
- Usage events, versioned pricing, cost estimates, reservations, and financial charges are separate concepts. Never use floating point for money.
- Prompt/response content retention is opt-in, separately permissioned, and subject to tenant retention policies.

Before public service: load-test the implemented distributed limits/reservations/deadlines, verify upstream input ceilings/prices and real SSO/IAM, enforce network egress and credential rotation, and add backups, restore testing, and workload observability. Configured-rate estimates are not vendor-invoice or customer-billing guarantees. See [governance](governance.md) and [routing](routing.md).

# Architecture

## Product boundary

One installation serves one enterprise. The installation is the identity and administration boundary, not a selectable tenant. Teams, Projects and private personal workspaces sit directly beneath it; Projects are not children of Teams. There are no enterprise organization tables, memberships or management endpoints.

[Enterprise rebuild](enterprise-rebuild.md) is the approved direction. This page describes the current source; it is not new acceptance evidence or a production-readiness claim. Earlier milestones and checks in [verification](verification.md) are historical unless explicitly rerun for this lineage.

```text
TLS ingress
    └─ Rust / Axum
       ├─ SPA + assets          React / Vite, browser sessions
       ├─ /api/v1/*             management and financial reporting
       ├─ /v1/*                 API-key inference
       └─ /health/*             liveness and readiness
                ├─ PostgreSQL  identity, configuration, admission, accounting
                └─ adapters    approved cloud/local upstreams
```

The gateway is a modular monolith with logically separate control and data planes. Production serves the separately built SPA from Rust; no Node.js inference proxy is required. The UI uses locally installed Bitop components and Grounded's Workspace/Admin composition patterns. Reference repositories are not runtime imports. Redis is not required by the durable controls.

## Installation and storage

`apps/gateway/enterprise_migrations/0001_enterprise.sql` starts a new `enterprise_v1` lineage. The old `apps/gateway/migrations/` files remain legacy evidence, not an upgrade path.

`migrate` is explicit. A read-only preflight runs **before DDL**, including before SQLx creates its tracking table. It refuses legacy or unrelated nonempty public schemas and dirty, changed or unexpected migration lineages. A recognized enterprise prefix can be upgraded explicitly. `serve` and readiness require the exact current lineage/checksums and installation family; neither migrates. This is lineage checking, not complete tamper-proof attestation of every index/function.

Use a separately approved fresh target for enterprise staging. Do not point the rebuild at an old database, automatically migrate it, reset it, or delete history. A development/test database owner is not an appropriate production runtime identity; review the release's runtime grants and privilege probes separately.

Admission and settlement follow one canonical, deadlock-free lock order (migration 0027, [governance](governance.md#scoped-admission-lock-order)): the catalog advisory lock (shared; exclusive for global catalog changes), then transaction-scoped authority advisory locks of the key's workspace type, workspace, issuing user and key lineage (shared in admission, exclusive in the management change of that scope), then the reservation row, then the `budget_totals`, minute and in-flight counter rows the write changes, in primary-key order. There is no installation-wide serializer on the request path any more: admissions of different workspaces run in parallel and admissions of one workspace serialize on that workspace's totals and counter rows, which keeps budgets exact. Revocation stays live because every authority change takes its scope lock exclusively (also enforced by database triggers), so no admission commits after it. The installation row lock remains for management-versus-management serialization (last-admin checks, SCIM, sign-in). `GATEWAY_ADMISSION_MODE=global` restores the former installation-row protocol for one release. Budget checks read trigger-maintained per-workspace and per-key-lineage, per-period totals (`budget_totals`) and workspace/key rate limits read trigger-maintained per-minute and in-flight counters (`rate_minute_counters`, `inflight_counters`), so their cost grows with neither history nor current traffic. Read paths take none of these locks: reports, usage, request logs, `/me` and `/v1/models` run in lock-free `REPEATABLE READ READ ONLY` snapshots (`apps/gateway/src/reporting.rs`) whose authorization checks (platform role, membership/ownership, key revalidation) and data share one snapshot, so they neither wait for nor block admission, settlement or management writes. A revocation that commits while such a read runs affects the next read, not that one (staleness bounded by the 10 s read deadline); admission still revalidates live under its locks, so revocation blocks new inference immediately. Reports, usage and logs may read their data from an optional reporting replica after authorizing on the primary ([operations](operations.md#read-snapshots-and-the-reporting-replica)). Bounded output does not imply bounded scans.

Gateway-owned files (`apps/gateway/src/filestore`, [file storage](file-storage.md)) live outside PostgreSQL. The local disk and S3-compatible backends both receive only ciphertext: the gateway encrypts every object with streaming AES-256-GCM under per-object data keys, wrapped by master keys that come from an environment reference. Object keys are generated by the gateway and bound into the encryption. `stored_files` holds metadata only. The file store reuses Bedrock's AWS identity references and allowlists; any explicit endpoint must be allowlisted (that entry is the approval for plaintext HTTP); redirects, ambient proxies and implicit retries are disabled. Storage never runs under the installation lock or any scope lock other than its own per-workspace quota lock.

## Identity and authorization

OIDC authentication is distinct from entitlement. Active `user`, `auditor` or `admin` platform grants are required. Manual/bootstrap and signed-group grants remain independently represented. Group mappings synchronize at sign-in and, when enabled, from SCIM pushes (`/scim/v2`, bearer token by hash, group provenance). The issuer JWKS is cached with bounded TTL, rate-limited single-flight refresh and a bounded outage grace. See [identity](identity.md) and [SCIM](scim.md).

A personal workspace is created on an entitled user's successful sign-in. It is owner-private for keys and request details, including against Platform Admins/Auditors. Their financial reporting may include personal totals without granting private-detail access. Ordinary shared members see their own human-key activity; shared administrators see workspace-wide activity.

Human inference keys require live platform entitlement and actual shared membership, even for a Platform Admin. Shared service-account keys require an active account/workspace, not continued employment of their creator. Removal/disablement revokes affected credentials; later reactivation never revives revoked keys. Rotation retains budget/model-restriction lineage.

Every attempt rechecks keys, membership, live model authorization and enabled target configuration under locks. The dispatched target must still match the loaded endpoint, credential reference, model and protocol metadata. Already-admitted work may complete after revocation; there is no promise of retroactive cancellation. Query scoping is still essential: this is not PostgreSQL row-level security.

## Models, catalogs and execution

A model has one global public alias and an explicit `supported_protocols` list. Provider connections hold redacted credential references and approved transport configuration. Deployments bind a model to an upstream identifier. Adapter capability checks further narrow the model's declared protocols; declaring a protocol does not make an incompatible deployment support it.

Catalogs have live defaults per workspace kind and replacement overrides. Personal owners/shared administrators select available catalog models; Platform Admins can assign models directly. Catalog presence alone is not permission. Catalog/direct provenance, current enabled state and key restrictions are checked during discovery and admission. Losing authorization retires affected selections without turning a restricted key into an unrestricted one.

The engine separates protocol codecs, typed generation/embedding requests, registry selection and provider transports. Registered profiles cover bounded Chat, Responses, Messages and embeddings combinations, not arbitrary SDK options. See [protocol matrix](protocol-matrix.md) and [provider adapters](provider-adapters.md). Realtime WebSocket sessions are one upstream attempt and reservation each, with budget windows reserved per response and settled per `response.done` ([realtime](realtime.md)). Async video jobs and native batches are also one attempt and reservation each: admitted at create, held until the provider reports a final state (background poller), with metadata-only job rows and gateway ids scoped to the creating workspace ([async jobs](async-jobs.md)). Gateway-run batches execute each line through the same engine with its own reservation, transferred out of the batch's ceiling, on a bounded worker pool with exactly-once line claims ([batches](batches.md)).

Admission records one execution and reservation per actual upstream attempt. Prices are immutable configured estimates, not invoices. Cache-aware settlement uses disjoint categories and exact integers; incomplete evidence retains conservative holds. Embeddings are input-only workloads, not chat requests with synthetic messages. Cost-center attribution is snapshotted at admission. See [governance](governance.md) and [cost reporting](cost-reporting.md).

## Serving and operations

- `GATEWAY_WEB_DIR` enables built SPA serving; invalid configured assets fail startup. API paths and missing assets must not be swallowed by SPA fallback.
- Development Vite proxies relative `/api/*`, `/v1/*`, `/health/*` requests via server-only `GATEWAY_INTERNAL_URL`. Never put secrets in `VITE_*` variables.
- Management uses Rust-owned sessions, exact Origin and CSRF checks. Inference keys are not administrative credentials. No CORS is enabled.
- `/health/live` needs no database; `/health/ready` has a two-second deadline and reports only coarse readiness.
- Request bodies are limited to 2 MiB; adapters separately bound complete bodies and frames. Generated request IDs, method, status and elapsed timing are logged, not prompts, bodies, query strings or credentials.
- No default prompt/response-body retention is introduced. Account cleanup retains financial attribution and audit history. Optional settled-detail compaction is not monetary deletion.

Real identity-provider interoperability, live local/server profile certification, load/lock contention, network egress, backup/restore and release gates require separate operational validation. Source tests and isolated provider checks are not whole-stack certification.

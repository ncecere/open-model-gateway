# Open Model Gateway

A single-enterprise inference gateway: Rust/Axum/Tokio/SQLx + PostgreSQL 17, with a React/Vite management SPA served by Rust. Node.js is a build/test dependency, not a production application server.

**Development software.** The enterprise rebuild is undergoing integrated acceptance. Local checks do not establish production readiness, real-IdP acceptance or universal provider compatibility. The approved requirements are in [enterprise rebuild](docs/enterprise-rebuild.md); current execution evidence belongs in [verification](docs/verification.md).

## Enterprise boundaries

- One installation is the enterprise boundary. Teams, Projects and owner-private personal workspaces are sibling security scopes; there is no organization picker or organization API.
- Platform Admin, Auditor and User are explicit entitlements, separate from successful SSO. Admin/Auditor include User. Shared workspaces have independent owner/admin/member grants.
- Generic, signed OIDC groups synchronize at sign-in. Manual grants survive mapped-source removal. Entitlement loss revokes human sessions/keys, with a 30-day grace period and automatic cleanup retaining accounting/audit history. Group loss is not detected between sign-ins in this release.
- Personal keys/request details remain private even from Platform Admin/Auditor; platform financial reports may include personal aggregate totals. Shared members see their own activity; administrators see shared-workspace activity.
- Models are selected from multiple available catalogs or assigned directly. Live type defaults and replacement overrides govern eligibility; losing access never restores retired key selections when eligibility returns.

The dashboard follows Grounded's Bitop UI and Workspace/Admin composition, with gateway-specific permissions and resources. See [dashboard](docs/dashboard.md), [management API](docs/management-api.md) and [Bitop provenance](docs/bitop-ui.md).

## What works today

- **Workspace:** Home, Models (catalog, model compare), API keys (one-time disclosure, rotate/disable/revoke, per-key models and limits, key safety findings), Logs (requests, generations, sessions), Usage & costs, Settings and Alerts for Team/Project admins.
- **Admin:** Users, Teams, Projects, SSO group mappings, Connections (OpenAI, Anthropic, OpenRouter, AWS Bedrock with default/profile/role access modes, approved local endpoints), Models with routes and immutable prices, Catalogs and defaults, limits and budgets, cost centers, Logs and Usage (Team/Project rows only), key safety, audit, Settings (general, limits, data & privacy, email, sign-in) and Alerts (budget thresholds, spend spikes, error rates, failing connections; in-app and email). Auditors get the same views read-only.
- **Checks:** Rust unit/integration tests on real PostgreSQL, mock-provider and SDK contracts, Vitest, and a Playwright journey with axe accessibility scans for every persona ([verification](docs/verification.md), [accessibility](docs/accessibility.md)).

**Planned, not implemented:** an OpenRouter video adapter (video has no supported provider: OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24), video remix/edits/references, native batches beyond OpenAI and Anthropic, image edits and vision input, token-by-token Responses/Messages streaming, active health probes, webhooks and key-expiry notifications, provider-invoice reconciliation. See [roadmap](docs/roadmap.md).

## Local account picker

See [local demo](docs/local-demo.md) for the fresh, loopback-only `gateway_enterprise_demo` database and five personas: Platform Admin, Auditor, Alex (workspace administrator), Blair (member), and an authenticated but unentitled SSO user.

The demo has disabled example deployments and illustrative prices, not paid credentials or fabricated usage. Personal workspaces are provisioned on actual entitled sign-in. Existing databases are never reset or upgraded into the enterprise schema.

## Initialize a new installation

Prerequisites: current stable Rust, Node.js 22.12+, npm and PostgreSQL 17 (Docker Compose is provided).

```sh
cp .env.example .env
npm ci
docker compose up -d --wait postgres
cargo run -p open-model-gateway -- migrate
```

The explicit migration command performs read-only compatibility checks before DDL. Only an empty database or a recognized enterprise migration prefix is accepted. Legacy migrations remain historical evidence, not an upgrade path. `serve` requires the exact current enterprise lineage and never migrates implicitly.

Configure your registered OIDC issuer/client and `${GATEWAY_PUBLIC_URL}/api/v1/auth/callback`; production requires HTTPS. The signed ID token must contain a verified email and the configured groups claim (default `groups`). Missing/malformed groups fail closed without erasing prior grants. Establish the first administrator through trusted provisioning:

```sh
cargo run -p open-model-gateway -- provision-user \
  --email operator@example.org --platform-admin
```

This grants entitlement and permits one controlled initial verified-email link; it does not authenticate a user or rebind an existing identity. Configure group mappings through Admin afterward. Never put credentials in `VITE_*` variables or upload provider secrets through the browser. See [identity](docs/identity.md).

```sh
cargo run -p open-model-gateway -- serve
# Optional Vite development proxy, in another terminal:
npm run dev:web
```

For a matched, Rust-served deployment:

```sh
npm run build:web
GATEWAY_WEB_DIR=apps/web/dist cargo run -p open-model-gateway -- serve
```

Keep the binary and compiled SPA from the same verified revision. Use a separate output directory for acceptance builds; never overwrite a running UI with an unverified build.

## Inference and accounting

| Route | Supported workload |
| --- | --- |
| `GET /health/live`, `/health/ready` | Liveness and exact migration readiness |
| `GET /v1/models` | Live workspace-authorized, enabled aliases |
| `POST /v1/chat/completions` | Bounded text/function-tool subset |
| `POST /v1/responses` | Stateless text/function-tool subset; OpenAI |
| `POST /v1/messages` | Text/tool subset; Anthropic and Bedrock |
| `POST /v1/embeddings` | String/string-batch input, float vectors; declared embedding profiles |
| `POST /v1/rerank` | `{model,query,documents,top_n?}` → scored indices; OpenRouter |
| `POST /v1/systemone` | TypeSafe System One contract (TypeSafe SDK compatible); OpenRouter |
| `/v1/videos`, `/v1/files`, `/v1/batches` | Gateway Files API; batches for any model (native OpenAI/Anthropic batch APIs or gateway-run lines, [batches](docs/batches.md)), workspace-owned gateway ids. `/v1/videos` returns `unsupported_capability`: no supported video provider since OpenAI's Videos API shutdown (2026-09-24) ([async jobs](docs/async-jobs.md)) |
| `/api/v1/*` | Browser-session management with exact-Origin/CSRF checks |

Inference keys never authorize management. Model-declared protocols must intersect adapter support. OpenAI, Anthropic, AWS Bedrock and OpenRouter have separate adapters; local vLLM, SGLang, Ollama and generic-compatible profiles have explicit, narrow contracts. Local HTTP requires exact server-controlled endpoint approval and pinned IP destinations; cloud requires HTTPS. Redirects, ambient proxies and implicit retries are disabled.

- Fixed-minute request/token quotas, leased concurrency and hard USD budgets per UTC day, ISO week or calendar month (chosen per policy layer) are shared across replicas. Optional installation-wide budgets add another ceiling.
- Immutable prices pin each attempt. Cache reads and disjoint write allocations are accounted without double charging aggregate writes. Amounts use exact integer micro-USD arithmetic.
- Unknown usage/rates are not zero. Partial known charges are not finite upper bounds. Monetary-budget requests without a finite admission bound are denied before dispatch; unresolved activity retains conservative holds.
- Embeddings are an input-only workload, not synthetic chat. Local prices are configured token rates, not measured GPU/electricity costs.
- Routing supports priority/weight, residency labels, passive cooldown and at most three explicitly permitted attempts. No failover follows a returned stream.
- Reports use strict UTC date intervals, known actual versus active held amounts, explicit accounting coverage and optional admission-time cost-center snapshots. Costs are configured estimates, not provider invoices.

See [protocol matrix](docs/protocol-matrix.md), [providers](docs/provider-adapters.md), [cache pricing](docs/cache-pricing.md), [governance](docs/governance.md) and [reports](docs/cost-reporting.md). Base64 image generation (`/v1/images/generations`, OpenAI `gpt-image-*` and OpenRouter) is a bounded first increment, as are audio transcription (`/v1/audio/transcriptions`) and speech (`/v1/audio/speech`) for OpenAI and OpenRouter. Chat Completions batches (OpenAI) are one attempt and reservation each, polled in the background and mock-tested only; video jobs share that path but have no supported provider until the planned OpenRouter video adapter ([async jobs](docs/async-jobs.md)). Image edits and vision are not implemented yet. Realtime audio is a bounded OpenAI WebSocket subset whose per-response holds are sized from the session's context ([realtime](docs/realtime.md)). Native Responses/Messages SSE currently buffers bounded content rather than delivering token-by-token frontend events.

## Verification and deployment

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
# SQLx creates disposable databases; use a dedicated test PostgreSQL role.
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54339/gateway \
  cargo test --workspace --all-features
npm run typecheck:web
npm run test:web
npm run test:demo
npm run test:container   # builds the image; needs Docker
npm run test:staging
npm run build:web
# Playwright + axe against a disposable database on 54339 (never the demo):
npm run test:browser
```

Mock-provider and SDK tests require no paid calls or real enterprise IdP modifications. See [staging](docs/staging.md) for restricted runtime roles, the image, TLS rehearsal and backup/restore tooling. **Use a new enterprise staging database; do not run initialization against an existing legacy deployment.** Production load/availability, real-provider certification, live IdP/SCIM acceptance, observability and off-host recovery acceptance remain unfinished.

## Releases and security

- **Releases:** [changelog](CHANGELOG.md) and per-release notes in [`docs/releases/`](docs/releases/) (migrations, new settings, upgrade steps, known limitations). Upgrades are explicit: back up, `migrate`, reapply runtime grants, `budget verify`.
- **Images:** from v0.3.0, `ghcr.io/ncecere/open-model-gateway` is published for linux/amd64 and linux/arm64 with SBOM and provenance attestations and a cosign keyless signature. Each architecture's exact digest passes the isolated staging rehearsal and a Trivy scan before it is tagged. Deploy by digest and [verify the signature](docs/releases/v0.3.0.md#verifying-the-images) first. v0.1.0 and v0.2.0 have no published images. After v0.3.0 the runtime image is distroless (no shell or curl): the binary is the entrypoint, reads `*_FILE` secrets itself and provides `open-model-gateway healthcheck` ([container image](docs/operations.md#container-image)).
- **Security:** report vulnerabilities privately as [SECURITY.md](SECURITY.md) describes. Only the latest minor release (0.3.x) gets fixes.

## License

[MIT](LICENSE). Copyright (c) 2026 Nicholas Cecere.

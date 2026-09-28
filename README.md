# Open Model Gateway

A Rust-first, multi-tenant AI gateway with a React + Vite management dashboard. Production serves the compiled SPA and APIs from Rust—no Node.js server.

**Early development, not ready for public launch.** Implemented: OIDC login, organization/personal/team/project access, platform-owned model infrastructure with delegated organization/workspace/user grants, non-overridable platform organization ceilings plus optional local policies, management APIs and dashboard, key/service-account lifecycle, execution reporting, and three inference protocols. See [platform administration](docs/platform-administration.md). PostgreSQL-backed quotas/budget reservations, versioned cost accounting, reconciliation, and opt-in routing are implemented. Production operations and live provider/IdP validation remain unfinished.

## Stack

- Rust, Axum, Tokio, SQLx, PostgreSQL 17.
- React, Vite, TypeScript, TanStack Router + Query, and vendored Bitop UI components/tokens.
- OpenAI and Anthropic HTTP adapters; official AWS SDK/SigV4 for Bedrock.
- Compile-time provider registry, separate client-protocol adapters, provider-independent engine.

```text
apps/gateway/       Rust service, migrations, database/provider tests
apps/web/           React + Vite management dashboard
tests/             Official OpenAI/Anthropic SDK contract tests
docs/              Architecture, operations/setup, supported protocol contracts
```

## Explore with demo accounts

For a local account picker like open-rag-system, see [Local dashboard demo](docs/local-demo.md). It provides **Platform Admin**, **Organization Admin**, **Alex (Team Admin)**, and **Blair (Member)** personas at **http://127.0.0.1:3000**, using an isolated `gateway_demo` database and an explicitly enabled loopback OIDC issuer. Sample providers are disabled; usage/costs are not fabricated. Production authentication is unchanged.

## Staging deployment

See [staging deployment](docs/staging.md) for the multi-stage production image, local HTTPS rehearsal, separate migrator/runtime database roles, private file secrets, backup/restore checks, and OIDC setup. [Live acceptance](docs/live-acceptance.md) lists the information and explicit opt-in checks needed before testing real SSO or paid providers. The default staging stack is loopback-only and does not modify the demo.

```sh
python3 scripts/staging.py init
python3 scripts/staging.py build
python3 scripts/staging.py db
python3 scripts/staging.py migrate
python3 scripts/staging.py up
python3 scripts/staging.py verify
```

## Run locally

Prerequisites: current stable Rust, Node.js 22.12+, npm, Docker Compose.

```sh
cp .env.example .env
cp apps/web/.env.example apps/web/.env.local
npm ci
docker compose up -d --wait postgres
cargo run -p open-model-gateway -- migrate
# Optional inference fixtures, NOT dashboard authentication:
cargo run -p open-model-gateway -- bootstrap-dev
```

Development bootstrap prints personal/team inference keys **once** and creates a disabled OpenAI example deployment. It requires `GATEWAY_ENV=development`; never run it against production. `/v1/models` stays empty until an authorized operator enables a connection/deployment and grants the model. No real provider is enabled automatically.

In separate terminals:

```sh
cargo run -p open-model-gateway -- serve
npm run dev:web
```

Web: <http://127.0.0.1:3000>; Rust: <http://127.0.0.1:8080>; PostgreSQL: `127.0.0.1:54329`.

If 8080 is occupied, change `GATEWAY_LISTEN` in `.env` and Vite's server-only `GATEWAY_INTERNAL_URL` in `apps/web/.env.local`. Vite proxies `/api/*`, `/v1/*`, and `/health/*`; browser requests remain same-origin. Never put secrets in `VITE_*` variables.

### Enable dashboard sign-in

For real data, configure an actual OIDC client; no production-login bypass or default administrator exists. For isolated exploration without an IdP, use the [local demo](docs/local-demo.md) instead:

```sh
# Example values: substitute your registered issuer/client.
GATEWAY_PUBLIC_URL=http://127.0.0.1:3000
GATEWAY_OIDC_ISSUER=https://your-issuer.example.org
GATEWAY_OIDC_CLIENT_ID=your-client-id
# Inject GATEWAY_OIDC_CLIENT_SECRET securely if your client requires it.
```

Register `${GATEWAY_PUBLIC_URL}/api/v1/auth/callback`. For a Rust-served build, use the Rust origin instead of port 3000. Production requires HTTPS. The issuer must return a signed, verified email claim.

Before first operator login, use trusted database access to explicitly provision its email:

```sh
cargo run -p open-model-gateway -- provision-user --email operator@example.org --platform-admin
```

This permits one initial verified-email link and grants the operator role. It does not authenticate anyone. Existing linked identities are never rebound by this command. Run without `--platform-admin` to authorize first linking of a preprovisioned ordinary user. New SSO users otherwise receive no organization membership or administrative privilege; an invitation grants membership. See [identity setup and security](docs/identity.md).

The [dashboard](docs/dashboard.md) separates platform Admin from contextual organization/workspace settings. Resource detail pages, bookmarkable pill tabs, server-side catalog search, and connected model/deployment setup preserve context. Actual membership and inherited authority are shown separately. It supports invitations, grants, user and service-account keys, executions, known-token totals, and sanitized audit history. Provider configuration is operator-only; personal workspaces remain private even from organization admins and operators. Provider credentials are references, not secrets uploaded through the browser. See [management API and permission matrix](docs/management-api.md).

## Governance, costs, and routing

- Organization, workspace, and key limits share PostgreSQL-backed admission across replicas: fixed-minute attempt/token quotas, leased concurrency, and calendar-month USD budgets.
- Keys may inherit model access, deny all models, or select up to 200 granted model UUIDs. Restrictions only narrow parent entitlements, are fixed at issuance, and survive rotation alongside budget consumption.
- Platform operators append immutable deployment prices. Each attempt pins its version and reserves the full configured hard input-token ceiling plus the explicitly requested output limit. **Configure accurate pricing and provider bounds before enabling traffic.**
- Unpriced usage is unknown, not free. Unknown/partial/failed usage retains its reservation; old unpriced activity can block a newly enabled budget until the next UTC month. Complete successful usage settles at integer micro-USD rates. Costs are configured-rate estimates, not provider invoices.
- Routing supports priorities, weights, cooldowns, required-residency labels and up to three explicitly allowed attempts. Ambiguous transport failover requires a separate opt-in because it can duplicate charges. No failover occurs after a stream is returned.
- A bounded worker reconciles expired leases every five seconds without refunding unknown charges. Operator reconciliation uses authoritative token counts and an evidence reference.
- Optional execution-detail retention compacts only settled records, preserving usage, prices, reservations and the immutable ledger.

Use the dashboard's governance/cost/routing/pricing screens or [management endpoints](docs/governance-api.md). See [enforcement semantics](docs/governance.md), [routing safety](docs/routing.md), and [Bitop integration](docs/bitop-ui.md).

```sh
# Bounded manual maintenance; normally expiry reconciliation runs in the server.
cargo run -p open-model-gateway -- reconcile-executions --limit 100
cargo run -p open-model-gateway -- compact-history --older-than-days 365 --limit 1000
```

Without policies/prices, inference remains usable with unknown cost; no artificial spending protection is claimed. With pricing, clients must send `max_completion_tokens` (Chat), `max_output_tokens` (Responses), or `max_tokens` (Messages).

## Inference

| Route | Contract |
| --- | --- |
| `GET /health/live`, `GET /health/ready` | Liveness and migration-aware readiness |
| `GET /v1/models` | Workspace-authorized, enabled model aliases |
| `POST /v1/chat/completions` | Text/function tools; OpenAI, Anthropic, Bedrock |
| `POST /v1/responses` | Native stateless text/function tools; OpenAI |
| `POST /v1/messages` | Native text/tool blocks; Anthropic, Bedrock |
| `/api/v1/*` | OIDC browser sessions, CSRF-protected management; inference keys never accepted |

Inference uses workspace keys, not browser sessions. Messages accepts either Bearer or `x-api-key` and requires `anthropic-version: 2023-06-01`. Other inference routes use Bearer authentication.

```sh
curl http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer $GATEWAY_API_KEY"

# Requires an enabled/granted compatible deployment and injected credentials.
curl http://127.0.0.1:8080/v1/responses \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"company/smart","input":"Hello","store":false,"max_output_tokens":128}'
```

**Not full upstream API compatibility.** Unsupported content, fields, protocols, and provider-specific capabilities fail explicitly. Responses is stateless: no hosted tools, reasoning, images, previous-response retrieval, or stored conversations. Native Responses/Messages SSE starts immediately but currently buffers bounded content before emitting ordered blocks; it is not token-by-token frontend delivery. Chat streams incrementally. See the [protocol matrix](docs/protocol-matrix.md), [adapter guide](docs/provider-adapters.md), and [Bedrock configuration](docs/bedrock.md).

## Verify

```sh
npm ci
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace

# PostgreSQL role must be allowed to create disposable databases.
# Node + npm dependencies are required for official SDK contracts.
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54329/gateway \
  cargo test --workspace --all-features
npm run typecheck:web
npm run test:web
npm run build:web
```

SQLx tests use isolated databases and real migrations. Tests cover tenant isolation, OIDC/CSRF/session lifecycle, membership/invitation/key concurrency, provider wire contracts and cancellation, and official OpenAI/Anthropic SDK JSON/streaming helpers. Mock issuers/providers are loopback-only; tests need no real SSO account or paid API calls. Live-provider and real-enterprise-IdP validation remains an operator acceptance step.

## Deployment direction

```sh
npm run build:web
GATEWAY_WEB_DIR=apps/web/dist cargo run -p open-model-gateway -- serve
```

Open the UI on the gateway port. Configure `GATEWAY_PUBLIC_URL` and the registered OIDC callback for that origin. Node is only a build/test dependency. Unset `GATEWAY_WEB_DIR` for API-only operation. Never point it at source code or a secret-bearing directory. Unknown API routes, missing assets, and traversal attempts do not fall through to SPA HTML.

The included Compose file runs **only PostgreSQL**. TLS ingress, production images, backups, observability, session cleanup, live JWKS refresh, provider-invoice reconciliation and customer billing still require work. Distributed controls need production load/availability testing; PostgreSQL is on the admission path and outages fail closed. [Architecture](docs/architecture.md) · [Roadmap](docs/roadmap.md).

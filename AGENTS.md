# Repository guidance

## Layout

- `apps/gateway`: Rust modular monolith; owns all database access and authorization.
- `apps/web`: React + Vite SPA with TypeScript, TanStack Router + Query, and Bitop UI components/tokens; registry source is copied into this repository, not imported across local projects. Never accesses PostgreSQL directly.
- `docs/architecture.md`: security boundaries and design decisions.
- `docs/roadmap.md`: implemented versus planned behavior.
- `docs/provider-adapters.md`: engine/adapter contracts, supported request subset, and secret configuration.

## Invariants

- Organization is the consuming tenant boundary. Tenant relationships use composite foreign keys; provider/model/deployment infrastructure is platform-owned and requires explicit organization entitlements.
- Inference keys belong to one workspace. Derive tenant and workspace from validated credentials, never client-selected headers or body fields.
- Personal workspaces cannot be shared. Teams and projects are sibling shared workspace kinds; their access requires current membership. Individual model grants apply within the user's own personal scope and never bypass shared workspace grants.
- Platform-assigned organization policy ceilings are separate from editable organization/workspace/key policies. All applicable limits compose; absent child limits inherit the shared parent allowance, never remove or reserve it.
- Inference API keys are not management credentials.
- Never store or log plaintext upstream credentials, inference tokens, or prompt/response bodies by default.
- Provider credentials are references; validate secret access and upstream endpoints before implementing resolution/execution.
- Client protocols and upstream providers are different layers. Do not silently discard unsupported capabilities.
- Add providers through `ProviderAdapter` and `ProviderRegistry`; do not add provider switches to the engine. Run shared and provider-specific mock contract tests.
- Dropping an inference future/stream must cancel upstream work. Never fabricate successful stream termination, retry implicitly, or treat unknown usage as zero. Failover requires explicit policy and must never occur after a stream is returned.
- Global catalog changes use the exclusive catalog transaction lock; admission takes its shared form before the consuming organization row. Governance admission/scoped configuration/settlement serialize on the organization row; every upstream attempt needs its own durable reservation. Unknown cost retains holds. Never delete unknown reservations or mutate immutable pricing/ledger history.
- Monetary API values are integer micro-USD strings, never JavaScript/Rust floats. Prices are estimates, not provider invoices. Preserve personal-workspace privacy in cost reporting/reconciliation.
- Routes that are not implemented must stay explicit about that; no fabricated inference responses or dashboard metrics.
- Production is one Rust service serving APIs and optionally the built SPA, with no Node.js runtime. Inference goes directly to Rust.
- `GATEWAY_WEB_DIR` points to a built `apps/web/dist` directory; unset means API-only. Invalid configured distributions must fail startup.
- Axum owns `/api/*`, `/v1/*`, and `/health/*`. SPA fallback must not swallow unknown API paths or missing assets.
- Browser API requests use same-origin relative URLs, never absolute API URLs. Never put secrets in public `VITE_*` variables.

## Web workflow

Install with `npm ci` and run `npm run dev:web`. Vite listens at `127.0.0.1:3000` and proxies `/api/*`, `/v1/*`, and `/health/*` to `GATEWAY_INTERNAL_URL` (default `http://127.0.0.1:8080`), a server-only setting loaded from `apps/web/.env.local`.

For a local production-style preview, run `npm run build:web`, then `GATEWAY_WEB_DIR=apps/web/dist cargo run -p open-model-gateway -- serve`. The UI is on the gateway port. Dashboard sign-in uses Rust-owned OIDC sessions; see `docs/identity.md` and `docs/management-api.md`. Inference keys never authorize management. Native protocol support/limits are in `docs/protocol-matrix.md`.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
# Install SDK test dependencies before all-feature integration tests:
npm ci
# With a PostgreSQL DATABASE_URL and permission to create test databases:
cargo test --workspace --all-features
npm run typecheck:web
npm run test:web
npm run build:web
```

`npm run test:web` runs Vitest unit tests; do not claim browser tests completed without running them. Use real database integration tests for tenancy constraints and authorization queries. Use mock upstream servers for provider contract tests; do not incur paid provider requests in CI. Keep the roadmap and README honest as features land.

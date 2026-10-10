# Repository guidance

The single-enterprise rebuild is in progress. `docs/enterprise-rebuild.md` records the approved product decisions and scope; it supersedes legacy multi-organization assumptions. Implement the new schema on fresh, explicitly selected databases only. Never reset or migrate an old installation implicitly.

## Layout

- `apps/gateway`: Rust modular monolith; owns all database access and authorization.
- `apps/web`: React + Vite SPA with TypeScript, TanStack Router + Query, and Bitop UI components/tokens; registry source is copied into this repository, not imported across local projects. Never accesses PostgreSQL directly.
- `docs/architecture.md`: security boundaries and design decisions.
- `docs/roadmap.md`: implemented versus planned behavior.
- `docs/provider-adapters.md`: engine/adapter contracts, supported request subset, and secret configuration.

## Invariants

- One installation serves one enterprise; there are no selectable organizations. Workspaces are the resource/security boundary. Team and Project are equivalent sibling shared kinds; personal workspaces are owner-private. Enforce workspace relationships with scoped foreign keys and live authorization.
- Inference keys belong to one workspace. Derive tenant and workspace from validated credentials, never client-selected headers or body fields.
- Personal workspaces cannot be shared. Platform Admins/Auditors may see personal usage/cost totals, never another owner's keys or request details. Shared-workspace members see their own activity; workspace admins see workspace-wide activity. Global metadata authority does not satisfy human-key membership requirements.
- Platform roles are User, Auditor and Admin; Auditor/Admin include User entitlement. OIDC authentication alone does not grant platform access. Group/manual grant provenance must remain separate. Revoked keys never reactivate when an account returns.
- Live workspace-type defaults and replacement platform overrides are separate from tighten-only local/key policies. All applicable limits compose; absent child limits inherit, never remove or reserve allowance. There are no installation-wide limits (no installation budget, rate, concurrency or job limit; removed in migration 0026): limits exist only on personal/team/project workspaces and keys, and installation-wide spend is visible only through a non-blocking alert. Do not reintroduce a global limit row that every admission touches. Increasing limits never resets spending.
- Catalog availability follows live type defaults unless replaced per workspace. Catalog-sourced model selection requires current catalog eligibility; direct platform assignment is independent. Removal must not silently resurrect old selections when later reassigned.
- Optional cost-center assignment is platform-controlled and snapshotted at admission; changes affect future usage only, never rewrite history.
- Inference API keys are not management credentials.
- Never store or log plaintext upstream credentials, inference tokens, or prompt/response bodies by default.
- Provider credentials are references; validate secret access and upstream endpoints before implementing resolution/execution.
- Client protocols and upstream providers are different layers. Do not silently discard unsupported capabilities.
- Add providers through `ProviderAdapter` and `ProviderRegistry`; do not add provider switches to the engine. Run shared and provider-specific mock contract tests.
- Dropping an inference future/stream must cancel upstream work. Never fabricate successful stream termination, retry implicitly, or treat unknown usage as zero. Failover requires explicit policy and must never occur after a stream is returned.
- Global catalog changes use the exclusive catalog transaction lock; admission takes its shared form before the singleton installation row. Admission/scoped configuration/settlement still serialize on that installation boundary (authorization and policy edits versus admission; it guards no limit data) until scale plan P3 replaces it with scoped locks; every upstream attempt needs its own durable reservation. Unknown cost retains holds. Never delete unknown reservations or mutate immutable pricing/ledger history.
- Monetary API values are integer micro-USD strings, never JavaScript/Rust floats. Prices are estimates, not provider invoices. Cache aggregates overlap their write allocations and must not be double charged. Missing rates/usage are not zero; a known held floor is not a proven upper bound. Embeddings are input-only workloads, not synthetic chat requests.
- Local HTTP endpoints require explicit server-controlled approval and pinned permitted destinations. Disable redirects, ambient proxies and implicit retries; never forward inference credentials upstream. Cloud connections require HTTPS.
- Routes that are not implemented must stay explicit about that; no fabricated inference responses or dashboard metrics.
- Production is one Rust service serving APIs and optionally the built SPA, with no Node.js runtime. Inference goes directly to Rust.
- `GATEWAY_WEB_DIR` points to a built `apps/web/dist` directory; unset means API-only. Invalid configured distributions must fail startup.
- Axum owns `/api/*`, `/v1/*`, and `/health/*`. SPA fallback must not swallow unknown API paths or missing assets.
- Browser API requests use same-origin relative URLs, never absolute API URLs. Never put secrets in public `VITE_*` variables.

## Web workflow

Install with `npm ci` and run `npm run dev:web`. Vite listens at `127.0.0.1:3000` and proxies `/api/*`, `/v1/*`, and `/health/*` to `GATEWAY_INTERNAL_URL` (default `http://127.0.0.1:8080`), a server-only setting loaded from `apps/web/.env.local`.

For a local production-style preview, run `npm run build:web`, then `GATEWAY_WEB_DIR=apps/web/dist cargo run -p open-model-gateway -- serve`. The UI is on the gateway port. Dashboard sign-in uses Rust-owned OIDC sessions; see `docs/identity.md` and `docs/management-api.md`. Inference keys never authorize management. Native protocol support/limits are in `docs/protocol-matrix.md`.

## Deployment

- `Dockerfile` builds the SPA and Rust binary into a non-root runtime. Never run migrations or bootstrap automatically in `serve`/the container entrypoint.
- `deploy/staging` and `scripts/staging.py` use a separate Compose project/volume, not the demo. Generated `.local/` secrets/backups must remain ignored and outside image build contexts.
- Database schema changes or new SQL operations must update and test `deploy/staging/runtime-grants.sql`; runtime has explicit table/column ACLs, never owner credentials. Preserve append-only price/ledger/audit access and protected platform-user columns.
- Real OIDC/provider acceptance is opt-in; no paid smoke requests, example credentials, or production privilege relaxation in automated tests.

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
npm run test:container
npm run test:staging
```

`npm run test:web` runs Vitest unit tests; do not claim browser tests completed without running them. Use real database integration tests for tenancy constraints and authorization queries. Use mock upstream servers for provider contract tests; do not incur paid provider requests in CI. Keep the roadmap and README honest as features land.

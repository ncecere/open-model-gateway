# Repository guidance

The single-enterprise rebuild is in progress. `docs/enterprise-rebuild.md` records the approved product decisions and scope; it supersedes legacy multi-organization assumptions. Implement the new schema on fresh, explicitly selected databases only. Never reset or migrate an old installation implicitly.

## Execution cadence

Keep work moving, and prove each change before it lands. The bar was raised on 2026-10-21 after the LiteLLM incidents (a scanner action stealing a publish token, malicious wheels from a compromised build, a config API reachable by any logged-in user): speed never outranks verification.

One GitHub issue at a time per worker, one loop per issue:

1. Branch from current `main`. Write a failing test that shows the problem (or, for a feature, the behaviour that doesn't exist yet).
2. Make the smallest change that passes it.
3. Run the checks below for the areas the change touches (browser tests for UI flows, a real PostgreSQL database for SQL/authorization changes).
4. **Prove it on the real path**, not only with unit tests and mocks: run the changed flow end to end against a running gateway (the local stack, or a `dev-env` cluster environment for chart, migration and multi-replica behaviour) and check the result, including the refusal cases.
5. **Review.** Every change gets a code review. A security-relevant change (authentication, authorization, egress, secrets and credentials, SQL and runtime grants, migrations, crypto, CI/supply chain, anything parsing untrusted input) gets two independent reviews, one of them adversarial (its job is to break the change), from different models. Fix actionable findings.
6. **The coordinator reads every diff before it merges**, including code written by agents, and re-runs whatever it doubts.
7. Open a PR. `main` is protected: merge only through a PR with every required check green, then close the issue.

- Releases: the coordinator decides when a minor or patch release is ready; the user decides v1.0.0. Before tagging: the full local gates, the staging rehearsal, the release notes checked against the tag (versions, migration ranges, breaking changes) and a candidate run on `dev-env`. After tagging: deploy to the homelab, run the end-to-end journeys (sign-in, keys, inference on each configured provider, admin flows, the new features), and file UI/UX findings as issues.
- **v1.0.0 requires an independent security review** (#61) and a clean authorization matrix (#195).
- Do not add bespoke evidence files, source hashing or copied candidate trees. CI, the PR and the issue are the record.
- Do not expand scope: no side investigations inside an issue. File a new issue instead. `backup/held-issue-work-b7f1d10` is a read-only archive of unaccepted work; do not port from it.
- Pause only for a real blocker: an unresolved failure, an ambiguous requirement, or an action outside this loop (paid provider calls beyond a small capped smoke test on the homelab, production deployment, existing installations, other namespaces/repositories). Never skip tests, weaken assertions or hide failures to keep moving.
- Updates are short: what is done, what is next, any blocker.

### Supply chain

- Pin every GitHub Action by commit SHA and every image by digest (`npm run test:supply-chain`). Tools downloaded at run time are pinned by version and checksum.
- Third-party actions never run in a job whose token can write (packages, contents, id-token): scanning and checking happen in read-only jobs; publishing and signing jobs run only the minimum pinned steps they need.
- Dependency updates are reviewed like code: read the changelog and the diff of anything with network, crypto or build-time execution.

## Layout

- `apps/gateway`: Rust modular monolith; owns all database access and authorization.
- `apps/web`: React + Vite SPA with TypeScript, TanStack Router + Query, and Bitop UI components/tokens; registry source is copied into this repository, not imported across local projects. Never accesses PostgreSQL directly.
- `docs/architecture.md`: security boundaries and design decisions.
- `docs/roadmap.md`: implemented versus planned behavior.
- `docs/provider-adapters.md`: engine/adapter contracts, supported request subset, and secret configuration.

## Invariants

- One installation serves one enterprise; there are no selectable organizations. Workspaces are the resource/security boundary. Team and Project are equivalent sibling shared kinds; personal workspaces are owner-private. Enforce workspace relationships with scoped foreign keys and live authorization.
- Inference keys belong to one workspace. Derive tenant and workspace from validated credentials, never client-selected headers or body fields.
- Personal workspaces cannot be shared. Platform Admins/Auditors may see personal usage/cost totals, never another owner's keys or request details. Shared-workspace members see their own activity; workspace admins see workspace-wide activity (files, batches and video jobs follow the workspace's object visibility: per user by default, or workspace-wide; deleting or cancelling always needs the creator, an admin or a service key). Global metadata authority does not satisfy human-key membership requirements.
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
- Global catalog changes use the exclusive catalog transaction lock; admission takes its shared form first. Then the canonical scope order (`governance::locks`, docs/governance.md): authority advisory locks type → workspace → user → key lineage (shared in admission, exclusive in any management change of that scope, taken up front before the first write; triggers back this up) → reservation row → totals/counter rows in primary-key order. Admission and settlement never take the installation row (management-versus-management only), except under `GATEWAY_ADMISSION_MODE=global`. No `FOR SHARE` on authority rows in the hot path. Every upstream attempt needs its own durable reservation. Unknown cost retains holds. Never delete unknown reservations or mutate immutable pricing/ledger history.
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

## Running a branch on the cluster

A branch can be tested as a running copy of the local demo on the bitop-talos homelab cluster instead of on the laptop. It is removed automatically after 8 hours.

1. Push the branch. `.github/workflows/branch-images.yml` builds `ghcr.io/ncecere/open-model-gateway:dev-<first 12 characters of the commit>` (amd64 only; it does not wait for `ci.yml`, and it skips the staging rehearsal, scan and signature that `image.yml` runs). A branch cut before that workflow existed has no image: rebase it onto `main`.
2. Create the environment with the `dev-env` tool from the homelab checkout, then forward it:

   ```sh
   DEVENV=~/homelab/bitop-talos/tools/dev-env/dev-env
   $DEVENV up omg my-name --ref my-branch   # ~75 s; checks the image exists first
   $DEVENV forward omg my-name              # http://127.0.0.1:<port> and the issuer on 127.0.0.1:18084; leave running
   $DEVENV creds omg my-name                # the demo accounts
   $DEVENV renew omg my-name --ttl 8h       # also: list, status, down
   ```

   `--ref` is a branch, tag or commit of this clone, or a release such as `v0.4.1`. The tool deploys the Helm chart of that ref, so a chart change is tested too. Always `down` an environment you are done with. Names are 2-20 characters (lowercase letters, digits, dashes). The tool needs `kubectl`, `helm` and Python 3 with PyYAML.

What it does is `docs/local-demo.md` in Kubernetes, in the order the invariants above require (nothing migrates or bootstraps from `serve`): it creates the `gateway_runtime` role, runs the chart's migrate Job (`migrate` and `deploy/staging/runtime-grants.sql`), runs `bootstrap-demo`, then starts the gateway. Sign in at the forwarded address with SSO as `operator@demo.invalid` (Platform Admin), `auditor@demo.invalid`, `alex@demo.invalid`, `blair@demo.invalid`, or `unentitled@demo.invalid` (authenticates, platform access denied); see `docs/local-demo.md` for what each can do.

- The demo is passwordless and loopback-only by design: when `GATEWAY_OIDC_ISSUER` is the demo issuer the gateway requires a loopback listener and a loopback database host, and `bootstrap-demo` seeds the personas under `http://127.0.0.1:18084`. So the gateway, a Postgres forwarder and the demo issuer run in one pod and are reached by `kubectl port-forward` only; there is no ingress. Never expose it or register its issuer anywhere: anyone who can reach the issuer can sign in as the platform admin.
- The issuer is forwarded to local port 18084, so only one omg environment can be forwarded at a time.
- Only `dev-omg` is used. Never deploy, apply or edit anything in another namespace to test a change. The environment has no resource limits and low scheduling priority; it yields to production, so do not leave it running.
- Postgres is version 18 (CloudNativePG). OMG's CI and `deploy/staging` use PostgreSQL 17, so a result that depends on the server version needs checking against 17 locally.
- Example connections and deployments start disabled, as in the local demo, and there is no model server or provider: nothing here is paid or live-provider acceptance.
- Secrets are generated per environment and stay in the cluster. Do not print or commit them.

Only environments exist. The Rust and web test suites, the container and staging tests, and the browser tests still run locally or on GitHub (see Checks); running them as Jobs on the cluster is planned.

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

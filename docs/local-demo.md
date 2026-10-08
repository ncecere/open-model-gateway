# Local enterprise demo

The demo is development-only and passwordless. Never register its issuer with a real deployment or point it at existing data. It uses a new Compose project/volume and the dedicated `gateway_enterprise_demo` database; it does not reset, migrate or reseed legacy databases.

## Accounts

Open **http://127.0.0.1:3000**, select SSO sign-in, then choose an account:

| Account | Email | Access |
| --- | --- | --- |
| Platform Admin | `operator@demo.invalid` | Installation configuration, shared-workspace administration and financial totals; no foreign personal keys/details |
| Platform Auditor | `auditor@demo.invalid` | Read-only Admin configuration, audit and aggregate costs; own personal workspace |
| Team Admin · Alex | `alex@demo.invalid` | Own personal workspace; owner/administrator of Product Team and Research Project |
| Platform User · Blair | `blair@demo.invalid` | Own personal workspace; member activity/own keys in Product and Research |
| Unentitled SSO user | `unentitled@demo.invalid` | Authentication succeeds, platform access is denied; no personal workspace |

Sign out through the account menu and sign in again to switch personas. Platform role and workspace membership are independent. Admin/Auditor can switch Workspace/Admin portals; Alex and Blair have Workspace only.

The issuer signs explicit group claims. Rust reconciles them against persisted mappings, preserving separate manual memberships. Alex's owner grants demonstrate that a lower group-derived membership does not overwrite a manual source. Personal workspaces are created only on actual entitled sign-in—not by fixture seeding.

## Start a fresh demo

Run from the repository root. If a matching database already exists, preserve it; do not delete/reset a volume to obtain new fixtures.

```sh
npm ci
docker compose -p omg-enterprise-demo -f deploy/demo/compose.yaml up -d --wait postgres
mkdir -p .local/enterprise-rebuild
cp .env.demo.example .local/enterprise-rebuild/demo-bootstrap.env
chmod 600 .local/enterprise-rebuild/demo-bootstrap.env
set -a
source .local/enterprise-rebuild/demo-bootstrap.env
set +a
cargo build -p open-model-gateway
./target/debug/open-model-gateway migrate
./target/debug/open-model-gateway bootstrap-demo
python3 scripts/demo-runtime.py
npm run build --workspace @omg/web -- --outDir ../../.local/enterprise-rebuild/web-dist
```

In one terminal:

```sh
OMG_ENABLE_LOCAL_DEMO=1 node scripts/demo-oidc.mjs
```

In another, source the private, restricted runtime environment (not the bootstrap account):

```sh
set -a
source .local/enterprise-rebuild/demo-runtime.env
set +a
./target/debug/open-model-gateway serve
```

The tracked demo Compose file binds PostgreSQL to **127.0.0.1:54349**, separate from the disposable regression cluster on 54339 and legacy development PostgreSQL on 54329. The gateway and issuer bind 3000 and 18084 respectively. There is no production login bypass.

`GATEWAY_ENV_FILE` selects the demo environment instead of implicitly loading an existing root `.env`. Unset `GATEWAY_OIDC_CLIENT_SECRET` before launching this public demo client. The runtime helper writes a 0600 private environment, refuses existing roles/files, and verifies the explicit runtime grants with rollback-only probes. Never print or commit that file. Serving uses this restricted role, not the bootstrap account. The test suite's explicit `runtime_privileges` probe verifies the allowlist using rollback-only fixtures on a disposable cluster; do not run that role/maintenance-ACL probe against real PostgreSQL.

## What to test

- Workspace/Admin switching, grouped Personal/Teams/Projects selector, breadcrumbs, responsive sidebar, color mode and command palette.
- Create shared workspaces from Admin, not the switcher. Membership in one Team does not grant access to a sibling Project.
- Create/revoke/rotate your own human keys, inspect one-time disclosure without recording secrets, and test service-account keys in shared workspaces. Global metadata authority without membership cannot mint a human key.
- Compare type catalog inheritance with Research's replacement override. Catalog availability enables self-service selection, not automatic selection of every model. Direct assignments survive unrelated catalog loss; retired key selections never revive.
- Configure live limits, immutable price versions, explicit model protocols and optional cost centers. Assignments affect future admission snapshots, not historical records.
- Verify Blair cannot manage Product or view Alex's activity, and Auditor cannot mutate platform configuration or open foreign personal details.
- Check empty cost reports and coverage labels. No fake request/cost data is seeded.

Example connections/deployments start **disabled**. Local HTTP approval is explicit in `.env.demo.example`; approval does not mean a server exists or a model is certified. Example prices are not vendor prices. Configure an actual local endpoint/model and enforced input bounds before enabling inference. Paid-provider tests require separate explicit approval and server-held credentials.

`bootstrap-demo` is transactional and idempotent after its own successful seed marker. It preserves edits, inactive users and history; it refuses unrelated existing users or email collisions. The old organization-persona upgrade flag is unsupported. Restart issuer and gateway together: the demo signing key is ephemeral, while live JWKS refresh remains future work.

To stop without deleting data:

```sh
docker compose -p omg-enterprise-demo -f deploy/demo/compose.yaml stop
```

Stopping the issuer/gateway is separate. Do not use `down -v` as a recovery or upgrade procedure.

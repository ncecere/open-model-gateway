# Explore the dashboard locally

Open **http://127.0.0.1:3000**, choose **Sign in with your organization**, then select a demo account:

| Picker account | Email / OIDC subject | Platform admin | Gateway demo organization role | Product team role |
| --- | --- | --- | --- | --- |
| **Platform Admin** | `operator@demo.invalid` / `operator` | Yes | Owner | Owner |
| **Organization Admin** | `orgadmin@demo.invalid` / `orgadmin` | No | Admin | Member |
| **Team Admin · Alex** | `alex@demo.invalid` / `alex` | No | Member | Admin |
| **Member · Blair** | `blair@demo.invalid` / `blair` | No | Member | Member |

These are initial seed roles, not roles assigned at each login. All four accounts have their own private personal workspace. There are no demo passwords.

- **Platform Admin** configures the global catalog, providers, deployments, pricing and routing directly at platform scope, assigns models and hard ceilings to organizations, and manages organization/team/project/user directories.
- **Organization Admin** administers the demo organization's teams/projects, members, assigned model access, and local policies within the platform ceiling. It does not configure providers, deployments or routes. Its direct Product membership is only `member`; organization administration is a separate permission source.
- **Team Admin · Alex** administers Product and its keys, service accounts, and members, not the organization or platform.
- **Member · Blair** uses a personal workspace and Product, managing their own keys/activity without administrative privileges.

Use the workspace switcher to select **Product** for shared-team features. As Platform Admin, **Admin → Platform** contains **Organizations**, **Teams**, **Projects**, and **Users**, with global Models and Oversight configuration available without selecting an organization. Teams lists shared teams across organizations with a parent-organization filter; open Organizations → Members for scoped membership management. **Users** is the platform-wide identity directory: a user is a global login identity, while organization and team memberships independently determine scoped access. Removing a membership is not deleting that identity. The issuer supplies a verified email and stable subject, not authorization roles; Rust reads permissions from persisted memberships. Organizations/teams/projects are created from their page buttons, never the context selector. Alex manages members, service accounts and limits through Product → Workspace settings; only the platform operator has an Admin portal. Organization administrators use Organization settings inside Workspace. No administrator can open somebody else's private personal workspace.

The header's **Search or jump to…** control (⌘K / Ctrl+K) searches permitted pages and your accessible contexts. **Refresh access** is available there and in the account menu. The account menu displays your current access level; there is no full-width bottom footer.

To compare accounts, use the bottom-left account menu → **Sign out**, then sign in and choose another persona. Selecting another workspace only changes context, not the signed-in identity.

The account-picker experience follows open-rag-system, but uses this gateway's existing OIDC/PKCE/nonce and server-side session flow rather than adding a production authentication bypass.

## Start again

Requires the existing local Docker PostgreSQL service, Rust, and Node for the demo issuer and frontend build. Run these from the repository root. Port 3000 must be free (do not also run Vite on that port).

```sh
docker compose up -d postgres
# Run once; an "already exists" error means the demo database is present.
docker compose exec -T postgres createdb -U gateway gateway_demo
npm ci
npm run build:web
cargo build -p open-model-gateway
```

Terminal 1 — keep the explicitly opted-in, loopback-only demo issuer running:

```sh
OMG_ENABLE_LOCAL_DEMO=1 node scripts/demo-oidc.mjs
```

Terminal 2 — seed and serve the compiled dashboard with Rust:

```sh
set -a
. ./.env.demo.example
set +a
unset GATEWAY_OIDC_CLIENT_SECRET
target/debug/open-model-gateway migrate
target/debug/open-model-gateway bootstrap-demo
target/debug/open-model-gateway serve
```

`bootstrap-demo` creates all four personas on a fresh, empty database. If the `gateway-demo` organization already exists, the default command is a strict no-op: it will not reset edits, memberships, disabled users, linking state, or revoked keys. It requires development mode and the dedicated **gateway_demo** database on a literal loopback host, plus a loopback HTTP bind. The server applies the same guard when configured with this demo issuer. Keep any production `.env`/credentials out of the demo checkout. The issuer starts before the gateway; if you restart the issuer, restart the gateway too because its signing key is ephemeral and the gateway currently discovers JWKS at startup.

### Upgrade an existing three-account demo without resetting it

With the environment from Terminal 2 loaded and a binary containing this upgrade:

```sh
target/debug/open-model-gateway bootstrap-demo --add-missing-personas
```

This explicit, transactional upgrade only adds `orgadmin@demo.invalid` when that email is absent (case-insensitive). It requires an **active** organization with slug `gateway-demo`; a missing, renamed, or disabled organization fails without changes. It never deletes or resets data, restores revoked keys, or modifies existing users' privileges/memberships. Concurrent upgrades are serialized.

If the email already exists—even disabled, demoted, unlinked, or belonging to another tenant—the upgrade is a **no-op**. It does not adopt, re-promote, revive, or relink that identity. The command reports this; it does not guarantee that an existing email can sign in or has the sample roles.

A newly added Organization Admin receives a private workspace, hashed sample keys with discarded secrets, a $5 illustrative personal budget, and grants only for currently enabled, personal-enabled models in the demo organization. Product receives only the new user's member relationship and sample team key, not changes to its existing grants or policy. If Product was renamed, the upgrade prefers Product by name, otherwise chooses an active team in that organization; if none exists, it omits team membership and the team key. It never creates, renames, or revives a team during upgrade. No sessions, identity links, usage, or audit history are fabricated.

After updating the issuer script and gateway binary, restart **both together**, issuer first, to refresh the cached JWKS. Then sign out and back in to choose **Organization Admin**. Do not delete the database to obtain the fourth account.

## What is real versus sample

The demo contains persisted example organizations, workspaces, users, a platform-owned catalog explicitly assigned to the demo organization, separate platform/local policies, and **illustrative price versions**. Projects can be created from their directory and use the same shared-workspace capabilities as teams. Changes you make exercise real management APIs and survive restarts. Provider connections and deployments start **disabled**, no usable provider credentials are supplied, and seeding never prints API-key secrets. Create or rotate a key through the dashboard if you want to explore the one-time-token flow.

There are **no fabricated executions, usage, costs, health observations, or invoices**. Empty activity/cost screens are intentional. Example prices and token ceilings are illustrative, not provider rate/capability claims. Replace both with accurate rates and hard input/output bounds before enabling real traffic. Enabling a real provider and supplying credentials is a separate action and may incur real charges.

## Safety boundary

This is passwordless **local exploration**, not secure public authentication. Never expose ports 3000/18084 through a tunnel, public bind, or reverse proxy; never register the demo issuer against real data. Production authentication still requires a real OIDC identity provider. The fixed demo issuer rejects unknown hosts, redirects, accounts, and cross-origin account-selection requests; authorization codes are short-lived, single-use, PKCE-bound, and signed.

Stop both foreground commands with Ctrl-C. To discard all demo changes, stop the gateway first and explicitly remove only its disposable database:

```sh
docker compose exec -T postgres dropdb -U gateway gateway_demo
```

Then repeat database creation, migration, and seeding. Do not run this against another database.

# Workspace dashboard

`apps/web` is a React/Vite SPA using the session-authenticated [management API](management-api.md). Rust owns authorization and data access; all requests are relative/same-origin. Built assets can be served by Rust without a production Node.js proxy. Inference keys/provider secrets are not dashboard login credentials.

## Workspace and Admin

The interface uses locally installed/locked Bitop components and Grounded's **Workspace/Admin** composition: sidebar, context selector, resource lists/details, breadcrumbs, pill tabs, settings forms and dialogs. Reference projects stay read-only with no cross-repository runtime imports. See [Bitop provenance](bitop-ui.md).

**Workspace** contains Overview, Models, API keys, Usage & costs and Workspace settings. Shared settings group members, invitations, service accounts and limits by capability. Personal workspaces never offer shared members/service accounts.

**Admin** is installation-scoped: Overview; Users, Teams, Projects, SSO groups; Provider connections, Models, Deployments, Catalogs; Costs, Pricing, Limits, Cost centers; Audit log. Platform Admins can write; Auditors enter read-only. Ordinary Platform Users do not receive the Admin portal from personal ownership. Model routing is on model details and deployment thresholds/pricing on deployment details; standalone pricing/routing routes also exist.

The selector groups **Personal / Teams / Projects**, switching only accessible workspaces. There is no organization picker/settings or organization navigation. Teams/Projects are siblings; creating them belongs on the Admin directory pages, not in the selector. Platform financial reports can include personal totals, but those records never become selectable foreign private workspaces for key/request browsing.

## Canonical links

Identifiers are UUIDs, not organization paths or public model aliases:

| Destination | URL |
| --- | --- |
| Admin overview | `/admin` |
| Shared directories/details | `/admin/teams[/{uuid}]`, `/admin/projects[/{uuid}]` |
| Users/model/connection/catalog lists/details | `/admin/users`, `/admin/models`, `/admin/connections`, `/admin/catalogs`, optionally `/{uuid}`; routes are `/admin/routes/{uuid}` |
| Platform controls | `/admin/limits`, `/admin/cost-centers`, `/admin/costs`, `/admin/sso-groups`, `/admin/audit`, `/admin/pricing` |
| Old URLs (rewritten in place) | `/admin/providers[/{uuid}]`, `/admin/policies`, `/admin/oidc`, `/admin/deployments/{uuid}` → the new paths; `/admin/deployments` and `/admin/routing` → `/admin/models` |
| Workspace overview | `/workspaces/{uuid}` |
| Workspace keys/models/costs/settings | `/workspaces/{uuid}/keys`, `/models`, `/costs`, `/settings` |
| Profile/invitation acceptance | `/profile`, `/invitations/accept` |

The router and location helpers are `src/router.tsx` and `src/lib/locations.ts`. Tabs use `?tab=`; catalog search/status/pagination and cost dates/filters are URL-backed. Recognized root navigation metadata is canonicalized, but removed organization URLs/`org` contexts are not retained as an organization compatibility feature. Invalid/unavailable workspace navigation is restored to an authorized destination rather than used to fetch foreign data.

Per-user portal/workspace memory stores navigation metadata only and is rechecked against live access. Search (⌘K/Ctrl+K) jumps to accessible destinations/session inventory, not prompts, secrets or private activity. Refresh access, Profile, invitation acceptance and Sign out are account actions. Navigation focuses the destination heading.

## Workflows

- **Platform setup:** approve endpoint/credential mechanism → create global alias and declare protocols → connect deployment → publish exact price/bounds and review routing → place model in catalogs/defaults or direct assignment. Configuration existence is not upstream readiness; there is no automatic paid probe.
- **People:** provision explicit entitlement or generic group mappings → create Team/Project with entitled owner → add independent manual/group memberships or invitations. Signed groups sync at sign-in; use manual suspension for immediate loss. Thirty-day lifecycle cleanup retains financial history.
- **Workspace:** owner/shared admin selects available catalog models; direct assignments remain separate. Issue a human key only with actual membership, or administer shared service-account keys. Inspect permitted activity and effective/local/platform policy provenance.
- **Costs:** choose UTC start/end (end exclusive, max 93 days), filters/comparison; inspect exact daily/breakdown amounts, known settled costs, active holds, coverage and overlapping health. Platform view has no private detail export. Workspace CSV is one explicit bounded live page.

## Sensitive state and exact values

`/me` provides actual `role`, `membership_source` and scoped capabilities. Inherited platform administration is not labelled membership. Capabilities are presentation snapshots; server mutations and admission recheck live authority. Current authorization snapshots partition read caches and remount dialogs; revoked access aborts requests and clears stale data.

Key/invitation plaintext is displayed once in transient dialog state, outside query/mutation caches. Closing/changing scope or losing access destroys it. It must not enter URLs, logs or browser storage. Clipboard copy is deliberate; clear the clipboard after secure storage. Human keys expire in 1–365 days; rotation retains restrictions/budget lineage. Admins can revoke another shared human key, not rotate/mint it. Re-enabling a service account does not revive its revoked keys.

Price/budget forms accept exact decimal USD; API amounts are integer micro-USD strings. New prices publish v2 with all four explicit cache-rate states, distinguishing unknown, priced zero and not-applicable. Read/write/default/TTL categories are not double-charged. V1 charges remain a separate report component. Unknown usage/cost is not zero; positive partial floors are not finite bounds. See [cache pricing](cache-pricing.md).

Provider forms accept allowlisted references or explicit local no-auth, never plaintext secrets. Bedrock uses workload identity/region; local profiles require server-approved pinned endpoints. No universal custom-server/SDK compatibility is implied. Ollama native embedding follow-up is not a fresh browser/live-server acceptance claim here.

## Validation boundaries

Useful checks are `npm run typecheck:web`, `npm run test:web`, `npm run build:web`. Unit/source-render tests are not browser end-to-end or accessibility certification. Inventory/choice controls remain bounded and some eagerly load accessible selections; this is not an unlimited search capability. Historical [verification](verification.md) and [local demo](local-demo.md) evidence must be identified by its lineage/date; this documentation refresh did not rerun those checks or declare production readiness.

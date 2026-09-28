# Workspace dashboard

The React application in `apps/web` uses the browser-session [management API](management-api.md). Requests are relative and same-origin; Rust owns authorization and database access. Inference keys and provider secrets never authorize the dashboard. Production serves built Vite assets from Rust, without a Node.js server.

For the four local OIDC personas, see [local demo](local-demo.md). The demo is not a production authentication fallback.

## Workspace and Admin

The interface follows Grounded's resource-oriented navigation using locally installed, locked Bitop components. See [Bitop sources and refresh instructions](bitop-ui.md). The reference checkouts are read-only; there are no cross-repository runtime imports.

**Workspace** is for everyday work: Overview, Models, API keys, Costs, and Workspace settings. Organization administrators also see **Organization settings**. Team/project administration stays within Workspace settings; it does not require entering a platform portal.

**Admin** is reserved for platform operators. Its context is fixed to the platform, with Overview, Organizations, Teams, Projects, Users, Models, Provider connections, Deployments, and Platform audit. Organization objects opened here retain platform context. Model routing lives on model details; deployment routing and immutable pricing live on deployment details. Organization assignments and hard ceilings live on the organization object. Older selector landing routes remain supported.

Organization settings retain the workspace sidebar and selector, using the last accessible workspace visited in that organization. A fresh settings link defaults to an accessible workspace in that same organization, preferring the caller's personal workspace. This is navigation context only: the settings URL, breadcrumbs, API requests, and authorization remain organization-scoped.

The workspace selector switches existing organizations and the caller's accessible personal/shared workspaces. Creation belongs on resource pages, never inside the selector. Teams and Projects remain sibling workspaces under an organization; personal workspaces remain owner-private, even from operators. Platform User and shared-workspace directories do not expose private workspace metadata.

The portal separation does not change authority: platform and organization administrators retain their existing inherited shared-workspace authority. No auditor/editor role, impersonation, break-glass access, or broader credential authority is added.

## Resource pages and links

Examples of canonical destinations:

| Destination | URL |
| --- | --- |
| Platform overview | `/admin` |
| Organization administration | `/admin/organizations/{org}` |
| Contextual organization settings | `/organizations/{org}/settings` |
| Workspace overview | `/organizations/{org}/workspaces/{ws}` |
| Workspace keys/models/costs/settings | workspace URL plus `/keys`, `/models`, `/costs`, or `/settings` |
| Model details | `/admin/models/{model}` |
| Provider details | `/admin/providers/{provider}` |
| Deployment details | `/admin/deployments/{deployment}` |

Resource headers, linked breadcrumbs, and pill tabs retain context. Only the active panel mounts. Tabs use `?tab=`; policy views/scopes, selected keys, and delegation recipients are also URL-backed. Catalog search/status filters and pagination survive reloads. Unknown or unavailable explicit scopes are not replaced with a different organization's data.

Legacy `/?page=…&org=…&ws=…` links translate to contextual destinations. The portal switch remembers an authorized destination per user/portal; the sidebar also remembers the last workspace per user/organization. Both use navigation metadata only. It does not store search text, form values, invitations, or API tokens. Authentication loss/logout clears remembered destinations and query state. Remembered destinations are checked against current access before reuse.

The header **Search or jump to…** (⌘K / Ctrl+K) searches accessible destinations and session inventory, not prompts, secrets, activity, or the entire user directory. Arrow keys/Enter select; Escape dismisses. **Refresh access** is in search and the sidebar account menu. Search navigation restores focus to the destination heading. The account menu also contains Profile, Accept invitation, and Sign out; there is no full-width footer.

## Membership and authority

`/api/v1/me` reports actual `membership_role`, effective `role`, `authority_source`, and scoped action `capabilities`. Inherited administration is labelled as such rather than presented as direct ownership. Human key issuance requires active organization membership and direct active shared-workspace membership. Service-account administration is a separate capability.

Capabilities are presentation snapshots, not authorization credentials. Mutations still recheck current roles and memberships under database locks. Read-cache partitioning and dialog teardown include membership/capability changes even when the effective role stays the same. Older payloads are handled conservatively; the legacy key form may verify direct membership through the authorized member list.

## Workflows

- **Platform setup:** configure an allowlisted provider credential reference → create a stable model alias → add a deployment → publish exact USD pricing/bounds and review routing → assign the model to an organization. Detail links preserve known relationships, and deployment creation can be scoped to its model or provider. Configuration checks report record existence only, never verified credentials or upstream readiness. They send no probes or inference.
- **Organization setup:** review/invite members → create a Team or Project → add direct members → delegate platform-assigned models → review local limits. Invitations still require secure out-of-band delivery; no email sender is implied. Organization administration requires actual organization membership to create an owned shared workspace, including for operators.
- **Workspace use:** review effective model grants → issue an eligible human/service key → inspect authorized usage and configured-rate costs. Missing grants point members to an administrator rather than an unavailable grant action. Workspace settings group Members, Service accounts, and Limits by permission. Personal spaces never offer shared members or service accounts.

Platform catalog lists support server-side literal text search and enabled/disabled filtering before pagination. Deployment lists can be scoped by model or provider. Direct resource GETs resolve detail pages without scanning every collection page. Other inventories remain bounded/paginated; session inventory and some selection controls still eagerly load accessible choices (maximum 20,000 before an explicit error). This is not completion of the full inventory/search roadmap.

## Safety-sensitive behavior

- API keys expire in 1–365 days. Inherit/selected/no-model restrictions remain fixed at issuance, only narrow current grants, and follow the key's budget lineage through rotation. Administrators can revoke another human's key, never rotate it. Service-account disablement permanently revokes its keys.
- Key and invitation responses bypass query and mutation caches. Tokens exist only in temporary dialog state; closing, changing scope/tab/selected record, losing access, or signing out removes them. They never enter URLs, logs, local storage, or session storage. Copying deliberately writes to the clipboard; clear it after secure storage.
- Edited forms ask before discarding through their close/cancel controls and warn before browser unload. Scope/access changes still destroy transient forms instead of retaining sensitive state in an unauthorized context.
- Platform ceilings, additional organization restrictions, and workspace/key limits stay separate. Child caps share parent allowance rather than reserving a slice. Non-organization administrators may only tighten durable existing caps. Unknown usage/cost is not zero; holds remain conservative.
- Budgets and prices use exact decimal US dollars with up to six fractional digits; API values remain integer micro-USD strings. Price versions and ledger history are immutable. Costs are estimates, not provider invoices. See [governance](governance.md).
- Members see their own human-key activity; administrators see authorized workspace activity. No prompts or generated content appear in execution history. Cost CSV export remains one explicit bounded page, not a complete invoice or stable snapshot.
- Provider forms accept allowlisted references, never plaintext secrets. Bedrock uses workload identity and an explicit region. No custom-provider support or automatic paid test has been introduced. See [provider adapters](provider-adapters.md).

## Validation

Run `npm run typecheck:web`, `npm run test:web`, and `npm run build:web`. Unit/source-render tests are not browser end-to-end tests; [verification](verification.md) records browser evidence separately. Local checks do not certify full accessibility, enterprise OIDC, provider compatibility, or production readiness.

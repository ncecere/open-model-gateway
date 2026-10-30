# Platform administration

## Ownership

One gateway installation is one enterprise. There is no organization picker, organization membership or tenant-specific model alias. Platform identities and sibling workspaces replace that hierarchy:

```text
Enterprise installation
├─ users / OIDC / platform roles
├─ providers → deployments → models / prices / routing
├─ catalogs / policy defaults / financial reporting / cost centers
└─ workspaces
   ├─ Teams
   ├─ Projects
   └─ private personal workspaces
```

Only Platform Admins create Teams/Projects. Each shared workspace has an explicitly assigned entitled owner; a Project does not inherit a Team's people or access. An entitled user's successful sign-in creates their personal workspace automatically.

| Role | Authority |
| --- | --- |
| Platform User | Own personal workspace; shared access only through membership. |
| Platform Auditor | User access plus platform-wide read-only configuration, financial totals and privacy-filtered audit. |
| Platform Admin | User access plus installation, user, shared-workspace, catalog, policy, infrastructure and allocation administration. |

Admin/Auditor includes User entitlement. Neither grants another user's private keys or request details.

**Personal workspace visibility.** Platform Admins and Auditors may see a personal workspace's identity (workspace id and owner label) with its usage, cost and storage totals and its platform-managed configuration (workspace policy and limits, catalog overrides). They never see its keys, request or execution details, prompts or responses, sessions, file contents or anything the owner chose. Provider-cost reconciliation is deliberately stricter: its personal subset is identity-free. The platform cost report, usage explore and storage usage list personal workspaces with their id (cost and usage name the row `Personal · <owner>`; storage shows the workspace's own name and kind), and `GET/PUT /platform/workspaces/{ws}/policy` and `/catalogs` work on a personal workspace (Admin and Auditor read, Admin writes). A catalog override is platform-assigned per workspace (the catalog API's `own_choice` fields count these overrides; they are not choices the owner made). The owner has no such route and reads only their own catalog; personal limits are platform-controlled.

Shared administrative visibility does not replace actual membership for issuing a human key.

## Configure model access

1. Configure an approved provider connection, global public model and deployment. Model `supported_protocols` must match the desired workload; adapter profiles narrow it further.
2. Publish immutable configured prices and accurate hard input/output bounds before traffic. Review residency, failure thresholds and explicit failover. A successful configuration write is not a provider probe.
3. Put approved models in catalogs and assign live catalog defaults by Personal/Team/Project kind.
4. Optionally replace one workspace's catalog list. Replacement is not an additive union; an empty replacement permits no catalog models. Delete the override to follow live defaults again.
5. Personal owners/shared administrators select models from available catalogs. For exceptions, Platform Admins add direct assignments, separately represented from catalog selections.

Model presence alone is not permission. Inference requires live authorization, enabled target configuration and the key's narrowing restrictions. Losing catalog eligibility retires its selections unless another eligible catalog remains; independent direct access can remain. Removed key selections are not revived by later regrant, and their restriction header remains deny-all when empty.

## Review deployment enablement changes

On a route page, **Review changes** previews the opposite enabled state before the existing Enable/Disable confirmation. The model's route menu also opens this review; retiring a route previews disablement before the existing retirement confirmation. Only browser-session Platform Admins can preview; Auditors remain read-only, and admin tokens cannot call this new endpoint.

Review the enabled-state diff, serving routes gained/lost, models losing their last serving route and affected active shared workspaces. Details are capped at 100 with a total and truncation notice; personal effects are omitted, not represented by an assumed zero. Catalog availability alone is not a selection. The preview respects live catalog inheritance/replacements and independent direct assignments.

Nothing is applied by opening or refreshing the review. **Continue to confirmation** opens the ordinary mutation confirmation; Cancel writes nothing. This is a configuration estimate, not a reservation, price/health test or inference admission simulation. Already-admitted requests may finish. Refresh if configuration changes: the preview returns the deployment enablement revision and observed dependency versions, but existing apply routes do not enforce revision preconditions yet. Rollback and bulk changes remain separate later slices. See [the API contract](management-api.md) for fields and refusals.

The route page's **History** tab is readable by browser-session Platform Admins and Auditors. It shows enablement before → after, the actor, timestamp and revision, with Older/Newer pagination and immutable IDs under Change details. Creation reads Not configured → Enabled/Disabled. Legacy events say **Values unavailable**: no values are reconstructed and there is no restore action in this slice. The history covers deployment enablement only, not pricing, routing, policies or secrets. Every actual state writer is captured by the database; unchanged writes create no history. Admin-token mutations retain token attribution even though tokens cannot read this browser-only history endpoint.

Migration **0067** and reapplied runtime grants are required. Existing deployments start at revision zero without invented historical snapshots. Deployment creation and every later actual enabled-state change are appended atomically with audit identity. Bootstrap/demo/SQL writes without a management actor are labelled System. Audit partition archival preserves snapshot values/IDs but can make actor attribution unavailable. Never apply the migration implicitly to an existing installation.

## Policy defaults and shared ceilings

Type defaults apply **independently to each workspace**, not to a pool of all Teams or Projects. Workspaces without platform overrides follow changes live. A replacement override replaces the type policy even when all fields are null. Local workspace/key restrictions compose with that platform layer and cannot raise or erase a stored local cap merely because a stricter parent currently masks it.

For example, a Team default of 2,000 requests/minute gives each inheriting Team that ceiling. A local cap of 500 imposes a second limit on that Team; it does not reserve 500 requests of anyone else's capacity. There is no installation-wide ceiling (removed in migration 0026); to watch total spend, add an installation spend alert. Absent child limits never remove parents.

Budgets use UTC calendar months and block new admission when allowance is exhausted. Changes and key rotation do not reset recorded consumption. Unknown/unbounded charges are not free. Ordinary users do not receive installation headroom that could reveal others' personal consumption. See [governance](governance.md).

Cost centers are optional Admin-controlled allocation labels, not authorization grants. Unassigned activity remains tracked as Unallocated. Assignment, rename and archival affect future activity; execution admission snapshots preserve historic ID/name/code.

## People and credentials

Manual and mapped-group access are independent sources. Signed generic group claims synchronize at sign-in; without SCIM, group removal outside a new sign-in is not immediately observable, so suspend manually for immediate denial. With [SCIM](scim.md), the identity provider pushes deactivation and pushed-group membership as they change. Entitlement loss revokes sessions/human keys; shared service credentials remain independent. Thirty-day cleanup keeps accounting/audit attribution through tombstones rather than cascading history deletion. See [identity](identity.md).

Workspace administrators manage shared members, invitations and service accounts. Human keys require actual membership and live entitlement. Service-account disablement revokes its keys. Reactivation never restores revoked credentials. Rotation retains restrictions, policy and consumed allowance; separately created keys have separate key scopes, bounded by their workspace/installation limits.

## Legal holds

A Platform Admin places a legal hold from a person's page or a team's or project's page (**Place legal hold…**) and releases it in Admin › Records › Legal holds (#75). Platform Auditors can list holds. A personal workspace is held through its owner.

- **What a hold stops:** while it's active the gateway removes none of that scope's data. The departed-user cleanup (30 days after suspension) waits, and a held person can't sign back in until released. Request-log compaction skips the scope's executions (by workspace, the user a key was issued to, or a personal workspace's owner). Stored files don't expire (an unfinished upload, which holds no data, is still removed). `archive partition` refuses a month that contains held executions or audit events.
- **What it doesn't do:** it grants no access: Admins still can't see another person's personal workspace, keys or requests. It stops the gateway's own removal jobs, not deliberate deletes: a user or workspace admin can still delete files (dashboard or Files API), batch runs remove their intermediate work files, ended sessions are still pruned after 30 days, and SCIM or an admin can still change a held person's email. It doesn't lock or freeze the account.
- **Record:** a short reference code (a case or ticket number: letters, digits and `._:/#-`, at most 64), who placed it and when, and when it was released. Never free text. Holds are released once and never deleted; placing and releasing are audited (`legal_hold.placed`, `legal_hold.released`). At most one active hold per target and reference.
- **Timing:** placing a hold on a person waits for, and is seen by, any cleanup of that person; placing or releasing any hold waits for a running `archive partition`. File expiry checks each file again just before deleting it; compaction sees a new hold from its next run.
- **Record:** the release time is the database's (it can't be backdated), the releaser is required, and a released hold never changes again. References compare without case.
- A person whose data was already removed can't be held (`409`).

API: `GET /api/v1/platform/legal-holds[?state=all]`, `POST /api/v1/platform/legal-holds` `{user_id | workspace_id, reference}`, `POST /api/v1/platform/legal-holds/{id}/release`. `GET /api/v1/platform/users/{id}` includes `legal_holds` (a count).

## UI and rollout

Grounded-style **Workspace** contains daily models, keys, usage/costs and workspace settings. **Admin** contains users, Teams, Projects, infrastructure, catalogs, limits, cost centers, SSO mappings, costs and audit. Auditors enter Admin read-only. See [dashboard](dashboard.md) for canonical UUID URLs and actual navigation behavior.

The enterprise schema is a fresh lineage in `apps/gateway/enterprise_migrations/`, not a conversion of legacy organizations. Explicit `migrate` performs read-only preflight before DDL and rejects old/unrelated databases. Serving checks exact lineage without migration. Stage against a fresh approved target; never reset or auto-migrate an old database to get a green check. Historical migrations/checks remain evidence of earlier milestones, not current rebuild validation.

Catalog advisory locks precede installation serialization; access mutations and admissions recheck live authority. Already-admitted work can finish after revocation. Measure contention, runtime ACLs, recovery and deployment behavior before operational adoption; documentation/source tests do not establish production readiness.

See [management API](management-api.md), [governance API](governance-api.md) and the authoritative [rebuild plan](enterprise-rebuild.md).

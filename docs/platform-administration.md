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

Admin/Auditor includes User entitlement. Neither grants another user's private keys or request details. Both may read authorized platform financial totals including personal totals. Shared administrative visibility does not replace actual membership for issuing a human key.

## Configure model access

1. Configure an approved provider connection, global public model and deployment. Model `supported_protocols` must match the desired workload; adapter profiles narrow it further.
2. Publish immutable configured prices and accurate hard input/output bounds before traffic. Review residency, failure thresholds and explicit failover. A successful configuration write is not a provider probe.
3. Put approved models in catalogs and assign live catalog defaults by Personal/Team/Project kind.
4. Optionally replace one workspace's catalog list. Replacement is not an additive union; an empty replacement permits no catalog models. Delete the override to follow live defaults again.
5. Personal owners/shared administrators select models from available catalogs. For exceptions, Platform Admins add direct assignments, separately represented from catalog selections.

Model presence alone is not permission. Inference requires live authorization, enabled target configuration and the key's narrowing restrictions. Losing catalog eligibility retires its selections unless another eligible catalog remains; independent direct access can remain. Removed key selections are not revived by later regrant, and their restriction header remains deny-all when empty.

## Policy defaults and shared ceilings

Type defaults apply **independently to each workspace**, not to a pool of all Teams or Projects. Workspaces without platform overrides follow changes live. A replacement override replaces the type policy even when all fields are null. Local workspace/key restrictions compose with that platform layer and cannot raise or erase a stored local cap merely because a stricter parent currently masks it.

For example, a Team default of 2,000 requests/minute gives each inheriting Team that ceiling. A local cap of 500 imposes a second limit on that Team; it does not reserve 500 requests of anyone else's capacity. There is no installation-wide ceiling (removed in migration 0026); to watch total spend, add an installation spend alert. Absent child limits never remove parents.

Budgets use UTC calendar months and block new admission when allowance is exhausted. Changes and key rotation do not reset recorded consumption. Unknown/unbounded charges are not free. Ordinary users do not receive installation headroom that could reveal others' personal consumption. See [governance](governance.md).

Cost centers are optional Admin-controlled allocation labels, not authorization grants. Unassigned activity remains tracked as Unallocated. Assignment, rename and archival affect future activity; execution admission snapshots preserve historic ID/name/code.

## People and credentials

Manual and mapped-group access are independent sources. Signed generic group claims synchronize at sign-in; without SCIM, group removal outside a new sign-in is not immediately observable, so suspend manually for immediate denial. With [SCIM](scim.md), the identity provider pushes deactivation and pushed-group membership as they change. Entitlement loss revokes sessions/human keys; shared service credentials remain independent. Thirty-day cleanup keeps accounting/audit attribution through tombstones rather than cascading history deletion. See [identity](identity.md).

Workspace administrators manage shared members, invitations and service accounts. Human keys require actual membership and live entitlement. Service-account disablement revokes its keys. Reactivation never restores revoked credentials. Rotation retains restrictions, policy and consumed allowance; separately created keys have separate key scopes, bounded by their workspace/installation limits.

## UI and rollout

Grounded-style **Workspace** contains daily models, keys, usage/costs and workspace settings. **Admin** contains users, Teams, Projects, infrastructure, catalogs, limits, cost centers, SSO mappings, costs and audit. Auditors enter Admin read-only. See [dashboard](dashboard.md) for canonical UUID URLs and actual navigation behavior.

The enterprise schema is a fresh lineage in `apps/gateway/enterprise_migrations/`, not a conversion of legacy organizations. Explicit `migrate` performs read-only preflight before DDL and rejects old/unrelated databases. Serving checks exact lineage without migration. Stage against a fresh approved target; never reset or auto-migrate an old database to get a green check. Historical migrations/checks remain evidence of earlier milestones, not current rebuild validation.

Catalog advisory locks precede installation serialization; access mutations and admissions recheck live authority. Already-admitted work can finish after revocation. Measure contention, runtime ACLs, recovery and deployment behavior before operational adoption; documentation/source tests do not establish production readiness.

See [management API](management-api.md), [governance API](governance-api.md) and the authoritative [rebuild plan](enterprise-rebuild.md).

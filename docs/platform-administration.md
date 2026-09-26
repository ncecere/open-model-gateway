# Platform administration and delegated access

## Ownership

The gateway is one platform. Users are platform identities; organization and workspace memberships provide scoped roles. Organizations remain the tenant boundary for membership, credentials issued to callers, execution attribution, budgets, and private data.

Platform administrators configure provider connections, models, deployments, routing and immutable configured prices once. Organizations receive model entitlements, not provider setup responsibilities. An organization administrator cannot create or change infrastructure through the API, even if they bypass the dashboard.

```text
Platform-owned infrastructure
  providers → deployments → model catalog / routing / prices
                         ↓ explicit organization entitlement
Organization (platform-assigned hard ceiling)
  ├─ Team (optional local limits, memberships, model grants)
  ├─ Project (optional local limits, memberships, model grants)
  └─ Personal workspaces (owner-private)
```

Projects and teams are sibling shared workspace kinds. Projects do not require a parent team. Both support memberships, administrators, human keys, service accounts, model grants, usage and policies. A human key requires active membership in both the organization and that shared workspace; inherited administrative visibility alone is not a human-key membership.

## Model access

1. A platform administrator grants a global model to an organization. Each organization can have its own client-facing alias.
2. Organization administrators delegate only assigned models to teams/projects, enable personal availability, or grant a model to an individual organization member for personal use.
3. A team/project request needs its workspace grant. A personal request needs an authorized personal grant or the owner's explicit individual grant. Individual grants do not bypass team/project restrictions.
4. Every request still needs an active key, current memberships, an organization entitlement and an enabled model/deployment/provider.

An individual grant is scoped to an organization, even though the user identity is global. It neither joins the user to that organization nor exposes their private workspace. Service accounts do not inherit individual human grants. Revoking an organization entitlement removes its delegated workspace/user grants; reassigning it does not silently restore those removed grants.

The platform catalog may be shared across organizations, but consumption and private data are not. A grant in one organization is not access in another. Personal workspace names, keys and activity remain owner-private, including from platform administrators.

## Policies: shared allowance, optional child caps

Platform-assigned organization ceilings and organization-managed restrictions are stored separately. Organization administrators cannot alter the platform ceiling. Every applicable limit is enforced, not replaced by the most specific setting.

For an organization with a **10,000 RPM** ceiling:

- A team or project with no RPM cap draws from the remaining organization allowance.
- A team capped at **2,000 RPM** must satisfy both its own counter and the organization counter.
- All teams, projects, personal workspaces and keys combined remain under the organization ceiling.
- Setting a child field to null means inherit; it does not disable the parent field.
- A configured child cap is not reserved capacity. Configured child caps are not summed as guaranteed allocations.
- Team/project administrators can only tighten existing explicit scope restrictions, not raise or clear them—even when a tighter parent temporarily masks them. Organization administrators can revise local restrictions within the platform ceiling.
- Explicit new child values above the applicable parent ceiling are rejected. If a parent is later lowered, it immediately restricts future admissions even if an older child configuration displays a larger value.

The same composition applies to token limits, leased concurrency and monthly budgets. RPM counts upstream attempts, including separately admitted fallback attempts. Windows remain fixed UTC minutes and UTC calendar months, not rolling windows. Unknown costs retain conservative holds. Prices are configured-rate estimates, not provider invoices.

## Administration screens

- **Platform administrator:** Organizations, Teams, Projects, Users, central model/provider/deployment/routing/pricing configuration, model access and hard organization policy ceilings, and privacy-filtered audit history. No organization selection is required merely to configure infrastructure.
- **Organization administrator:** assigned models, teams/projects, members/invitations, delegated access, local policies and scoped reporting. No provider, deployment or routing setup.
- **Team/project administrator:** administration and permitted policy controls for their shared workspace; no expansion of organization entitlements or parent ceilings.
- **Member:** assigned resources and own keys/activity; no administrative portal from personal ownership alone.

The context selector only switches existing contexts. Creation belongs on directory pages. Header search is permission-scoped navigation, not a search of credentials, prompts, responses or other users' private activity.

## Migration and locking

Apply migrations explicitly before deploying the matching application. `0007_platform_catalog.sql` separates global infrastructure references from consuming-tenant foreign keys; `0008_projects.sql` adds the shared project kind. Existing resource IDs, consuming organization attribution, pinned prices, reservations and immutable ledger history are retained. Organization model aliases are preserved when canonical legacy model names need disambiguation.

Some infrastructure tables retain nullable `organization_id` as **legacy provenance**, not ownership or runtime authorization. New platform API inserts leave it null. A trusted legacy SQL/bootstrap model insert that supplies it receives an initial organization entitlement atomically; HTTP callers cannot use it to create infrastructure or bypass entitlement checks. The old model `personal_enabled` field is compatibility input, not the authority for organization personal-access policy.

Global catalog mutations take the exclusive catalog transaction lock; admission takes its shared form before the consuming organization lock. Entitlement changes serialize with admission on that organization. Admission revalidates current credentials, memberships, grants and target configuration before dispatch. Already-admitted upstream work can finish after revocation; newly admitted attempts cannot rely on a stale cached grant.

See [management API](management-api.md), [governance API](governance-api.md), [governance accounting](governance.md), and [routing](routing.md) for detailed contracts.

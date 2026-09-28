# Management API contract

Browser sessions only; inference keys never authorize management. Mutations require exact configured Origin and `X-CSRF-Token` matching `omg_csrf`. Success responses are JSON. Handler errors use `{error:{code,message}}`; malformed path/query/JSON extraction errors may be plain text. Collection responses use `{data:[...]}` with default limit 100, max 200 and offsets 0–100000. UUID identifiers; UTC timestamps; no secret values in list responses.

See [platform administration](platform-administration.md) for ownership, inheritance and migration behavior, and [governance API](governance-api.md) for policy/pricing/routing payloads.

## Identity and directories

- `GET /api/v1/auth/config` → `{enabled}`; `GET /api/v1/auth/login` starts OIDC; callback returns to `/`.
- `POST /api/v1/auth/logout` ends the current browser session.
- `GET /api/v1/me` → `{user:{id,email,platform_admin},organizations:[...],workspaces:[...]}`. Organizations include `id,name,slug,role,membership_role,authority_source,capabilities`; workspaces include `id,organization_id,name,kind,role,membership_role,authority_source,capabilities,own_key_denial_reason`. Kind is `personal|team|project`; private spaces only appear for their owner. `role` remains effective authority; `membership_role` is actual active membership (personal ownership reports `owner`), or null. `authority_source` is `platform|organization|direct|personal`, distinguishing inherited administration from membership. These additive presentation fields do not change authorization.
- `GET /api/v1/platform/users` → `{data:[{id,email,platform_admin,disabled_at,created_at}]}`. Current operator only; no private workspace, key or activity metadata.
- `GET /api/v1/platform/teams` and `/api/v1/platform/projects`, optional `organization_id` UUID filter → `{data:[{id,organization_id,organization_name,name,kind,role:"operator"}]}`. Active shared resources only; no personal spaces.
- `GET /api/v1/orgs` → `{data:[{id,name,slug,role,membership_role,created_at}]}`. `membership_role` is actual active membership, independent of platform-operator access.
- `POST /api/v1/orgs` `{name,slug}` → `{id}`; operator creates an organization, owner membership and private workspace.
- `PATCH /api/v1/orgs/{org}` `{name}` → `{ok:true}`; organization admin/operator.
- `GET /api/v1/orgs/{org}/teams` and `/api/v1/orgs/{org}/projects` → authorized active shared directories. Org admins/operators see all; ordinary members see their active memberships.
- `POST /api/v1/orgs/{org}/workspaces` `{name,kind?:"team"|"project"}` → `{id}`. Default kind is team. Requires org administration AND actual active organization membership; platform authority is not a replacement for creator membership.
- `PATCH /api/v1/workspaces/{ws}` `{name}` renames an administered team/project. Personal spaces are not addressable through this shared-directory operation.
- `POST /api/v1/orgs/{org}/personal-workspace` `{}` creates/retrieves the current member's private workspace.

Organization capabilities are `create_workspace`, `create_personal_workspace`, `manage_members`, `manage_owners`, `delegate_models`, and `manage_policy`. Workspace capabilities are `issue_own_key`, `manage_members`, `manage_owners`, `manage_service_accounts`, `delegate_models`, `manage_policy`, and `view_all_activity`. A denied human-key capability carries `organization_membership_required` or `workspace_membership_required`; an allowed one has null reason. Capabilities reflect the session read, not a durable authorization grant: mutations continue to revalidate under current locks.

## Central infrastructure — platform operators only

These resources do not require an organization context. Organizations consume assigned models instead of setting up infrastructure.

- `GET/POST /api/v1/platform/providers`; POST `{name,provider,credential_ref,endpoint?,region?,enabled}` → `{id}`. GET projects `{id,name,provider,endpoint,region,enabled}` only. Credential references/values are never returned. Fixed vendor endpoints and allowlisted references remain mandatory.
- `GET /api/v1/platform/providers/{id}` returns one redacted provider object. `PATCH` accepts `{enabled,credential_ref?}`.
- `GET/POST /api/v1/platform/models`; POST `{public_name,display_name,enabled}` → `{id}`.
- `GET /api/v1/platform/models/{id}` returns one model object. `PATCH` accepts `{enabled}`.
- `GET/POST /api/v1/platform/deployments`; POST `{model_id,provider_connection_id,upstream_model,enabled}` → `{id}`.
- `GET /api/v1/platform/deployments/{id}` returns one deployment object. `PATCH` accepts `{enabled}`.
- `GET/POST /api/v1/platform/deployments/{id}/prices`: immutable configured-rate versions.
- `GET/PUT /api/v1/platform/models/{id}/routing` and `/api/v1/platform/deployments/{id}/routing`: central routing, residency and passive-health configuration.
- `GET /api/v1/platform/audit`: bounded administrative events, excluding other users' private workspace activity and credential/prompt metadata.

The three platform catalog collections accept `q` (trimmed, case-insensitive literal substring, at most 200 characters) and `enabled=true|false`. Deployment collections additionally accept `model_id` and `provider_connection_id` UUID filters. Filters apply before pagination and preserve deterministic ordering; `%` and `_` are literal search characters, not wildcards. Direct GETs return an object, not `{data:...}`, and require current platform authority even when the ID is known. Missing authorized records return 404. No credential references/values are added to any response.

Old organization infrastructure endpoints return 403, including for operators. They cannot be used to configure providers, models, deployments, prices or routes. Use platform endpoints for central setup and assigned catalog/grant endpoints for delegation.

## Entitlements and delegated model access

- `GET /api/v1/orgs/{org}/models` lists the organization's assigned catalog: `{id,public_name,display_name,enabled,personal_enabled}`. The public alias and personal-availability setting belong to the organization entitlement; the global model is independently enabled/disabled by the platform.
- `PUT /api/v1/platform/orgs/{org}/models/{model}` `{public_name?}` assigns a global model; the default alias is its canonical name. Operator only.
- `DELETE /api/v1/platform/orgs/{org}/models/{model}` revokes the entitlement and its delegated workspace/user grants atomically. Regranting does not revive removed child grants.
- `PUT /api/v1/orgs/{org}/models/{model}/personal-access` `{enabled}` sets organization-wide personal availability within assigned models, including existing/future personal workspaces, without returning private workspace metadata.
- `GET /api/v1/workspaces/{ws}/grants` → `{data:[{model_id,public_name,display_name,workspace_granted,individual_granted}]}`. Owner-private personal results include individual grants, deduplicated with workspace grants; shared workspaces never inherit individual grants. Workspace DELETE removes only the workspace grant; individual grants are managed through the user-grant endpoint.
- `POST /api/v1/workspaces/{ws}/grants` `{model_id}` and `DELETE /api/v1/workspaces/{ws}/grants/{model}` delegate/revoke an assigned model. Org admin/operator; normal private-workspace ownership checks remain.
- `GET/POST /api/v1/orgs/{org}/users/{user}/grants`; POST `{model_id}`. `DELETE /api/v1/orgs/{org}/users/{user}/grants/{model}` revokes. Target must be an active organization member; org admin/operator only. Individual grants apply to that user's own personal scope, not to service accounts or as a bypass of team/project grants. No private workspace enumeration is required.

## Memberships and invitations

- `GET /api/v1/orgs/{org}/members` → `{data:[{user_id,email,role,disabled_at}]}`; org admin.
- `PATCH /api/v1/orgs/{org}/members/{user}` `{role,disabled}`; owner protections apply.
- `GET/POST /api/v1/workspaces/{ws}/members`; POST `{user_id,role}` adds an active organization member to an administered team/project.
- `PATCH /api/v1/workspaces/{ws}/members/{user}` `{role,disabled}`. Disabling membership revokes affected keys; re-enabling does not revive them. Last-owner/owner-only operations are protected.
- `GET/POST /api/v1/orgs/{org}/invitations`; POST `{email,organization_role:"member"|"admin",workspace_id?:uuid,workspace_role:"member"|"admin"}` → `{id,token}` once. Workspace target may be a team/project, never personal.
- `DELETE /api/v1/orgs/{org}/invitations/{id}` revokes.
- `POST /api/v1/invitations/accept` `{token}` → `{organization_id}`; signed-in matching verified email, no implicit promotion/reactivation. Invitation delivery remains out of band.

## Keys, service accounts and reporting

- `GET/POST /api/v1/workspaces/{ws}/keys`; POST `{name,expires_in_days:1..365,service_account_id?:uuid,model_ids?:uuid[]|null}` → `{id,token,model_ids}` once. Omitted/null `model_ids` inherits current grants; `[]` denies all models; a nonempty list restricts the key to those model UUIDs intersected with current grants. At most 200 raw entries; duplicates are removed and IDs sorted. Selected models must be effectively granted to this workspace (owner-personal individual grants also qualify); disabled-but-granted models may be preconfigured. Aliases and foreign/ungranted IDs are rejected. Human shared-workspace keys require direct active membership even for inherited org admins. Members list their own human keys; workspace admins list workspace keys. Lists return restriction metadata, never the token.
- `POST /api/v1/workspaces/{ws}/keys/{key}/rotate` `{expires_in_days:1..365}` → `{id,token}` atomically replaces/revokes the old key and preserves governance lineage, model restrictions, limits and consumed allowance. Only own human keys or admin-managed service-account keys, never another human's key. Restriction edits are not accepted during rotation or through a separate update endpoint; create a distinct key and revoke the old one to change selections. A distinct key has its own key-budget scope; parent limits still aggregate all keys.
- `DELETE /api/v1/workspaces/{ws}/keys/{key}` revokes.

Restrictions are additional constraints, never model grants. They are rechecked in model listing, deployment discovery and locked admission before dispatch/accounting. Revoking an **organization model assignment** deletes its key selections but preserves restriction headers: a formerly restricted key with no remaining selections denies all, and reassignment does not revive those selections. Temporary removal/regrant of a workspace grant does not rewrite the stored allowlist; effective access is always its intersection with current entitlements. Existing keys without a restriction header inherit unchanged. Personal metadata remains owner-private.
- `GET/POST /api/v1/workspaces/{ws}/service-accounts`; POST `{name}` → `{id}`. Team/project admin only.
- `PATCH /api/v1/workspaces/{ws}/service-accounts/{id}` `{disabled}`. Disablement revokes keys; re-enablement does not restore them.
- `GET /api/v1/workspaces/{ws}/executions` and `/usage`: scoped execution metadata and last-30-day usage. Workspace admins see workspace activity; members see their own human-key activity. No prompts/outputs.
- `GET /api/v1/orgs/{org}/audit`: scoped administrative history. Personal events remain owner-only.
- Cost summaries, bounded CSV export, policies and evidence-backed reconciliation are detailed in [governance API](governance-api.md).

All UI checks are presentation only. Mutations recheck current authority under locks and commit audit with state changes; inference admissions independently check current entitlements and parent limits.

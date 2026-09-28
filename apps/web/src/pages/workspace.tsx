import { api, wsPath, orgPath, type Session, type Workspace, type Organization, type Key, type ServiceAccount, type Grant, type Model, type Member } from "../lib/api";
import { canManageKey, canRotateKey, permissions } from "../lib/permissions";
import { ownKeyHelp } from "../lib/access";
import { expiryField, nameField, roleField, disabledField, uuidError, type Field } from "../lib/forms";
import { Badge, CollectionTable, DateTime, ErrorNotice, Heading, Id, RowActions, useAction, useChoices } from "../components/ui";

import { keyModelBody, keyModelFields, keyModelOptions, keyModelRestrictionHelp, keyModelSummary, keyRotationDescription } from "../lib/key-models";

export type Scope = { session: Session; workspace: Workspace; organization: Organization };
export function Keys({ session, workspace, organization }: Scope) {
  const ask = useAction();
  const path = `${wsPath(workspace.id)}/keys`;
  const p = permissions(session, organization, workspace);
  const accounts = useChoices<ServiceAccount>(`${wsPath(workspace.id)}/service-accounts`, p.manageServiceAccounts);
  const teamMembers = useChoices<Member>(`${wsPath(workspace.id)}/members`, workspace.capabilities === undefined && workspace.kind !== "personal" && p.workspaceAdmin);
  const canIssueUserKey = workspace.capabilities ? workspace.capabilities.issue_own_key : (p.createUserKey && (workspace.kind === "personal" || !p.workspaceAdmin)) || (p.createPersonalWorkspace && workspace.kind !== "personal" && !!teamMembers.data?.some((member) => member.user_id === session.user.id && !member.disabled_at));
  const grants = useChoices<Grant>(`${wsPath(workspace.id)}/grants`);
  const modelOptions = keyModelOptions(grants.data ?? []);
  const grantsReady = grants.isSuccess && !grants.isFetching;
  const create = () => ask({ title: "Create API key", description: "This key authorizes inference only in this workspace. It cannot sign in to the dashboard.", fields: [nameField, expiryField, ...keyModelFields(modelOptions), ...(p.manageServiceAccounts ? [{ name: "service_account_id", label: "Service account", type: "select", required: !canIssueUserKey, help: canIssueUserKey ? "Leave blank to issue a personal user key." : "User keys require active organization and explicit shared workspace membership. Choose a service account.", options: accounts.data?.filter((a) => !a.disabled_at).map((a) => ({ value: a.id, label: a.name })) ?? [] } satisfies Field] : [])], submitLabel: "Create key", secretLabel: "Save your API key", run: (v) => api(path, { method: "POST", body: { name: v.name, expires_in_days: Number(v.expires_in_days), ...keyModelBody(v, modelOptions), ...(v.service_account_id ? { service_account_id: v.service_account_id } : {}) } }) });
  return <><Heading title="API keys" description={p.workspaceAdmin ? "Workspace inference credentials. You can manage all keys in this workspace." : "Your inference credentials for this workspace. Keys have a required expiry."} actions={<button className="button" disabled={!grantsReady || (!canIssueUserKey && !(p.manageServiceAccounts && accounts.data?.some((a) => !a.disabled_at)))} onClick={create}>Create key</button>} /><p className="help">{keyModelRestrictionHelp}</p>{grants.isFetching && <p role="status" className="loading">Loading effective model access before creating a key…</p>}{grants.isError && <><p className="notice">Effective model access could not be fully loaded. Retry before creating a key.</p><ErrorNotice error={grants.error} retry={() => void grants.refetch()} /></>}{accounts.isError && <ErrorNotice error={accounts.error} retry={() => void accounts.refetch()} />}{teamMembers.isError && <ErrorNotice error={teamMembers.error} retry={() => void teamMembers.refetch()} />}{!canIssueUserKey && (workspace.capabilities || teamMembers.data) && <p className="notice">{ownKeyHelp(workspace)}</p>}<CollectionTable<Key> path={path} label="API keys" empty="Create a key to call the inference API using a granted model." rowKey={(key) => key.id} columns={[
    { title: "Name", render: (key) => <><strong>{key.name}</strong><Id value={key.id} /></> },
    { title: "Issued to", render: (key) => key.service_account_id ? <><Badge>Service account</Badge><Id value={key.service_account_id} /></> : <><Badge>{key.issued_to_user_id === session.user.id ? "You" : "User"}</Badge><Id value={key.issued_to_user_id} /></> },
    { title: "Status", render: (key) => <Badge tone={key.revoked_at ? "neutral" : new Date(key.expires_at).getTime() <= Date.now() ? "bad" : "good"}>{key.revoked_at ? "Revoked" : new Date(key.expires_at).getTime() <= Date.now() ? "Expired" : "Active"}</Badge> },
    { title: "Model access", render: (key) => { const summary = keyModelSummary(key, grants.isSuccess ? modelOptions : []); return summary.models.length ? <details className="key-model-summary"><summary>{summary.label}</summary><ul>{summary.models.map((label, index) => <li key={key.model_ids![index]}>{label}</li>)}</ul></details> : summary.label; } },
    { title: "Created", render: (key) => <DateTime value={key.created_at} /> },
    { title: "Expires", render: (key) => <DateTime value={key.expires_at} /> },
    { title: "Actions", render: (key) => !key.revoked_at && canManageKey(session, workspace, key) ? <RowActions>{canRotateKey(session, workspace, key) && (canIssueUserKey || !!key.service_account_id) && <button className="button secondary small" onClick={() => ask({ title: `Rotate ${key.name}?`, description: keyRotationDescription, danger: true, fields: [expiryField], submitLabel: "Rotate key", secretLabel: "Save your replacement key", run: (v) => api(`${path}/${key.id}/rotate`, { method: "POST", body: { expires_in_days: Number(v.expires_in_days) } }) })}>Rotate</button>}<button className="button ghost destructive small" onClick={() => ask({ title: `Revoke ${key.name}?`, description: "This permanently revokes the key. Clients using it will no longer be able to make inference requests.", danger: true, submitLabel: "Revoke key", run: () => api(`${path}/${key.id}`, { method: "DELETE" }) })}>Revoke</button></RowActions> : <span className="muted">—</span> },
  ]} /></>;
}

export function ServiceAccounts({ workspace }: Scope) {
  const ask = useAction();
  const path = `${wsPath(workspace.id)}/service-accounts`;
  return <><Heading title="Service accounts" description={`Non-human identities for ${workspace.kind} workloads. Disabling an account also revokes its keys.`} actions={<button className="button" onClick={() => ask({ title: "Create service account", fields: [nameField], submitLabel: "Create account", run: (v) => api(path, { method: "POST", body: { name: v.name } }) })}>Create account</button>} /><CollectionTable<ServiceAccount> path={path} label="Service accounts" empty="Create an account for a shared application, then issue its key from API keys." rowKey={(a) => a.id} columns={[
    { title: "Name", render: (a) => <><strong>{a.name}</strong><Id value={a.id} /></> },
    { title: "Status", render: (a) => <Badge tone={a.disabled_at ? "neutral" : "good"}>{a.disabled_at ? "Disabled" : "Active"}</Badge> },
    { title: "Disabled at", render: (a) => <DateTime value={a.disabled_at} /> },
    { title: "Actions", render: (a) => <button className={`button ${a.disabled_at ? "secondary" : "ghost destructive"} small`} onClick={() => ask({ title: `${a.disabled_at ? "Enable" : "Disable"} ${a.name}?`, description: a.disabled_at ? "Enabling the account does not restore revoked keys. Issue a new key after enabling." : "All keys for this service account will be revoked. This cannot be undone by re-enabling the account.", danger: !a.disabled_at, submitLabel: a.disabled_at ? "Enable account" : "Disable account", run: () => api(`${path}/${a.id}`, { method: "PATCH", body: { disabled: !a.disabled_at } }) })}>{a.disabled_at ? "Enable" : "Disable"}</button> },
  ]} /></>;
}

export function WorkspaceMembers({ session, workspace, organization }: Scope) {
  const ask = useAction();
  const path = `${wsPath(workspace.id)}/members`;
  const orgAdmin = permissions(session, organization, workspace).organizationAdmin;
  const members = useChoices<Member>(`${orgPath(organization.id)}/members`, orgAdmin);
  const addField: Field = orgAdmin ? { name: "user_id", label: "Organization member", type: "select", required: true, options: members.data?.filter((m) => !m.disabled_at).map((m) => ({ value: m.user_id, label: m.email })) ?? [] } : { name: "user_id", label: "Organization member user ID", required: true, validate: uuidError, help: "Ask an organization admin for the active member’s user UUID. Shared workspace admins cannot browse organization membership." };
  return <><Heading title={workspace.kind === "project" ? "Project members" : "Team members"} description="Only active organization members can join. Personal workspaces cannot be shared." actions={<button className="button" disabled={orgAdmin && !members.data} onClick={() => ask({ title: `Add ${workspace.kind} member`, fields: [addField, { ...roleField, options: roleField.options?.filter((option) => option.value !== "owner" || workspace.role === "owner") }], submitLabel: "Add member", run: (v) => api(path, { method: "POST", body: { user_id: v.user_id, role: v.role } }) })}>Add existing member</button>} />{members.isError && <ErrorNotice error={members.error} retry={() => void members.refetch()} />}<MembersTable path={path} title={workspace.kind === "project" ? "Project members" : "Team members"} canManageOwners={workspace.role === "owner"} /></>;
}
export function MembersTable({ path, title, canManageOwners }: { path: string; title: string; canManageOwners: boolean }) {
  const ask = useAction();
  return <CollectionTable<Member> path={path} label={title} empty="No members are visible in this scope." rowKey={(m) => m.user_id} columns={[
    { title: "Member", render: (m) => <><strong>{m.email}</strong><Id value={m.user_id} /></> },
    { title: "Role", render: (m) => <Badge>{m.role}</Badge> },
    { title: "Status", render: (m) => <Badge tone={m.disabled_at ? "neutral" : "good"}>{m.disabled_at ? "Disabled" : "Active"}</Badge> },
    { title: "Actions", render: (m) => m.role === "owner" && !canManageOwners ? <span className="muted">Owner protected</span> : <button className="button secondary small" onClick={() => ask({ title: `Manage ${m.email}`, description: "Changes affect access immediately. Disabling also revokes this member’s keys in this scope; enabling does not restore those keys. The gateway prevents removal of the last owner.", danger: true, fields: [{ ...roleField, value: m.role, options: roleField.options?.filter((option) => option.value !== "owner" || canManageOwners) }, { ...disabledField, value: String(!!m.disabled_at) }], submitLabel: "Update membership", run: (v) => api(`${path}/${m.user_id}`, { method: "PATCH", body: { role: v.role, disabled: v.disabled === "true" } }) })}>Manage</button> },
  ]} />;
}
export function Grants({ session, workspace, organization }: Scope) {
  const ask = useAction();
  const path = `${wsPath(workspace.id)}/grants`;
  const allowed = permissions(session, organization, workspace).manageGrants;
  const models = useChoices<Model>(`${orgPath(organization.id)}/models`, allowed);
  const grants = useChoices<Grant>(path, allowed);
  const choices = models.data?.filter((model) => !grants.data?.some((g) => g.model_id === model.id)) ?? [];
  return <><Heading title="Model access" description="Models delegated from the organization’s assigned catalog. Grants cannot enlarge the parent entitlement and do not guarantee an enabled deployment is available." actions={allowed && <button className="button" disabled={!models.data || !grants.data || !choices.length} onClick={() => ask({ title: "Grant model access", fields: [{ name: "model_id", label: "Model alias", type: "select", required: true, options: choices.map((m) => ({ value: m.id, label: `${m.display_name} (${m.public_name})${m.enabled ? "" : " — disabled"}` })) }], submitLabel: "Grant access", run: (v) => api(path, { method: "POST", body: { model_id: v.model_id } }) })}>Grant model</button>} />{allowed && models.isError && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}{allowed && grants.isError && <ErrorNotice error={grants.error} retry={() => void grants.refetch()} />}{allowed && models.data && grants.data && !choices.length && <p className="notice">{models.data.length ? "All organization model aliases are already granted." : "Ask a platform operator to assign a model to this organization before delegating access."}</p>}<CollectionTable<Grant> path={path} label="Model grants" empty={allowed ? "Grant a model alias to allow inference from this workspace." : "Ask an organization administrator to grant model access."} rowKey={(g) => g.model_id} columns={[
    { title: "Model", render: (g) => <strong>{g.display_name}</strong> },
    { title: "API model name", render: (g) => <code>{g.public_name}</code> },
    ...(workspace.kind === "personal" ? [{ title: "Grant source", render: (g: Grant) => g.individual_granted ? g.workspace_granted ? "Personal workspace and individual" : "Individual · organization-managed" : "Personal workspace" }] : []),
    ...(allowed ? [{ title: "Actions", render: (g: Grant) => g.workspace_granted === false ? <span className="muted">Manage individual access under Assigned models</span> : <button className="button ghost destructive small" onClick={() => ask({ title: `Remove access to ${g.public_name}?`, description: g.individual_granted ? "Removes the workspace grant only. Your individual grant will continue to authorize this model." : "This workspace’s keys will no longer be authorized to use this model alias.", danger: true, submitLabel: "Remove grant", run: () => api(`${path}/${g.model_id}`, { method: "DELETE" }) })}>Remove grant</button> }] : []),
  ]} /></>;
}

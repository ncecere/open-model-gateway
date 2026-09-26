import { api, orgPath, API, type Organization, type Session, type Invitation, type Audit } from "../lib/api";
import { roleOptions, type Field } from "../lib/forms";
import { Badge, Button, CollectionTable, DateTime, Heading, Id, useAction } from "../components/ui";
import { MembersTable } from "./workspace";

export function OrganizationMembers({ organization, onInvite }: { organization: Organization; onInvite?: () => void }) {
  return <><Heading title="Organization members" description={`${organization.name} · Organization roles control assigned model delegation, teams and projects, not platform infrastructure. Invite a person to add membership; their platform identity is not duplicated.`} actions={onInvite && <Button onClick={onInvite}>Invite member</Button>} /><MembersTable path={`${orgPath(organization.id)}/members`} title="Organization members" canManageOwners={organization.role === "owner" || organization.role === "operator"} /></>;
}
export function Invitations({ session, organization }: { session: Session; organization: Organization }) {
  const ask = useAction();
  const path = `${orgPath(organization.id)}/invitations`;
  const workspaces = session.workspaces.filter((w) => w.organization_id === organization.id && w.kind !== "personal");
  const role: Field = { name: "organization_role", label: "Organization role", type: "select", required: true, value: "member", options: roleOptions.filter((r) => r.value !== "owner") };
  return <><Heading title="Invitations" description="Invitation tokens are shown once. Deliver them securely out of band; the gateway does not send email. Recipients must sign in with the matching verified email." actions={<button className="button" onClick={() => ask({ title: "Invite a member", fields: [{ name: "email", label: "Email address", type: "email", required: true, maxLength: 254 }, role, { name: "workspace_id", label: "Team or project", type: "select", options: workspaces.map((w) => ({ value: w.id, label: `${w.name} · ${w.kind}` })), help: "Leave blank for organization-only membership. Only visible shared workspaces are listed." }, { ...role, name: "workspace_role", label: "Workspace role", help: "Only applies when a team or project is selected." }], submitLabel: "Create invitation", secretLabel: "Save invitation token", run: (v) => api(path, { method: "POST", body: { email: v.email, organization_role: v.organization_role, workspace_role: v.workspace_role, ...(v.workspace_id ? { workspace_id: v.workspace_id } : {}) } }) })}>Create invitation</button>} /><CollectionTable<Invitation> path={path} label="Invitations" empty="Create an invitation to bring a new member into the organization." rowKey={(i) => i.id} columns={[
    { title: "Email", render: (i) => <strong>{i.email}</strong> },
    { title: "Organization role", render: (i) => <Badge>{i.organization_role}</Badge> },
    { title: "Team or project", render: (i) => i.workspace_id ? <>{workspaces.find((w) => w.id === i.workspace_id)?.name ?? <Id value={i.workspace_id} />}<div className="muted">{i.workspace_role}</div></> : <span className="muted">Organization only</span> },
    { title: "Status", render: (i) => <Badge tone={i.accepted_at ? "good" : "neutral"}>{i.accepted_at ? "Accepted" : i.revoked_at ? "Revoked" : new Date(i.expires_at).getTime() <= Date.now() ? "Expired" : "Pending"}</Badge> },
    { title: "Expires", render: (i) => <DateTime value={i.expires_at} /> },
    { title: "Actions", render: (i) => !i.accepted_at && !i.revoked_at && new Date(i.expires_at).getTime() > Date.now() ? <button className="button ghost destructive small" onClick={() => ask({ title: `Revoke invitation for ${i.email}?`, description: "The token will stop working. You can create a new invitation if needed.", danger: true, submitLabel: "Revoke invitation", run: () => api(`${path}/${i.id}`, { method: "DELETE" }) })}>Revoke</button> : <span className="muted">—</span> },
  ]} /></>;
}
export function AcceptInvitation({ email }: { email: string }) {
  const ask = useAction();
  return <><Heading title="Accept an invitation" description="Join an organization using a token shared with you by its administrator." /><section className="panel prose"><h2>Use your invitation token</h2><p>You are signed in as <strong>{email}</strong>. This must match the invitation’s verified email address.</p><p>Paste the token in the secure form. Do not add it to a URL. Accepting an invitation does not reactivate disabled memberships or promote existing roles.</p><button className="button" onClick={() => ask({ title: "Accept invitation", fields: [{ name: "token", label: "Invitation token", type: "password", required: true, maxLength: 64, validate: (v) => /^[0-9a-f]{64}$/i.test(v) ? undefined : "Enter the complete 64-character invitation token.", help: "Paste the exact token your administrator shared." }], submitLabel: "Accept invitation", run: (v) => api(`${API}/invitations/accept`, { method: "POST", body: { token: v.token } }) })}>Enter invitation token</button></section></>;
}
export function AuditHistory({ organization }: { organization?: Organization }) {
  return <><Heading title={organization ? "Audit history" : "Platform audit"} description="Bounded administrative oversight. Other people’s private workspace events, credentials and request content are never exposed." /><CollectionTable<Audit> path={organization ? `${orgPath(organization.id)}/audit` : `${API}/platform/audit`} label="Audit events" empty="No audit events are visible in this scope." pageSize={50} rowKey={(event) => event.id} columns={[
    { title: "Time", render: (event) => <DateTime value={event.created_at} /> },
    { title: "Action", render: (event) => <code>{event.action}</code> },
    { title: "Actor", render: (event) => <Id value={event.actor_user_id} /> },
    { title: "Target", render: (event) => <Id value={event.target_id} /> },
    { title: "Workspace", render: (event) => <Id value={event.workspace_id} /> },
  ]} /></>;
}

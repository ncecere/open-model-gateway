// Workspace invitations and audit; there is no organization tenant in this installation.
import { ScrollText } from "lucide-react";
import { Card } from "../components/ui/card/card";
import { api, wsPath, API, platformPath, type Session, type Workspace, type Invitation, type Audit } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { auditEventLabel, resourceTypeLabel, workspaceRoleLabels } from "../lib/people";
import { auditActor, auditTarget, auditWorkspace, type AuditTarget } from "../lib/audit-names";
import { StatusPill } from "../components/templates/status-pill";
import { DirectoryTable, copyIdAction, signInFacet, type FacetState } from "../components/people";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { roleOptions } from "../lib/forms";
import { Button, CollectionTable, DateTime, Heading, Stack, Panel, useAction } from "../components/ui";
import { ActionMenu } from "../components/templates/action-menu";
import s from "./shared.module.css";
export function Invitations({ workspace }: { session: Session; workspace: Workspace }) {
  const ask = useAction(), path = `${wsPath(workspace.id)}/invitations`;
  return <Card title="Invitations" description="The code is shown once and emailed if email is set up; otherwise send it yourself. It expires in 3 days and works only for that email." actions={workspace.capabilities.manage_members && <Button size="sm" variant="secondary" onClick={() => ask({ title: "Invite by email", fields: [{ name: "email", label: "Email address", type: "email", required: true, maxLength: 320 }, { name: "role", label: "Workspace role", type: "select", required: true, value: "member", options: roleOptions.filter(o => o.value !== "owner") }], submitLabel: "Create invitation", secretLabel: "Copy the invite code", secretNoun: "invite code", run: (v, signal) => api(path, { method: "POST", body: { email: v.email, role: v.role }, signal }) })}>Invite by email</Button>}><CollectionTable<Invitation> path={path} label="Invitations" empty="Invite someone by email when they can't be found by search." rowKey={i => i.id} columns={[{ title: "Email", render: i => i.email }, { title: "Role", render: i => workspaceRoleLabels[i.role as keyof typeof workspaceRoleLabels] ?? i.role }, { title: "Status", render: i => <InvitationStatus invitation={i} /> }, { title: "Expires", render: i => i.accepted_at || i.revoked_at ? <span className={s.muted}>—</span> : <DateTime value={i.expires_at} /> }, { title: "Actions", render: i => <ActionMenu label={`Actions for ${i.email}`} actions={[{ label: "Revoke invitation…", danger: true, hidden: !!i.accepted_at || !!i.revoked_at || !workspace.capabilities.manage_members, onSelect: () => ask({ title: `Revoke invitation for ${i.email}?`, description: "The invite code stops working.", danger: true, submitLabel: "Revoke invitation", successNotice: "Invitation revoked.", run: (_, signal) => api(`${path}/${encodeURIComponent(i.id)}`, { method: "DELETE", signal }) }) }]} /> }]} /></Card>;
}
/** Accepted (done), Pending, or Revoked/Expired (inactive): the same pills as keys and accounts. */
function InvitationStatus({ invitation: i }: { invitation: Invitation }) {
  if (i.accepted_at) return <StatusPill status="active" label="Accepted" />;
  if (i.revoked_at) return <StatusPill status="revoked" />;
  return Date.parse(i.expires_at) <= Date.now() ? <StatusPill status="disabled" label="Expired" /> : <StatusPill status="pending" label="Pending" />;
}
export function AcceptInvitation({ email }: { email: string }) {
  const ask = useAction(); return <Stack gap={6} className={s.page}><Heading title="Accept invitation" description="Join a team or project with the invite code you were sent." /><Panel title="Invite code"><p>Signed in as <strong>{email}</strong>. The code works only for this email.</p><Button onClick={() => ask({ title: "Accept invitation", fields: [{ name: "token", label: "Invite code", type: "password", required: true, maxLength: 64, validate: v => /^[0-9a-f]{64}$/i.test(v) ? undefined : "Enter the full 64-character code." }], submitLabel: "Accept invitation", run: (v, signal) => api(`${API}/invitations/accept`, { method: "POST", body: { token: v.token }, signal }) })}>Enter invite code</Button></Panel></Stack>;
}
/*
 * Audit log: who (email, not identifiers), a humanised action with its code
 * beneath, and the target's type with its name where an already-loaded list
 * resolves it; identifiers move to the row "…" menu. Provenance: Grounded
 * web/src/pages/admin/logs/audit.tsx and components/audit/labels.ts
 * (read-only reference). The gateway's privacy filtering is unchanged: the
 * platform log never includes personal-workspace events.
 */
export function AuditHistory({ session, workspace }: { session?: Session; workspace?: Workspace }) {
  const platform = !workspace;
  const scope = workspace ? { platform: false as const, ws: workspace.id } : { platform: true as const };
  const target = (e: Audit) => auditTarget(e, scope);
  // Admin: sign-in side effects (group syncs) are hidden unless asked for (review #12); URL-backed as hide_sign_ins=false.
  const nav = useDashboardNavigation(), showSignIns = nav?.search.hide_sign_ins === "false";
  const signIns: FacetState | undefined = platform ? { values: { hide_sign_ins: showSignIns ? [] : ["true"] }, onChange: next => { const hide = Array.isArray(next.hide_sign_ins) && next.hide_sign_ins[0] === "true"; nav?.navigate({ ...nav.search, hide_sign_ins: hide ? undefined : "false", offset: undefined }); } } : undefined;
  const label = (t: AuditTarget) => t.search ? <ResourceLink search={t.search}>{t.name}</ResourceLink> : t.name;
  const columns: DataTableColumn<Audit>[] = [
    { id: "when", header: "When", rowHeader: true, hideable: false, cell: e => <DateTime value={e.created_at} /> },
    { id: "who", header: "Who", cell: e => { if (!e.actor_user_id) return <span className={s.muted}>System</span>; const who = auditActor(e, scope, session); return who ? label(who) : <span className={s.muted}>{platform ? "Unknown user" : "Workspace member"}</span>; } },
    // The event code is in the tooltip (and the row's "…" menu), not under every row.
    { id: "action", header: "Action", cell: e => <span title={e.action}>{auditEventLabel(e)}</span> },
    { id: "target", header: "Target", cell: e => { const t = target(e); return <CellText primary={t ? label(t) : <BitopBadge size="sm" variant="outline">{resourceTypeLabel(e.resource_type)}</BitopBadge>} secondary={t && !t.typed ? resourceTypeLabel(e.resource_type) : undefined} />; } }, // "API key in Product" already says its type
    { id: "workspace", header: platform ? "Team or project" : "Workspace", defaultHidden: !platform, cell: e => { const w = platform ? auditWorkspace(e) : workspace ? { name: workspace.name, search: { page: "workspace-settings", ws: workspace.id } as DashboardSearch } : undefined; return w ? label(w) : <span className={s.muted}>—</span>; } },
  ];
  const table = <DirectoryTable<Audit> path={workspace ? `${wsPath(workspace.id)}/audit` : `${platformPath}/audit`} label="Audit events" storageKey={platform ? "platform-audit" : "workspace-audit"} rowKey={e => e.id} rowLabel={e => `${auditEventLabel(e)} event`} columns={columns}
      facets={platform ? [signInFacet] : undefined} facetState={signIns}
      rowActions={e => [copyIdAction(e.resource_id, "Copy target ID"), copyIdAction(e.actor_user_id, "Copy actor ID"), copyIdAction(e.id, "Copy event ID"), copyIdAction(e.action, "Copy event code")]}
      empty={{ icon: <ScrollText />, title: "No audit events yet.", description: "No audit events are visible in this scope." }} />;
  // Inside workspace Settings it is a tab: a card like the other tabs, never a second page title (h1 "Settings" + h2 "Audit log").
  if (workspace) return <Card title="Audit log" description="Changes to models, keys, members and limits.">{table}</Card>;
  return <Stack gap={6} className={s.page}><Heading title="Audit log" description="Changes across the gateway. Other people's personal-workspace activity isn't shown." />{table}</Stack>;
}

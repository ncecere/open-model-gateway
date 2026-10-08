// Workspace invitations and audit; there is no organization tenant in this installation.
import { ScrollText } from "lucide-react";
import { Card } from "../components/ui/card/card";
import { api, wsPath, API, platformPath, type Session, type Workspace, type Invitation, type Audit, type PlatformUser, type Member, type CostCenter, type GroupMapping, type Model, type Catalog, type Provider, type ServiceAccount } from "../lib/api";
import type { KeyRow } from "../lib/keys";
import type { WorkspaceCatalogModel } from "../lib/model-setup";
import type { DashboardSearch } from "../lib/permissions";
import { auditEventLabel, resourceTypeLabel, type DirectoryWorkspace } from "../lib/people";
import { DirectoryTable, copyIdAction, signInFacet, type FacetState } from "../components/people";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { roleOptions } from "../lib/forms";
import { Badge, Button, CollectionTable, DateTime, Heading, Stack, Panel, useAction, useChoices } from "../components/ui";
import { ActionMenu } from "../components/templates/action-menu";
import s from "./shared.module.css";
export function Invitations({ workspace }: { session: Session; workspace: Workspace }) {
  const ask = useAction(), path = `${wsPath(workspace.id)}/invitations`;
  return <Card title="Invitations" description="We'll show the invite code once. Send it to them yourself, because no email is sent. It expires in 3 days and only works for someone who signs in with that email." actions={workspace.capabilities.manage_members && <Button size="sm" variant="secondary" onClick={() => ask({ title: "Invite by email", fields: [{ name: "email", label: "Email address", type: "email", required: true, maxLength: 320 }, { name: "role", label: "Workspace role", type: "select", required: true, value: "member", options: roleOptions.filter(o => o.value !== "owner") }], submitLabel: "Create invitation", secretLabel: "Copy the invite code", run: (v, signal) => api(path, { method: "POST", body: { email: v.email, role: v.role }, signal }) })}>Invite by email</Button>}><CollectionTable<Invitation> path={path} label="Invitations" empty="Invite someone by email when they can't be found by search." rowKey={i => i.id} columns={[{ title: "Email", render: i => i.email }, { title: "Role", render: i => i.role }, { title: "Status", render: i => <Badge>{i.accepted_at ? "Accepted" : i.revoked_at ? "Revoked" : Date.parse(i.expires_at) <= Date.now() ? "Expired" : "Pending"}</Badge> }, { title: "Expires", render: i => <DateTime value={i.expires_at} /> }, { title: "Actions", render: i => <ActionMenu label={`Actions for ${i.email}`} actions={[{ label: "Revoke invitation…", danger: true, hidden: !!i.accepted_at || !!i.revoked_at || !workspace.capabilities.manage_members, onSelect: () => ask({ title: `Revoke invitation for ${i.email}?`, description: "The existing token will stop working.", danger: true, submitLabel: "Revoke invitation", run: (_, signal) => api(`${path}/${encodeURIComponent(i.id)}`, { method: "DELETE", signal }) }) }]} /> }]} /></Card>;
}
export function AcceptInvitation({ email }: { email: string }) {
  const ask = useAction(); return <Stack gap={6} className={s.page}><Heading title="Accept invitation" description="Join a Team or Project with a securely shared invitation token." /><Panel title="Use your invitation token"><p>Signed in as <strong>{email}</strong>. The invitation must match your verified sign-in email. Signing in alone doesn't give access.</p><p>Never put invitation tokens in URLs or persistent storage. Existing higher manual roles are preserved.</p><Button onClick={() => ask({ title: "Accept invitation", fields: [{ name: "token", label: "Invitation token", type: "password", required: true, maxLength: 64, validate: v => /^[0-9a-f]{64}$/i.test(v) ? undefined : "Enter the full 64-character token." }], submitLabel: "Accept invitation", run: (v, signal) => api(`${API}/invitations/accept`, { method: "POST", body: { token: v.token }, signal }) })}>Enter invitation token</Button></Panel></Stack>;
}
/*
 * Audit log: who (email, not identifiers), a humanised action with its code
 * beneath, and the target's type with its name where an already-loaded list
 * resolves it; identifiers move to the row "…" menu. Provenance: Grounded
 * web/src/pages/admin/logs/audit.tsx and components/audit/labels.ts
 * (read-only reference). The gateway's privacy filtering is unchanged: the
 * platform log never includes personal-workspace events.
 */
type Target = { name: string; search?: DashboardSearch };
const named = <T extends { id: string },>(rows: T[] | undefined, name: (row: T) => string, search?: (row: T) => DashboardSearch) => new Map((rows ?? []).map(r => [r.id, { name: name(r), search: search?.(r) }] as const));
export function AuditHistory({ session, workspace }: { session?: Session; workspace?: Workspace }) {
  const platform = !workspace, wsMembers = !!workspace?.capabilities.manage_members;
  const users = useChoices<PlatformUser>(`${platformPath}/users`, platform), members = useChoices<Member>(workspace ? `${wsPath(workspace.id)}/members` : "", wsMembers);
  const shared = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`, platform), centers = useChoices<CostCenter>(`${platformPath}/cost-centers`, platform), mappings = useChoices<GroupMapping>(`${platformPath}/oidc/group-mappings`, platform);
  const models = useChoices<Model>(`${platformPath}/models`, platform), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`, platform), providers = useChoices<Provider>(`${platformPath}/providers`, platform);
  // Workspace scope: names of this workspace's keys (the ones the viewer can see), models and service accounts.
  const ws = workspace ? wsPath(workspace.id) : "";
  const keys = useChoices<KeyRow>(workspace ? `${ws}/keys` : "", !!workspace && workspace.capabilities.issue_own_key), wsModels = useChoices<WorkspaceCatalogModel>(workspace ? `${ws}/catalog` : "", !!workspace);
  const accounts = useChoices<ServiceAccount>(workspace ? `${ws}/service-accounts` : "", !!workspace?.capabilities.manage_service_accounts);
  const people = new Map<string, Target>([...named(users.data, u => u.display_name?.trim() || u.email || "Retained identity", u => ({ page: "user-detail", record: u.id }))]);
  for (const m of members.data ?? []) people.set(m.user_id, { name: m.display_name?.trim() || m.email });
  if (session) people.set(session.user.id, { name: platform ? session.user.email : "You" });
  const spaces = named<Workspace>([...(workspace ? [workspace] : []), ...shared.data ?? []].filter(w => w.kind !== "personal"), w => w.name, w => platform ? { page: "workspace-detail", record: w.id, kind: w.kind } : { page: "workspace-settings", ws: w.id });
  const lookup: Record<string, Map<string, Target>> = {
    user: people, workspace: spaces,
    cost_center: named(centers.data, c => c.name, () => ({ page: "cost-centers" })), group_mapping: named(mappings.data, m => m.group_value, () => ({ page: "oidc" })),
    model: platform ? named(models.data, m => m.display_name || m.public_name, m => ({ page: "model-detail", record: m.id })) : new Map((wsModels.data ?? []).map(m => [m.model_id, { name: m.display_name || m.public_name, search: { page: "grants", ws: workspace!.id } as DashboardSearch }] as const)),
    key: workspace ? named(keys.data, k => k.name, k => ({ page: "key-detail", ws: workspace.id, record: k.id })) : new Map(),
    service_account: workspace ? named(accounts.data, a => a.name, () => ({ page: "workspace-settings", ws: workspace.id, tab: "service-accounts" })) : new Map(),
    catalog: named(catalogs.data, c => c.name, c => ({ page: "catalog-detail", record: c.id })), provider: named(providers.data, p => p.name, p => ({ page: "provider-detail", record: p.id })),
  };
  // Loaded lists resolve linkable names; routes/prices fall back to the server's derived name. Admin never reads a
  // workspace's keys or service accounts, so those targets are named by their team/project: "API key in Product".
  const target = (e: Audit): Target | undefined => (e.resource_type && e.resource_id ? lookup[e.resource_type]?.get(e.resource_id) : undefined) ?? (e.target_name ? { name: e.target_name } : undefined) ?? inWorkspace(e);
  const inWorkspace = (e: Audit): Target | undefined => { const w = platform && e.workspace_id ? spaces.get(e.workspace_id) : undefined; return w && e.resource_type && e.resource_type !== "workspace" ? { name: `${resourceTypeLabel(e.resource_type)} in ${w.name}`, search: w.search } : undefined; };
  // Admin: sign-in side effects (group syncs) are hidden unless asked for (review #12); URL-backed as hide_sign_ins=false.
  const nav = useDashboardNavigation(), showSignIns = nav?.search.hide_sign_ins === "false";
  const signIns: FacetState | undefined = platform ? { values: { hide_sign_ins: showSignIns ? [] : ["true"] }, onChange: next => { const hide = Array.isArray(next.hide_sign_ins) && next.hide_sign_ins[0] === "true"; nav?.navigate({ ...nav.search, hide_sign_ins: hide ? undefined : "false", offset: undefined }); } } : undefined;
  const label = (t: Target) => t.search ? <ResourceLink search={t.search}>{t.name}</ResourceLink> : t.name;
  const columns: DataTableColumn<Audit>[] = [
    { id: "when", header: "When", rowHeader: true, hideable: false, cell: e => <DateTime value={e.created_at} /> },
    { id: "who", header: "Who", cell: e => !e.actor_user_id ? <span className={s.muted}>System</span> : people.has(e.actor_user_id) ? label(people.get(e.actor_user_id)!) : <span className={s.muted}>{platform ? "Unknown user" : "Workspace member"}</span> },
    // The event code is in the tooltip (and the row's "…" menu), not under every row.
    { id: "action", header: "Action", cell: e => <span title={e.action}>{auditEventLabel(e)}</span> },
    { id: "target", header: "Target", cell: e => { const t = target(e); return <CellText primary={t ? label(t) : <BitopBadge size="sm" variant="outline">{resourceTypeLabel(e.resource_type)}</BitopBadge>} secondary={t ? resourceTypeLabel(e.resource_type) : undefined} />; } },
    { id: "workspace", header: platform ? "Team or project" : "Workspace", defaultHidden: !platform, cell: e => { const w = e.workspace_id ? spaces.get(e.workspace_id) : undefined; return w ? label(w) : <span className={s.muted}>—</span>; } },
  ];
  return <Stack gap={6} className={s.page}><Heading title="Audit log" description={workspace ? "Changes to this workspace's models, keys, members and limits." : "Changes across the gateway. Other people's personal-workspace activity isn't shown."} />
    <DirectoryTable<Audit> path={workspace ? `${wsPath(workspace.id)}/audit` : `${platformPath}/audit`} label="Audit events" storageKey={platform ? "platform-audit" : "workspace-audit"} rowKey={e => e.id} rowLabel={e => `${auditEventLabel(e)} event`} columns={columns}
      facets={platform ? [signInFacet] : undefined} facetState={signIns}
      rowActions={e => [copyIdAction(e.resource_id, "Copy target ID"), copyIdAction(e.actor_user_id, "Copy actor ID"), copyIdAction(e.id, "Copy event ID"), copyIdAction(e.action, "Copy event code")]}
      empty={{ icon: <ScrollText />, title: "No audit events yet.", description: "No audit events are visible in this scope." }} /></Stack>;
}

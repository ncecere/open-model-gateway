import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Plus, UserPlus, UsersRound } from "lucide-react";
import { StatusPill } from "../components/templates/status-pill";
import { api, wsPath, platformPath, platformWorkspacePath, type Collection, type Session, type Workspace, type Key, type ServiceAccount, type Grant, type Member } from "../lib/api";
import { permissions } from "../lib/permissions";
import { nameField, parseCheckboxValues, roleField, roleOptions, type Field } from "../lib/forms";
import { addEach, batchFailureMessage } from "../lib/grants";
import { protocolLabel } from "../lib/model-setup";
import { toast } from "../components/ui/toast/toast";
import { Badge, Button, CollectionTable, DateTime, ErrorNotice, FormField, Heading, Id, NativeSelect, Stack, Alert, useAction, useApi, useChoices, type Action } from "../components/ui";
import { ActionMenu } from "../components/templates/action-menu";
import { IconCell, LabIcon } from "../components/provider-icon";
import { GrantBadges, LocalTable, PersonCell, PersonIdentity, RoleBadge, UserStatusBadge, copyIdAction } from "../components/people";
import { Card } from "../components/ui/card/card";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { workspaceRoleLabels, type DirectoryUser } from "../lib/people";
import { notAvailable } from "../lib/home";
import { Invitations } from "./organization";
import { ResourceLink } from "../components/navigation-link";
import { Dialog } from "../components/ui/dialog/dialog";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Combobox, type ComboboxOption } from "../components/ui/combobox/combobox";
import { WorkspaceModels } from "./model-catalog";
import s from "./shared.module.css";
export type Scope = { session: Session; workspace: Workspace };
/** API keys moved to pages/keys.tsx (list, create dialog) and pages/key-detail.tsx (key page). */
export { Keys } from "./keys";
export function ServiceAccounts({ session, workspace }: Scope) {
  const ask = useAction(), path = `${wsPath(workspace.id)}/service-accounts`, allowed = permissions(session, workspace).manageServiceAccounts;
  if (!allowed) return <Alert tone="info">Only workspace admins manage service accounts.</Alert>;
  // A card like Members (review rule 2): the action sits in the card header; status is a pill, never plain text.
  return <Card title="Service accounts" description="Accounts for apps and automations. Their keys keep working when people leave the team. Disable the account to stop them." actions={<Button size="sm" variant="secondary" onClick={() => ask({ title: "Create service account", fields: [nameField], submitLabel: "Create account", run: (v, signal) => api(path, { method: "POST", body: { name: v.name }, signal }) })}><Plus aria-hidden /> Create account</Button>}><CollectionTable<ServiceAccount> path={path} label="Service accounts" empty="Create an account, then create its key from API keys." rowKey={a => a.id} columns={[{ title: "Name", render: a => a.name }, { title: "Status", render: a => <StatusPill status={a.disabled_at ? "disabled" : "active"} /> }, { title: "Actions", render: a => <ActionMenu label={`Actions for ${a.name}`} actions={[{ label: "Create a key in API keys", hidden: !!a.disabled_at, render: <ResourceLink search={{ page: "keys", ws: workspace.id }} /> }, copyIdAction(a.id, "Copy account ID"), { label: a.disabled_at ? "Enable account" : "Disable account…", danger: !a.disabled_at, onSelect: () => ask({ title: `${a.disabled_at ? "Enable" : "Disable"} ${a.name}?`, description: a.disabled_at ? "Enabling doesn't restore revoked keys. Create new keys afterwards." : "All of this account's keys are revoked. Enabling it again doesn't restore them.", danger: !a.disabled_at, submitLabel: a.disabled_at ? "Enable account" : "Disable account", run: (_, signal) => api(`${path}/${encodeURIComponent(a.id)}`, { method: "PATCH", body: { disabled: !a.disabled_at }, signal }) }) }]} /> }]} /></Card>;
}
/*
 * Members as a card (Grounded admin team › Members): description, "Add member"
 * in the card header, search, Columns, avatar + email, role and grant-source
 * badges and a row "…" menu; pending invitations below for workspace admins.
 * People are added by choosing a person (email search), never by pasting an
 * ID. Workspace mode searches `member-candidates` (ux-api-contract §3); Admin
 * › Teams/Projects uses the platform member routes and the user directory
 * (contract §2). Provenance: Grounded web/src/pages/admin/people/team-members.tsx
 * (read-only reference).
 */
export function WorkspaceMembers({ session, workspace, platform = false }: Scope & { platform?: boolean }) {
  const [adding, setAdding] = useState(false);
  const writable = platform ? session.capabilities.platform_write : workspace.capabilities.manage_members;
  // Owner grants: actual owners in Workspace mode; Platform Admins through the platform routes.
  const canManageOwners = platform ? session.capabilities.platform_write : workspace.role === "owner";
  const path = platform ? `${platformWorkspacePath(workspace.id)}/members` : `${wsPath(workspace.id)}/members`;
  const readable = platform || workspace.capabilities.manage_members || (workspace.capabilities as { view_members?: boolean }).view_members === true;
  const title = workspace.kind === "project" ? "Project members" : "Team members";
  if (!readable) return <Card title="Members" description={`People with access to ${workspace.name}.`}><EmptyState size="compact" icon={<UsersRound />} title={workspace.role ? `You're ${workspace.role === "admin" ? "an" : "a"} ${workspaceRoleLabels[workspace.role]} of ${workspace.name}.` : "Members"} description="Only workspace admins can see the full member list and add people. Ask one if you need someone added." /></Card>;
  return <Stack gap={6}>
    <Card title="Members" description="People with access. Access from an SSO group updates when they sign in and can't be removed here." actions={writable && <Button size="sm" variant="secondary" onClick={() => setAdding(true)}><UserPlus aria-hidden /> Add member</Button>}>
      <MembersTable path={path} title={title} writable={writable} canManageOwners={canManageOwners} selfId={session.user.id} />
    </Card>
    {!platform && workspace.capabilities.manage_members && <Invitations session={session} workspace={workspace} />}
    {adding && <AddMemberDialog workspace={workspace} path={path} platform={platform} canManageOwners={canManageOwners} onClose={() => setAdding(false)} />}
  </Stack>;
}
type Candidate = { user_id: string; email: string };
/** Choose a person by email, then a role. Never asks for a user ID. */
export function AddMemberDialog({ workspace, path, platform, canManageOwners, onClose }: { workspace: Workspace; path: string; platform: boolean; canManageOwners: boolean; onClose: () => void }) {
  const client = useQueryClient(), [text, setText] = useState(""), [q, setQ] = useState(""), [picked, setPicked] = useState<ComboboxOption | null>(null), [role, setRole] = useState("member"), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  useEffect(() => { const timer = setTimeout(() => setQ(text.trim()), 250); return () => clearTimeout(timer); }, [text]);
  const searchable = !platform && q.length > 0 && q.length <= 200;
  const candidates = useApi<Collection<Candidate>>(`${wsPath(workspace.id)}/member-candidates?q=${encodeURIComponent(q)}`, searchable);
  // Platform Admins choose from the entitled directory; workspace admins never browse it.
  const directory = useChoices<DirectoryUser>(`${platformPath}/users?status=active`, platform);
  const unavailable = !platform && notAvailable(candidates.error);
  const items: ComboboxOption[] = platform ? (directory.data ?? []).filter(u => u.platform_role && u.email && !u.disabled_at).map(u => ({ value: u.id, label: u.email! })) : searchable ? (candidates.data?.data ?? []).map(c => ({ value: c.user_id, label: c.email })) : [];
  const options = picked && !items.some(i => i.value === picked.value) ? [picked, ...items] : items;
  const emptyText = platform ? (directory.isPending ? "Loading people…" : "No one matches.") : !q ? "Type part of an email address." : candidates.isFetching ? "Searching…" : "No one matches. People need platform access first, and can't already be members.";
  async function submit() {
    if (!picked || busy) return;
    setBusy(true); setError(undefined);
    try { await api(path, { method: "POST", body: { user_id: picked.value, role } }); void client.invalidateQueries({ queryKey: ["api"] }); toast.success(`${picked.label} added`); onClose(); }
    catch (caught) { setError(caught); }
    finally { setBusy(false); }
  }
  return <Dialog open title={`Add someone to ${workspace.name}`} description="Choose a person by email. They get access straight away." onOpenChange={open => { if (!open && !busy) onClose(); }} hideClose={busy}
    footer={<><Button variant="secondary" disabled={busy} onClick={onClose}>Cancel</Button><Button loading={busy} disabled={!picked || unavailable} onClick={() => void submit()}>Add member</Button></>}>
    <Stack gap={4}>
      {unavailable ? <Alert tone="info" title="People search isn't available yet">This gateway can't search people yet. Invite them by email from the Invitations section instead.</Alert> : <>
        <FormField label="Person" description={platform ? "Active people with platform access." : "Search by email. Only people with platform access who aren't members yet are listed."}>
          <Combobox<string> items={options} filter={platform ? undefined : null} value={picked?.value ?? null} onValueChange={(_, option) => setPicked(option)} onInputValueChange={setText} placeholder="name@example.com" emptyText={emptyText} disabled={busy} limit={50} />
        </FormField>
        {candidates.isError && !unavailable && <ErrorNotice error={candidates.error} retry={() => void candidates.refetch()} />}
        {directory.isError && <ErrorNotice error={directory.error} retry={() => void directory.refetch()} />}
        <FormField label="Role"><NativeSelect value={role} disabled={busy} onChange={event => setRole(event.target.value)}>{roleOptions.filter(o => o.value !== "owner" || canManageOwners).map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect></FormField>
      </>}
      {error !== undefined && <ErrorNotice error={error} />}
    </Stack>
  </Dialog>;
}
export function MembersTable({ path, title, canManageOwners, writable, selfId }: { path: string; title: string; canManageOwners: boolean; writable: boolean; /** The viewer, shown as "(you)". */ selfId?: string }) {
  const ask = useAction();
  const columns: DataTableColumn<Member>[] = [
    { id: "member", header: "Person", accessor: m => [m.display_name, m.email].filter(Boolean).join(" "), rowHeader: true, hideable: false, sortable: true, cell: m => <PersonIdentity person={m} self={m.user_id === selfId} /> },
    { id: "role", header: "Role", accessor: m => m.role ?? "", sortable: true, cell: m => <RoleBadge role={m.role} /> },
    { id: "sources", header: "Access", defaultHiddenNarrow: true, accessor: m => m.membership_source ?? "", cell: m => <GrantBadges grants={m.grants} empty="Unknown source" /> },
    { id: "status", header: "Status", accessor: m => m.disabled_at ? "suspended" : "active", cell: m => <UserStatusBadge user={{ disabled_at: m.disabled_at ?? null }} /> },
  ];
  return <LocalTable<Member> path={path} label={title} storageKey="workspace-members" search={{ placeholder: "Name or email" }} rowKey={m => m.user_id} rowLabel={m => m.email} columns={columns}
    rowActions={m => [
      { label: "Set manual role…", hidden: !writable, disabled: m.role === "owner" && !canManageOwners, disabledReason: "Only a workspace owner or Platform Admin can change owners", onSelect: () => ask({ title: `Set manual role for ${m.email}`, description: "Changes only the manual grant. Group-granted access remains independently effective. The server protects the last owner.", fields: [{ ...roleField, value: m.grants?.find(g => g.source === "manual")?.role ?? "member", options: roleField.options?.filter(o => o.value !== "owner" || canManageOwners) }], submitLabel: "Set manual role", run: (v, signal) => api(path, { method: "POST", body: { user_id: m.user_id, role: v.role }, signal }) }) },
      copyIdAction(m.user_id, "Copy user ID"),
      { label: "Remove manual access…", danger: true, hidden: !writable || !m.grants?.some(g => g.source === "manual"), disabled: m.role === "owner" && !canManageOwners, disabledReason: "Owner protected", onSelect: () => ask({ title: `Remove manual access for ${m.email}?`, description: "Group grants are retained. Keys are revoked only when effective workspace membership is lost. Removing the last owner is rejected.", danger: true, submitLabel: "Remove manual grant", run: (_, signal) => api(`${path}/${encodeURIComponent(m.user_id)}`, { method: "DELETE", signal }) }) },
    ]}
    empty={{ icon: <UsersRound />, title: "No members yet.", description: "No active grants are visible in this scope." }} />;
}
/**
 * Select several available models at once. The API adds one model per request, so this loops with a progress
 * notice; each model succeeds or fails on its own, and a retry only re-sends the ones not yet added.
 */
export function addModelsAction(path: string, choices: Grant[]): Action {
  const added = new Set<string>(), label = (id: string) => choices.find(g => g.model_id === id)?.display_name ?? id;
  return { title: "Add approved models", description: "Choose one or more models from your available catalogs.", fields: [{ name: "model_ids", label: "Available models", type: "checkboxes", required: true, value: "[]", maxSelections: choices.length, options: choices.map(g => ({ value: g.model_id, label: `${g.display_name} · ${g.public_name}` })) }], submitLabel: "Add selected models", successNotice: "Models added.",
    run: async (v, signal) => {
      const selected = parseCheckboxValues(v.model_ids), pending = selected.filter(id => !added.has(id));
      let notice = toast.add({ title: `Adding models: 0 of ${pending.length}`, timeout: 0 });
      const result = await addEach(pending, id => api(path, { method: "POST", body: { model_id: id }, signal }), signal, (done, total) => { toast.close(notice); notice = toast.add({ title: `Adding models: ${done} of ${total}`, timeout: done === total ? 3000 : 0 }); });
      toast.close(notice);
      result.added.forEach(id => added.add(id));
      if (result.failed.length || result.stopped) throw new Error(batchFailureMessage({ ...result, added: selected.filter(id => added.has(id)) }, selected.length, label));
    } };
}
/** Workspace › Models: the eligible catalog with Select/Remove (model-catalog.tsx). */
export function Grants({ session, workspace }: Scope) { return <WorkspaceModels session={session} workspace={workspace} />; }

/*
 * Admin People pages: Users, one user, Teams/Projects, SSO groups and cost
 * centers. Layout follows Grounded's admin people pages (ListPage lists with
 * search, segmented filters, Columns and row "…" menus; one-page user record
 * with a header "…" menu). Provenance: Grounded web/src/pages/admin/people/
 * {users,user,teams}.tsx and group-mapping/rules.tsx (read-only reference).
 * Identifiers are never shown as text; "Copy ID" lives in the "…" menus.
 */
import { DetailTime } from "../components/templates/when";
import { History, LayoutDashboard, Landmark, Network, Plus, UserCheck, UserPlus, Users, UsersRound, UserX } from "lucide-react";
import { api, platformPath, type Session, type Workspace, type PlatformRole, type CostCenter, type GroupMapping } from "../lib/api";
import { nameField, enabledField, uuidError, type Field } from "../lib/forms";
import { activeGrants, grantLabel, grantRemoval, kindLabels, platformRoleLabels, type DirectoryUser, type DirectoryWorkspace, type UserRecord } from "../lib/people";
import { Button, DateTime, ErrorNotice, Heading, Stack, Panel, Alert, useAction, useApi, useChoices } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { ResourcePage } from "../components/resource-page";
import { ActionMenu, type ActionItem } from "../components/templates/action-menu";
import { DirectoryTable, GrantBadges, LocalTable, PersonCell, PersonIdentity, PlatformRoleBadge, PlatformRoleCell, RoleBadge, UserStatusBadge, WorkspaceStatusBadge, copyIdAction } from "../components/people";
import { UserActivity } from "../components/user-activity";
import { useResourceName } from "../components/layout/breadcrumbs";
import { Badge, StatusBadge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { DescriptionList, FactsLine } from "../components/ui/description-list/description-list";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { PageHeader } from "../components/ui/page-header/page-header";
import { Table, Td, Tr } from "../components/ui/table/table";
import { Time } from "../components/ui/time/time";
import s from "./shared.module.css";
export type NavigateScope = (ws?: string, page?: import("../lib/permissions").Page) => void;
export const platformRoleOptions = ["user", "auditor", "admin"].map(value => ({ value, label: `Platform ${value[0].toUpperCase()}${value.slice(1)}` }));
export const platformRoleField: Field = { name: "platform_role", label: "Platform role", type: "select", required: true, value: "user", options: platformRoleOptions, help: "Admins and auditors can also use the gateway as users. Team and project membership is set separately." };
export function workspaceCreateBody(v: Record<string, string>, kind: "team" | "project") { return { name: v.name, kind, owner_user_id: v.owner_user_id }; }
const enc = encodeURIComponent;
const userName = (u: { email: string | null; display_name?: string | null }) => u.display_name?.trim() || u.email || "Retained identity";
type Ask = ReturnType<typeof useAction>;
/** Confirms and removes a manual/bootstrap platform grant (DELETE /platform/users/{id}/roles/{role}); the server enforces the safeguards. */
const removeGrant = (ask: Ask, user: DirectoryUser, grant: { role: string; source: string }) => ask({ ...grantRemoval(user, grant), danger: true, successNotice: "Grant removed.", run: (_, signal) => api(`${platformPath}/users/${enc(user.id)}/roles/${enc(grant.role)}`, { method: "DELETE", signal }) });

/** The initial-owner field: entitled active users by email, or an ID when the directory can't load. */
function ownerField(users: DirectoryUser[] | undefined): Field {
  const options = users?.filter(u => u.platform_role && !u.disabled_at && u.email).map(u => ({ value: u.id, label: u.email! }));
  return options ? { name: "owner_user_id", label: "Initial owner", type: "select", required: true, options, help: "An active user with a platform role. This creates an independent manual owner membership." } : { name: "owner_user_id", label: "Initial owner user ID", required: true, validate: uuidError, help: "An active user with a platform role. This creates an independent manual owner membership." };
}

export function PlatformTeams({ session, kind }: { session: Session; kind: "team" | "project" }) {
  const ask = useAction(), title = kind === "project" ? "Projects" : "Teams", noun = kind === "project" ? "Project" : "Team";
  const users = useChoices<DirectoryUser>(`${platformPath}/users?status=active`, session.capabilities.create_workspace);
  const create = session.capabilities.create_workspace && <Button onClick={() => ask({ title: `Create ${kind}`, fields: [nameField, ownerField(users.data)], submitLabel: `Create ${kind}`, run: (v, signal) => api(`${platformPath}/workspaces`, { method: "POST", body: workspaceCreateBody(v, kind), signal }) })}><Plus aria-hidden />{`Create ${kind}`}</Button>;
  const detail = (w: Workspace, tab?: string) => ({ page: "workspace-detail" as const, record: w.id, kind, tab });
  const columns: DataTableColumn<DirectoryWorkspace>[] = [
    { id: "name", header: noun, rowHeader: true, hideable: false, cell: w => <PersonCell name={w.name} shape="square"><CellText primary={<ResourceLink search={detail(w)}>{w.name}</ResourceLink>} /></PersonCell> },
    { id: "members", header: "Members", numeric: true, defaultHiddenNarrow: true, cell: w => w.member_count ?? <span className={s.muted}>—</span> },
    { id: "cost-center", header: "Cost center", cell: w => w.cost_center ? <CellText primary={w.cost_center.name} secondary={<span className={s.mono}>{w.cost_center.code}</span>} /> : <span className={s.muted}>{w.cost_center_id ? "Assigned" : "Unallocated"}</span> },
    { id: "status", header: "Status", cell: w => <WorkspaceStatusBadge disabled={!!w.disabled_at} /> },
    { id: "created", header: "Created", defaultHidden: true, cell: w => <DateTime value={w.created_at} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title={title} description={kind === "project" ? "Shared workspaces that cut across teams. A project has its own members; being in a team doesn't add you." : "Shared workspaces with their own members, models and limits."} actions={create} />
    <DirectoryTable<DirectoryWorkspace> path={`${platformPath}/workspaces?kind=${kind}`} label={title} storageKey={`platform-${kind}s`} search={{ placeholder: "Name" }} rowKey={w => w.id} rowLabel={w => w.name} columns={columns}
      facets={[{ id: "status", label: "Status", options: [{ value: "active", label: "Active" }, { value: "disabled", label: "Disabled" }] }]}
      rowActions={w => [
        { label: "Open", render: <ResourceLink search={detail(w)} /> },
        { label: "Members", render: <ResourceLink search={detail(w, "members")} /> },
        { label: "Limits", render: <ResourceLink search={detail(w, "limits")} /> },
        copyIdAction(w.id),
      ]}
      empty={{ icon: kind === "project" ? <Network /> : <UsersRound />, title: `No ${title.toLowerCase()} yet.`, filtered: `No ${title.toLowerCase()} match.`, description: `Only Platform Admins create ${title.toLowerCase()}. Personal workspaces are never listed in this shared directory.`, action: create || undefined }} />
  </Stack>;
}

export function PlatformUsers({ session }: { session: Session }) {
  const ask = useAction();
  const provision = session.capabilities.platform_write && <Button onClick={() => ask({ title: "Add user", description: "The user must still sign in through the configured OIDC provider. Their personal workspace is created the first time they sign in.", fields: [{ name: "email", label: "Email", type: "email", required: true, maxLength: 320 }, platformRoleField], submitLabel: "Add user", run: (v, signal) => api(`${platformPath}/users`, { method: "POST", body: { email: v.email, platform_role: v.platform_role }, signal }) })}><UserPlus aria-hidden /> Add user</Button>;
  const columns: DataTableColumn<DirectoryUser>[] = [
    { id: "user", header: "User", rowHeader: true, hideable: false, cell: u => u.cleaned_at ? <PersonCell name={userName(u)}><CellText primary={<ResourceLink search={{ page: "user-detail", record: u.id }}>{userName(u)}</ResourceLink>} secondary="Attribution retained" /></PersonCell> : <PersonIdentity person={u} self={u.id === session.user.id} link={{ page: "user-detail", record: u.id }} /> },
    { id: "role", header: "Platform role", cell: u => <PlatformRoleCell user={u} /> },
    { id: "grants", header: "Grants", defaultHiddenNarrow: true, cell: u => <GrantBadges grants={u.role_grants ? activeGrants(u.role_grants) : undefined} empty={u.role_grants ? undefined : "Unknown source"} /> }, // Revoking a grant happens on the user's page, with confirmation (review #44).
    { id: "status", header: "Status", cell: u => <UserStatusBadge user={u} /> },
    { id: "shared", header: "Teams & projects", numeric: true, defaultHiddenNarrow: true, cell: u => u.shared_workspace_count ?? <span className={s.muted}>—</span> },
    { id: "last-sign-in", header: "Last sign-in", cell: u => u.last_sign_in_at ? <DateTime value={u.last_sign_in_at} /> : <span className={s.muted}>{u.last_sign_in_at === null ? "Never" : "—"}</span> },
    { id: "created", header: "Added", defaultHidden: true, cell: u => <DateTime value={u.created_at} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Users" description="Everyone who can use this gateway. Signing in alone doesn't give access; a role comes from an SSO group or a manual grant." actions={provision} />
    <DirectoryTable<DirectoryUser> path={`${platformPath}/users`} label="Users" storageKey="platform-users" search={{ placeholder: "Name or email" }} rowKey={u => u.id} rowLabel={userName} columns={columns}
      facets={[{ id: "role", label: "Platform role", options: [{ value: "none", label: "No role" }, { value: "user", label: "Platform user" }, { value: "auditor", label: "Platform auditor" }, { value: "admin", label: "Platform admin" }] }, { id: "status", label: "Status", options: [{ value: "active", label: "Active" }, { value: "suspended", label: "Suspended" }] }]}
      rowActions={u => [{ label: "Open", render: <ResourceLink search={{ page: "user-detail", record: u.id }} /> }, copyIdAction(u.id)]}
      empty={{ icon: <Users />, title: "No users yet.", filtered: "No users match.", description: "Add a user, or map an SSO group to a role.", action: provision || undefined }} />
  </Stack>;
}

export function UserDetail({ session, id, tab, onTabChange }: { session: Session; id: string; tab?: string; onTabChange: (tab: string) => void }) {
  const q = useApi<UserRecord>(`${platformPath}/users/${enc(id)}`, session.capabilities.platform_read), ask = useAction();
  useResourceName(q.data?.id === id ? userName(q.data) : undefined);
  if (q.isPending) return <p role="status">Loading user…</p>; if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />; if (q.data.id !== id) return <ErrorNotice error={new Error("The returned user does not match this record.")} />;
  const u = q.data, name = userName(u), writable = session.capabilities.platform_write && !u.cleaned_at, self = u.id === session.user.id, path = `${platformPath}/users/${enc(id)}`;
  const grants = u.role_grants ?? [], current = activeGrants(grants), revoked = grants.filter(g => g.revoked_at), memberships = u.shared_memberships;
  const grantRole = () => ask({ title: "Grant manual platform role", fields: [platformRoleField], submitLabel: "Grant role", run: (v, signal) => api(`${path}/roles`, { method: "POST", body: { role: v.platform_role }, signal }) });
  const actions: ActionItem[] = [
    { label: "Grant platform role…", icon: <UserPlus aria-hidden />, hidden: !writable, onSelect: grantRole },
    { label: "Change email…", hidden: !writable, onSelect: () => ask({ title: `Change email for ${name}`, description: "Changing the email revokes this user's browser sessions. Sign-in still requires the configured OIDC provider.", fields: [{ name: "email", label: "Email", type: "email", required: true, value: u.email ?? "", maxLength: 320 }], submitLabel: "Save email", run: (v, signal) => api(path, { method: "PATCH", body: { email: v.email }, signal }) }) },
    { label: "Reactivate", icon: <UserCheck aria-hidden />, hidden: !writable || !u.disabled_at, onSelect: () => ask({ title: `Reactivate ${name}?`, description: "Restores access from their remaining grants. Revoked keys and sessions do not come back; the user signs in again and issues new keys.", submitLabel: "Reactivate", run: (_, signal) => api(path, { method: "PATCH", body: { disabled: false }, signal }) }) },
    { label: "Suspend…", icon: <UserX aria-hidden />, danger: true, hidden: !writable || !!u.disabled_at, disabled: self, disabledReason: "You can't suspend yourself. Ask another Platform Admin.", onSelect: () => ask({ title: `Suspend ${name}?`, description: "Immediate: revokes sessions and user-owned keys. Group synchronization cannot undo it, and reactivation does not restore revoked credentials. Last-admin and last-owner safeguards apply.", danger: true, submitLabel: "Suspend user", run: (_, signal) => api(path, { method: "PATCH", body: { disabled: true }, signal }) }) },
    copyIdAction(u.id),
  ];
  const signIn = (value: string | null | undefined) => value ? <DetailTime value={value} /> : value === null ? "Never" : "Unavailable";
  const overview = <Stack gap={6}>
    {u.disabled_at && !u.cleaned_at && <Alert tone="warning" title="Suspended">{grants.some(g => !g.revoked_at) ? "Role grants are retained but give no access while suspended. " : ""}Reactivation does not restore revoked sessions or keys.{u.cleanup_due_at ? <> Grace-period cleanup <Time value={u.cleanup_due_at} format="date" />.</> : null}</Alert>}
    {u.cleaned_at && <Alert tone="info" title="Cleaned">Personal data was removed after the grace period. Attribution in history is retained.</Alert>}
    <Card title="Profile and access" actions={writable && <Button size="sm" variant="secondary" onClick={grantRole}><UserPlus aria-hidden /> Grant role</Button>}>
      <DescriptionList items={[
        { label: "Name", value: u.display_name?.trim() || <span className={s.muted}>Not provided by the identity provider</span> },
        { label: "Email", value: u.email ?? <span className={s.muted}>Retained identity</span> },
        { label: "Platform role", value: <PlatformRoleCell user={u} /> },
        { label: "Role grants", value: grants.length ? <span className={s.badges}>{current.length > 0 && <GrantBadges grants={current} groupHint="SSO group grants update at sign-in" onRemove={writable ? g => removeGrant(ask, u, g) : undefined} />}{revoked.map((g, i) => <Badge key={g.id ?? i} size="sm" variant="outline" tone="neutral"><span className={s.muted}>{grantLabel(g)} · revoked</span></Badge>)}</span> : <span className={s.muted}>No grants recorded.</span> },
        { label: "First signed in", value: signIn(u.first_sign_in_at) },
        { label: "Last sign-in", value: signIn(u.last_sign_in_at) },
      ]} />
    </Card>
  </Stack>;
  return <ResourcePage title={name} meta={<><UserStatusBadge user={u} />{u.platform_role && <PlatformRoleBadge role={u.platform_role} />}</>} description={self ? "This is you. Another Platform Admin can suspend your account." : "Platform roles do not grant another person's private keys or request details."}
      facts={<FactsLine items={[{ label: "Last sign-in", value: u.last_sign_in_at ? <>Last sign-in <DateTime value={u.last_sign_in_at} /></> : u.last_sign_in_at === null ? "Never signed in" : undefined }, { label: "Shared workspaces", value: memberships ? `${memberships.length} ${memberships.length === 1 ? "shared workspace" : "shared workspaces"}` : undefined }].filter(f => f.value !== undefined)} />} actions={<ActionMenu label="More actions" size="md" actions={actions} />} tab={tab} onTabChange={onTabChange} tabs={[
    { value: "overview", label: "Overview", icon: <LayoutDashboard aria-hidden />, content: overview },
    { value: "workspaces", label: "Shared workspaces", icon: <UsersRound aria-hidden />, count: memberships?.length, content: <>
    <Card title="Shared workspaces" description="Teams and Projects this person can use. Personal workspaces are private and never listed." flush={!!memberships?.length}>
      {!memberships ? <p className={s.muted}>Shared memberships are not available from this gateway.</p> : memberships.length === 0 ? <EmptyState size="compact" icon={<UsersRound />} title="Not a member of any team or project." description="Owners and workspace admins add members on the workspace's Members page." /> :
        <Table caption="Shared workspace memberships" columns={["Workspace", "Kind", "Role", "Access"]}>{memberships.map(m => <Tr key={m.workspace_id}>
          <Td><PersonCell name={m.name} shape="square"><CellText primary={<ResourceLink search={{ page: "workspace-detail", record: m.workspace_id, kind: m.kind }}>{m.name}</ResourceLink>} secondary={m.disabled_at ? "Disabled" : undefined} /></PersonCell></Td>
          <Td><Badge>{kindLabels[m.kind]}</Badge></Td><Td><RoleBadge role={m.role} /></Td><Td><GrantBadges grants={m.grants ?? m.sources.map(source => ({ role: m.role, source }))} /></Td>
        </Tr>)}</Table>}
    </Card>
    </> },
    { value: "activity", label: "Activity", icon: <History aria-hidden />, content: <UserActivity user={u} name={name} /> },
  ]} />;
}

export function mappingFields(session: Session, mapping?: GroupMapping, shared?: Workspace[]): Field[] {
  return [{ name: "issuer", label: "OIDC issuer", required: true, value: mapping?.issuer ?? "", maxLength: 2048, help: "Exact issuer of the configured generic identity provider. This is a role mapping, not a client-secret form.", validate: v => { try { const u = new URL(v); return (u.protocol === "https:" || u.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(u.hostname)) && !u.username && !u.password && !u.search && !u.hash ? undefined : "Use a clean HTTPS issuer URL."; } catch { return "Enter the provider's issuer URL."; } } }, { name: "group_value", label: "Verified group-claim value", required: true, value: mapping?.group_value ?? "", maxLength: 512, help: "Group claim paths are configured server-side (GATEWAY_OIDC_GROUPS_CLAIM). Do not paste tokens or client secrets." }, { name: "target_kind", label: "Mapping target", type: "select", required: true, value: mapping?.target_kind ?? "platform", options: [{ value: "platform", label: "Platform role" }, { value: "workspace", label: "Team or Project membership" }] }, { ...platformRoleField, value: mapping?.platform_role ?? "user", visibleWhen: v => v.target_kind === "platform" }, { name: "workspace_id", label: "Shared workspace", type: "select", required: true, value: mapping?.workspace_id ?? "", options: (shared ?? session.workspaces).filter(w => w.kind !== "personal").map(w => ({ value: w.id, label: `${w.name} · ${w.kind}` })), visibleWhen: v => v.target_kind === "workspace" }, { name: "workspace_role", label: "Membership role", type: "select", required: true, value: mapping?.workspace_role ?? "member", options: [{ value: "member", label: "Member" }, { value: "admin", label: "Admin" }], visibleWhen: v => v.target_kind === "workspace", help: "Group mappings cannot grant workspace ownership." }, { ...enabledField, value: String(mapping?.enabled ?? true) }];
}
export function mappingBody(v: Record<string, string>) { return { issuer: v.issuer, group_value: v.group_value, target_kind: v.target_kind, platform_role: v.target_kind === "platform" ? v.platform_role as PlatformRole : null, workspace_id: v.target_kind === "workspace" ? v.workspace_id : null, workspace_role: v.target_kind === "workspace" ? v.workspace_role : null, enabled: v.enabled === "true" }; }
export function OidcMappings({ session }: { session: Session }) {
  const ask = useAction(), path = `${platformPath}/oidc/group-mappings`, writable = session.capabilities.platform_write;
  const shared = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`, session.capabilities.platform_read), names = new Map([...session.workspaces, ...shared.data ?? []].map(w => [w.id, w]));
  const create = writable && <Button onClick={() => ask({ title: "Create SSO group mapping", fields: mappingFields(session, undefined, shared.data), submitLabel: "Create mapping", run: (v, signal) => api(path, { method: "POST", body: mappingBody(v), signal }) })}><Plus aria-hidden /> Create mapping</Button>;
  const target = (m: GroupMapping) => m.target_kind === "platform" ? (m.platform_role ? platformRoleLabels[m.platform_role] : "Platform") : names.get(m.workspace_id ?? "")?.name ?? "Unknown workspace";
  const columns: DataTableColumn<GroupMapping>[] = [
    { id: "group", header: "IdP group", accessor: m => m.group_value, rowHeader: true, hideable: false, cell: m => <span className={s.mono}>{m.group_value}</span> },
    { id: "target", header: "Grants", accessor: target, cell: m => m.target_kind === "platform" ? <PlatformRoleBadge role={m.platform_role} /> : <span className={s.badges}>{m.workspace_id && names.has(m.workspace_id) ? <ResourceLink search={{ page: "workspace-detail", record: m.workspace_id, kind: names.get(m.workspace_id)!.kind as "team" | "project" }}>{target(m)}</ResourceLink> : <span className={s.muted}>{target(m)}</span>}{m.workspace_role && <RoleBadge role={m.workspace_role} />}</span> },
    { id: "issuer", header: "Issuer", accessor: m => m.issuer, defaultHidden: true, cell: m => <span className={s.mono}>{m.issuer}</span> },
    { id: "status", header: "Status", accessor: m => m.enabled ? "enabled" : "disabled", cell: m => m.enabled ? <StatusBadge tone="success">Enabled</StatusBadge> : <StatusBadge tone="neutral">Disabled</StatusBadge> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="SSO groups" description="Give people a platform role or team/project membership from their identity-provider groups. People without a mapped role can't sign in." actions={create} />
    <Alert tone="info">Group changes apply the next time someone signs in, not continuously. To remove access right away, suspend the user. Manual grants aren't affected by group changes.</Alert>
    <LocalTable<GroupMapping> path={path} label="SSO group mappings" storageKey="sso-groups" search={{ placeholder: "Group or target" }} rowKey={m => m.id} rowLabel={m => m.group_value} columns={columns}
      facets={[{ id: "status", label: "Status", options: [{ value: "enabled", label: "Enabled" }, { value: "disabled", label: "Disabled" }], accessor: m => m.enabled ? "enabled" : "disabled" }]}
      rowActions={m => [
        { label: "Edit mapping…", hidden: !writable, onSelect: () => ask({ title: "Edit SSO group mapping", fields: mappingFields(session, m, shared.data), run: (v, signal) => api(`${path}/${enc(m.id)}`, { method: "PATCH", body: mappingBody(v), signal }) }) },
        copyIdAction(m.id),
        { label: "Delete mapping…", danger: true, hidden: !writable, onSelect: () => ask({ title: "Delete group mapping?", description: "Mapped grants are reconciled at the next sign-in. Independently assigned manual grants are preserved. Use suspension for immediate access revocation.", danger: true, submitLabel: "Delete mapping", run: (_, signal) => api(`${path}/${enc(m.id)}`, { method: "DELETE", signal }) }) },
      ]}
      empty={{ icon: <Network />, title: "No SSO group mappings.", description: "Create provider-neutral mappings. Authentication alone must never infer a platform role.", action: create || undefined }} />
  </Stack>;
}

export function CostCenters({ session }: { session: Session }) {
  const ask = useAction(), path = `${platformPath}/cost-centers`, writable = session.capabilities.platform_write, fields = (c?: CostCenter): Field[] => [{ ...nameField, value: c?.name }, { name: "code", label: "Code", required: true, value: c?.code ?? "", maxLength: 120 }];
  const centers = useChoices<CostCenter>(path, session.capabilities.platform_read), shared = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`, writable);
  const create = writable && <Button onClick={() => ask({ title: "Create cost center", fields: fields(), submitLabel: "Create cost center", run: (v, signal) => api(path, { method: "POST", body: { name: v.name, code: v.code }, signal }) })}><Plus aria-hidden /> Create cost center</Button>;
  const columns: DataTableColumn<CostCenter>[] = [
    { id: "name", header: "Cost center", accessor: c => c.name, rowHeader: true, hideable: false, cell: c => <PersonCell name={c.name} shape="square"><CellText primary={c.name} /></PersonCell> },
    { id: "code", header: "Code", accessor: c => c.code, cell: c => <span className={s.mono}>{c.code}</span> },
    { id: "status", header: "Status", accessor: c => c.archived_at ? "archived" : "active", cell: c => c.archived_at ? <StatusBadge tone="neutral">Archived</StatusBadge> : <StatusBadge tone="success">Active</StatusBadge> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Cost centers" description="Label spending for chargeback. Changes apply to new requests only; past usage keeps its label. Usage without a cost center is Unallocated." actions={create} />
    <LocalTable<CostCenter> rows={centers.data} loading={centers.isFetching} error={centers.error} retry={() => void centers.refetch()} label="Cost centers" storageKey="cost-centers" search={{ placeholder: "Name or code" }} rowKey={c => c.id} rowLabel={c => c.name} columns={columns}
      facets={[{ id: "status", label: "Status", options: [{ value: "active", label: "Active" }, { value: "archived", label: "Archived" }], accessor: c => c.archived_at ? "archived" : "active" }]}
      rowActions={c => [
        { label: "Edit details…", hidden: !writable || !!c.archived_at, onSelect: () => ask({ title: "Edit cost center", description: "Historical execution labels remain unchanged.", fields: fields(c), run: (v, signal) => api(`${path}/${enc(c.id)}`, { method: "PATCH", body: { name: v.name, code: v.code }, signal }) }) },
        copyIdAction(c.id),
        { label: "Archive cost center…", danger: true, hidden: !writable || !!c.archived_at, onSelect: () => ask({ title: `Archive ${c.name}?`, description: "Past usage keeps this label. New requests need an active cost center or are Unallocated.", danger: true, submitLabel: "Archive cost center", run: (_, signal) => api(`${path}/${enc(c.id)}`, { method: "DELETE", signal }) }) },
      ]}
      empty={{ icon: <Landmark />, title: "No cost centers yet.", description: "Allocation is optional. Unassigned usage is fully tracked as Unallocated.", action: create || undefined }} />
    {writable && <Panel title="Assign a workspace"><p>Sets the future allocation of a Team or Project. Personal-workspace allocations are set from the platform cost report's Workspaces breakdown; this page never fetches another person's personal-workspace metadata, keys or activity.</p><Button variant="secondary" disabled={!shared.isSuccess || !centers.isSuccess} onClick={() => ask({ title: "Assign workspace allocation", fields: [{ name: "workspace_id", label: "Team or project", type: "select", required: true, options: shared.data?.map(w => ({ value: w.id, label: `${w.name} · ${kindLabels[w.kind]}${w.cost_center ? ` · ${w.cost_center.name}` : ""}` })) ?? [] }, { name: "cost_center_id", label: "Cost center", type: "select", options: centers.data?.filter(c => !c.archived_at).map(c => ({ value: c.id, label: `${c.name} · ${c.code}` })) ?? [], help: "Blank removes allocation; future requests are Unallocated." }], submitLabel: "Set future allocation", run: (v, signal) => api(`${platformPath}/workspaces/${enc(v.workspace_id)}`, { method: "PATCH", body: { cost_center_id: v.cost_center_id || null }, signal }) })}>Assign workspace</Button>{(shared.isError || centers.isError) && <ErrorNotice error={shared.error ?? centers.error} />}</Panel>}
  </Stack>;
}

import { useEffect, type ReactNode } from "react";
import { Bot, Boxes, Gauge, LayoutDashboard, Library, ScrollText, Settings2, ShieldCheck, UsersRound } from "lucide-react";
import { api, wsPath, platformWorkspacePath, type Session, type Workspace, type CostCenter, type CatalogAvailability, type Grant } from "../lib/api";
import { ResourcePage } from "../components/resource-page";
import { DateTime, ErrorNotice, Panel, Stack, Alert, StatCard, useApi, useChoices, Button, useAction } from "../components/ui";
import { nameField } from "../lib/forms";
import { kindLabels, type DirectoryWorkspace } from "../lib/people";
import { SettingsForm } from "../components/templates/settings-form";
import { ActionMenu } from "../components/templates/action-menu";
import { RoleBadge, WorkspaceStatusBadge, copyIdAction } from "../components/people";
import { Badge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { DangerAction, DangerZone } from "../components/templates/notices";
import { WorkspaceMembers, ServiceAccounts } from "./workspace";
import { AuditHistory } from "./organization";
import { EffectiveLimits } from "./workspace-limits";
import { permissions } from "../lib/permissions";
import { canRenameWorkspace } from "../lib/access";
import { Governance } from "./governance";
import { ScopeLimits } from "../components/scope-limits";
import { EffectiveAccess } from "../components/effective-access";
import { WorkspaceModelAccess } from "./model-access";
import { ResourceLink } from "../components/navigation-link";
import type { Scope } from "./workspace";
import s from "./shared.module.css";
/*
 * Settings (Workspace portal). Team/Project: General, Members, Service accounts
 * (admins), Limits, Access, Audit log. Personal: Limits (read-only, set by the
 * platform), Access and Audit log; its name is fixed and it has no General tab.
 * Access (effective access: why a model can't be used, layers collapsed) is its
 * own tab so Limits stays a short table.
 * API keys and Usage & costs are pages of their own, not settings tabs
 * (their old ?tab= links redirect; lib/locations.ts). Members see read-only
 * Members, Limits and Audit log.
 */
export function workspaceSettingsTabs(session: Session, workspace: Workspace): string[] {
  if (workspace.kind === "personal") return ["limits", "access", "audit"];
  return ["overview", "members", ...(permissions(session, workspace).manageServiceAccounts ? ["service-accounts"] : []), "limits", "access", "audit"];
}
export function WorkspaceSettings({ session, workspace, tab, onTabChange }: Scope & { tab?: string; onTabChange: (tab: string) => void }) {
  const personal = workspace.kind === "personal", editLimits = !personal && permissions(session, workspace).managePolicy;
  // Icon pill tabs like Grounded's settings and the Admin detail tabs (review #32).
  const content: Record<string, { label: string; icon: ReactNode; content: ReactNode }> = {
    overview: { label: "General", icon: <Settings2 aria-hidden />, content: <WorkspaceGeneral session={session} workspace={workspace} /> },
    members: { label: "Members", icon: <UsersRound aria-hidden />, content: <WorkspaceMembers session={session} workspace={workspace} /> },
    "service-accounts": { label: "Service accounts", icon: <Bot aria-hidden />, content: <ServiceAccounts session={session} workspace={workspace} /> },
    limits: { label: "Limits", icon: <Gauge aria-hidden />, content: editLimits ? <Governance session={session} workspace={workspace} /> : <EffectiveLimits workspace={workspace} /> },
    access: { label: "Access", icon: <ShieldCheck aria-hidden />, content: <EffectiveAccess workspace={workspace} title="Access" canManageModels={permissions(session, workspace).manageGrants} /> },
    audit: { label: "Audit log", icon: <ScrollText aria-hidden />, content: <AuditHistory session={session} workspace={workspace} /> },
  };
  // Invitations now live in Members.
  const current = tab === "invitations" ? "members" : tab;
  return <ResourcePage title="Settings" description={personal ? "Private to you. Admins see cost totals only." : `Members, limits and access for ${workspace.name}.`} tab={current} onTabChange={onTabChange} tabs={workspaceSettingsTabs(session, workspace).map(value => ({ value, ...content[value]! }))} />;
}
function WorkspaceGeneral({ session, workspace }: Scope) {
  const rename = canRenameWorkspace(workspace);
  // /me has no cost center: read the workspace itself, and never show "None" until it is known.
  const detail = useApi<Workspace>(wsPath(workspace.id)), center = detail.data?.cost_center;
  const centerText = detail.isPending ? "Loading…" : detail.isError ? "Unknown · couldn't load" : center ? `${center.name} · ${center.code} · set by a Platform Admin` : detail.data?.cost_center_id ? "Assigned · set by a Platform Admin" : "None · set by a Platform Admin";
  return <Stack gap={6}>
    <Card title="About this workspace">
      <DescriptionList items={[
        { label: "Type", value: kindLabels[workspace.kind] },
        { label: "Your role", value: <RoleBadge role={workspace.role} /> },
        { label: "Cost center", value: centerText },
        ...(workspace.created_at ? [{ label: "Created", value: <DateTime value={workspace.created_at} /> }] : []),
        ...(rename ? [] : [{ label: "Name", value: workspace.name }]),
      ]} />
    </Card>
    {rename && <Panel title="Rename"><SettingsForm fields={[{ ...nameField, value: workspace.name }]} writable onSave={(v, signal) => api(wsPath(workspace.id), { method: "PATCH", body: { name: v.name }, signal })} /></Panel>}
    {session.capabilities.platform_read && <p className={s.note}>You can also manage this {kindLabels[workspace.kind].toLowerCase()} from <ResourceLink search={{ page: "workspace-detail", record: workspace.id, kind: workspace.kind }}>Admin</ResourceLink>.</p>}
  </Stack>;
}
/*
 * Admin › Teams/Projects › one workspace. Header, icon tabs with counts and an
 * Overview of stat cards follow Grounded's admin team page (provenance:
 * Grounded web/src/pages/admin/people/team.tsx, team-overview.tsx; read-only
 * reference). Only counts already served by existing endpoints are shown:
 * no spend, usage or activity numbers are fabricated.
 */
export function WorkspaceDetail({ session, id, kind, tab, onTabChange }: { session: Session; id: string; kind?: "personal" | "team" | "project"; tab?: string; onTabChange: (tab: string) => void }) {
  const mine = session.workspaces.find(w => w.id === id), modelsVisible = session.capabilities.platform_write || !!mine?.role;
  const q = useApi<DirectoryWorkspace>(platformWorkspacePath(id), session.capabilities.platform_read), centers = useChoices<CostCenter>("/api/v1/platform/cost-centers", session.capabilities.platform_write), ask = useAction();
  const catalogs = useApi<CatalogAvailability>(`${platformWorkspacePath(id)}/catalogs`, session.capabilities.platform_read && q.isSuccess && q.data.kind !== "personal");
  const models = useChoices<Grant>(`${wsPath(id)}/models`, modelsVisible && q.isSuccess && q.data.kind !== "personal");
  // The former Catalogs and Models tabs are one "Model access" tab; old links land there.
  const legacyTab = tab === "catalogs" || tab === "models";
  useEffect(() => { if (legacyTab) onTabChange("model-access"); }, [legacyTab]);
  if (q.isPending) return <p role="status">Loading shared workspace…</p>; if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id || q.data.kind === "personal" || kind && kind !== q.data.kind) return <ErrorNotice error={new Error("Shared directory resource does not match this route.")} />;
  // Administrative metadata supplies no invented membership. /me capabilities remain authoritative.
  // A disabled workspace stays readable (Members, Models, Limits, General) but read-only until it's enabled.
  const disabled = !!q.data.disabled_at, writable = session.capabilities.platform_write && !disabled;
  const readSession: Session = disabled ? { ...session, capabilities: { ...session.capabilities, platform_write: false } } : session;
  const workspace: Workspace = { ...q.data, owner_user_id: null, role: mine?.role ?? null, membership_source: mine?.membership_source ?? null, capabilities: { issue_own_key: false, manage_members: writable, manage_service_accounts: writable, manage_policy: writable, delegate_models: writable, view_all_activity: writable } };
  const data = q.data, noun = kindLabels[workspace.kind], members = data.member_count, center = data.cost_center;
  const toggle = () => ask({ title: `${workspace.disabled_at ? "Enable" : "Disable"} ${workspace.name}?`, description: workspace.disabled_at ? "Revoked keys don't come back; issue new ones after enabling." : "Refuses new requests and revokes its keys. Past usage is kept.", danger: !workspace.disabled_at, submitLabel: workspace.disabled_at ? "Enable workspace" : "Disable workspace", run: (_, signal) => api(platformWorkspacePath(id), { method: "PATCH", body: { disabled: !workspace.disabled_at }, signal }) });
  // Admin is read-only for Auditors even if an independent membership authorizes
  // workspace-mode administration. Member/private operations stay in Workspace.
  // Each fact once: kind and status in the header, members and cost center in the stat cards (tab counts repeat nothing new).
  return <ResourcePage title={workspace.name} meta={<><Badge size="sm">{noun}</Badge><WorkspaceStatusBadge disabled={!!workspace.disabled_at} /></>}
    actions={<ActionMenu label="More actions" size="md" actions={[{ label: "Open workspace", hidden: !mine?.role || disabled, render: <ResourceLink search={{ page: "overview", ws: id }} /> }, { label: "Enable workspace…", hidden: !session.capabilities.platform_write || !disabled, onSelect: toggle }, copyIdAction(id), { label: "Disable workspace…", danger: true, hidden: !writable || !!workspace.disabled_at, onSelect: toggle }]} />}
    notices={disabled && <Alert tone="warning" title="Disabled">Requests are refused and keys were revoked; enabling doesn't restore them. Read-only until enabled.</Alert>}
    tab={legacyTab ? "model-access" : tab} onTabChange={onTabChange} tabs={[
      { value: "overview", label: "Overview", icon: <LayoutDashboard aria-hidden />, content: <Stack gap={6}>
        <div className={s.stats}>
          <StatCard label="Members" value={members ?? "—"} icon={<UsersRound />} hint={mine?.role ? <>You are <RoleBadge role={mine.role} /></> : "Users holding an active grant"} />
          <StatCard label="Available catalogs" value={catalogs.data ? catalogs.data.effective_catalog_ids.length : "—"} icon={<Library />} hint={catalogs.data ? catalogs.data.mode === "inherit" ? `Uses ${noun.toLowerCase()} defaults` : "Own catalog choice" : catalogs.isError ? "Unavailable" : undefined} />
          {modelsVisible && <StatCard label="Models available" value={models.data ? models.data.length : "—"} icon={<Boxes />} hint={models.data ? `${models.data.filter(g => g.direct_granted).length} assigned directly` : models.isError ? "Unavailable" : undefined} />}
          <StatCard label="Cost center" value={center?.code ?? (workspace.cost_center_id ? "Assigned" : "Unallocated")} hint={center ? center.name : workspace.cost_center_id ? "Name unavailable" : "Applies to new requests"} />
        </div>
        <Card title="Details"><DescriptionList items={[...(mine?.role ? [] : [{ label: "Your membership", value: <span className={s.muted}>None</span> }]), { label: "Created", value: <DateTime value={workspace.created_at} /> }]} /></Card>
      </Stack> },
      { value: "members", label: "Members", icon: <UsersRound aria-hidden />, count: members, content: <WorkspaceMembers session={readSession} workspace={workspace} platform /> },
      { value: "model-access", label: "Models", icon: <Boxes aria-hidden />, count: modelsVisible ? models.data?.length : undefined, content: <WorkspaceModelAccess session={session} workspace={workspace} /> },
      { value: "limits", label: "Limits", icon: <Gauge aria-hidden />, content: <ScopeLimits mode="replacement" path={`${platformWorkspacePath(id)}/policy`} writable={writable} kind={workspace.kind} /> },
      { value: "access", label: "Access", icon: <ShieldCheck aria-hidden />, content: <EffectiveAccess workspace={workspace} title="Access" /> },
      { value: "settings", label: "General", icon: <Settings2 aria-hidden />, content: <Stack gap={6}><Panel title="Administrative settings"><SettingsForm fields={[{ ...nameField, value: workspace.name }, { name: "cost_center_id", label: "Cost center", type: "select", value: workspace.cost_center_id ?? "", options: centers.data?.filter(c => !c.archived_at).map(c => ({ value: c.id, label: `${c.name} · ${c.code}` })) ?? [], help: "Applies to new requests only. Empty: Unallocated." }]} writable={writable && centers.isSuccess} onSave={(v, signal) => api(platformWorkspacePath(id), { method: "PATCH", body: { name: v.name, cost_center_id: v.cost_center_id || null }, signal })} />{centers.isError && <ErrorNotice error={centers.error} />}</Panel>{session.capabilities.platform_write && (disabled
        ? <Panel title="Disabled"><Button variant="secondary" onClick={toggle}>Enable workspace</Button></Panel>
        : <DangerZone><DangerAction title={`Disable ${noun.toLowerCase()}`} description="Refuses new requests and revokes its keys. Enabling later doesn't restore them." action={<Button variant="danger" onClick={toggle}>Disable workspace</Button>} /></DangerZone>)}</Stack> },
    ]} />;
}

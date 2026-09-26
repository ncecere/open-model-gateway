import { Activity, Building2, Coins, Cpu, FileClock, KeyRound, LayoutDashboard, Mail, Network, Plug, Route, Settings, Shield, Users, UserRound } from "lucide-react";
import type { Organization, Session, Workspace } from "./api";
import { canAdministerOrganization, isAdmin, type DashboardSearch, type Page } from "./permissions";

export const navigation = [
  { page: "overview", label: "Overview", icon: LayoutDashboard, group: "Workspace" },
  { page: "keys", label: "API keys", icon: KeyRound, group: "Workspace" },
  { page: "grants", label: "Model access", icon: Cpu, group: "Workspace" },
  { page: "service-accounts", label: "Service accounts", icon: Settings, group: "Workspace" },
  { page: "costs", label: "Costs", icon: Coins, group: "Workspace" },
  { page: "governance", label: "Governance", icon: Shield, group: "Workspace" },
  { page: "organizations", label: "Organizations", icon: Building2, group: "Platform" },
  { page: "platform-teams", label: "Teams", icon: Network, group: "Platform" },
  { page: "platform-projects", label: "Projects", icon: Network, group: "Platform" },
  { page: "users", label: "Users", icon: UserRound, group: "Platform" },
  { page: "teams", label: "Teams", icon: Network, group: "Organization" },
  { page: "projects", label: "Projects", icon: Network, group: "Organization" },
  { page: "assigned-models", label: "Assigned models", icon: Cpu, group: "Organization" },
  { page: "organization-policy", label: "Organization limits", icon: Shield, group: "Organization" },
  { page: "organization-members", label: "Members", icon: Users, group: "Organization" },
  { page: "invitations", label: "Invitations", icon: Mail, group: "Organization" },
  { page: "members", label: "Workspace members", icon: Users, group: "Shared workspace" },
  { page: "model-access", label: "Model access", icon: Shield, group: "Models" },
  { page: "models", label: "Models", icon: Cpu, group: "Models" },
  { page: "providers", label: "Provider connections", icon: Plug, group: "Models" },
  { page: "deployments", label: "Deployments", icon: Activity, group: "Models" },
  { page: "routing", label: "Routing", icon: Route, group: "Models" },
  { page: "pricing", label: "Pricing", icon: Coins, group: "Oversight" },
  { page: "platform-audit", label: "Platform audit", icon: FileClock, group: "Oversight" },
  { page: "audit", label: "Audit history", icon: FileClock, group: "Organization" },
] satisfies { page: Page; label: string; icon: typeof Users; group: string }[];

export function pageScope(page: Page): "platform" | "organization" | "workspace" {
  if (["organizations", "users", "platform-teams", "platform-projects", "models", "providers", "deployments", "routing", "pricing", "model-access", "platform-audit"].includes(page)) return "platform";
  if (["teams", "projects", "organization-members", "invitations", "assigned-models", "organization-policy", "audit"].includes(page)) return "organization";
  return "workspace";
}
export function adminGroups(page: Page, platformAdmin = false, organizationAdmin = true) {
  return platformAdmin
    ? ["Platform", "Models", "Oversight", ...(pageScope(page) === "workspace" ? ["Shared workspace"] : [])]
    : [...(organizationAdmin ? ["Organization"] : []), "Shared workspace"];
}
export function scopeSearch(page: Page, org?: string, ws?: string): DashboardSearch {
  const scope = pageScope(page);
  return { page, org: scope === "platform" ? undefined : org, ws: scope === "workspace" ? ws : undefined };
}
export function adminLanding(session: Session, org?: Organization): Page {
  if (session.user.platform_admin || !canAdministerOrganization(session, org)) return "organizations";
  return organizationLanding(session, org!);
}
export function organizationLanding(session: Session, org: Organization): "teams" | "projects" {
  if (isAdmin(org.role)) return "teams";
  const shared = session.workspaces.filter(ws => ws.organization_id === org.id && isAdmin(ws.role));
  return shared.some(ws => ws.kind === "project") && !shared.some(ws => ws.kind === "team") ? "projects" : "teams";
}
export function adminDestination(session: Session, org?: Organization, workspace?: Workspace): DashboardSearch {
  if (session.user.platform_admin) return scopeSearch("organizations");
  if (org && isAdmin(org.role)) return scopeSearch(organizationLanding(session, org), org.id);
  const managed = session.workspaces.filter(ws => ws.kind !== "personal" && isAdmin(ws.role) && session.organizations.some(o => o.id === ws.organization_id));
  const selected = managed.find(ws => ws.id === workspace?.id) ?? managed.find(ws => ws.organization_id === org?.id);
  if (selected) return scopeSearch("members", selected.organization_id, selected.id);
  const administeredOrg = session.organizations.find(o => isAdmin(o.role));
  if (administeredOrg) return scopeSearch(organizationLanding(session, administeredOrg), administeredOrg.id);
  const first = managed[0];
  return first ? scopeSearch("members", first.organization_id, first.id) : scopeSearch("overview", org?.id);
}
export type ContextItem = { label: string; org?: string; ws?: string; page?: Page };
// Workspace mode switches all accessible resources. Admin mode only offers
// administered scopes; parent membership is not organization administration.
export function contextOptions(session: Session, org?: Organization, admin = false): { label: string; items: ContextItem[] }[] {
  if (admin) {
    const managed = session.workspaces.filter(ws => ws.kind !== "personal" && isAdmin(ws.role) && session.organizations.some(o => o.id === ws.organization_id));
    const item = (ws: Workspace): ContextItem => ({ label: session.organizations.length > 1 ? `${ws.name} · ${session.organizations.find(o => o.id === ws.organization_id)!.name}` : ws.name, org: ws.organization_id, ws: ws.id, page: "members" });
    return [
      { label: "Platform", items: session.user.platform_admin ? [{ label: "Platform", page: "organizations" as const }] : [] },
      { label: "Organizations", items: session.organizations.filter(o => session.user.platform_admin || isAdmin(o.role)).map(o => ({ label: o.name, org: o.id, page: organizationLanding(session, o) })) },
      { label: "Teams", items: managed.filter(ws => ws.kind === "team").map(item) },
      { label: "Projects", items: managed.filter(ws => ws.kind === "project").map(item) },
    ].filter(group => group.items.length);
  }
  const workspaces = session.workspaces.filter(ws => ws.organization_id === org?.id);
  return [
    { label: "Platform", items: session.user.platform_admin ? [{ label: "Platform", page: "organizations" as const }] : [] },
    { label: "Organizations", items: session.organizations.map(o => ({ label: o.name, org: o.id, ws: undefined as string | undefined })) },
    { label: "Personal · private", items: workspaces.filter(ws => ws.kind === "personal").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
    { label: "Teams", items: workspaces.filter(ws => ws.kind === "team").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
    { label: "Projects", items: workspaces.filter(ws => ws.kind === "project").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
  ].filter(group => group.items.length);
}

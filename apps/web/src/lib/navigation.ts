import { Activity, Building2, Coins, Cpu, FileClock, KeyRound, LayoutDashboard, Network, Plug, Settings, UserRound } from "lucide-react";
import type { Organization, Session, Workspace } from "./api";
import { canView, isAdmin, type DashboardSearch, type Page } from "./permissions";
import { canonicalSearch, dashboardHref, parseDashboardLocation } from "./locations";

export const navigation = [
  { page: "overview", label: "Overview", icon: LayoutDashboard, group: "Workspace" },
  { page: "keys", label: "API keys", icon: KeyRound, group: "Workspace" },
  { page: "grants", label: "Models", icon: Cpu, group: "Workspace" },
  { page: "costs", label: "Costs", icon: Coins, group: "Workspace" },
  { page: "workspace-settings", label: "Workspace settings", icon: Settings, group: "Workspace" },
  { page: "organization-settings", label: "Organization settings", icon: Building2, group: "Workspace" },
  { page: "platform-overview", label: "Overview", icon: LayoutDashboard, group: "Platform" },
  { page: "organizations", label: "Organizations", icon: Building2, group: "Platform" },
  { page: "platform-teams", label: "Teams", icon: Network, group: "Platform" },
  { page: "platform-projects", label: "Projects", icon: Network, group: "Platform" },
  { page: "users", label: "Users", icon: UserRound, group: "Platform" },
  { page: "models", label: "Models", icon: Cpu, group: "Models" },
  { page: "providers", label: "Provider connections", icon: Plug, group: "Models" },
  { page: "deployments", label: "Deployments", icon: Activity, group: "Models" },
  { page: "platform-audit", label: "Platform audit", icon: FileClock, group: "Oversight" },
] satisfies { page: Page; label: string; icon: typeof Settings; group: string }[];

export function pageScope(page: Page): "platform" | "organization" | "workspace" {
  if (["platform-overview", "model-detail", "provider-detail", "deployment-detail", "organizations", "users", "platform-teams", "platform-projects", "models", "providers", "deployments", "routing", "pricing", "model-access", "platform-audit"].includes(page)) return "platform";
  if (["organization-detail", "organization-settings", "teams", "projects", "organization-members", "invitations", "assigned-models", "organization-policy", "audit"].includes(page)) return "organization";
  return "workspace";
}
export function isAdminPage(page: Page) { return pageScope(page) === "platform" || page === "organization-detail"; }
export function adminGroups(_page: Page, platformAdmin = false, _organizationAdmin = true) { return platformAdmin ? ["Platform", "Models", "Oversight"] : []; }
export function scopeSearch(page: Page, org?: string, ws?: string): DashboardSearch {
  const scope = pageScope(page);
  const standalone = page === "profile" || page === "accept-invitation";
  return canonicalSearch({ page, org: scope === "platform" || standalone ? undefined : org, ws: scope === "workspace" && !standalone ? ws : undefined });
}
export function organizationLanding(_session: Session, _org: Organization): Page { return "organization-settings"; }
export function adminLanding(session: Session, org?: Organization): Page { return adminDestination(session, org).page!; }
export function adminDestination(session: Session, org?: Organization, workspace?: Workspace): DashboardSearch {
  if (session.user.platform_admin) return scopeSearch("platform-overview");
  if (org && isAdmin(org.role)) return scopeSearch("organization-settings", org.id);
  const managed = session.workspaces.filter(ws => ws.kind !== "personal" && isAdmin(ws.role) && session.organizations.some(o => o.id === ws.organization_id));
  const selected = managed.find(ws => ws.id === workspace?.id) ?? managed.find(ws => ws.organization_id === org?.id) ?? managed[0];
  if (selected) return scopeSearch("workspace-settings", selected.organization_id, selected.id);
  const administeredOrg = session.organizations.find(o => isAdmin(o.role));
  return administeredOrg ? scopeSearch("organization-settings", administeredOrg.id) : scopeSearch("overview", org?.id);
}
export type ContextItem = { label: string; org?: string; ws?: string; page?: Page };
export function contextOptions(session: Session, org?: Organization, admin = false): { label: string; items: ContextItem[] }[] {
  // Admin has fixed Platform context; objects in that portal never switch its sidebar.
  if (admin) return session.user.platform_admin ? [{ label: "Platform", items: [{ label: "Platform", page: "platform-overview" }] }] : [];
  const workspaces = session.workspaces.filter(ws => ws.organization_id === org?.id);
  return [
    { label: "Organizations", items: session.organizations.map(o => ({ label: o.name, org: o.id, ws: undefined as string | undefined })) },
    { label: "Personal · private", items: workspaces.filter(ws => ws.kind === "personal").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
    { label: "Teams", items: workspaces.filter(ws => ws.kind === "team").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
    { label: "Projects", items: workspaces.filter(ws => ws.kind === "project").map(ws => ({ label: ws.name, org: ws.organization_id, ws: ws.id })) },
  ].filter(group => group.items.length);
}

// Old directory bookmarks were also used by scoped administrators. Translate
// them within the requested organization instead of sending them into Admin.
export function resolveLegacyDashboardLocation(href: string, session: Session): DashboardSearch | undefined {
  const parsed = parseDashboardLocation(href);
  if (!parsed || session.user.platform_admin) return parsed;
  const url = new URL(href, "https://dashboard.invalid");
  const legacy = url.pathname === "/" ? url.searchParams.get("page") : null;
  if (legacy !== "organizations" && legacy !== "teams" && legacy !== "projects") return parsed;
  const org = parsed.org ? session.organizations.find(item => item.id === parsed.org)
    : session.organizations.find(item => isAdmin(item.role)) ?? session.organizations.find(item => session.workspaces.some(ws => ws.organization_id === item.id && ws.kind !== "personal" && isAdmin(ws.role)));
  if (!org) return parsed;
  if (isAdmin(org.role)) return { page: "organization-settings", org: org.id, tab: legacy === "organizations" ? "overview" : legacy };
  const managed = session.workspaces.find(ws => ws.organization_id === org.id && ws.kind !== "personal" && isAdmin(ws.role) && (legacy === "organizations" || ws.kind === (legacy === "projects" ? "project" : "team")));
  return managed ? { page: "workspace-settings", org: org.id, ws: managed.id } : parsed;
}

export function resolveDashboardSearch(input: DashboardSearch, session: Session): DashboardSearch {
  const search = canonicalSearch(input);
  const page = search.page ?? (session.user.platform_admin ? "platform-overview" : "overview");
  if (pageScope(page) === "platform" || page === "profile" || page === "accept-invitation") return { ...search, page, org: undefined, ws: undefined };
  // An explicit unavailable scope is preserved for the not-found screen. In
  // particular, a bad org must never resolve a workspace from the first org.
  const org = search.org ?? (search.ws ? session.workspaces.find(ws => ws.id === search.ws)?.organization_id : session.organizations[0]?.id);
  const available = session.organizations.some(item => item.id === org);
  const workspaces = available ? session.workspaces.filter(ws => ws.organization_id === org) : [];
  const ws = pageScope(page) === "workspace" ? search.ws ?? workspaces.find(ws => ws.kind === "personal")?.id ?? workspaces[0]?.id : undefined;
  return { ...search, page, org, ws };
}
// Sidebar context is navigation-only. Organization settings keep their own
// route/API scope and never inherit a workspace's permissions or data.
export function sidebarWorkspace(session: Session, search: DashboardSearch, remembered?: Workspace): Workspace | undefined {
  const org = session.organizations.find(item => item.id === search.org);
  if (!org || !search.page || isAdminPage(search.page) || search.page === "profile" || search.page === "accept-invitation") return undefined;
  const available = session.workspaces.filter(item => item.organization_id === org.id);
  if (search.ws) return available.find(item => item.id === search.ws);
  if (search.page !== "organization-settings" || !canView(search.page, session, org)) return undefined;
  return available.find(item => item.id === remembered?.id)
    ?? available.find(item => item.kind === "personal") ?? available[0];
}

export function authorizedLocation(search: DashboardSearch, session: Session): boolean {
  const page = search.page;
  if (!page) return false;
  const org = session.organizations.find(o => o.id === search.org);
  const ws = session.workspaces.find(w => w.id === search.ws && w.organization_id === org?.id);
  if (search.org && !org || search.ws && !ws) return false;
  if (pageScope(page) === "organization" && !org) return false;
  return canView(page, session, org, ws);
}
const storagePrefix = "omg:portal:";
type Portal = "admin" | "workspace";
const storageKey = (session: Session, portal: Portal) => `${storagePrefix}${session.user.id}:${portal}`;
export function rememberPortal(storage: Pick<Storage, "setItem">, session: Session, search: DashboardSearch) {
  if (!authorizedLocation(search, session) || search.page === "profile" || search.page === "accept-invitation") return;
  // Search text is deliberately excluded: storage holds navigation metadata only.
  const { page, org, ws, record, tab } = canonicalSearch(search);
  try { storage.setItem(storageKey(session, isAdminPage(page!) ? "admin" : "workspace"), dashboardHref({ page, org, ws, record, tab })); } catch { /* Storage may be disabled. */ }
}
export function rememberedPortal(storage: Pick<Storage, "getItem" | "removeItem">, session: Session, portal: Portal): DashboardSearch | undefined {
  try {
    const key = storageKey(session, portal);
    const href = storage.getItem(key);
    const search = href ? parseDashboardLocation(href) : undefined;
    if (search?.page && authorizedLocation(search, session) && isAdminPage(search.page) === (portal === "admin") && search.page !== "profile" && search.page !== "accept-invitation") {
      const { page, org, ws, record, tab } = search;
      return { page, org, ws, record, tab };
    }
    storage.removeItem(key);
  } catch { /* Storage may be disabled. */ }
  return undefined;
}
const workspaceContextKey = (session: Session, org: string) => `${storagePrefix}${session.user.id}:context:${org}`;
export function rememberWorkspace(storage: Pick<Storage, "setItem">, session: Session, search: DashboardSearch) {
  if (!search.org || !search.ws || !search.page || pageScope(search.page) !== "workspace" || search.page === "profile" || search.page === "accept-invitation" || !authorizedLocation(search, session)) return;
  try { storage.setItem(workspaceContextKey(session, search.org), search.ws); } catch { /* Storage may be disabled. */ }
}
export function rememberedWorkspace(storage: Pick<Storage, "getItem" | "removeItem">, session: Session, org: string): Workspace | undefined {
  try {
    const key = workspaceContextKey(session, org);
    const id = storage.getItem(key);
    const workspace = session.organizations.some(item => item.id === org)
      ? session.workspaces.find(item => item.organization_id === org && item.id === id) : undefined;
    if (workspace) return workspace;
    storage.removeItem(key);
  } catch { /* Storage may be disabled. */ }
  return undefined;
}
export function clearRememberedPortals(storage: Pick<Storage, "length" | "key" | "removeItem">) {
  try { for (let i = storage.length - 1; i >= 0; i--) { const key = storage.key(i); if (key?.startsWith(storagePrefix)) storage.removeItem(key); } } catch { /* Storage may be disabled. */ }
}

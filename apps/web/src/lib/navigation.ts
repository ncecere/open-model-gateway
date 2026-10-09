import { BellRing, LayoutDashboard, KeyRound, Cpu, ChartColumn, Settings, Users, UsersRound, FolderKanban, Plug, Library, Gauge, Network, FileClock, Building2, House, ListTree, SlidersHorizontal, ShieldCheck, Mail, LogIn, ShieldAlert, FileText } from "lucide-react";
import type { Session, Workspace } from "./api";
import { canView, inWorkspacePortal, platformPages, userPages, type Page, type DashboardSearch } from "./permissions";
import { canonicalSearch, dashboardHref, parseDashboardLocation } from "./locations";
/**
 * Sidebar items. Workspace portal (Grounded's two-level pattern): a user-level "Workspace" group
 * (Home), then a group titled with the selected workspace's name holding that workspace's pages
 * (`group: SELECTED`). Admin groups are unchanged.
 */
export const SELECTED = "Selected workspace";
export const navigation = [
  { page: "home", label: "Home", icon: House, group: "Workspace" },
  { page: "overview", label: "Overview", icon: LayoutDashboard, group: SELECTED }, { page: "grants", label: "Models", icon: Cpu, group: SELECTED }, { page: "keys", label: "API keys", icon: KeyRound, group: SELECTED }, { page: "requests", label: "Logs", icon: ListTree, group: SELECTED }, { page: "files", label: "Files", icon: FileText, group: SELECTED }, { page: "costs", label: "Usage & costs", icon: ChartColumn, group: SELECTED }, { page: "workspace-settings", label: "Settings", icon: Settings, group: SELECTED },
  { page: "platform-overview", label: "Overview", icon: LayoutDashboard, group: "" },
  { page: "users", label: "Users", icon: Users, group: "People" }, { page: "platform-teams", label: "Teams", icon: UsersRound, group: "People" }, { page: "platform-projects", label: "Projects", icon: FolderKanban, group: "People" }, { page: "oidc", label: "SSO groups", icon: Network, group: "People" },
  // Deployments are a model's routes and routing policy lives on the model page. Their old
  // pages stay reachable by deep link (see hiddenPages) but leave the sidebar and jump search.
  { page: "providers", label: "Connections", icon: Plug, group: "Models" }, { page: "models", label: "Models", icon: Cpu, group: "Models" }, { page: "catalogs", label: "Catalogs", icon: Library, group: "Models" },
  { page: "platform-costs", label: "Usage & costs", icon: ChartColumn, group: "Usage & spend" }, { page: "platform-logs", label: "Logs", icon: ListTree, group: "Usage & spend" }, { page: "cost-centers", label: "Cost centers", icon: Building2, group: "Usage & spend" }, { page: "platform-audit", label: "Audit log", icon: FileClock, group: "Records" }, { page: "key-safety", label: "Key safety", icon: ShieldAlert, group: "Records" },
  // Admin › Settings (docs/settings.md): installation-wide settings; Limits moved here as "Defaults & limits".
  { page: "settings-general", label: "General", icon: SlidersHorizontal, group: "Settings" }, { page: "policies", label: "Defaults & limits", icon: Gauge, group: "Settings" }, { page: "settings-privacy", label: "Data & privacy", icon: ShieldCheck, group: "Settings" }, { page: "settings-email", label: "Email", icon: Mail, group: "Settings" }, { page: "settings-alerts", label: "Alerts", icon: BellRing, group: "Settings" }, { page: "settings-sign-in", label: "Sign-in", icon: LogIn, group: "Settings" },
] satisfies { page: Page; label: string; icon: typeof Settings; group: string }[];
export type NavItem = typeof navigation[number];
export function pageScope(page: Page): "platform" | "workspace" { return platformPages.has(page) ? "platform" : "workspace"; }
export const isAdminPage = (page: Page) => platformPages.has(page);
export function adminGroups(_page: Page) { return ["", "People", "Models", "Usage & spend", "Records", "Settings"]; }
/**
 * Workspace-portal sidebar sections: "Workspace" (user-level) and, when a workspace is selected,
 * one titled with its name. Items are filtered by live /me capabilities.
 */
export function workspaceSections(session: Session, active?: Workspace): { id: string; label: string; items: NavItem[] }[] {
  const sections = [{ id: "user", label: "Workspace", items: navigation.filter(n => n.group === "Workspace") }];
  if (active && inWorkspacePortal(session, active)) sections.push({ id: "selected", label: active.name, items: navigation.filter(n => n.group === SELECTED && canView(n.page, session, active)) });
  return sections;
}
/** Labels of pages outside the sidebar: create forms, records and legacy deep links. */
export const hiddenPages: Partial<Record<Page, string>> = { "batch-detail": "Batch", "alert-rule-detail": "Alert rule", "workspace-alert-detail": "Alert rule", notifications: "Notifications", "model-new": "Add model", pricing: "Pricing", deployments: "All routes", routing: "Routing", "deployment-detail": "Route", "request-detail": "Request", "session-detail": "Session", "platform-log-detail": "Request", "platform-log-session": "Session", "key-detail": "API key", "workspace-model": "Model", "model-compare": "Compare models", "platform-model-compare": "Compare models" };
/** The sidebar item (and breadcrumb parent) a record, form or legacy page belongs to. */
export const navParents: Partial<Record<Page, Page>> = { "batch-detail": "requests", "alert-rule-detail": "settings-alerts", "workspace-alert-detail": "workspace-settings", "workspace-detail": "platform-teams", "model-new": "models", "model-detail": "models", "provider-detail": "providers", deployments: "models", routing: "models", pricing: "models", "deployment-detail": "models", "catalog-detail": "catalogs", "user-detail": "users", "request-detail": "requests", "session-detail": "requests", "platform-log-detail": "platform-logs", "platform-log-session": "platform-logs", "key-detail": "keys", "workspace-model": "grants", "model-compare": "grants", "platform-model-compare": "models" };
export function activeAdminGroup(page: Page): string | undefined {
  return navigation.find(item => item.page === (navParents[page] ?? page))?.group || undefined;
}
export function scopeSearch(page: Page, ws?: string): DashboardSearch { return canonicalSearch({ page, ws: platformPages.has(page) || userPages.has(page) ? undefined : ws }); }
/** Workspaces the Workspace portal offers: own Personal plus actual Team/Project memberships. */
export function portalWorkspaces(session: Session): Workspace[] { return session.workspaces.filter(ws => inWorkspacePortal(session, ws)); }
export const personalWorkspace = (session: Session) => session.workspaces.find(w => w.kind === "personal" && w.owner_user_id === session.user.id && !w.disabled_at);
export type ContextItem = { label: string; ws: string; workspace: Workspace };
export function contextOptions(session: Session): { label: string; items: ContextItem[] }[] {
  const eligible = portalWorkspaces(session);
  return ([["personal", "Personal"], ["team", "Teams"], ["project", "Projects"]] as const).map(([kind, label]) => ({ label, items: eligible.filter(ws => ws.kind === kind).map(workspace => ({ label: workspace.name, ws: workspace.id, workspace })) })).filter(g => g.items.length);
}
export function resolveDashboardSearch(input: DashboardSearch, session: Session): DashboardSearch {
  const search = canonicalSearch(input), page = search.page ?? "overview";
  if (platformPages.has(page) || userPages.has(page)) return { ...search, page, ws: undefined };
  return { ...search, page, ws: search.ws ?? personalWorkspace(session)?.id ?? portalWorkspaces(session).find(w => w.kind !== "personal")?.id };
}
export const resolveLegacyDashboardLocation = (href: string, _session?: Session) => parseDashboardLocation(href);
export function authorizedLocation(search: DashboardSearch, session: Session) {
  if (!search.page) return false;
  const workspace = session.workspaces.find(ws => ws.id === search.ws);
  if (search.ws && (!workspace || workspace.disabled_at)) return false;
  return canView(search.page, session, workspace);
}
const prefix = "omg:enterprise:portal:";
const key = (session: Session, portal: "admin" | "workspace") => `${prefix}${session.user.id}:${portal}`;
const contextKey = (session: Session) => `${prefix}${session.user.id}:context`;
export function rememberPortal(storage: Pick<Storage, "setItem">, session: Session, search: DashboardSearch) {
  if (!authorizedLocation(search, session) || search.page === "profile" || search.page === "accept-invitation") return;
  const { page, ws, record, tab, kind } = canonicalSearch(search);
  try { storage.setItem(key(session, isAdminPage(page!) ? "admin" : "workspace"), dashboardHref({ page, ws, record, tab, kind })); } catch { /* Storage is optional. */ }
}
export function rememberedPortal(storage: Pick<Storage, "getItem" | "removeItem">, session: Session, portal: "admin" | "workspace") {
  const k = key(session, portal); let saved: string | null; try { saved = storage.getItem(k); } catch { return; } const parsed = parseDashboardLocation(saved ?? "");
  if (parsed?.page && authorizedLocation(parsed, session) && isAdminPage(parsed.page) === (portal === "admin") && parsed.page !== "profile" && parsed.page !== "accept-invitation") return parsed;
  try { storage.removeItem(k); } catch { /* Storage is optional. */ } return;
}
export function clearRememberedPortals(storage: Pick<Storage, "length" | "key" | "removeItem">) { try { for (let i = storage.length - 1; i >= 0; i--) { const k = storage.key(i); if (k?.startsWith(prefix)) storage.removeItem(k); } } catch { /* Unavailable storage. */ } }
export function sidebarWorkspace(session: Session, search: DashboardSearch, remembered?: Workspace) {
  const eligible = portalWorkspaces(session);
  if (search.ws) return eligible.find(w => w.id === search.ws);
  return eligible.find(w => w.id === remembered?.id) ?? eligible.find(w => w.kind === "personal") ?? eligible[0];
}
export function rememberWorkspace(storage: Pick<Storage, "setItem">, session: Session, search: DashboardSearch) { if (search.ws && authorizedLocation(search, session) && !isAdminPage(search.page!) && !userPages.has(search.page!)) try { storage.setItem(contextKey(session), search.ws); } catch { /* Storage is optional. */ } }
export function rememberedWorkspace(storage: Pick<Storage, "getItem" | "removeItem">, session: Session) { const k = contextKey(session); try { const w = portalWorkspaces(session).find(w => w.id === storage.getItem(k)); if (!w) storage.removeItem(k); return w; } catch { return; } }
/**
 * Where sign-in lands: Home, with the caller's Personal workspace selected in the sidebar (the
 * selection is reset so a previous user's or session's context never carries over). Without a
 * Personal workspace the remembered or first membership stays selected.
 */
export function landingSearch(session: Session, storage?: Pick<Storage, "setItem">): DashboardSearch {
  const personal = personalWorkspace(session);
  if (personal && storage) rememberWorkspace(storage, session, { page: "overview", ws: personal.id });
  return { page: "home" };
}

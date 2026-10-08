import { dashboardSearch, identifier, type DashboardSearch, type Page } from "./permissions";
export const platformPaths: Partial<Record<Page, string>> = { "platform-overview": "/admin", "platform-teams": "/admin/teams", "platform-projects": "/admin/projects", users: "/admin/users", models: "/admin/models", "model-new": "/admin/models/new", providers: "/admin/connections", deployments: "/admin/deployments", catalogs: "/admin/catalogs", policies: "/admin/settings/limits", "settings-general": "/admin/settings/general", "settings-privacy": "/admin/settings/privacy", "settings-email": "/admin/settings/email", "settings-sign-in": "/admin/settings/sign-in", "cost-centers": "/admin/cost-centers", "platform-costs": "/admin/costs", oidc: "/admin/sso-groups", "platform-audit": "/admin/audit", "platform-logs": "/admin/logs", "platform-log-session": "/admin/logs/session", pricing: "/admin/pricing", routing: "/admin/routing" };
/** Workspace pages and their path under /workspaces/{ws}. */
const workspaceSegments: Partial<Record<Page, string>> = { overview: "", keys: "/keys", grants: "/models", requests: "/logs", "session-detail": "/session", costs: "/costs", "workspace-settings": "/settings" };
/** Workspace records routed as /workspaces/{ws}/{collection}/{record}. */
export const workspaceRecords: Partial<Record<Page, string>> = { "request-detail": "requests", "key-detail": "keys", "workspace-model": "models" };
const detailCollections: Partial<Record<Page, string>> = { "model-detail": "models", "provider-detail": "connections", "deployment-detail": "routes", "catalog-detail": "catalogs", "user-detail": "users", "platform-log-detail": "logs" };
/** Old workspace list URLs that open (and are rewritten to) their new path: /workspaces/{ws}/requests → /workspaces/{ws}/logs. */
const legacyWorkspaceSegments: Record<string, Page> = { requests: "requests" };
/** Old record URLs that still open (and are rewritten to) their new path: /admin/deployments/{id} → /admin/routes/{id}, /admin/providers/{id} → /admin/connections/{id}. */
const legacyDetailCollections: Record<string, Page> = { deployments: "deployment-detail", providers: "provider-detail" };
/** Old list URLs (review rule 10: Connections, Limits, SSO groups); they open the page and are rewritten to its new path. */
export const legacyPlatformPaths: Record<string, Page> = { "/admin/providers": "providers", "/admin/policies": "policies", "/admin/limits": "policies", "/admin/settings": "settings-general", "/admin/oidc": "oidc" };
export function canonicalSearch(input: DashboardSearch): DashboardSearch {
  const s = dashboardSearch(input);
  // Legacy pages: the routing picker and the all-routes list now live on each model page.
  if (s.page === "routing" || s.page === "deployments") return { page: "models" };
  // Admin › Pricing is now the Models table with "Unpriced only" (prices are edited on model/route pages).
  if (s.page === "pricing") return { page: "models", layout: "table", pricing: "unpriced" };
  if (s.page === "workspace-settings" && (s.tab === "keys" || s.tab === "costs")) return { ...s, page: s.tab === "keys" ? "keys" : "costs", tab: undefined };
  // Key limits moved from Settings › Limits › Key limits to each key's page.
  if (s.page === "workspace-settings" && s.tab === "limits" && s.scope === "keys") return { ...s, page: s.record ? "key-detail" : "keys", tab: undefined, scope: undefined };
  const aliases: Partial<Record<Page, string>> = { members: "members", "service-accounts": "service-accounts", invitations: "invitations", governance: "limits", audit: "audit" };
  return s.page && aliases[s.page] ? { ...s, page: "workspace-settings", tab: aliases[s.page] } : s;
}
export function dashboardHref(input: DashboardSearch): string {
  const search = canonicalSearch(input), { page, ws, record } = search;
  let path = page && platformPaths[page] || "/";
  if (page === "home") path = "/home";
  else if (page === "profile") path = "/profile";
  else if (page === "accept-invitation") path = "/invitations/accept";
  else if (page === "workspace-detail" && record) path = `/admin/${search.kind === "project" ? "projects" : "teams"}/${encodeURIComponent(record)}`;
  else if (page && detailCollections[page] && record) path = `/admin/${detailCollections[page]}/${encodeURIComponent(record)}`;
  else if (page && workspaceRecords[page] && ws && record) path = `/workspaces/${encodeURIComponent(ws)}/${workspaceRecords[page]}/${encodeURIComponent(record)}`;
  else if (ws && (!page || page in workspaceSegments)) path = `/workspaces/${encodeURIComponent(ws)}${page ? workspaceSegments[page] : ""}`;
  const query = new URLSearchParams();
  if (path === "/") for (const key of ["page", "ws"] as const) if (search[key]) query.set(key, search[key]);
  if (record && !(page && (detailCollections[page] || workspaceRecords[page] && path !== "/")) && page !== "workspace-detail") query.set("record", record);
  for (const key of ["tab", "q", "enabled", "offset", "model", "provider", "scope", "view", "start_date", "end_date", "compare", "workspace_id", "cost_center_id", "actor_user_id", "service_account_id", "connection", "accounting_status", "key_id", "model_id", "status", "range", "cursor", "cols", "density", "type", "sort", "layout", "connections", "min_price", "max_price", "policy", "readiness", "deprecated", "eligibility", "pricing", "cost_status", "metric", "group", "then", "top", "role", "hide_sign_ins", "finish_reason", "streamed", "session_id"] as const) if (search[key] !== undefined) query.set(key, String(search[key]));
  return path + (query.size ? `?${query}` : "");
}
export function parseDashboardLocation(href: string): DashboardSearch | undefined {
  if (!href.startsWith("/") || href.startsWith("//") || href.includes("\\")) return;
  let url: URL; try { url = new URL(href, "https://dashboard.invalid"); } catch { return; }
  const raw = Object.fromEntries(url.searchParams), query = dashboardSearch(raw), path = url.pathname;
  if (path === "/") {
    if (raw.org !== undefined || raw.page !== undefined && !query.page || raw.ws !== undefined && !query.ws) return;
    return canonicalSearch(query);
  }
  const metadata = { ...query, page: undefined, ws: undefined, record: undefined };
  const platform = Object.entries(platformPaths).find(([, p]) => p === path) ?? (legacyPlatformPaths[path] ? [legacyPlatformPaths[path]!, path] as const : undefined);
  if (platform) return platform[0] === "routing" || platform[0] === "deployments" || platform[0] === "pricing" ? canonicalSearch({ page: platform[0] }) : { ...metadata, page: platform[0] as Page, record: query.record };
  if (path === "/home") return { ...metadata, page: "home" };
  if (path === "/profile") return { ...metadata, page: "profile" };
  if (path === "/invitations/accept") return { ...metadata, page: "accept-invitation" };
  const parts = path.split("/").slice(1); let id: string | undefined;
  try { id = identifier(decodeURIComponent(parts[parts[0] === "admin" ? 2 : 1] ?? "")); } catch { return; }
  if (!id) return;
  if (parts[0] === "admin" && parts.length === 3) {
    if (parts[1] === "teams" || parts[1] === "projects") return { ...metadata, page: "workspace-detail", record: id, kind: parts[1] === "projects" ? "project" : "team" };
    const detail = Object.entries(detailCollections).find(([, c]) => c === parts[1])?.[0] as Page | undefined ?? legacyDetailCollections[parts[1]!];
    return detail ? { ...metadata, page: detail, record: id } : undefined;
  }
  if (parts[0] === "workspaces" && parts.length === 4) {
    const recordPage = Object.entries(workspaceRecords).find(([, c]) => c === parts[2])?.[0] as Page | undefined;
    let record: string | undefined; try { record = identifier(decodeURIComponent(parts[3]!)); } catch { return; }
    return recordPage && record ? { ...metadata, page: recordPage, ws: id, record } : undefined;
  }
  if (parts[0] !== "workspaces" || parts.length > 3) return;
  const page = parts.length === 2 ? "overview" : (Object.entries(workspaceSegments).find(([, segment]) => segment === `/${parts[2]}`)?.[0] as Page | undefined ?? legacyWorkspaceSegments[parts[2]!]);
  return page ? { ...metadata, page, ws: id, record: query.record } : undefined;
}

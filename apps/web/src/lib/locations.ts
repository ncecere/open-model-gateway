import { dashboardSearch, type DashboardSearch, type Page } from "./permissions";

const platformPaths = {
  "platform-overview": "/admin", organizations: "/admin/organizations", "platform-teams": "/admin/teams", "platform-projects": "/admin/projects", users: "/admin/users",
  models: "/admin/models", providers: "/admin/providers", deployments: "/admin/deployments", routing: "/admin/routing", pricing: "/admin/pricing", "model-access": "/admin/model-access", "platform-audit": "/admin/audit",
} satisfies Partial<Record<Page, string>>;
const orgAliases: Partial<Record<Page, string>> = { teams: "teams", projects: "projects", "organization-members": "members", invitations: "invitations", "assigned-models": "models", "organization-policy": "limits", audit: "audit" };
const workspaceAliases: Partial<Record<Page, string>> = { members: "members", "service-accounts": "service-accounts", governance: "limits" };

/** Legacy page names remain accepted by callbacks, but never become new URLs. */
export function canonicalSearch(input: DashboardSearch): DashboardSearch {
  const search = dashboardSearch(input);
  const page = search.page;
  if (page && orgAliases[page]) return { ...search, page: "organization-settings", ws: undefined, tab: orgAliases[page] };
  if (page && workspaceAliases[page]) return { ...search, page: "workspace-settings", tab: workspaceAliases[page] };
  return search;
}

/** Pure, same-origin location builder. Only explicitly supported metadata survives. */
export function dashboardHref(input: DashboardSearch): string {
  const search = canonicalSearch(input);
  const { page, org, ws, record } = search;
  let path = "/";
  if (page && page in platformPaths) path = platformPaths[page as keyof typeof platformPaths];
  else if (page === "profile") path = "/profile";
  else if (page === "accept-invitation") path = "/invitations/accept";
  else if (page === "organization-detail" && org) path = `/admin/organizations/${encodeURIComponent(org)}`;
  else if (page === "organization-settings" && org) path = `/organizations/${encodeURIComponent(org)}/settings`;
  else if (record && ["model-detail", "provider-detail", "deployment-detail"].includes(page ?? "")) path = `/admin/${page === "model-detail" ? "models" : page === "provider-detail" ? "providers" : "deployments"}/${encodeURIComponent(record)}`;
  else if (org && (!page || ["overview", "keys", "grants", "costs", "workspace-settings"].includes(page))) {
    path = `/organizations/${encodeURIComponent(org)}/workspaces`;
    if (ws) path += `/${encodeURIComponent(ws)}${page === "keys" ? "/keys" : page === "grants" ? "/models" : page === "costs" ? "/costs" : page === "workspace-settings" ? "/settings" : ""}`;
  }
  const query = new URLSearchParams();
  // Incomplete callback destinations are resolved against /me on the root route.
  if (path === "/") for (const key of ["page", "org", "ws", "record"] as const) if (search[key]) query.set(key, search[key]);
  if (record && !["model-detail", "provider-detail", "deployment-detail"].includes(page ?? "")) query.set("record", record);
  for (const key of ["tab", "q", "enabled", "offset", "model", "provider", "scope", "view", "recipientKind", "recipient"] as const) if (search[key] !== undefined) query.set(key, String(search[key]));
  return path + (query.size ? `?${query}` : "");
}

/** Undefined means a real 404, never an implicit overview fallback. */
export function parseDashboardLocation(href: string): DashboardSearch | undefined {
  if (!href.startsWith("/") || href.startsWith("//") || href.includes("\\")) return undefined;
  let url: URL;
  try { url = new URL(href, "https://dashboard.invalid"); } catch { return undefined; }
  const raw = Object.fromEntries(url.searchParams);
  const query = dashboardSearch(raw);
  const path = url.pathname;
  if (path === "/") {
    if ((raw.page !== undefined && !query.page) || (raw.org !== undefined && !query.org) || (raw.ws !== undefined && !query.ws) || (raw.record !== undefined && !query.record)) return undefined;
    return canonicalSearch(query);
  }
  // Scope in canonical URLs comes exclusively from the path, never the query.
  const metadata = { ...query, page: undefined, org: undefined, ws: undefined };
  const platform = Object.entries(platformPaths).find(([, value]) => value === path);
  if (platform) return { ...metadata, page: platform[0] as Page };
  if (path === "/profile") return { ...metadata, page: "profile" };
  if (path === "/invitations/accept") return { ...metadata, page: "accept-invitation" };
  const parts = path.split("/").slice(1);
  let id: string | undefined;
  try { id = dashboardSearch({ record: decodeURIComponent(parts[2] ?? "") }).record; } catch { return undefined; }
  if (parts[0] === "admin" && parts.length === 3 && id) {
    if (parts[1] === "organizations") return { ...metadata, page: "organization-detail", org: id };
    const page = parts[1] === "models" ? "model-detail" : parts[1] === "providers" ? "provider-detail" : parts[1] === "deployments" ? "deployment-detail" : undefined;
    if (page) return { ...metadata, page, record: id };
  }
  if (parts[0] !== "organizations") return undefined;
  let org: string | undefined;
  let ws: string | undefined;
  try { org = dashboardSearch({ org: decodeURIComponent(parts[1] ?? "") }).org; ws = dashboardSearch({ ws: decodeURIComponent(parts[3] ?? "") }).ws; } catch { return undefined; }
  if (!org) return undefined;
  if (parts.length === 3 && parts[2] === "settings") return { ...metadata, page: "organization-settings", org };
  if (parts[2] !== "workspaces") return undefined;
  if (parts.length === 3) return { ...metadata, page: "overview", org };
  if (!ws || parts.length > 5) return undefined;
  const page = parts.length === 4 ? "overview" : ({ keys: "keys", models: "grants", costs: "costs", settings: "workspace-settings" } as const)[parts[4] as "keys"];
  return page ? { ...metadata, page, org, ws } : undefined;
}

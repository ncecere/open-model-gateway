import type { Session, Workspace, Key } from "./api";
export const isAdmin = (role?: string | null) => role === "owner" || role === "admin";
export function permissions(session: Session, workspace?: Workspace) {
  const privateAccess = !workspace || workspace.kind !== "personal" || workspace.owner_user_id === session.user.id;
  const c = privateAccess ? workspace?.capabilities : undefined;
  return {
    platformRead: session.capabilities.platform_read, platformWrite: session.capabilities.platform_write,
    workspaceAdmin: !!c?.view_all_activity, manageProviders: session.capabilities.platform_write,
    managePricing: session.capabilities.platform_write, managePolicy: !!c?.manage_policy,
    reconcileCosts: session.capabilities.platform_write && !!workspace && privateAccess,
    manageTeam: !!c?.manage_members && workspace?.kind !== "personal",
    manageServiceAccounts: !!c?.manage_service_accounts && workspace?.kind !== "personal",
    manageGrants: !!c?.delegate_models, createWorkspace: session.capabilities.create_workspace,
    createUserKey: !!c?.issue_own_key, privateAccess,
  };
}
export function canManageKey(session: Session, workspace: Workspace, key: Key) {
  return permissions(session, workspace).privateAccess && (key.issued_to_user_id === session.user.id || workspace.capabilities.manage_service_accounts || (workspace.capabilities.manage_members && workspace.kind !== "personal"));
}
export function canRotateKey(session: Session, workspace: Workspace, key: Key) {
  return permissions(session, workspace).privateAccess && (key.issued_to_user_id === session.user.id && workspace.capabilities.issue_own_key || !!key.service_account_id && workspace.capabilities.manage_service_accounts);
}
export const canAdminister = (session: Session) => session.capabilities.platform_read;
export const pages = ["settings-alerts", "alert-rule-detail", "workspace-alert-detail", "notifications", "home", "requests", "request-detail", "key-detail", "workspace-model", "platform-overview", "workspace-settings", "workspace-detail", "model-new", "model-detail", "provider-detail", "deployment-detail", "catalog-detail", "user-detail", "platform-teams", "platform-projects", "catalogs", "policies", "cost-centers", "platform-costs", "oidc", "platform-audit", "users", "overview", "keys", "service-accounts", "members", "grants", "invitations", "models", "providers", "deployments", "audit", "governance", "costs", "routing", "pricing", "profile", "accept-invitation", "settings-general", "settings-privacy", "settings-email", "settings-sign-in", "session-detail", "platform-logs", "platform-log-detail", "platform-log-session", "key-safety", "model-compare", "platform-model-compare"] as const;
export type Page = typeof pages[number];
export const platformPages = new Set<Page>(["settings-alerts", "alert-rule-detail", "platform-overview", "workspace-detail", "model-new", "model-detail", "provider-detail", "deployment-detail", "catalog-detail", "user-detail", "platform-teams", "platform-projects", "catalogs", "policies", "cost-centers", "platform-costs", "oidc", "platform-audit", "users", "models", "providers", "deployments", "routing", "pricing", "settings-general", "settings-privacy", "settings-email", "settings-sign-in", "platform-logs", "platform-log-detail", "platform-log-session", "key-safety", "platform-model-compare"]);
/** User-level pages: no workspace in the URL (Home, profile, invitation acceptance). */
export const userPages = new Set<Page>(["notifications", "home", "profile", "accept-invitation"]);
/**
 * The Workspace portal follows membership only: the caller's own Personal workspace and the
 * Teams/Projects they actually belong to. Platform authority over other shared workspaces is
 * exercised from Admin, never by presenting staff as members here.
 */
export function inWorkspacePortal(session: Session, workspace: Workspace) {
  if (workspace.disabled_at) return false;
  return workspace.kind === "personal" ? workspace.owner_user_id === session.user.id : workspace.role !== null;
}
export function canView(page: Page, session: Session, workspace?: Workspace) {
  if (userPages.has(page)) return true;
  if (platformPages.has(page)) return session.capabilities.platform_read;
  if (!workspace || workspace.kind === "personal" && workspace.owner_user_id !== session.user.id || workspace.kind !== "personal" && workspace.role === null) return false;
  if (page === "members" || page === "invitations") return workspace.kind !== "personal" && workspace.capabilities.manage_members;
  if (page === "service-accounts") return workspace.kind !== "personal" && workspace.capabilities.manage_service_accounts;
  // No inherited operator authority may authorize personal keys/activity.
  // Alert rules of a Team/Project: its admins (personal alerts are built in).
  if (page === "workspace-alert-detail") return workspace.kind !== "personal" && workspace.capabilities.manage_policy;
  if (page === "keys" || page === "key-detail") return workspace.role !== null || workspace.capabilities.manage_service_accounts;
  return true;
}
export type DashboardSearch = { ws?: string; page?: Page; record?: string; tab?: string; q?: string; enabled?: "true" | "false"; offset?: number; model?: string; provider?: string; scope?: "workspace" | "keys"; view?: "effective" | "local" | "inherited"; kind?: "personal" | "team" | "project"; start_date?: string; end_date?: string; compare?: "none" | "previous_period"; workspace_id?: string; cost_center_id?: string; actor_user_id?: string; service_account_id?: string; connection?: string; accounting_status?: "pending" | "unknown" | "settled" | "missing"; /** Requests/keys lists (wave 2). `status`: one key status, or a comma list of request (attempt) statuses. */ key_id?: string; status?: string; /** Usage & costs filter: one model (route's model UUID). */ model_id?: string; range?: RangePreset; cursor?: string; cols?: string; density?: "compact"; /** Models catalog (wave 2): workload tab, sort, list/table and URL-backed facets. */ type?: CatalogType; sort?: "name" | "price" | "newest"; layout?: "table"; connections?: string; min_price?: string; max_price?: string; policy?: string; readiness?: string; deprecated?: "hide"; eligibility?: string; /** Models table: only models with an enabled route that has no price (the former Admin › Pricing). */ pricing?: "unpriced"; /** Usage & costs (wave 2): records status, explore pivot / chart metric. */ cost_status?: CostStatusFilter; metric?: string; group?: string; then?: string; top?: string; /** Directory filters (Admin › Users, Audit log). */ role?: DirectoryRole; hide_sign_ins?: "true" | "false"; /** Logs filters: finish reasons (comma list), streamed, client session id (any printable text, not an identifier). */ finish_reason?: string; streamed?: "true" | "false"; session_id?: string; /** Logs: only async jobs (video, batch). */ workload?: "jobs"; /** Model compare: 2 to 4 model ids (comma list). */ ids?: string; /** Key safety: severities (comma list); API keys list: only keys with findings. */ severity?: string; risk?: "attention" };
export const directoryRoles = ["none", "user", "auditor", "admin"] as const;
export type DirectoryRole = typeof directoryRoles[number];
export const costStatusFilters = ["final", "on_hold", "cost_unknown", "not_recorded"] as const;
export type CostStatusFilter = typeof costStatusFilters[number];
export const catalogTypes = ["generation", "embeddings", "images", "audio_transcriptions", "audio_speech", "rerank", "systemone", "realtime", "videos", "batches"] as const;
export type CatalogType = typeof catalogTypes[number];
/** Comma-separated subset of `allowed` (deduplicated), else undefined. */
const subset = (value: unknown, allowed: readonly string[]) => { if (typeof value !== "string" || value.length > 200) return; const picked = [...new Set(value.split(","))].filter(v => allowed.includes(v)); return picked.length ? picked.join(",") : undefined; };
/** Request statuses (requests list) and key statuses (API keys list) share the `status` URL key. */
export const listStatuses = ["firing", "resolved", "unread", "succeeded", "failed", "cancelled", "indeterminate", "in_progress", "active", "disabled", "revoked", "expired", "all", "suspended", "enabled", "archived"] as const;
export const attemptStatuses = ["succeeded", "failed", "cancelled", "indeterminate", "in_progress"] as const;
export type ListStatus = typeof listStatuses[number];
export const rangePresets = ["today", "7d", "30d", "90d", "month", "custom"] as const;
export type RangePreset = typeof rangePresets[number];
export const dashboardTabs = ["alerts", "rules", "history", "overview", "members", "invitations", "models", "policy", "audit", "service-accounts", "settings", "deployments", "pricing", "routing", "limits", "catalogs", "model-access", "groups", "roles", "general", "effective", "local", "inherited", "installation", "personal", "team", "project", "costs", "keys", "routes", "availability", "workspaces", "explore", "records", "chart", "activity", "defaults", "requests", "generations", "sessions", "access"] as const;
export const identifier = (value: unknown) => typeof value === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._~-]{0,127}$/.test(value) ? value : undefined;
export function dashboardSearch(search: Record<string, unknown>): DashboardSearch {
  const result: DashboardSearch = { ws: identifier(search.ws), page: typeof search.page === "string" && pages.includes(search.page as Page) ? search.page as Page : undefined };
  for (const key of ["record", "workspace_id", "cost_center_id", "actor_user_id", "service_account_id", "connection", "key_id", "model_id", "cursor"] as const) { const value = identifier(search[key]); if (value) result[key] = value; }
  if (listStatuses.includes(search.status as ListStatus)) result.status = search.status as ListStatus;
  else { const several = subset(search.status, attemptStatuses); if (several) result.status = several; }
  if (rangePresets.includes(search.range as RangePreset)) result.range = search.range as RangePreset;
  if (typeof search.cols === "string" && /^[a-z0-9_-]{1,40}(?:,[a-z0-9_-]{1,40}){0,19}$/.test(search.cols)) result.cols = search.cols;
  if (search.density === "compact") result.density = "compact";
  if (costStatusFilters.includes(search.cost_status as CostStatusFilter)) result.cost_status = search.cost_status as CostStatusFilter;
  for (const key of ["metric", "group", "then"] as const) if (typeof search[key] === "string" && /^[a-z_]{1,32}$/.test(search[key])) result[key] = search[key];
  const top = typeof search.top === "number" ? String(search.top) : search.top; if (typeof top === "string" && /^\d{1,2}$/.test(top)) result.top = top;
  if (catalogTypes.includes(search.type as CatalogType)) result.type = search.type as CatalogType;
  if (search.sort === "name" || search.sort === "price" || search.sort === "newest") result.sort = search.sort;
  if (search.layout === "table") result.layout = "table";
  if (search.deprecated === "hide") result.deprecated = "hide";
  const finish = subset(search.finish_reason, ["stop", "length", "tool_calls", "content_filter", "error", "cancelled", "unknown"]); if (finish) result.finish_reason = finish;
  if (search.streamed === "true" || search.streamed === true) result.streamed = "true"; else if (search.streamed === "false" || search.streamed === false) result.streamed = "false";
  if (search.workload === "jobs") result.workload = "jobs";
  if (typeof search.session_id === "string" && search.session_id.length <= 128 && search.session_id.trim() === search.session_id && search.session_id !== "" && !/[\u0000-\u001f\u007f-\u009f]/.test(search.session_id)) result.session_id = search.session_id;
  if (search.pricing === "unpriced") result.pricing = "unpriced";
  if (typeof search.ids === "string" && search.ids.length <= 600) { const ids = [...new Set(search.ids.split(","))].slice(0, 4).map(identifier).filter(Boolean); if (ids.length) result.ids = ids.join(","); }
  const severity = subset(search.severity, ["high", "medium", "low"]); if (severity) result.severity = severity;
  if (search.risk === "attention") result.risk = "attention";
  if (typeof search.connections === "string") { const ids = search.connections.split(",").slice(0, 20).map(identifier).filter(Boolean); if (ids.length) result.connections = ids.join(","); }
  for (const key of ["min_price", "max_price"] as const) if (typeof search[key] === "string" && /^\d{0,13}(?:\.\d{0,6})?$/.test(search[key]) && search[key] !== "") result[key] = search[key];
  const policy = subset(search.policy, ["allow", "deny", "unknown"]), readiness = subset(search.readiness, ["ready", "needs_attention", "needs_setup", "not_serving", "retired", "unknown"]), eligibility = subset(search.eligibility, ["selected", "direct", "available_from_catalog"]);
  if (policy) result.policy = policy; if (readiness) result.readiness = readiness; if (eligibility) result.eligibility = eligibility;
  for (const key of ["model", "provider", "q"] as const) if (typeof search[key] === "string" && search[key].length <= 200 && !/[\u0000-\u001f\u007f]/.test(search[key])) result[key] = search[key];
  if (typeof search.tab === "string" && dashboardTabs.includes(search.tab as typeof dashboardTabs[number])) result.tab = search.tab;
  if (search.enabled === "true" || search.enabled === "false") result.enabled = search.enabled;
  if (directoryRoles.includes(search.role as DirectoryRole)) result.role = search.role as DirectoryRole;
  if (search.hide_sign_ins === "true" || search.hide_sign_ins === true) result.hide_sign_ins = "true";
  else if (search.hide_sign_ins === "false" || search.hide_sign_ins === false) result.hide_sign_ins = "false";
  if (search.scope === "workspace" || search.scope === "keys") result.scope = search.scope;
  if (search.view === "effective" || search.view === "local" || search.view === "inherited") result.view = search.view;
  if (search.kind === "personal" || search.kind === "team" || search.kind === "project") result.kind = search.kind;
  for (const key of ["start_date", "end_date"] as const) if (typeof search[key] === "string" && /^\d{4}-\d{2}-\d{2}$/.test(search[key])) result[key] = search[key];
  if (search.compare === "none" || search.compare === "previous_period") result.compare = search.compare;
  if (["pending", "unknown", "settled", "missing"].includes(String(search.accounting_status))) result.accounting_status = search.accounting_status as DashboardSearch["accounting_status"];
  const offset = typeof search.offset === "number" ? search.offset : typeof search.offset === "string" && /^\d{1,7}$/.test(search.offset) ? Number(search.offset) : undefined;
  if (offset !== undefined && Number.isSafeInteger(offset) && offset >= 0 && offset <= 100000) result.offset = offset;
  return result;
}

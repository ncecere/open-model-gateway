import type { Organization, Session, Workspace, Key } from "./api";

export const isAdmin = (role?: string | null) => role === "owner" || role === "admin" || role === "operator";
export function permissions(session: Session, org?: Organization, workspace?: Workspace) {
  const organizationAdmin = !!org && isAdmin(org.role);
  const workspaceAdmin = !!workspace && isAdmin(workspace.role);
  const shared = workspace?.kind === "team" || workspace?.kind === "project";
  // Older /me payloads do not distinguish inherited workspace authority. Never
  // infer human membership from an inherited admin role in that case.
  const organizationMember = !!org && (org.membership_role !== undefined ? !!org.membership_role : org.role !== "operator" && org.authority_source !== "platform");
  const inherited = workspace?.authority_source ? workspace.authority_source === "platform" || workspace.authority_source === "organization" : shared && (session.user.platform_admin || organizationAdmin);
  const workspaceMember = !!workspace && (workspace.membership_role !== undefined ? !!workspace.membership_role : !inherited);
  const oc = org?.capabilities;
  const wc = workspace?.capabilities;
  return {
    organizationAdmin, workspaceAdmin,
    createOrganization: session.user.platform_admin,
    manageProviders: session.user.platform_admin,
    managePricing: session.user.platform_admin,
    managePolicy: workspace ? (wc?.manage_policy ?? (organizationAdmin || (workspaceAdmin && shared))) : (oc?.manage_policy ?? organizationAdmin),
    reconcileCosts: session.user.platform_admin && !!workspace,
    manageTeam: shared && (wc?.manage_members ?? workspaceAdmin),
    manageServiceAccounts: shared && (wc?.manage_service_accounts ?? workspaceAdmin),
    manageGrants: !!workspace && (wc?.delegate_models ?? organizationAdmin),
    createWorkspace: oc?.create_workspace ?? (organizationAdmin && organizationMember),
    createPersonalWorkspace: oc?.create_personal_workspace ?? organizationMember,
    createUserKey: !!workspace && (wc?.issue_own_key ?? (organizationMember && workspaceMember)),
  };
}
export function canManageKey(session: Session, workspace: Workspace, key: Key) {
  return isAdmin(workspace.role) || key.issued_to_user_id === session.user.id;
}
export function canRotateKey(session: Session, workspace: Workspace, key: Key) {
  return key.issued_to_user_id === session.user.id || (!!key.service_account_id && (workspace.capabilities?.manage_service_accounts ?? isAdmin(workspace.role)));
}
// Admin is a platform portal, not a synonym for administration of a workspace.
export function canAdminister(session: Session) { return session.user.platform_admin; }
export function canAdministerOrganization(session: Session, org?: Organization) {
  return !!org && (isAdmin(org.role) || session.workspaces.some(ws => ws.organization_id === org.id && ws.kind !== "personal" && isAdmin(ws.role)));
}
export const pages = ["platform-overview", "organization-detail", "organization-settings", "workspace-settings", "model-detail", "provider-detail", "deployment-detail", "organizations", "teams", "projects", "platform-teams", "platform-projects", "model-access", "assigned-models", "organization-policy", "platform-audit", "users", "overview", "keys", "service-accounts", "members", "grants", "organization-members", "invitations", "models", "providers", "deployments", "audit", "governance", "costs", "routing", "pricing", "profile", "accept-invitation"] as const;
export type Page = typeof pages[number];
export function canView(page: Page, session: Session, org?: Organization, workspace?: Workspace) {
  const p = permissions(session, org, workspace);
  if (page === "profile" || page === "accept-invitation") return true;
  if (["platform-overview", "organization-detail", "model-detail", "provider-detail", "deployment-detail", "organizations", "users", "platform-teams", "platform-projects", "models", "providers", "deployments", "routing", "pricing", "platform-audit", "model-access"].includes(page)) return session.user.platform_admin;
  if (page === "teams" || page === "projects") return canAdministerOrganization(session, org);
  if (["organization-settings", "organization-members", "invitations", "assigned-models", "organization-policy", "audit"].includes(page)) return p.organizationAdmin;
  if (page === "governance") return p.organizationAdmin || !!workspace;
  if (!workspace) return false;
  if (page === "workspace-settings") return true; // Overview and inherited limits remain readable by members.
  if (page === "members") return p.manageTeam;
  if (page === "service-accounts") return p.manageServiceAccounts;
  return true;
}
export type DashboardSearch = { org?: string; ws?: string; page?: Page; record?: string; tab?: string; q?: string; enabled?: "true" | "false"; offset?: number; model?: string; provider?: string; scope?: "workspace" | "organization" | "keys"; view?: "effective" | "local" | "inherited"; recipientKind?: "team" | "project" | "user"; recipient?: string };
export const dashboardTabs = ["overview", "teams", "projects", "members", "invitations", "models", "policy", "audit", "service-accounts", "governance", "settings", "deployments", "pricing", "access", "routing", "limits"] as const;
const identifier = (value: unknown) => typeof value === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._~-]{0,127}$/.test(value) ? value : undefined;
export function dashboardSearch(search: Record<string, unknown>): DashboardSearch {
  const result: DashboardSearch = {
    org: identifier(search.org), ws: identifier(search.ws),
    page: typeof search.page === "string" && pages.includes(search.page as Page) ? search.page as Page : undefined,
  };
  for (const key of ["record", "model", "provider", "recipient"] as const) { const value = identifier(search[key]); if (value) result[key] = value; }
  if (typeof search.tab === "string" && dashboardTabs.includes(search.tab as typeof dashboardTabs[number])) result.tab = search.tab;
  if (typeof search.q === "string" && search.q.length <= 200 && !/[\u0000-\u001f\u007f]/.test(search.q)) result.q = search.q;
  if (search.enabled === "true" || search.enabled === "false") result.enabled = search.enabled;
  if (search.scope === "workspace" || search.scope === "organization" || search.scope === "keys") result.scope = search.scope;
  if (search.view === "effective" || search.view === "local" || search.view === "inherited") result.view = search.view;
  if (search.recipientKind === "team" || search.recipientKind === "project" || search.recipientKind === "user") result.recipientKind = search.recipientKind;
  const offset = typeof search.offset === "number" ? search.offset : typeof search.offset === "string" && /^\d{1,7}$/.test(search.offset) ? Number(search.offset) : undefined;
  if (offset !== undefined && Number.isSafeInteger(offset) && offset >= 0 && offset <= 100_000) result.offset = offset;
  return result;
}

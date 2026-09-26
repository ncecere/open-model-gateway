import type { Organization, Session, Workspace, Key } from "./api";

export const isAdmin = (role?: string | null) => role === "owner" || role === "admin" || role === "operator";
export function permissions(session: Session, org?: Organization, workspace?: Workspace) {
  const organizationAdmin = !!org && isAdmin(org.role);
  const workspaceAdmin = !!workspace && isAdmin(workspace.role);
  const shared = workspace?.kind === "team" || workspace?.kind === "project";
  const organizationMember = !!org && (org.role !== "operator" || !!org.membership_role);
  return {
    organizationAdmin, workspaceAdmin,
    createOrganization: session.user.platform_admin,
    manageProviders: session.user.platform_admin,
    managePricing: session.user.platform_admin,
    managePolicy: organizationAdmin || (workspaceAdmin && shared),
    reconcileCosts: session.user.platform_admin && !!workspace,
    manageTeam: workspaceAdmin && shared,
    manageServiceAccounts: workspaceAdmin && shared,
    manageGrants: organizationAdmin && !!workspace,
    createWorkspace: organizationAdmin && organizationMember,
    createPersonalWorkspace: organizationMember,
    createUserKey: !!workspace && organizationMember,
  };
}
export function canManageKey(session: Session, workspace: Workspace, key: Key) {
  return isAdmin(workspace.role) || key.issued_to_user_id === session.user.id;
}
export function canRotateKey(session: Session, workspace: Workspace, key: Key) {
  return key.issued_to_user_id === session.user.id || (!!key.service_account_id && isAdmin(workspace.role));
}
export function canAdminister(session: Session) {
  return session.user.platform_admin || session.organizations.some(org => isAdmin(org.role)) || session.workspaces.some(ws => ws.kind !== "personal" && isAdmin(ws.role));
}
export function canAdministerOrganization(session: Session, org?: Organization) {
  return !!org && (isAdmin(org.role) || session.workspaces.some(ws => ws.organization_id === org.id && ws.kind !== "personal" && isAdmin(ws.role)));
}
export const pages = ["organizations", "teams", "projects", "platform-teams", "platform-projects", "model-access", "assigned-models", "organization-policy", "platform-audit", "users", "overview", "keys", "service-accounts", "members", "grants", "organization-members", "invitations", "models", "providers", "deployments", "audit", "governance", "costs", "routing", "pricing", "profile", "accept-invitation"] as const;
export type Page = typeof pages[number];
export function canView(page: Page, session: Session, org?: Organization, workspace?: Workspace) {
  const p = permissions(session, org, workspace);
  if (page === "profile" || page === "accept-invitation") return true;
  if (page === "organizations") return canAdminister(session);
  if (["users", "platform-teams", "platform-projects", "models", "providers", "deployments", "routing", "pricing", "platform-audit", "model-access"].includes(page)) return session.user.platform_admin;
  if (page === "teams" || page === "projects") return canAdministerOrganization(session, org);
  if (["organization-members", "invitations", "assigned-models", "organization-policy", "audit"].includes(page)) return p.organizationAdmin;
  if (page === "governance") return p.organizationAdmin || !!workspace;
  if (!workspace) return false;
  if (page === "members") return p.manageTeam;
  if (page === "service-accounts") return p.manageServiceAccounts;
  return true;
}
export type DashboardSearch = { org?: string; ws?: string; page?: Page };
export function dashboardSearch(search: Record<string, unknown>): DashboardSearch {
  return {
    org: typeof search.org === "string" ? search.org : undefined,
    ws: typeof search.ws === "string" ? search.ws : undefined,
    page: typeof search.page === "string" && pages.includes(search.page as Page) ? search.page as Page : undefined,
  };
}

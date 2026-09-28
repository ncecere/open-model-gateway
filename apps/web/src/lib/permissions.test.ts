import { describe, expect, it } from "vitest";
import type { Session, Organization, Workspace, Key } from "./api";
import { permissions, canView, canManageKey, canRotateKey, dashboardSearch } from "./permissions";

const organization: Organization = { id: "org", name: "Example", slug: "example", role: "member" };
const workspace: Workspace = { id: "ws", organization_id: "org", name: "Personal", kind: "personal", role: "owner" };
const session: Session = { user: { id: "me", email: "me@example.org", platform_admin: false }, organizations: [organization], workspaces: [workspace] };
const key: Key = { id: "key", name: "Example", issued_to_user_id: "other", service_account_id: null, created_at: "", expires_at: "", revoked_at: null };
describe("dashboard permissions (backend remains authoritative)", () => {
  it("keeps personal spaces private and never offers team membership or service accounts", () => {
    const p = permissions(session, organization, workspace);
    expect(p.workspaceAdmin).toBe(true);
    expect(p.manageTeam).toBe(false);
    expect(p.manageServiceAccounts).toBe(false);
    expect(canView("members", session, organization, workspace)).toBe(false);
    expect(canView("service-accounts", session, organization, workspace)).toBe(false);
  });
  it("allows team admins but not members to manage team membership", () => {
    expect(permissions(session, organization, { ...workspace, kind: "team", role: "admin" }).manageTeam).toBe(true);
    expect(permissions(session, organization, { ...workspace, kind: "team", role: "member" }).manageTeam).toBe(false);
  });
  it("does not confuse personal-workspace ownership with organization administration", () => {
    expect(permissions(session, organization, workspace).organizationAdmin).toBe(false);
    expect(canView("models", session, organization, workspace)).toBe(false);
    expect(canView("grants", session, organization, workspace)).toBe(true);
    expect(permissions(session, organization, workspace).manageGrants).toBe(false);
  });
  it("hides all infrastructure from org admins, including reads", () => {
    const admin = { ...organization, role: "admin" as const };
    expect(canView("providers", session, admin, workspace)).toBe(false);
    expect(permissions(session, admin, workspace).manageProviders).toBe(false);
    expect(permissions({ ...session, user: { ...session.user, platform_admin: true } }, admin, workspace).manageProviders).toBe(true);
  });
  it("requires actual organization membership for operator-owned workspaces and user keys", () => {
    const operator: Session = { ...session, user: { ...session.user, platform_admin: true } };
    const org: Organization = { ...organization, role: "operator" };
    expect(permissions(operator, org, workspace)).toMatchObject({ createOrganization: true, createWorkspace: false, createPersonalWorkspace: false, createUserKey: false, manageProviders: true });
    expect(permissions(operator, { ...org, membership_role: "owner" }, workspace)).toMatchObject({ createWorkspace: true, createUserKey: true });
  });
  it("uses server capabilities rather than confusing effective authority with membership", () => {
    const shared = { ...workspace, kind: "team" as const, role: "admin" as const, authority_source: "organization" as const, membership_role: null };
    const orgAdmin = { ...organization, role: "admin" as const };
    expect(permissions(session, orgAdmin, shared).createUserKey).toBe(false);
    expect(permissions(session, orgAdmin, { ...shared, membership_role: "member" }).createUserKey).toBe(true);
    const capabilities = { issue_own_key: false, manage_members: false, manage_owners: false, manage_service_accounts: false, delegate_models: false, manage_policy: false, view_all_activity: false };
    expect(permissions(session, orgAdmin, { ...shared, capabilities })).toMatchObject({ createUserKey: false, manageTeam: false, manageServiceAccounts: false, manageGrants: false, managePolicy: false });
    expect(permissions(session, orgAdmin, { ...shared, capabilities: { ...capabilities, issue_own_key: true } }).createUserKey).toBe(true);
    expect(permissions(session, { ...orgAdmin, capabilities: { create_workspace: false, create_personal_workspace: false, manage_members: true, manage_owners: false, delegate_models: true, manage_policy: false } })).toMatchObject({ createWorkspace: false, createPersonalWorkspace: false, managePolicy: false });
  });
  it("fails closed for unknown direct membership under inherited shared authority", () => {
    const shared = { ...workspace, kind: "team" as const, role: "admin" as const };
    expect(permissions(session, { ...organization, role: "admin" }, shared).createUserKey).toBe(false);
    expect(permissions({ ...session, user: { ...session.user, platform_admin: true } }, organization, shared).createUserKey).toBe(false);
    expect(permissions(session, organization, { ...shared, authority_source: "direct" }).createUserKey).toBe(true);
  });
  it("lets admins revoke other humans’ keys, but never rotate them", () => {
    expect(canManageKey(session, workspace, key)).toBe(true);
    expect(canRotateKey(session, workspace, key)).toBe(false);
    expect(canRotateKey(session, workspace, { ...key, issued_to_user_id: "me" })).toBe(true);
    expect(canRotateKey(session, workspace, { ...key, issued_to_user_id: null, service_account_id: "service" })).toBe(true);
    expect(canRotateKey(session, { ...workspace, role: "member" }, { ...key, issued_to_user_id: null, service_account_id: "service" })).toBe(false);
  });
  it("allows members to manage only their own keys", () => {
    expect(canManageKey(session, { ...workspace, role: "member" }, key)).toBe(false);
    expect(canManageKey(session, { ...workspace, role: "member" }, { ...key, issued_to_user_id: "me" })).toBe(true);
  });
  it("keeps profile and invitation acceptance available before joining an organization", () => {
    expect(canView("profile", session)).toBe(true);
    expect(canView("accept-invitation", session)).toBe(true);
    expect(canView("overview", session)).toBe(false);
  });
  it("separates governance reads, org-admin writes, and operator pricing/reconciliation", () => {
    const member = { ...workspace, kind: "team" as const, role: "member" as const };
    expect(canView("governance", session, organization, member)).toBe(true);
    expect(canView("costs", session, organization, member)).toBe(true);
    expect(permissions(session, organization, member)).toMatchObject({ managePolicy: false, managePricing: false, reconcileCosts: false });
    expect(canView("routing", session, organization, member)).toBe(false);
    expect(canView("pricing", session, organization, member)).toBe(false);
    const admin = { ...organization, role: "admin" as const };
    expect(permissions(session, admin, member)).toMatchObject({ managePolicy: true, managePricing: false, reconcileCosts: false });
    expect(canView("routing", session, admin)).toBe(false);
    expect(canView("governance", session, admin)).toBe(true);
    expect(canView("pricing", session, admin)).toBe(false);
    const operator = { ...session, user: { ...session.user, platform_admin: true } };
    expect(permissions(operator, admin, member)).toMatchObject({ managePricing: true, reconcileCosts: true });
    expect(canView("costs", operator, admin)).toBe(false);
    expect(permissions(operator, admin).reconcileCosts).toBe(false);
  });
  it("dispatches the four new pages without carrying unknown or secret search values", () => {
    for (const page of ["governance", "costs", "routing", "pricing"] as const) expect(dashboardSearch({ page, token: "secret" })).toEqual({ org: undefined, ws: undefined, page });
  });
  it("keeps navigation search typed and drops secret/unknown search fields", () => {
    expect(dashboardSearch({ org: "org", ws: "ws", page: "keys", token: "secret" })).toEqual({ org: "org", ws: "ws", page: "keys" });
    expect(dashboardSearch({ org: 12, ws: {}, page: "unknown" })).toEqual({ org: undefined, ws: undefined, page: undefined });
  });
});

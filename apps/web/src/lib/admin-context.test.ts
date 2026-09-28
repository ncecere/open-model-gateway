import { describe, expect, it } from "vitest";
import type { Organization, Session, Workspace } from "./api";
import { adminDestination, adminGroups, contextOptions, scopeSearch } from "./navigation";
import { effectivePolicy, type Policy } from "./governance";

const org: Organization = { id: "org", name: "Company", slug: "company", role: "member" };
const team: Workspace = { id: "team", name: "Product", organization_id: org.id, kind: "team", role: "admin" };
const personal: Workspace = { ...team, id: "personal", kind: "personal", role: "owner" };
const project: Workspace = { ...team, id: "project", name: "Research", kind: "project" };
const session: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [personal, team, project] };
const empty: Policy = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null };

describe("administration context selection", () => {
  it("fixes Admin to Platform, including while viewing an organization object", () => {
    const operator = { ...session, user: { ...session.user, platform_admin: true } };
    const groups = [{ label: "Platform", items: [{ label: "Platform", page: "platform-overview" }] }];
    expect(contextOptions(operator, org, true)).toEqual(groups);
    expect(contextOptions({ ...operator, organizations: [], workspaces: [] }, undefined, true)).toEqual(groups);
    expect(adminGroups("organization-detail", true)).toEqual(["Platform", "Models", "Oversight"]);
    expect(scopeSearch("platform-overview", org.id, team.id)).toEqual({ page: "platform-overview", org: undefined, ws: undefined });
  });
  it("does not offer a platform portal to organization or shared-workspace admins", () => {
    expect(contextOptions(session, org, true)).toEqual([]);
    expect(adminGroups("members", false, false)).toEqual([]);
    expect(contextOptions({ ...session, organizations: [{ ...org, role: "owner" }] }, org, true)).toEqual([]);
  });
  it("retains ordinary organization and personal switching in Workspace mode", () => {
    expect(contextOptions(session, org).map(g => g.label)).toEqual(["Organizations", "Personal · private", "Teams", "Projects"]);
  });
  it("lands shared admins in settings for the selected or first managed workspace", () => {
    expect(adminDestination(session, org, project)).toEqual({ page: "workspace-settings", org: "org", ws: "project" });
    expect(adminDestination(session, org, personal)).toEqual({ page: "workspace-settings", org: "org", ws: "team" });
    expect(adminDestination({ ...session, workspaces: [personal] }, org)).toEqual({ page: "overview", org: "org", ws: undefined });
  });
  it("preserves contextual organization settings and platform destinations", () => {
    const adminOrg = { ...org, role: "admin" as const };
    expect(adminDestination({ ...session, organizations: [adminOrg] }, adminOrg)).toEqual({ page: "organization-settings", org: "org", ws: undefined });
    expect(adminDestination({ ...session, user: { ...session.user, platform_admin: true } }, org, team)).toEqual({ page: "platform-overview", org: undefined, ws: undefined });
  });
});

describe("displayed effective organization caps", () => {
  it("retains platform maximums when local caps are unset and applies stricter values", () => {
    const parent = { ...empty, requests_per_minute: 120, concurrent_requests: 8, monthly_budget_microusd: "50000000" };
    expect(effectivePolicy(empty, parent)).toEqual(parent);
    expect(effectivePolicy({ ...empty, requests_per_minute: 80, monthly_budget_microusd: "10000000" }, parent)).toMatchObject({ requests_per_minute: 80, concurrent_requests: 8, monthly_budget_microusd: "10000000" });
    expect(effectivePolicy({ ...empty, requests_per_minute: 200 }, parent).requests_per_minute).toBe(120);
  });
  it("handles absent parents and money beyond JavaScript's safe integer range without rounding", () => {
    expect(effectivePolicy(empty, empty)).toEqual(empty);
    expect(effectivePolicy({ ...empty, requests_per_minute: 40 }, empty).requests_per_minute).toBe(40);
    expect(effectivePolicy({ ...empty, monthly_budget_microusd: "9007199254740993" }, { ...empty, monthly_budget_microusd: "9007199254740992" }).monthly_budget_microusd).toBe("9007199254740992");
  });
});

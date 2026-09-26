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
  it("lets operators return from an organization or workspace to Platform without stale scope", () => {
    const operator = { ...session, user: { ...session.user, platform_admin: true } };
    const root = contextOptions(operator, org, true).find(group => group.label === "Platform")!.items[0];
    expect(root).toEqual({ label: "Platform", page: "organizations" });
    expect(contextOptions(operator, org).find(group => group.label === "Platform")!.items[0]).toEqual(root);
    expect(scopeSearch(root.page!, root.org, root.ws)).toEqual({ page: "organizations", org: undefined, ws: undefined });
    expect(contextOptions({ ...operator, organizations: [], workspaces: [] }, undefined, true)).toEqual([{ label: "Platform", items: [root] }]);
  });
  it("offers shared admins only managed teams/projects, not parent or personal admin contexts", () => {
    const groups = contextOptions({ ...session, workspaces: [...session.workspaces, { ...team, id: "ordinary", role: "member" }] }, org, true);
    expect(groups.map(g => g.label)).toEqual(["Teams", "Projects"]);
    expect(groups.flatMap(g => g.items).map(i => i.ws)).toEqual(["team", "project"]);
    expect(groups.flatMap(g => g.items).every(i => i.page === "members")).toBe(true);
    expect(adminGroups("members", false, false)).toEqual(["Shared workspace"]);
  });
  it("retains ordinary organization and personal switching in Workspace mode", () => {
    expect(contextOptions(session, org).map(g => g.label)).toEqual(["Organizations", "Personal · private", "Teams", "Projects"]);
  });
  it("handles mixed org-admin and shared-admin roles across organizations without stranding scopes", () => {
    const other: Organization = { ...org, id: "other", name: "Other", role: "admin" };
    const mixed = { ...session, organizations: [org, other] };
    const groups = contextOptions(mixed, other, true);
    expect(groups.find(g => g.label === "Organizations")!.items.map(i => i.org)).toEqual(["other"]);
    expect(groups.find(g => g.label === "Teams")!.items[0]).toMatchObject({ label: "Product · Company", org: "org", ws: "team", page: "members" });
  });
  it("lands shared admins directly in the selected or first managed workspace", () => {
    expect(adminDestination(session, org, project)).toEqual({ page: "members", org: "org", ws: "project" });
    expect(adminDestination(session, org, personal)).toEqual({ page: "members", org: "org", ws: "team" });
    const other = { ...org, id: "other" };
    expect(adminDestination({ ...session, organizations: [other, org] }, other)).toEqual({ page: "members", org: "org", ws: "team" });
    expect(adminDestination({ ...session, workspaces: [personal] }, org)).toEqual({ page: "overview", org: "org", ws: undefined });
  });
  it("preserves org and platform administration destinations for their actual administrators", () => {
    const adminOrg = { ...org, role: "admin" as const };
    expect(adminDestination({ ...session, organizations: [adminOrg] }, adminOrg)).toEqual({ page: "teams", org: "org", ws: undefined });
    expect(adminDestination({ ...session, user: { ...session.user, platform_admin: true } }, org, team)).toEqual({ page: "organizations", org: undefined, ws: undefined });
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

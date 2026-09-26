import { describe, expect, it } from "vitest";
import type { Organization, Session, Workspace } from "./api";
import { jumpTargets } from "./search";
const org: Organization = { id: "college", slug: "college", name: "College", role: "member" };
const team: Workspace = { id: "product", organization_id: org.id, name: "Product", kind: "team", role: "member" };
const personal: Workspace = { ...team, id: "personal", name: "My workspace", kind: "personal", role: "owner" };
const member: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [team, personal] };
const ids = (s: Session, o = s.organizations[0]) => jumpTargets(s, o, personal).map(target => target.id);
describe("permission-scoped jump search", () => {
  it("does not offer administrative destinations to ordinary members", () => {
    expect(ids(member)).toContain("page:keys");
    for (const page of ["users", "platform-teams", "organizations", "teams", "organization-members", "members"]) expect(ids(member)).not.toContain(`page:${page}`);
  });
  it("offers team but not organization administration to team admins", () => {
    const manager: Session = { ...member, workspaces: [{ ...team, role: "admin" }, personal] };
    expect(ids(manager)).toContain("page:teams");
    expect(ids(manager)).not.toContain("page:organization-members");
    expect(ids(manager)).not.toContain("page:platform-teams");
    expect(jumpTargets(manager, org, manager.workspaces[0]).map(t => t.id)).toContain("page:members");
  });
  it("offers organization management without granting global users to org admins", () => {
    const manager: Session = { ...member, organizations: [{ ...org, role: "admin" }] };
    expect(ids(manager)).toContain("page:organization-members");
    expect(ids(manager)).not.toContain("page:users");
  });
  it("uses platform destinations without leftover organization/workspace scope", () => {
    const admin = { ...member, user: { ...member.user, platform_admin: true } };
    const targets = jumpTargets(admin, org, personal);
    for (const page of ["organizations", "platform-teams", "users"]) expect(targets.find(t => t.id === `page:${page}`)?.search).toEqual({ page, org: undefined, ws: undefined });
    expect(targets.filter(t => t.label === "Teams")).toHaveLength(1);
  });
  it("jumps across authorized contexts using each workspace's own organization", () => {
    const other = { ...org, id: "science", slug: "science", name: "Science" };
    const otherTeam = { ...team, id: "it", name: "IT", organization_id: other.id };
    const targets = jumpTargets({ ...member, organizations: [org, other], workspaces: [team, personal, otherTeam] }, org, personal);
    expect(targets.find(t => t.id === "workspace:it")).toMatchObject({ hint: "Science", keywords: ["Science", "science", "team"], search: { page: "overview", org: "science", ws: "it" } });
    expect(targets.filter(t => t.group === "Personal · private")).toHaveLength(1);
    expect(new Set(targets.map(t => t.id)).size).toBe(targets.length);
  });
  it("supports account destinations with no organization and no invented resources", () => {
    const targets = jumpTargets({ ...member, organizations: [], workspaces: [] });
    expect(targets.map(t => t.id)).toEqual(["page:profile", "page:accept-invitation"]);
  });
});

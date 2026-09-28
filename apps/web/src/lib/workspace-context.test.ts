import { describe, expect, it } from "vitest";
import type { Session, Workspace } from "./api";
import { canView, type DashboardSearch } from "./permissions";
import { dashboardHref } from "./locations";
import { clearRememberedPortals, navigation, rememberedWorkspace, rememberWorkspace, scopeSearch, sidebarWorkspace } from "./navigation";

const personal: Workspace = { id: "personal", organization_id: "org", name: "Personal", kind: "personal", role: "owner" };
const team: Workspace = { id: "team", organization_id: "org", name: "Product", kind: "team", role: "admin" };
const project: Workspace = { ...team, id: "project", name: "Research", kind: "project" };
const foreign: Workspace = { ...personal, id: "foreign", organization_id: "other" };
const session: Session = {
  user: { id: "me", email: "me@example.invalid", platform_admin: false },
  organizations: [{ id: "org", name: "Organization", slug: "org", role: "admin" }, { id: "other", name: "Other", slug: "other", role: "admin" }],
  workspaces: [personal, team, project, foreign],
};
const settings: DashboardSearch = { page: "organization-settings", org: "org" };
function storage() {
  const entries = new Map<string, string>();
  return { entries, get length() { return entries.size; }, key: (index: number) => [...entries.keys()][index] ?? null, getItem: (key: string) => entries.get(key) ?? null, setItem: (key: string, value: string) => { entries.set(key, value); }, removeItem: (key: string) => { entries.delete(key); } };
}

describe("workspace navigation beside organization settings", () => {
  it("keeps the full sidebar linked to the selected workspace without changing organization scope", () => {
    const context = sidebarWorkspace(session, settings, team);
    const links = navigation.filter(item => item.group === "Workspace" && canView(item.page, session, session.organizations[0], context));
    expect(links.map(item => item.label)).toEqual(["Overview", "API keys", "Models", "Costs", "Workspace settings", "Organization settings"]);
    expect(links.map(item => dashboardHref(scopeSearch(item.page, "org", context?.id)))).toEqual([
      "/organizations/org/workspaces/team", "/organizations/org/workspaces/team/keys", "/organizations/org/workspaces/team/models",
      "/organizations/org/workspaces/team/costs", "/organizations/org/workspaces/team/settings", "/organizations/org/settings",
    ]);
    expect(settings).toEqual({ page: "organization-settings", org: "org" });
  });
  it("remembers a team or project across organization tabs and reloads, with metadata only", () => {
    const store = storage();
    for (const ws of [team, project]) {
      rememberWorkspace(store, session, { page: "keys", org: "org", ws: ws.id, record: "private-key-id", q: "sensitive text" });
      rememberWorkspace(store, session, settings);
      rememberWorkspace(store, session, { ...settings, tab: "members" });
      expect([...store.entries.values()]).toEqual([ws.id]);
      expect(sidebarWorkspace(session, settings, rememberedWorkspace(store, session, "org"))).toEqual(ws);
    }
  });
  it("uses an accessible same-organization default on a fresh deep link", () => {
    expect(sidebarWorkspace(session, settings)).toEqual(personal);
    expect(sidebarWorkspace({ ...session, workspaces: [team, project, foreign] }, settings)).toEqual(team);
    expect(sidebarWorkspace({ ...session, workspaces: [foreign] }, settings)).toBeUndefined();
  });
  it("isolates remembered contexts by user and organization and never carries a foreign workspace into settings", () => {
    const store = storage();
    rememberWorkspace(store, session, { page: "overview", org: "other", ws: foreign.id });
    rememberWorkspace(store, session, { page: "overview", org: "org", ws: team.id });
    expect(rememberedWorkspace(store, session, "other")).toEqual(foreign);
    expect(rememberedWorkspace(store, { ...session, user: { ...session.user, id: "another-user" } }, "org")).toBeUndefined();
    expect(sidebarWorkspace(session, settings, foreign)).toEqual(personal);
    expect(sidebarWorkspace(session, { ...settings, org: "missing" }, team)).toBeUndefined();
  });
  it("validates fresh inventory, removes revoked context, and clears all context on logout", () => {
    const store = storage();
    rememberWorkspace(store, session, { page: "overview", org: "org", ws: team.id });
    expect(rememberedWorkspace(store, { ...session, workspaces: [personal] }, "org")).toBeUndefined();
    expect(store.length).toBe(0);
    rememberWorkspace(store, session, { page: "overview", org: "org", ws: personal.id });
    expect(rememberedWorkspace(store, { ...session, organizations: [] }, "org")).toBeUndefined();
    rememberWorkspace(store, session, { page: "overview", org: "org", ws: team.id });
    rememberWorkspace(store, session, { page: "overview", org: "other", ws: foreign.id });
    store.setItem("unrelated", "keep");
    clearRememberedPortals(store);
    expect([...store.entries]).toEqual([["unrelated", "keep"]]);
  });
  it("never substitutes remembered context for an explicit unavailable workspace or platform scope", () => {
    expect(sidebarWorkspace(session, { page: "keys", org: "org", ws: "missing" }, team)).toBeUndefined();
    expect(sidebarWorkspace(session, { page: "keys", org: "org", ws: foreign.id }, team)).toBeUndefined();
    expect(sidebarWorkspace(session, { page: "organization-detail", org: "org" }, team)).toBeUndefined();
    expect(sidebarWorkspace(session, { page: "models", org: "org", ws: team.id }, team)).toBeUndefined();
    expect(sidebarWorkspace(session, { page: "keys", org: "org", ws: personal.id }, team)).toEqual(personal);
  });
  it("does not grant settings authority or restore absent private workspaces from stale metadata", () => {
    const member: Session = { ...session, organizations: [{ ...session.organizations[0], role: "member" }], workspaces: [team] };
    expect(sidebarWorkspace(member, settings, personal)).toBeUndefined();
    const operator = { ...session, user: { ...session.user, platform_admin: true }, workspaces: [team] };
    expect(sidebarWorkspace(operator, settings, personal)).toEqual(team);
  });
  it("ignores invalid context writes and tolerates disabled storage", () => {
    const store = storage();
    rememberWorkspace(store, session, { page: "keys", org: "org", ws: foreign.id });
    rememberWorkspace(store, session, { page: "keys", org: "org", ws: "missing" });
    rememberWorkspace(store, session, { page: "models", org: "org", ws: team.id });
    rememberWorkspace(store, session, { ...settings, ws: team.id });
    rememberWorkspace(store, session, { page: "profile", org: "org", ws: team.id });
    expect(store.length).toBe(0);
    const blocked = { getItem() { throw new Error("disabled"); }, setItem() { throw new Error("disabled"); }, removeItem() { throw new Error("disabled"); } };
    expect(() => rememberWorkspace(blocked, session, { page: "keys", org: "org", ws: team.id })).not.toThrow();
    expect(rememberedWorkspace(blocked, session, "org")).toBeUndefined();
  });
});

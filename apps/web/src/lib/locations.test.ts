import { describe, expect, it } from "vitest";
import type { Session } from "./api";
import { canonicalSearch, dashboardHref, parseDashboardLocation } from "./locations";
import { dashboardSearch, type DashboardSearch } from "./permissions";
import { authorizedLocation, clearRememberedPortals, isAdminPage, rememberedPortal, rememberPortal, resolveDashboardSearch, resolveLegacyDashboardLocation, scopeSearch } from "./navigation";
const session: Session = {
  user: { id: "me", email: "me@example.invalid", platform_admin: true },
  organizations: [{ id: "org", name: "Organization", slug: "org", role: "operator" }, { id: "other", name: "Other", slug: "other", role: "member" }],
  workspaces: [{ id: "personal", organization_id: "org", name: "My private workspace", kind: "personal", role: "owner" }, { id: "team", organization_id: "other", name: "Shared", kind: "team", role: "member" }],
};
const pairs: [DashboardSearch, string][] = [
  [{ page: "platform-overview" }, "/admin"], [{ page: "organizations" }, "/admin/organizations"],
  [{ page: "organization-detail", org: "org" }, "/admin/organizations/org"],
  [{ page: "organization-settings", org: "org", tab: "members" }, "/organizations/org/settings?tab=members"],
  [{ page: "overview", org: "org", ws: "personal" }, "/organizations/org/workspaces/personal"],
  [{ page: "keys", org: "org", ws: "personal" }, "/organizations/org/workspaces/personal/keys"],
  [{ page: "grants", org: "org", ws: "personal" }, "/organizations/org/workspaces/personal/models"],
  [{ page: "costs", org: "org", ws: "personal" }, "/organizations/org/workspaces/personal/costs"],
  [{ page: "workspace-settings", org: "other", ws: "team", tab: "limits" }, "/organizations/other/workspaces/team/settings?tab=limits"],
  [{ page: "model-detail", record: "m", tab: "routing" }, "/admin/models/m?tab=routing"],
  [{ page: "provider-detail", record: "p" }, "/admin/providers/p"],
  [{ page: "deployment-detail", record: "d", tab: "pricing" }, "/admin/deployments/d?tab=pricing"],
  [{ page: "profile" }, "/profile"], [{ page: "accept-invitation" }, "/invitations/accept"],
];
function storage() {
  const entries = new Map<string, string>();
  return { entries, get length() { return entries.size; }, key: (i: number) => Array.from(entries.keys())[i] ?? null, setItem: (key: string, value: string) => { entries.set(key, value); }, getItem: (key: string) => entries.get(key) ?? null, removeItem: (key: string) => { entries.delete(key); } };
}
describe("canonical dashboard locations", () => {
  it.each(pairs)("round trips %j", (search, href) => {
    expect(dashboardHref(search)).toBe(href);
    expect(parseDashboardLocation(href)).toMatchObject(search);
    expect(dashboardHref(parseDashboardLocation(href)!)).toBe(href);
  });
  it("canonicalizes legacy workspace and organization pages into scoped tabs", () => {
    for (const [page, tab] of [["members", "members"], ["service-accounts", "service-accounts"], ["governance", "limits"]] as const) {
      const parsed = parseDashboardLocation(`/?page=${page}&org=other&ws=team&token=secret`)!;
      expect(parsed).toMatchObject({ page: "workspace-settings", org: "other", ws: "team", tab });
      expect(dashboardHref(parsed)).toBe(`/organizations/other/workspaces/team/settings?tab=${tab}`);
    }
    expect(dashboardHref({ page: "organization-policy", org: "org", ws: "personal" })).toBe("/organizations/org/settings?tab=limits");
    expect(canonicalSearch({ page: "organization-members", org: "org" })).toMatchObject({ page: "organization-settings", tab: "members" });
  });
  it("keeps platform-origin organization objects in Admin and contextual settings in Workspace", () => {
    expect(isAdminPage(parseDashboardLocation("/admin/organizations/org")!.page!)).toBe(true);
    expect(isAdminPage(parseDashboardLocation("/organizations/org/settings")!.page!)).toBe(false);
    expect(resolveDashboardSearch({}, session).page).toBe("platform-overview");
  });
  it("uses path scope, never stale or hostile query scope", () => {
    expect(parseDashboardLocation("/organizations/other/workspaces/team/keys?org=org&ws=personal&page=providers")).toMatchObject({ page: "keys", org: "other", ws: "team" });
    expect(parseDashboardLocation("/admin/models?org=org&ws=personal")).toMatchObject({ page: "models", org: undefined, ws: undefined });
  });
  it("returns not-found for unknown paths, invalid IDs, and unrecognized legacy pages", () => {
    for (const href of ["/oops", "/admin/unknown", "/admin/models/m/unknown", "/organizations/org/workspaces/team/members", "/api/v1/me", "//evil.invalid/admin", "https://evil.invalid/admin", "/?page=oops", "/?org=..%2Fsecret", "/organizations/org/workspaces/%2Fsecret", "/admin/models/%E0%A4%A"]) expect(parseDashboardLocation(href), href).toBeUndefined();
  });
  it("whitelists URL state and rejects secrets and arbitrary query objects", () => {
    expect(dashboardSearch({ page: "deployments", q: "Model name", enabled: "false", offset: "100", model: "model", provider: "provider", token: "secret", credentials: { raw: "secret" } })).toEqual({ page: "deployments", org: undefined, ws: undefined, q: "Model name", enabled: "false", offset: 100, model: "model", provider: "provider" });
    expect(dashboardSearch({ enabled: "maybe", offset: -1, tab: "secret", scope: "other", view: "raw", recipientKind: "personal", record: "bad/record", q: "x".repeat(201) })).toEqual({ page: undefined, org: undefined, ws: undefined });
    expect(dashboardHref({ ...({ token: "secret" } as object), page: "profile" })).toBe("/profile");
    expect(dashboardSearch({ q: "x".repeat(200), offset: 100000 })).toMatchObject({ q: "x".repeat(200), offset: 100000 });
    expect(dashboardSearch({ offset: 100001 }).offset).toBeUndefined();
  });
  it("round trips nested limits and delegation selectors without client-selected API scope", () => {
    const state: DashboardSearch = { page: "workspace-settings", org: "other", ws: "team", tab: "limits", scope: "keys", view: "inherited", record: "key-id" };
    expect(parseDashboardLocation(dashboardHref(state))).toMatchObject(state);
    const delegation: DashboardSearch = { page: "organization-settings", org: "org", tab: "models", recipientKind: "user", recipient: "user-id" };
    expect(parseDashboardLocation(dashboardHref(delegation))).toMatchObject(delegation);
    expect(scopeSearch("keys", "org", "personal")).not.toHaveProperty("recipient");
  });
});
describe("authorized defaults and portal history", () => {
  it("keeps old directory bookmarks useful for scoped administrators without widening access", () => {
    const manager: Session = { ...session, user: { ...session.user, platform_admin: false }, organizations: [{ ...session.organizations[1], role: "member" }], workspaces: [{ ...session.workspaces[1], role: "admin" }] };
    expect(resolveLegacyDashboardLocation("/?page=teams&org=other", manager)).toMatchObject({ page: "workspace-settings", org: "other", ws: "team" });
    expect(resolveLegacyDashboardLocation("/?page=organizations", manager)).toMatchObject({ page: "workspace-settings", org: "other", ws: "team" });
    expect(resolveLegacyDashboardLocation("/?page=teams&org=missing", manager)).toMatchObject({ org: "missing" });
    expect(resolveLegacyDashboardLocation("/organizations/other/settings?tab=teams", manager)?.page).toBe("organization-settings");
    expect(authorizedLocation(resolveLegacyDashboardLocation("/organizations/other/settings?tab=teams", manager)!, manager)).toBe(false);
  });
  it("never falls back from explicitly missing/mismatched scopes or reuses another org's personal data", () => {
    const missing = resolveDashboardSearch({ page: "overview", org: "missing", ws: "personal" }, session);
    expect(missing).toMatchObject({ org: "missing", ws: "personal" });
    expect(authorizedLocation(missing, session)).toBe(false);
    expect(resolveDashboardSearch({ page: "overview", org: "other" }, session)).toMatchObject({ org: "other", ws: "team" });
    expect(authorizedLocation(resolveDashboardSearch({ page: "overview", org: "other", ws: "personal" }, session), session)).toBe(false);
    expect(resolveDashboardSearch({ page: "overview", ws: "unknown" }, session).org).toBeUndefined();
  });
  it("records metadata only, isolates users, and revalidates against refreshed /me", () => {
    const store = storage();
    rememberPortal(store, session, { page: "keys", org: "org", ws: "personal", q: "sensitive text", enabled: "false", offset: 200 });
    expect([...store.entries.values()]).toEqual(["/organizations/org/workspaces/personal/keys"]);
    expect(rememberedPortal(store, session, "workspace")).toMatchObject({ page: "keys", ws: "personal" });
    expect(rememberedPortal(store, { ...session, user: { ...session.user, id: "other-user" } }, "workspace")).toBeUndefined();
    expect(rememberedPortal(store, { ...session, workspaces: [] }, "workspace")).toBeUndefined();
    expect(store.length).toBe(0);
  });
  it("forgets revoked platform authority and clears portal entries on logout without touching unrelated storage", () => {
    const store = storage();
    rememberPortal(store, session, { page: "models" });
    expect(rememberedPortal(store, { ...session, user: { ...session.user, platform_admin: false } }, "admin")).toBeUndefined();
    rememberPortal(store, session, { page: "organizations" });
    rememberPortal(store, session, { page: "overview", org: "other", ws: "team" });
    store.setItem("unrelated", "keep");
    clearRememberedPortals(store);
    expect([...store.entries]).toEqual([["unrelated", "keep"]]);
  });
  it("does not retain malformed, external, standalone, or cross-portal stored destinations", () => {
    const store = storage();
    for (const href of ["//evil.invalid/admin", "/admin/unknown", "/profile", "/admin/models", "/organizations/org/workspaces/someone-elses-personal"]) {
      store.setItem("omg:portal:me:workspace", href);
      expect(rememberedPortal(store, session, "workspace")).toBeUndefined();
    }
  });
});

import { describe, expect, it } from "vitest";
import { createMemoryHistory, createRouter } from "@tanstack/react-router";
import { dashboardRouteTree } from "./router";

describe("explicit dashboard routes", () => {
  it.each([
    ["/admin", "/admin"], ["/admin/organizations", "/admin/organizations"], ["/admin/organizations/org?tab=teams", "/admin/organizations/$org"],
    ["/organizations/org/settings?tab=members", "/organizations/$org/settings"], ["/organizations/org/workspaces/ws", "/organizations/$org/workspaces/$ws"],
    ["/organizations/org/workspaces/ws/keys", "/organizations/$org/workspaces/$ws/keys"], ["/organizations/org/workspaces/ws/settings?tab=limits", "/organizations/$org/workspaces/$ws/settings"],
    ["/admin/models/model?tab=routing", "/admin/models/$record"], ["/admin/providers/provider", "/admin/providers/$record"], ["/admin/deployments/deployment?tab=pricing", "/admin/deployments/$record"],
    ["/profile", "/profile"], ["/invitations/accept", "/invitations/accept"], ["/?page=keys&org=org&ws=ws", "/"],
  ])("matches %s without a catch-all", async (href, routeId) => {
    const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
    await router.load();
    expect(router.state.matches.at(-1)?.routeId).toBe(routeId);
    expect(router.state.matches.every(match => match.status === "success")).toBe(true);
  });
  it.each(["/not-a-page", "/admin/missing", "/admin/models/model/unknown", "/organizations/org/workspaces/ws/nonsense", "/api/v1/missing", "/assets/missing.js"])("returns real not-found status for %s", async href => {
    const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
    await router.load();
    expect(router.state.matches.some(match => match.status === "notFound" || match._notFound)).toBe(true);
  });
  it("preserves scoped URL state through back/forward navigation", async () => {
    const history = createMemoryHistory({ initialEntries: ["/admin/models/model?tab=routing", "/organizations/org/workspaces/ws/settings?tab=limits&scope=keys&record=key"], initialIndex: 1 });
    const router = createRouter({ routeTree: dashboardRouteTree, history });
    await router.load();
    expect(router.state.matches.at(-1)?.search).toMatchObject({ tab: "limits", scope: "keys", record: "key" });
    history.back();
    await router.load();
    expect(router.state.matches.at(-1)?.params).toMatchObject({ record: "model" });
    expect(router.state.matches.at(-1)?.search).toMatchObject({ tab: "routing" });
    history.forward();
    await router.load();
    expect(router.state.matches.at(-1)?.params).toMatchObject({ org: "org", ws: "ws" });
  });
});

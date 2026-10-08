import { describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { DashboardNavigationProvider, ResourceLink, useDashboardNavigation } from "./navigation-link";
import type { DashboardSearch } from "../lib/permissions";
describe("resource navigation links", () => {
  it("renders canonical anchors without requiring router context", () => { expect(renderToStaticMarkup(<ResourceLink search={{ page: "model-detail", record: "model", tab: "routing" }}>Model</ResourceLink>)).toContain('href="/admin/models/model?tab=routing"'); expect(renderToStaticMarkup(<ResourceLink search={{ page: "members", ws: "team" }} target="_blank" rel="noopener">Members</ResourceLink>)).toContain('href="/workspaces/team/settings?tab=members"'); });
  it("provides typed navigation while preserving native fallback anchors", () => { const search: DashboardSearch = { page: "workspace-settings", ws: "team", tab: "members" }, navigate = vi.fn(); let context: ReturnType<typeof useDashboardNavigation>; function Consumer() { context = useDashboardNavigation(); return <ResourceLink search={{ page: "profile" }}>Profile</ResourceLink>; } expect(renderToStaticMarkup(<DashboardNavigationProvider search={search} navigate={navigate}><Consumer /></DashboardNavigationProvider>)).toContain('href="/profile"'); expect(context!).toEqual({ search, navigate }); context!.navigate({ page: "keys", ws: "team" }); expect(navigate).toHaveBeenCalledWith({ page: "keys", ws: "team" }); renderToStaticMarkup(<Consumer />); expect(context).toBeUndefined(); });
});
describe("breadcrumb trail", () => {
  it("puts the model between Models and its route, with an icon on the root crumb", async () => {
    const { shellCrumbs } = await import("./layout/shell"), { admin } = await import("../lib/test-fixtures");
    const trail = shellCrumbs(admin, { page: "deployment-detail", record: "r" }, "OpenAI route", undefined, { label: "GPT-6 Luna", to: { page: "model-detail", record: "m" } });
    expect(trail.map(c => c.label)).toEqual(["Admin", "Models", "GPT-6 Luna", "OpenAI route"]);
    expect(trail[2]!.to).toEqual({ page: "model-detail", record: "m" });
    expect(trail[0]!.icon).toBeTruthy();
  });
});

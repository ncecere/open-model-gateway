import { describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { DashboardNavigationProvider, ResourceLink, useDashboardNavigation } from "./navigation-link";
import type { DashboardSearch } from "../lib/permissions";

describe("resource navigation links", () => {
  it("renders canonical anchors without requiring a router or navigation provider", () => {
    expect(renderToStaticMarkup(<ResourceLink search={{ page: "model-detail", record: "model", tab: "routing" }}>Model</ResourceLink>)).toBe('<a class="resource-link" href="/admin/models/model?tab=routing">Model</a>');
    expect(renderToStaticMarkup(<ResourceLink className="button secondary" search={{ page: "members", org: "org", ws: "team" }} target="_blank" rel="noopener">Members</ResourceLink>)).toContain('href="/organizations/org/workspaces/team/settings?tab=members"');
  });
  it("provides typed search and navigation while preserving fallback anchors", () => {
    const search: DashboardSearch = { page: "organization-settings", org: "org", tab: "members" };
    const navigate = vi.fn();
    let context: ReturnType<typeof useDashboardNavigation>;
    function Consumer() { context = useDashboardNavigation(); return <ResourceLink search={{ page: "profile" }}>Profile</ResourceLink>; }
    expect(renderToStaticMarkup(<DashboardNavigationProvider search={search} navigate={navigate}><Consumer /></DashboardNavigationProvider>)).toContain('href="/profile"');
    expect(context!).toEqual({ search, navigate });
    context!.navigate({ page: "keys", org: "org", ws: "team" });
    expect(navigate).toHaveBeenCalledWith({ page: "keys", org: "org", ws: "team" });
    renderToStaticMarkup(<Consumer />);
    expect(context).toBeUndefined();
  });
});

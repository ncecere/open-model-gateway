// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { admin, policy, testClient } from "../lib/test-fixtures";
import { limitScopeOf } from "../pages/settings/limits";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

async function mount(href: string) {
  const client = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
  await router.load();
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { router, client };
}
function serve() {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") return Promise.resolve(Response.json(admin));
    if (init?.method === "PUT") return Promise.resolve(Response.json({ policy }));
    if (path.endsWith("/policy")) return Promise.resolve(Response.json({ policy: { ...policy, requests_per_minute: path.includes("/team/") ? 60 : null } }));
    return Promise.resolve(Response.json({ data: [], has_more: false }));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}

describe("Admin › Settings › Defaults & limits tabs", () => {
  it("still opens from the old /admin/limits address, rewritten under Settings", async () => {
    serve();
    const { router, client } = await mount("/admin/limits?tab=team");
    expect(await screen.findByRole("heading", { name: "Defaults & limits", level: 1 })).toBeTruthy();
    expect(await screen.findByRole("table", { name: "Team default limits" })).toBeTruthy();
    await waitFor(() => expect(router.state.location.pathname).toBe("/admin/settings/limits"));
    expect(router.state.location.search).toMatchObject({ tab: "team" });
    client.clear();
  });
  it("shows one scope per pill tab, backed by ?tab=", async () => {
    const user = userEvent.setup(), fetch = serve();
    const { router, client } = await mount("/admin/settings/limits?tab=team");
    expect(await screen.findByRole("table", { name: "Team default limits" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Team default" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.queryByRole("table", { name: "Installation ceiling limits" })).toBeNull();
    // Only the open scope is fetched.
    expect(fetch.mock.calls.map(c => String(c[0])).filter(p => p.endsWith("/policy"))).toEqual(["/api/v1/platform/workspace-types/team/policy"]);
    await user.click(screen.getByRole("tab", { name: "Project default" }));
    await waitFor(() => expect(router.state.location.search).toMatchObject({ tab: "project" }));
    expect(await screen.findByRole("table", { name: "Project default limits" })).toBeTruthy();
    expect(limitScopeOf(undefined)).toBe("installation"); expect(limitScopeOf("nope")).toBe("installation"); expect(limitScopeOf("personal")).toBe("personal");
    client.clear();
  });
  it("asks before switching tabs with unsaved edits, then discards them", async () => {
    const user = userEvent.setup(), fetch = serve();
    const { router, client } = await mount("/admin/settings/limits?tab=team");
    const table = await screen.findByRole("table", { name: "Team default limits" });
    const rpm = within(table).getByRole("textbox", { name: /Requests per minute · Team default/ });
    await user.clear(rpm); await user.type(rpm, "75");
    expect(await screen.findByText("Unsaved changes: Team default")).toBeTruthy();
    await user.click(screen.getByRole("tab", { name: "Personal default" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Switch tabs without saving?")).toBeTruthy();
    // Keep editing: still on Team with the edit.
    await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(router.state.location.search).toMatchObject({ tab: "team" });
    expect((within(screen.getByRole("table", { name: "Team default limits" })).getByRole("textbox", { name: /Requests per minute · Team default/ }) as HTMLInputElement).value).toBe("75");
    // Discard and switch.
    await user.click(screen.getByRole("tab", { name: "Personal default" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Discard and switch" }));
    await waitFor(() => expect(router.state.location.search).toMatchObject({ tab: "personal" }));
    expect(await screen.findByRole("table", { name: "Personal default limits" })).toBeTruthy();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(fetch.mock.calls.some(c => (c[1] as RequestInit | undefined)?.method === "PUT")).toBe(false);
    // Back on Team: the discarded edit is gone.
    await user.click(screen.getByRole("tab", { name: "Team default" }));
    const again = await screen.findByRole("table", { name: "Team default limits" });
    await waitFor(() => expect((within(again).getByRole("textbox", { name: /Requests per minute · Team default/ }) as HTMLInputElement).value).toBe("60"));
    client.clear();
  });
  it("saves only the open scope", async () => {
    const user = userEvent.setup(), fetch = serve();
    const { client } = await mount("/admin/settings/limits?tab=project");
    const table = await screen.findByRole("table", { name: "Project default limits" });
    await user.type(within(table).getByRole("textbox", { name: /Requests per minute · Project default/ }), "30");
    await user.click(await screen.findByRole("button", { name: "Save limits" }));
    await waitFor(() => expect(fetch.mock.calls.filter(c => (c[1] as RequestInit | undefined)?.method === "PUT").map(c => String(c[0]))).toEqual(["/api/v1/platform/workspace-types/project/policy"]));
    client.clear();
  });
});

// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests, type Session } from "../lib/api";
import { admin, personal, session, team, testClient } from "../lib/test-fixtures";

const notice = "Workspace not found or not available";
const clients: ReturnType<typeof testClient>[] = [];
beforeEach(() => {
  localStorage.clear(); sessionStorage.clear();
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(() => { cleanup(); abortRequests(); for (const client of clients.splice(0)) client.clear(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });
async function mount(href: string, me: Session = session) {
  const fetch = vi.fn(async (input: string) => {
    if (input === "/api/v1/me") return Response.json(me);
    if (input === "/api/v1/me/notifications/summary") return Response.json({ unread: 0 });
    return Response.json({ data: [], models: [], not_serving: [], has_more: false });
  });
  vi.stubGlobal("fetch", fetch);
  const client = testClient(); clients.push(client);
  const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
  await router.load(); render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { router, fetch };
}
describe("unavailable workspace bookmarks", () => {
  it.each([
    ["missing", session],
    ["foreign", { ...admin, workspaces: [...admin.workspaces, { ...personal, id: "foreign", name: "Private owner name", owner_user_id: "someone-else" }] }],
    ["disabled", { ...session, workspaces: [...session.workspaces, { ...team, id: "disabled", name: "Disabled private label", disabled_at: "2026-10-01T00:00:00Z" }] }],
    ["staff-only", { ...admin, workspaces: [...admin.workspaces, { ...team, id: "staff-only", name: "Not a membership", role: null }] }],
  ] as const)("redirects %s to Home with the same non-identifying notice and no resource reads", async (ws, me) => {
    const { router, fetch } = await mount(`/workspaces/${ws}/keys`, me);
    await waitFor(() => expect(router.state.location.pathname).toBe("/home"));
    expect(await screen.findByText(notice)).toBeTruthy();
    expect(screen.getByText("Your workspaces")).toBeTruthy();
    expect(document.body.textContent).not.toMatch(/Private owner name|Disabled private label|Not a membership/);
    expect(fetch.mock.calls.some(([path]) => path.startsWith(`/api/v1/workspaces/${ws}/`))).toBe(false);
    expect(router.state.location.search).toEqual({});
    expect(JSON.stringify(router.state.location.state)).not.toMatch(/foreign|disabled|staff-only|missing|Private owner/);
    // Following a valid Home link removes the contextual notice without granting anything new.
    await userEvent.setup().click(screen.getByRole("link", { name: "Product: API keys" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/workspaces/team/keys"));
    expect(screen.queryByText(notice)).toBeNull();
  });
  it("retains valid workspace access and does not invent a notice on ordinary Home", async () => {
    const { router, fetch } = await mount("/workspaces/team/keys");
    await screen.findByRole("heading", { name: "API keys", exact: true });
    expect(router.state.location.pathname).toBe("/workspaces/team/keys");
    expect(fetch.mock.calls.some(([path]) => path.startsWith("/api/v1/workspaces/team/keys"))).toBe(true);
    expect(screen.queryByText(notice)).toBeNull();
    await act(async () => { await router.navigate({ to: "/home" }); });
    await screen.findByText("Your workspaces");
    expect(screen.queryByText(notice)).toBeNull();
  });
  it("keeps a page-specific permission denial in its workspace rather than calling the workspace unavailable", async () => {
    const { router, fetch } = await mount("/workspaces/team/report-schedules/schedule/runs/run", { ...session, workspaces: session.workspaces.map(w => w.id === "team" ? { ...w, role: "member" } : w) });
    await screen.findByRole("heading", { name: "Access not available", exact: true });
    expect(router.state.location.pathname).toBe("/workspaces/team/report-schedules/schedule/runs/run");
    expect(screen.queryByText(notice)).toBeNull();
    expect(fetch.mock.calls.some(([path]) => path.includes("/report-schedules/"))).toBe(false);
  });
});

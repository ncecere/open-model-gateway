// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests, type Session } from "../lib/api";
import { admin, auditor, member, personal, session, team, testClient } from "../lib/test-fixtures";
import type { AlertRule, Notification } from "../lib/alerts";
import { bellCount, bellLabel } from "./notification-bell";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const rule: AlertRule = { id: "r1", scope: "installation", workspace_id: null, kind: "budget_threshold", name: "Monthly budgets", enabled: true, budget_layers: ["type", "local"], thresholds: [50, 80, 100], spike_factor_percent: null, min_spend_microusd: null, window_minutes: null, error_rate_percent: null, min_requests: null, consecutive_failures: null, provider_connection_id: null, provider_connection: null, notify_workspace_admins: false, notify_platform_admins: true, notify_emails: [], firing: 1, last_fired_at: "2026-10-08T00:00:00Z", created_at: "2026-10-08T00:00:00Z", updated_at: "2026-10-08T00:00:00Z" };
const notification: Notification = { id: "e1", rule: { id: "r1", name: "Monthly budgets", scope: "installation", deleted: false }, builtin: false, kind: "budget_threshold", state: "firing", severity: "warning", level: 80, summary: "Workspace monthly budget reached 80%", details: { unknown_cost_requests: 1 }, workspace: { id: "team", name: "Product", kind: "team" }, connection: null, fired_at: "2026-10-08T00:00:00Z", resolved_at: null, resolution: null, email: null, read: false };

type Handler = (path: string, init?: RequestInit) => unknown;
function serve(who: Session = admin, handler: Handler = () => undefined) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") return Promise.resolve(Response.json(who));
    const custom = handler(path, init);
    if (custom !== undefined) return Promise.resolve(Response.json(custom));
    if (path.startsWith("/api/v1/me/notifications/summary")) return Promise.resolve(Response.json({ unread: 3, firing: 1 }));
    if (path.startsWith("/api/v1/platform/alerts/rules")) return Promise.resolve(Response.json({ data: [rule] }));
    return Promise.resolve(Response.json({ data: [], has_more: false }));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
async function mount(href: string) {
  const client = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
  await router.load();
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { router, client };
}
const sent = (fetch: ReturnType<typeof vi.fn>, method: string) => fetch.mock.calls.filter(c => (c[1] as RequestInit | undefined)?.method === method).map(c => [String(c[0]), JSON.parse(String((c[1] as RequestInit).body ?? "null"))]);

describe("Admin › Settings › Alerts", () => {
  it("is a Settings sidebar entry with a compact rules list and a New rule action", async () => {
    serve();
    const { client } = await mount("/admin/settings/alerts");
    await screen.findByRole("heading", { name: "Alerts", level: 1 });
    expect(within(screen.getByRole("navigation", { name: "Main" })).getByRole("link", { name: "Alerts" }).getAttribute("href")).toBe("/admin/settings/alerts");
    expect(await screen.findByText("50/80/100% of Type default, Workspace budgets")).toBeTruthy();
    expect(screen.getByText("Firing · 1")).toBeTruthy();
    expect(screen.getAllByRole("link", { name: /New rule/ })[0]!.getAttribute("href")).toBe("/admin/alerts/new");
    client.clear();
  });
  it("creates a rule from the header actions with exact values, validating first", async () => {
    const user = userEvent.setup(), fetch = serve(admin, (path, init) => init?.method === "POST" && path === "/api/v1/platform/alerts/rules" ? { ...rule, id: "r2" } : undefined);
    const { client, router } = await mount("/admin/alerts/new");
    await screen.findByRole("heading", { name: "New alert rule", level: 1 });
    await user.type(screen.getByRole("textbox", { name: "Name" }), "Spend spikes");
    await user.selectOptions(screen.getByRole("combobox", { name: /Type/ }), "spend_spike");
    const factor = screen.getByRole("textbox", { name: /7-day hourly average/ });
    await user.clear(factor); await user.type(factor, "1");
    await user.click(screen.getByRole("button", { name: "Create rule" }));
    expect(await screen.findByText("Enter a multiple from 1.1 to 1000.")).toBeTruthy();
    expect(sent(fetch, "POST")).toEqual([]);
    await user.clear(factor); await user.type(factor, "2.5");
    await user.type(screen.getByRole("textbox", { name: /Other addresses/ }), "Finance@Example.com");
    await user.click(screen.getByRole("button", { name: "Create rule" }));
    await waitFor(() => expect(sent(fetch, "POST")).toEqual([["/api/v1/platform/alerts/rules", { name: "Spend spikes", kind: "spend_spike", enabled: true, notify_platform_admins: true, notify_emails: ["finance@example.com"], spike_factor_percent: 250, min_spend_microusd: "1000000" }]]));
    await waitFor(() => expect(router.state.location.pathname).toBe("/admin/settings/alerts"));
    client.clear();
  });
  it("creates a non-blocking installation spend rule with an exact amount", async () => {
    const user = userEvent.setup(), fetch = serve(admin, (path, init) => init?.method === "POST" && path === "/api/v1/platform/alerts/rules" ? { ...rule, id: "r3" } : undefined);
    const { client } = await mount("/admin/alerts/new");
    await screen.findByRole("heading", { name: "New alert rule", level: 1 });
    // The budget kind offers no installation layer.
    expect(screen.queryByRole("checkbox", { name: "Installation" })).toBeNull();
    await user.type(screen.getByRole("textbox", { name: "Name" }), "Total spend");
    await user.selectOptions(screen.getByRole("combobox", { name: /Type/ }), "spend_threshold");
    expect(screen.getByText(/never blocks/)).toBeTruthy();
    await user.selectOptions(screen.getByRole("combobox", { name: /Period/ }), "week");
    await user.type(screen.getByRole("textbox", { name: /Amount/ }), "9007199254.740993");
    const at = screen.getByRole("textbox", { name: /Alert at/ });
    await user.clear(at); await user.type(at, "100, 80");
    await user.click(screen.getByRole("button", { name: "Create rule" }));
    await waitFor(() => expect(sent(fetch, "POST")).toEqual([["/api/v1/platform/alerts/rules", { name: "Total spend", kind: "spend_threshold", enabled: true, notify_platform_admins: true, notify_emails: [], spend_period: "week", spend_amount_microusd: "9007199254740993", thresholds: [80, 100] }]]));
    client.clear();
  });
  it("gives Auditors the list and read-only rule facts, never a form", async () => {
    serve(auditor, path => path === "/api/v1/platform/alerts/rules/r1" ? rule : undefined);
    const { client } = await mount("/admin/alerts/r1");
    expect(await screen.findByText("50/80/100% of Type default, Workspace budgets")).toBeTruthy();
    expect(document.querySelector("main input, main textarea, main select")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save rule" })).toBeNull();
    cleanup(); client.clear();
    const again = await mount("/admin/settings/alerts");
    await screen.findByText("Monthly budgets");
    expect(screen.queryByRole("link", { name: /New rule/ })).toBeNull();
    again.client.clear();
  });
});

describe("Notifications", () => {
  it("formats the bell", () => { expect(bellLabel(0)).toBe("Notifications"); expect(bellLabel(3)).toBe("Notifications, 3 unread"); expect(bellCount(120)).toBe("99+"); });
  it("shows the unread count in the top bar and a routed page with read controls", async () => {
    const user = userEvent.setup(), fetch = serve(session, (path, init) => init?.method === "POST" ? { marked: 1 } : path.startsWith("/api/v1/me/notifications?") ? { data: [notification], has_more: false } : undefined);
    const { client } = await mount("/notifications");
    await screen.findByRole("heading", { name: "Notifications", level: 1 });
    const bell = await screen.findByRole("link", { name: "Notifications, 3 unread" });
    expect(bell.getAttribute("href")).toBe("/notifications");
    expect(await screen.findByText("Workspace monthly budget reached 80%")).toBeTruthy();
    expect(screen.getByText(/Some cost unknown/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /Mark all read/ }));
    await waitFor(() => expect(sent(fetch, "POST")).toEqual([["/api/v1/me/notifications/read", { all: true }]]));
    client.clear();
  });
});

describe("Workspace Settings › Alerts", () => {
  it("is a tab for Team/Project admins with workspace rules", async () => {
    serve(session, path => path.startsWith("/api/v1/workspaces/team/alerts/rules") ? { data: [{ ...rule, scope: "workspace", workspace_id: "team", budget_layers: ["local"], notify_platform_admins: false, notify_workspace_admins: true }], writable: true } : undefined);
    const { client } = await mount("/workspaces/team/settings?tab=alerts");
    expect(await screen.findByText("50/80/100% of Workspace budgets")).toBeTruthy();
    expect(screen.getAllByRole("link", { name: /New rule/ })[0]!.getAttribute("href")).toBe("/workspaces/team/alerts/new");
    client.clear();
  });
  it("is absent for members and built in for personal workspaces", async () => {
    serve({ ...session, workspaces: [personal, member] });
    const first = await mount("/workspaces/team/settings");
    await screen.findByRole("tab", { name: /Members/ });
    expect(screen.queryByRole("tab", { name: /Alerts/ })).toBeNull();
    cleanup(); first.client.clear();
    serve();
    const second = await mount("/workspaces/personal/settings?tab=alerts");
    expect(await screen.findByText("Built-in budget alerts")).toBeTruthy();
    expect(screen.queryByRole("link", { name: /New rule/ })).toBeNull();
    second.client.clear();
    expect(team.capabilities.manage_policy).toBe(true);
  });
});

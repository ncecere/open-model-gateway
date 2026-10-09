// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider, Outlet } from "@tanstack/react-router";
import { DashboardNavigationProvider, ResourceLink } from "./navigation-link";
import type { ReactNode } from "react";
import { ActionProvider } from "./ui";
import { SettingsForm } from "./templates/settings-form";
import { WorkspaceCatalogs } from "../pages/model-access";
import { WorkspaceDetail } from "../pages/resource-details";
import { ScopeLimits } from "./scope-limits";
import { abortRequests } from "../lib/api";
import type { Policy } from "../lib/governance";
import { admin, auditor, policy, team, testClient } from "../lib/test-fixtures";
import { nameField } from "../lib/forms";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const catalogs = [{ id: "local", name: "Local datacenter", description: null }, { id: "cloud", name: "Approved cloud", description: null }, { id: "demo", name: "demo", description: null }];
function mount(node: ReactNode, client = testClient()) {
  return { client, ...render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>) };
}
/** Serves `path` from a mutable state and records every write. */
function serve(path: string, initial: unknown, extra: [string, unknown][] = [], onWrite: (method: string, body: unknown) => unknown = () => undefined) {
  const client = testClient(); let state = initial;
  client.setQueryData(["api", undefined, path], state);
  for (const [p, data] of extra) client.setQueryData(p.endsWith(" choices") ? ["api", undefined, p.slice(0, -8), "choices"] : ["api", undefined, p], data);
  const fetch = vi.fn(async (url: string, options: RequestInit = {}) => {
    if (url === path) {
      if (options.method === "PUT" || options.method === "DELETE") { state = onWrite(options.method, options.body ? JSON.parse(String(options.body)) : undefined) ?? state; return Response.json({ ok: true }); }
      return Response.json(state);
    }
    for (const [p, data] of extra) if (url.startsWith(p.replace(/ choices$/, ""))) return Response.json(p.endsWith(" choices") ? { data } : data);
    throw new Error(`Unexpected request: ${url}`);
  });
  vi.stubGlobal("fetch", fetch);
  return { client, fetch, writes: () => fetch.mock.calls.filter(([, o]) => o?.method === "PUT" || o?.method === "DELETE").map(([, o]) => ({ method: o!.method, body: o!.body ? JSON.parse(String(o!.body)) : undefined })) };
}
const catalogPath = "/api/v1/platform/workspaces/project/catalogs", typeCatalogs = "/api/v1/platform/workspace-types/project/catalogs";
const catalogExtra: [string, unknown][] = [["/api/v1/platform/catalogs choices", catalogs], [typeCatalogs, { kind: "project", catalog_ids: ["local", "cloud"] }]];
const checked = (name: string) => screen.getByRole("checkbox", { name }).getAttribute("aria-checked");

describe("Model access · catalogs", () => {
  it("shows live defaults by name and prefills Choose catalogs with what is available now", async () => {
    const user = userEvent.setup(), api = serve(catalogPath, { mode: "inherit", catalog_ids: [], effective_catalog_ids: ["local", "cloud"] }, catalogExtra, (_, body) => ({ mode: "replace", catalog_ids: (body as { catalog_ids: string[] }).catalog_ids, effective_catalog_ids: (body as { catalog_ids: string[] }).catalog_ids }));
    mount(<WorkspaceCatalogs session={admin} workspaceId="project" kind="project" />, api.client);
    expect(screen.getByText("Defaults: Local datacenter · Approved cloud")).toBeDefined();
    expect(screen.queryByRole("checkbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save catalogs" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Reset/ })).toBeNull();
    await user.click(screen.getByRole("radio", { name: "Choose catalogs" }));
    expect(checked("Local datacenter")).toBe("true");
    expect(checked("Approved cloud")).toBe("true");
    expect(checked("demo")).toBe("false");
    await user.click(screen.getByRole("checkbox", { name: "demo" }));
    await user.click(screen.getByRole("button", { name: "Save catalogs" }));
    await waitFor(() => expect(api.writes()).toHaveLength(1));
    expect(api.writes()[0]).toEqual({ method: "PUT", body: { mode: "replace", catalog_ids: ["local", "cloud", "demo"] } });
    api.client.clear();
  });
  it("saves an explicit empty choice only after warning", async () => {
    const user = userEvent.setup(), api = serve(catalogPath, { mode: "inherit", catalog_ids: [], effective_catalog_ids: ["local"] }, catalogExtra);
    mount(<WorkspaceCatalogs session={admin} workspaceId="project" kind="project" />, api.client);
    await user.click(screen.getByRole("radio", { name: "Choose catalogs" }));
    await user.click(screen.getByRole("checkbox", { name: "Local datacenter" }));
    expect(screen.getByText(/will have no catalog models/)).toBeDefined();
    expect(screen.getByText(/not restored if the catalog becomes available again/)).toBeDefined();
    await user.click(screen.getByRole("button", { name: "Save catalogs" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "PUT", body: { mode: "replace", catalog_ids: [] } }]));
    api.client.clear();
  });
  it("switching back to the defaults is a confirmed reset (DELETE)", async () => {
    const user = userEvent.setup(), api = serve(catalogPath, { mode: "replace", catalog_ids: ["demo"], effective_catalog_ids: ["demo"] }, catalogExtra);
    mount(<WorkspaceCatalogs session={admin} workspaceId="project" kind="project" />, api.client);
    expect(checked("demo")).toBe("true");
    await user.click(screen.getByRole("radio", { name: "Use Project defaults" }));
    await user.click(screen.getByRole("button", { name: "Save catalogs" }));
    expect(api.writes()).toHaveLength(0);
    const dialog = await screen.findByRole("dialog");
    expect(dialog.textContent).toContain("Use Project defaults again?");
    await user.click(screen.getByRole("button", { name: "Use Project defaults" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "DELETE", body: undefined }]));
    api.client.clear();
  });
  it("discard restores the server state; read-only cannot change anything", async () => {
    const user = userEvent.setup(), api = serve(catalogPath, { mode: "inherit", catalog_ids: [], effective_catalog_ids: ["local"] }, catalogExtra);
    const view = mount(<WorkspaceCatalogs session={admin} workspaceId="project" kind="project" />, api.client);
    await user.click(screen.getByRole("radio", { name: "Choose catalogs" }));
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(screen.queryByRole("checkbox")).toBeNull();
    view.unmount();
    mount(<WorkspaceCatalogs session={auditor} workspaceId="project" kind="project" />, api.client);
    expect(screen.getAllByRole("radio").every(r => r.hasAttribute("data-disabled") || r.getAttribute("aria-disabled") === "true")).toBe(true);
    expect(screen.queryByRole("button", { name: "Save catalogs" })).toBeNull();
    expect(api.writes()).toHaveLength(0);
    api.client.clear();
  });
});

const policyPath = "/api/v1/platform/workspaces/project/policy";
const typeDefault = { requests_per_minute: 60, tokens_per_minute: 100000, concurrent_requests: 8, monthly_budget_microusd: "100000000", budget_period: "month" as const };
const inherited = { mode: "inherit", policy: typeDefault, effective: typeDefault, provenance: { platform_source: "type_default", platform: typeDefault, local: policy, key: null, type_default: typeDefault }, budgets: [] };
describe("Limits · replacement override", () => {
  it("prefills the override from the defaults and saves stacked budgets", async () => {
    const user = userEvent.setup(), api = serve(policyPath, inherited);
    mount(<ScopeLimits mode="replacement" kind="project" path={policyPath} writable />, api.client);
    expect(screen.queryByRole("textbox")).toBeNull();
    await user.click(screen.getByRole("radio", { name: "Override for this project" }));
    const rpm = screen.getByRole("textbox", { name: /^Requests per minute · / }) as HTMLInputElement;
    expect(rpm.value).toBe("60");
    expect((screen.getByRole("textbox", { name: /^Monthly budget \(USD\)/ }) as HTMLInputElement).value).toBe("100.00");
    expect(screen.getByText("Resets monthly on the 1st at 00:00 UTC")).toBeDefined();
    await user.click(screen.getByRole("button", { name: "Add budget" }));
    await user.type(screen.getByRole("textbox", { name: /^Daily budget \(USD\)/ }), "5");
    expect(screen.getAllByText("Resets daily at 00:00 UTC").length).toBeGreaterThan(0);
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "PUT", body: { requests_per_minute: 60, tokens_per_minute: 100000, concurrent_requests: 8, concurrent_jobs: null, storage_bytes: null, budgets: [{ period: "day", amount_microusd: "5000000" }, { period: "month", amount_microusd: "100000000" }] } }]));
    api.client.clear();
  });
  it("saves a removed budget row as no budget, not the inherited type default (live acceptance F3)", async () => {
    const user = userEvent.setup(), api = serve(policyPath, inherited);
    mount(<ScopeLimits mode="replacement" kind="project" path={policyPath} writable />, api.client);
    await user.click(screen.getByRole("radio", { name: "Override for this project" }));
    await user.click(screen.getByRole("button", { name: "Remove monthly budget" }));
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "PUT", body: { requests_per_minute: 60, tokens_per_minute: 100000, concurrent_requests: 8, concurrent_jobs: null, storage_bytes: null, budgets: [] } }]));
    api.client.clear();
  });
  it("allows one budget per period: the period select offers only unused periods and Add stops at four", async () => {
    const user = userEvent.setup(), api = serve(policyPath, inherited);
    mount(<ScopeLimits mode="replacement" kind="project" path={policyPath} writable />, api.client);
    await user.click(screen.getByRole("radio", { name: "Override for this project" }));
    for (let i = 0; i < 3; i++) await user.click(screen.getByRole("button", { name: "Add budget" }));
    expect((screen.getByRole("button", { name: "Add budget" }) as HTMLButtonElement).disabled).toBe(true);
    const selects = screen.getAllByRole("combobox", { name: /^Budget period/ });
    expect(selects).toHaveLength(4);
    for (const select of selects) expect(select.querySelectorAll("option")).toHaveLength(1);
    api.client.clear();
  });
  it("returning to the defaults deletes the override after confirmation", async () => {
    const user = userEvent.setup(), api = serve(policyPath, { ...inherited, mode: "replace", policy: { ...policy, budget_period: "day" }, provenance: { ...inherited.provenance, platform_source: "workspace_override", platform: policy } });
    mount(<ScopeLimits mode="replacement" kind="project" path={policyPath} writable />, api.client);
    await user.click(screen.getByRole("radio", { name: "Use Project defaults" }));
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    await screen.findByRole("dialog");
    await user.click(screen.getByRole("button", { name: "Use Project defaults" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "DELETE", body: undefined }]));
    api.client.clear();
  });
});
describe("Limits · tighten-only workspace restrictions", () => {
  const local = (stored: Policy = policy) => ({ policy: stored, effective: typeDefault, provenance: { platform_source: "type_default", platform: typeDefault, local: stored, key: null } });
  it("shows inherited placeholders, rejects values above the parent for the same period, and lets other periods stack", async () => {
    const user = userEvent.setup(), api = serve("/api/v1/workspaces/team/policy", local());
    mount(<ScopeLimits mode="local" path="/api/v1/workspaces/team/policy" writable />, api.client);
    const rpm = screen.getByRole("textbox", { name: /^Requests per minute/ });
    expect(rpm.getAttribute("placeholder")).toBe("Inherited: 60");
    await user.type(rpm, "61");
    expect(screen.getByText("Can't be higher than the Team default limit (60).")).toBeDefined();
    expect((screen.getByRole("button", { name: "Save limits" }) as HTMLButtonElement).disabled).toBe(true);
    await user.clear(rpm); await user.type(rpm, "30");
    await user.click(screen.getByRole("button", { name: "Add monthly cap" }));
    const monthly = screen.getByRole("textbox", { name: /^Monthly budget \(USD\)/ });
    expect(monthly.getAttribute("placeholder")).toBe("At most $100.00");
    await user.type(monthly, "500");
    expect(screen.getByText("Can't be higher than the Team default monthly budget ($100.00).")).toBeDefined();
    expect((screen.getByRole("button", { name: "Save limits" }) as HTMLButtonElement).disabled).toBe(true);
    await user.clear(monthly); await user.type(monthly, "50");
    // A daily budget isn't compared with the monthly parent: both apply, each over its own window.
    await user.click(screen.getByRole("button", { name: "Add budget" }));
    await user.type(screen.getByRole("textbox", { name: /^Daily budget \(USD\)/ }), "1000");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "PUT", body: { requests_per_minute: 30, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, storage_bytes: null, budgets: [{ period: "day", amount_microusd: "1000000000" }, { period: "month", amount_microusd: "50000000" }] } }]));
    api.client.clear();
  });
  it("never raises or removes a saved budget: its period is fixed and it can only be lowered", async () => {
    const user = userEvent.setup(), stored = { ...policy, monthly_budget_microusd: "5000000", budget_period: "week" as const, budgets: [{ period: "week" as const, amount_microusd: "5000000" }] }, api = serve("/api/v1/workspaces/team/policy", local(stored));
    mount(<ScopeLimits mode="local" path="/api/v1/workspaces/team/policy" writable />, api.client);
    expect((screen.getByRole("combobox", { name: /^Budget period/ }) as HTMLSelectElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: /^Saved weekly budget can only be lowered/ }) as HTMLButtonElement).disabled).toBe(true);
    const weekly = screen.getByRole("textbox", { name: /^Weekly budget \(USD\)/ });
    await user.clear(weekly); await user.type(weekly, "6");
    expect(screen.getByText("A saved budget can only be lowered (now $5.00).")).toBeDefined();
    expect((screen.getByRole("button", { name: "Save limits" }) as HTMLButtonElement).disabled).toBe(true);
    await user.clear(weekly); await user.type(weekly, "4.5");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() => expect(api.writes()).toEqual([{ method: "PUT", body: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, storage_bytes: null, budgets: [{ period: "week", amount_microusd: "4500000" }] } }]));
    api.client.clear();
  });
});
describe("Team/Project detail tabs", () => {
  it.each(["catalogs", "models"])("redirects the old ?tab=%s to Model access", tab => {
    const client = testClient(), onTabChange = vi.fn();
    client.setQueryData(["api", undefined, "/api/v1/platform/workspaces/project"], { ...team, id: "project", kind: "project", name: "Research", member_count: 2 });
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({ data: [] })));
    mount(<WorkspaceDetail session={admin} id="project" kind="project" tab={tab} onTabChange={onTabChange} />, client);
    expect(onTabChange).toHaveBeenCalledWith("model-access");
    expect(screen.getByRole("tab", { name: /^Models/ }).getAttribute("aria-selected")).toBe("true");
    expect(screen.queryByRole("tab", { name: /^Catalogs/ })).toBeNull();
    client.clear();
  });
});

describe("SettingsForm pristine-submit lifecycle", () => {
  it("does not block navigation merely because a pristine override can be created", async () => {
    const user = userEvent.setup(), client = testClient(), save = vi.fn(), root = createRootRoute({ component: Outlet });
    let router: ReturnType<typeof createRouter>;
    const first = createRoute({ getParentRoute: () => root, path: "/", component: () => <DashboardNavigationProvider search={{ page: "workspace-settings", ws: "project" }} navigate={() => void router.navigate({ to: "/profile" })}><SettingsForm fields={[]} writable allowPristineSubmit onSave={save} /><ResourceLink search={{ page: "profile" }}>Your profile</ResourceLink></DashboardNavigationProvider> });
    const second = createRoute({ getParentRoute: () => root, path: "/profile", component: () => <h1>Destination profile</h1> });
    router = createRouter({ routeTree: root.addChildren([first, second]), history: createMemoryHistory({ initialEntries: ["/"] }) });
    await router.load();
    render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
    await screen.findByRole("button", { name: "Save custom settings" });
    await user.click(screen.getByRole("link", { name: "Your profile" }));
    await screen.findByRole("heading", { name: "Destination profile" });
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(save).not.toHaveBeenCalled();
    client.clear();
  });
  it("allows creation again after an existing header is reset, including after a dirty save", async () => {
    const user = userEvent.setup(), client = testClient(), save = vi.fn().mockResolvedValue({});
    const node = (allowPristineSubmit: boolean) => <QueryClientProvider client={client}><SettingsForm fields={[{ ...nameField, value: "Original" }]} writable allowPristineSubmit={allowPristineSubmit} onSave={save} /></QueryClientProvider>;
    const result = render(node(true));
    await user.click(screen.getByRole("button", { name: "Save custom settings" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /Save/ })).toBeNull());
    result.rerender(node(false));
    await user.type(screen.getByRole("textbox", { name: /^Name/ }), " edited");
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /Save/ })).toBeNull());
    result.rerender(node(true));
    expect(screen.getByRole("button", { name: "Save custom settings" })).toBeDefined();
    expect(result.container.querySelector("form")?.hasAttribute("data-dirty")).toBe(false);
    expect(save).toHaveBeenCalledTimes(2);
    client.clear();
  });
  it("consumes pristine permission on success even before a caller refetches its header", async () => {
    const user = userEvent.setup(), save = vi.fn().mockResolvedValue({}), result = mount(<SettingsForm fields={[]} writable allowPristineSubmit onSave={save} />);
    await user.click(screen.getByRole("button", { name: "Save custom settings" }));
    await screen.findByText("Settings saved.");
    fireEvent.submit(result.container.querySelector("form")!);
    expect(save).toHaveBeenCalledOnce();
    expect(screen.queryByRole("button", { name: /Save/ })).toBeNull();
    result.client.clear();
  });
  it("validates pristine submissions before onSave and preserves discard for actual edits", async () => {
    const user = userEvent.setup(), save = vi.fn(), result = mount(<SettingsForm fields={[nameField]} writable allowPristineSubmit onSave={save} />);
    await user.click(screen.getByRole("button", { name: "Save custom settings" }));
    expect(save).not.toHaveBeenCalled();
    const input = screen.getByRole("textbox", { name: /^Name/ });
    expect(document.activeElement).toBe(input);
    await user.type(input, "Edited");
    expect(result.container.querySelector("form")?.getAttribute("data-dirty")).toBe("true");
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect((input as HTMLInputElement).value).toBe("");
    expect(screen.queryByRole("button", { name: "Discard" })).toBeNull();
    expect(screen.getByRole("button", { name: "Save custom settings" })).toBeDefined();
    result.client.clear();
  });
  it("preserves explicit retry after failure and aborts a pending pristine save on unmount", async () => {
    const user = userEvent.setup(); let signal: AbortSignal | undefined, resolve!: () => void;
    const save = vi.fn().mockRejectedValueOnce(new Error("Override rejected")).mockImplementationOnce((_: unknown, s: AbortSignal) => { signal = s; return new Promise<void>(r => { resolve = r; }); });
    const result = mount(<SettingsForm fields={[]} writable allowPristineSubmit onSave={save} />), invalidate = vi.spyOn(result.client, "invalidateQueries");
    await user.click(screen.getByRole("button", { name: "Save custom settings" }));
    await screen.findByText("Override rejected");
    expect(invalidate).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Save custom settings" }));
    fireEvent.submit(result.container.querySelector("form")!);
    expect(save).toHaveBeenCalledTimes(2);
    expect(signal?.aborted).toBe(false);
    result.unmount();
    expect(signal?.aborted).toBe(true);
    resolve(); await Promise.resolve();
    expect(invalidate).not.toHaveBeenCalled();
    result.client.clear();
  });
});

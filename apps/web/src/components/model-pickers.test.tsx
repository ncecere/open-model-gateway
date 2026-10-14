// @vitest-environment jsdom
/*
 * R06 (#68): wherever a model is chosen or filtered, it is searched on the
 * server (`/platform/models?q=…&limit=20`) and a selected model is named from
 * its own record; no page loads every platform model (a `useChoices` query).
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider, type QueryClient } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { Routing } from "../pages/governance";
import { Deployments } from "../pages/catalog";
import { admin, model, provider, report, testClient } from "../lib/test-fixtures";

const models = "/api/v1/platform/models";
const modelId = "0190a5f4-7b1e-7c3d-9a2b-4c5d6e7f8a9b";
/** Queries that load every platform model (the old `useChoices` pattern). */
const fullModelLists = (client: QueryClient) => client.getQueryCache().findAll().map(q => q.queryKey).filter(k => k[3] === "choices" && String(k[2]).startsWith(models)).map(k => String(k[2]));
/** Requests that page through every platform model (`limit=200&offset=…`). */
const fullModelFetches = (fetch: ReturnType<typeof serve>) => fetch.mock.calls.map(c => String(c[0])).filter(p => p.startsWith(`${models}?`) && p.includes("offset="));
const paths = (client: QueryClient) => client.getQueryCache().findAll().map(q => String(q.queryKey[2]));
function serve(handler: (path: string) => unknown) {
  const fetch = vi.fn().mockImplementation((path: string) => { if (path === "/api/v1/me") return Promise.resolve(Response.json(admin)); const body = handler(path); return Promise.resolve(Response.json(body === undefined ? { data: [], has_more: false } : body)); });
  vi.stubGlobal("fetch", fetch); return fetch;
}
const mount = (node: ReactNode, client = testClient()) => { render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>); return client; };

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("model pickers search the server (R06, #68)", () => {
  it("Admin › Usage & costs: the Model filter searches, the selected model is named from its record, and workspace actions pick by search", async () => {
    const none = { metric: "spend", group_by: "key", then_by: null, period: { start_date: "2026-10-01", end_date: "2026-10-09" }, total: { value: "0", held_microusd: "0", unresolved_attempts: "0" }, rows: [], series: [] };
    const fetch = serve(p => p === `${models}/${modelId}` ? { ...model, id: modelId } : p.includes("/usage/explore?") ? none : p.startsWith("/api/v1/platform/cost-report?") ? { ...report, scope: "platform", breakdowns: { ...report.breakdowns, workspaces: [{ id: "w1", name: "Research", totals: report.totals }] } } : undefined);
    const client = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [`/admin/costs?tab=records&model_id=${modelId}`] }) });
    await router.load(); render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
    const table = await screen.findByRole("table", { name: "By workspace" });
    await within(table).findByText("Research");
    // Records match on the model's API name, read from that one record.
    await waitFor(() => expect(fetch.mock.calls.some(c => String(c[0]).startsWith("/api/v1/platform/cost-report?") && String(c[0]).includes("model=company%2Fsmart"))).toBe(true));
    expect(paths(client)).toContain(`${models}?limit=20`);
    expect(fullModelLists(client)).toEqual([]);
    expect(fullModelFetches(fetch)).toEqual([]);
    client.clear();
  });
  it("Routing: models are searched as you type, never listed in full", async () => {
    const user = userEvent.setup();
    const fetch = serve(p => p.startsWith(`${models}?q=sma&`) ? { data: [model] } : p === `${models}?limit=20` ? { data: [] } : p === `${models}/${model.id}/routing` ? { policy: { strategy: "priority", max_attempts: 1, allow_ambiguous_failover: false, required_residency: null } } : undefined);
    const client = mount(<Routing session={admin} />);
    await user.type(screen.getByRole("combobox", { name: "Model" }), "sma");
    await user.click(await screen.findByRole("option", { name: model.public_name }));
    expect(await screen.findByText("Open model page")).toBeTruthy();
    expect(paths(client)).toContain(`${models}?q=sma&limit=20`);
    expect(fullModelLists(client)).toEqual([]); expect(fullModelFetches(fetch)).toEqual([]);
    client.clear();
  });
  it("a failed model search or lookup is reported as an error, never as no models or a removed model", async () => {
    const failing = (status: number) => Promise.resolve(Response.json({ error: { code: String(status), message: status === 404 ? "Not found" : "Management storage unavailable" } }, { status }));
    // Routing: a failed search shows the error, not "No models match."
    vi.stubGlobal("fetch", vi.fn().mockImplementation((path: string) => path === "/api/v1/me" ? Promise.resolve(Response.json(admin)) : path.startsWith(`${models}?`) ? failing(503) : Promise.resolve(Response.json({ data: [] }))));
    const user = userEvent.setup(), client = mount(<Routing session={admin} />);
    await user.type(screen.getByRole("combobox", { name: "Model" }), "sma");
    expect(await screen.findByText("Management storage unavailable")).toBeTruthy();
    client.clear(); cleanup();
    // Records: a 503 looking up the filtered model is an error; only a 404 means it's gone.
    for (const [status, gone] of [[503, false], [404, true]] as const) {
      const none = { metric: "spend", group_by: "key", then_by: null, period: { start_date: "2026-10-01", end_date: "2026-10-09" }, total: { value: "0", held_microusd: "0", unresolved_attempts: "0" }, rows: [], series: [] };
      vi.stubGlobal("fetch", vi.fn().mockImplementation((path: string) => path === "/api/v1/me" ? Promise.resolve(Response.json(admin)) : path === `${models}/${modelId}` ? failing(status) : Promise.resolve(Response.json(path.includes("/usage/explore?") ? none : path.startsWith("/api/v1/platform/cost-report?") ? { ...report, scope: "platform" } : { data: [], has_more: false }))));
      const records = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [`/admin/costs?tab=records&model_id=${modelId}`] }) });
      await router.load(); render(<QueryClientProvider client={records}><RouterProvider router={router} /></QueryClientProvider>);
      if (gone) expect(await screen.findByText(/isn.t available any more/)).toBeTruthy();
      else { expect(await screen.findByText("Management storage unavailable")).toBeTruthy(); expect(screen.queryByText(/isn.t available any more/)).toBeNull(); }
      records.clear(); cleanup();
    }
  });
  it("All routes › Add route: the model is searched in the dialog; only whether any model exists is read first", async () => {
    const user = userEvent.setup();
    const fetch = serve(p => p === `${models}?limit=1` ? { data: [model] } : p.startsWith("/api/v1/platform/providers?") ? { data: [provider], has_more: false } : p.startsWith(`${models}?q=sma&`) ? { data: [{ ...model, id: modelId }] } : undefined);
    const client = mount(<Deployments session={admin} />);
    await user.click(screen.getByRole("button", { name: "Add route" }));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByRole("combobox", { name: "Model" }), "sma");
    await user.click(await screen.findByRole("option", { name: "Smart model · company/smart" }));
    expect(fetch.mock.calls.map(c => String(c[0]))).toContain(`${models}?limit=1`);
    expect(fullModelLists(client)).toEqual([]); expect(fullModelFetches(fetch)).toEqual([]);
    client.clear();
  });
});

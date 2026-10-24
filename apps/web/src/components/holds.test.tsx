// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, cleanup, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { admin, auditor, member, personal, project, report, session, testClient } from "../lib/test-fixtures";
import type { HoldRow, HoldsPage } from "../lib/holds";

const ROOT = "11111111-1111-4111-8111-111111111111", EXEC = "22222222-2222-4222-8222-222222222222";
const row = (patch: Partial<HoldRow> = {}): HoldRow => ({ execution_id: EXEC, root_request_id: ROOT, workspace: { id: "team", name: "Product", kind: "team" }, public_model: "company/smart", provider: "openai", workload_kind: "generation", unknown_since: "2026-10-01T00:00:00Z", age_hours: 30, held_microusd: "9007199254740993", unbounded_cost: false, unresolved_reason: "missing_billing_usage", can_reconcile: false, ...patch });
const empty = { count: "0", known_floor_microusd: null, unbounded_count: "0", oldest_age_hours: null };
const page = (patch: Partial<HoldsPage> = {}): HoldsPage => ({ observed_at: "2026-10-02T06:00:00Z", data: [row()], totals: { count: "1", known_floor_microusd: "9007199254740993", unbounded_count: "0", oldest_age_hours: 30 }, personal: null, has_more: false, next_cursor: null, ...patch });
const detail = { root_request_id: ROOT, workspace_id: "team", attempts: [{ attempt_number: 1, execution_id: EXEC, state: "failed", input_tokens: "9007199254740993", output_tokens: "2", billing_usage: null, meter_usage: null, cost_microusd: null, held_microusd: "9007199254740993", accounting_state: "unknown", unresolved_reason: "incomplete_usage", price_id: "p1", pricing_version: 2 }] };

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

async function mount(href: string, client = testClient()) { const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) }); await router.load(); const ui = render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>); return { router, client, ...ui }; }
type Handler = (path: string, init?: RequestInit) => unknown | Response | Promise<Response>;
function serve(me: object | (() => object), handler: Handler) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") return Promise.resolve(Response.json(typeof me === "function" ? me() : me));
    const body = handler(path, init);
    if (body instanceof Promise) return body;
    if (body instanceof Response) return Promise.resolve(body);
    return Promise.resolve(body === undefined ? Response.json({ data: [], has_more: false }) : Response.json(body));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
/** The holds table once its rows (not the loading skeleton) are shown. */
async function loadedTable(rows = 1) { const table = await screen.findByRole("table", { name: "Aged unknown holds" }); await waitFor(() => expect(within(table).getAllByRole("row").length).toBeGreaterThan(rows)); return table; }
const urls = (fetch: ReturnType<typeof serve>) => fetch.mock.calls.map(c => String(c[0]));
const asMember = { ...session, workspaces: [personal, member] };
const notFound = () => Response.json({ error: { code: "404", message: "Not found" } }, { status: 404 });

describe("Usage & costs › Holds tab", () => {
  it("adds a Holds tab on Admin and on the workspace page, and the tab sets ?tab=holds", async () => {
    const user = userEvent.setup();
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? page() : p.startsWith("/api/v1/platform/cost-report?") ? { ...report, scope: "platform" } : p.includes("/usage/explore?") ? { rows: [], series: [] } : undefined);
    const { router, client } = await mount("/admin/costs?tab=records");
    await user.click(await screen.findByRole("tab", { name: "Holds" }));
    await waitFor(() => expect(router.state.location.search.tab).toBe("holds"));
    expect(screen.getByRole("tab", { name: "Holds" }).getAttribute("aria-selected")).toBe("true");
    await loadedTable();
    client.clear(); cleanup();
    serve(session, p => p.startsWith("/api/v1/workspaces/team/holds") ? page() : p.includes("/usage/explore?") ? { rows: [], series: [] } : undefined);
    const again = await mount("/workspaces/team/costs?tab=records");
    await user.click(await screen.findByRole("tab", { name: "Holds" }));
    await waitFor(() => expect(again.router.state.location.search.tab).toBe("holds"));
    await loadedTable();
    again.client.clear();
  });

  it("shows exact floors, the unbounded badge, reasons and the not-an-upper-bound copy for a workspace admin", async () => {
    const fetch = serve(session, p => p.startsWith("/api/v1/workspaces/team/holds") ? page({ data: [row(), row({ execution_id: "e2", root_request_id: "r2", held_microusd: null, unbounded_cost: true, unresolved_reason: "unbounded_cost_or_unknown_rate", age_hours: 200 })], totals: { count: "2", known_floor_microusd: "9007199254740993", unbounded_count: "1", oldest_age_hours: 200 } }) : undefined);
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    const table = await loadedTable(2);
    expect(urls(fetch).filter(u => u.includes("/holds"))).toEqual(["/api/v1/workspaces/team/holds?limit=50"]);
    expect(within(table).getAllByRole("columnheader").map(h => h.textContent)).toEqual(expect.arrayContaining(["Unknown for", "Floor", "Reason", "Model", "Request"]));
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(2);
    expect(rows[0]!.textContent).toContain("1 day 6 hours"); expect(rows[0]!.textContent).toContain("At least $9,007,199,254.740993");
    expect(rows[0]!.textContent).toContain("company/smart"); expect(rows[0]!.textContent).toContain("openai"); expect(rows[0]!.textContent).toContain("Text");
    expect(within(rows[0]!).queryByText("Unbounded")).toBeNull();
    expect(rows[1]!.textContent).toContain("8 days 8 hours"); expect(rows[1]!.textContent).toContain("No floor known"); expect(within(rows[1]!).getByText("Unbounded")).toBeTruthy();
    expect(rows[1]!.textContent).not.toMatch(/\$0(?![.,\d])/);
    expect(within(rows[0]!).getByRole("link", { name: /Request 11111111/ }).getAttribute("href")).toBe(`/workspaces/team/requests/${ROOT}`);
    const text = document.body.textContent!;
    expect(text).toContain("not an upper bound"); expect(text).toContain("Nothing is refunded"); expect(text).toContain("Everyone in Product");
    expect(screen.queryByRole("button", { name: /Reconcile/ })).toBeNull();
    // Totals: exact decimal strings and counts, never summed with spend.
    expect(text).toContain("Known floor (at least)"); expect(within(screen.getByRole("group", { name: "Aged unknown hold totals" })).getByText("$9,007,199,254.740993")).toBeTruthy(); expect(text).toContain("8 days 8 hours");
    client.clear();
  });

  it("tells a member they see only their own holds and a personal owner that these are theirs", async () => {
    serve(asMember, p => p.startsWith("/api/v1/workspaces/team/holds") ? page() : undefined);
    const first = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    expect(document.body.textContent).toContain("Only holds from your own keys in Product");
    expect(document.body.textContent).not.toContain("Everyone in Product");
    expect(screen.queryByRole("button", { name: /Reconcile/ })).toBeNull();
    first.client.clear(); cleanup();
    serve(session, p => p.startsWith("/api/v1/workspaces/personal/holds") ? page({ data: [row({ workspace: { id: "personal", name: "Personal", kind: "personal" } })] }) : undefined);
    const second = await mount("/workspaces/personal/costs?tab=holds");
    const table = await loadedTable();
    expect(document.body.textContent).toContain("Your holds");
    expect(within(table).getByRole("link", { name: /Request 11111111/ }).getAttribute("href")).toBe(`/workspaces/personal/requests/${ROOT}`);
    second.client.clear();
  });

  it("on Admin lists Team/Project rows with a workspace column and one identity-free personal aggregate with no drill-down", async () => {
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? page({ personal: { count: "3", known_floor_microusd: "4200", unbounded_count: "1", oldest_age_hours: 400 } }) : undefined);
    const { client } = await mount("/admin/costs?tab=holds");
    const table = await loadedTable();
    expect(within(table).getAllByRole("columnheader").map(h => h.textContent)).toContain("Workspace");
    expect(within(table).getByText("Product")).toBeTruthy();
    expect(within(table).getByRole("link", { name: /Request 11111111/ }).getAttribute("href")).toBe(`/admin/logs/${ROOT}`);
    const aggregate = screen.getByRole("group", { name: "Personal workspaces, combined" });
    expect(aggregate.textContent).toContain("3"); expect(aggregate.textContent).toContain("At least $0.0042"); expect(aggregate.textContent).toContain("1 unbounded"); expect(aggregate.textContent).toContain("16 days 16 hours");
    expect(aggregate.textContent).toContain("no workspace, owner or request is shown");
    expect(within(aggregate).queryAllByRole("link")).toHaveLength(0); expect(within(aggregate).queryAllByRole("button")).toHaveLength(0);
    client.clear();
  });

  it("shows no personal card on a workspace route and an empty personal aggregate plainly", async () => {
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? page({ data: [], totals: empty, personal: empty }) : undefined);
    const { client } = await mount("/admin/costs?tab=holds");
    await screen.findByText("No aged unknown holds");
    expect(screen.queryByRole("table", { name: "Aged unknown holds" })).toBeNull();
    expect(screen.getByRole("group", { name: "Personal workspaces, combined" }).textContent).toContain("None");
    client.clear();
  });

  it("pages by keyset cursor with Load more, appends rows and stops at the end", async () => {
    const user = userEvent.setup(), cursor = "1790000000000000_33333333-3333-4333-8333-333333333333";
    const fetch = serve(session, p => { if (!p.startsWith("/api/v1/workspaces/team/holds")) return undefined; return p.includes("cursor=") ? page({ data: [row({ execution_id: "e9", root_request_id: "r9", age_hours: 90 })], has_more: false, next_cursor: null, totals: { count: "2", known_floor_microusd: "10", unbounded_count: "0", oldest_age_hours: 90 } }) : page({ has_more: true, next_cursor: cursor, totals: { count: "2", known_floor_microusd: "10", unbounded_count: "0", oldest_age_hours: 90 } }); });
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    const table = await loadedTable();
    expect(within(table).getAllByRole("row")).toHaveLength(2);
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(within(screen.getByRole("table", { name: "Aged unknown holds" })).getAllByRole("row")).toHaveLength(3));
    expect(urls(fetch).filter(u => u.includes("/holds"))).toEqual(["/api/v1/workspaces/team/holds?limit=50", `/api/v1/workspaces/team/holds?limit=50&cursor=${cursor}`]);
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    client.clear();
  });

  it("says not available on an older gateway (404) instead of an empty queue or a crash", async () => {
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? notFound() : undefined);
    const { client } = await mount("/admin/costs?tab=holds");
    await screen.findByText("Holds review isn't available");
    expect(document.body.textContent).toContain("older version");
    expect(screen.queryByRole("table", { name: "Aged unknown holds" })).toBeNull(); expect(screen.queryByText("No aged unknown holds")).toBeNull();
    client.clear();
  });

  it("surfaces other failures with a retry, not as an empty queue", async () => {
    serve(session, p => p.startsWith("/api/v1/workspaces/team/holds") ? Response.json({ error: { code: "503", message: "The hold queue took too long; try again later" } }, { status: 503 }) : undefined);
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    await screen.findByText(/took too long/);
    expect(screen.queryByText("No aged unknown holds")).toBeNull(); expect(screen.queryByText("Holds review isn't available")).toBeNull();
    client.clear();
  });

  it("offers Reconcile only on can_reconcile rows, never to an Auditor", async () => {
    const rows = [row({ can_reconcile: true }), row({ execution_id: "e2", root_request_id: "r2", can_reconcile: false })];
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? page({ data: rows, totals: { count: "2", known_floor_microusd: "1", unbounded_count: "0", oldest_age_hours: 30 } }) : undefined);
    const first = await mount("/admin/costs?tab=holds");
    await loadedTable();
    expect(screen.getAllByRole("button", { name: /^Reconcile/ })).toHaveLength(1);
    first.client.clear(); cleanup();
    serve(auditor, p => p.startsWith("/api/v1/platform/holds") ? page({ data: rows, totals: { count: "2", known_floor_microusd: "1", unbounded_count: "0", oldest_age_hours: 30 } }) : undefined);
    const second = await mount("/admin/costs?tab=holds");
    await loadedTable();
    expect(screen.queryByRole("button", { name: /^Reconcile/ })).toBeNull();
    second.client.clear();
  });

  it("opens the existing reconcile dialog with the pinned counts and posts to the existing reconcile endpoint", async () => {
    const user = userEvent.setup();
    let reconciled = false;
    const fetch = serve(admin, (p, init) => {
      if (p.startsWith("/api/v1/platform/holds")) return reconciled ? page({ data: [], totals: empty }) : page({ data: [row({ can_reconcile: true })] });
      if (p === `/api/v1/platform/logs/requests/${ROOT}`) return detail;
      if (p === `/api/v1/workspaces/team/costs/${EXEC}/reconcile` && init?.method === "POST") { reconciled = true; return { ok: true }; }
      return undefined;
    });
    const { client } = await mount("/admin/costs?tab=holds");
    await loadedTable();
    await user.click(screen.getByRole("button", { name: /^Reconcile/ }));
    const dialog = await screen.findByRole("dialog", { name: "Reconcile pinned usage" });
    // The pinned original observation is preserved; the same fields as the cost-records flow.
    expect((within(dialog).getByLabelText(/Original raw input tokens/) as HTMLInputElement).value).toBe("9007199254740993");
    expect(dialog.textContent).toContain("Unknown rates are not free");
    await user.type(within(dialog).getByLabelText(/Authoritative evidence reference/), "provider-usage-ref-1");
    await user.click(within(dialog).getByRole("button", { name: "Reconcile usage" }));
    await waitFor(() => expect(reconciled).toBe(true));
    const post = fetch.mock.calls.find(c => String(c[0]).endsWith("/reconcile"))!;
    expect(JSON.parse(String((post[1] as RequestInit).body))).toMatchObject({ input_tokens: "9007199254740993", output_tokens: "2", evidence: "provider-usage-ref-1" });
    // The row leaves the queue once the hold settles.
    await screen.findByText("No aged unknown holds");
    client.clear();
  });

  it("does not open a dialog when the request's attempt cannot be found", async () => {
    const user = userEvent.setup();
    serve(admin, p => p.startsWith("/api/v1/platform/holds") ? page({ data: [row({ can_reconcile: true })] }) : p === `/api/v1/platform/logs/requests/${ROOT}` ? { ...detail, attempts: [] } : undefined);
    const { client } = await mount("/admin/costs?tab=holds");
    await loadedTable();
    await user.click(screen.getByRole("button", { name: /^Reconcile/ }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /^Reconcile/ })?.hasAttribute("disabled")).toBeFalsy());
    expect(screen.queryByRole("dialog", { name: "Reconcile pinned usage" })).toBeNull();
    client.clear();
  });

  it("does not fetch period or filter data for the Holds tab's own list and hides the period filter bar", async () => {
    const fetch = serve(session, p => p.startsWith("/api/v1/workspaces/team/holds") ? page() : undefined);
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    expect(screen.queryByRole("combobox", { name: /Period/ })).toBeNull();
    expect(urls(fetch).filter(u => u.includes("/holds")).every(u => !u.includes("start_date"))).toBe(true);
    expect(document.body.textContent).toContain("aren't filtered by period");
    client.clear();
  });

  // The filter bar is hidden on this tab, so nothing may populate it: no model, key, service-account, member, cost-center or name lookups.
  const filterLookups = /\/models|\/keys|\/service-accounts|\/usage\/|\/cost-centers|\/cost-report|\/policy|\/costs\?/;
  it.each([
    ["a workspace admin", session, "/workspaces/team/costs?tab=holds&key_id=k1&model_id=m1&service_account_id=s1", "/api/v1/workspaces/team/holds"],
    ["Admin", admin, "/admin/costs?tab=holds&model_id=m1&workspace_id=w1", "/api/v1/platform/holds"],
  ])("fetches nothing for the filter bar on the Holds tab (%s)", async (_who, me, href, holdsUrl) => {
    const fetch = serve(me, p => p.startsWith(holdsUrl) ? page() : undefined);
    const { client } = await mount(href);
    await loadedTable();
    await new Promise(resolve => setTimeout(resolve, 400)); // past the option search debounce
    const requested = urls(fetch).filter(u => u !== "/api/v1/me");
    expect(requested.filter(u => filterLookups.test(u))).toEqual([]);
    expect(requested.filter(u => u.startsWith(holdsUrl))).toHaveLength(1);
    client.clear();
  });
});

/*
 * One persistent QueryClient across navigation and session changes, as in the app: a different workspace or authority
 * must never show the previous scope's rows, totals, cursors or reconcile controls, including a pagination response
 * that arrives late.
 */
describe("Holds cache scope", () => {
  const cursor = "1790000000000000_33333333-3333-4333-8333-333333333333";
  const rowsOf = (ws: string, tag: string) => ({ workspace: { id: ws, name: ws === "team" ? "Product" : "Research", kind: ws === "team" ? "team" as const : "project" as const }, public_model: `model-${tag}` });
  const deferred = () => { let resolve!: (r: Response) => void; const promise = new Promise<Response>(r => { resolve = r; }); return { promise, resolve }; };
  const bodyText = () => document.body.textContent ?? "";

  it("workspace A to B: A's rows, totals and cursor never appear under B, even when A's next page arrives late", async () => {
    const user = userEvent.setup(), late = deferred();
    const fetch = serve(session, p => {
      if (p.startsWith("/api/v1/workspaces/team/holds")) return p.includes("cursor=") ? late.promise : page({ data: [row({ ...rowsOf("team", "A1"), execution_id: "a1", root_request_id: "ra1" })], has_more: true, next_cursor: cursor, totals: { count: "5", known_floor_microusd: "111", unbounded_count: "2", oldest_age_hours: 50 } });
      if (p.startsWith("/api/v1/workspaces/project/holds")) return page({ data: [row({ ...rowsOf("project", "B1"), execution_id: "b1", root_request_id: "rb1" })], totals: { count: "1", known_floor_microusd: "7", unbounded_count: "0", oldest_age_hours: 26 } });
      return undefined;
    });
    const { router, client } = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    expect(bodyText()).toContain("model-A1");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(urls(fetch).some(u => u.includes(`cursor=${cursor}`))).toBe(true));
    await router.navigate({ to: "/workspaces/$ws/costs", params: { ws: "project" }, search: { tab: "holds" } as never });
    await waitFor(() => expect(bodyText()).toContain("model-B1"));
    expect(bodyText()).not.toContain("model-A1");
    late.resolve(Response.json(page({ data: [row({ ...rowsOf("team", "A2"), execution_id: "a2", root_request_id: "ra2" })], totals: { count: "5", known_floor_microusd: "111", unbounded_count: "2", oldest_age_hours: 50 } })));
    await new Promise(resolve => setTimeout(resolve, 100));
    const text = bodyText();
    for (const leaked of ["model-A1", "model-A2", "Product"]) expect(text, leaked).not.toContain(leaked);
    expect(within(screen.getByRole("table", { name: "Aged unknown holds" })).getAllByRole("row")).toHaveLength(2);
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    // B only ever asked for its own first page: A's cursor was never reused.
    expect(urls(fetch).filter(u => u.startsWith("/api/v1/workspaces/project/holds"))).toEqual(["/api/v1/workspaces/project/holds?limit=50"]);
    client.clear();
  });

  it("admin to member: the previous authority's rows, totals and Reconcile controls are gone while the new answer is pending", async () => {
    let me: object = admin;
    const pending = deferred();
    serve(() => me, p => {
      if (!p.startsWith("/api/v1/workspaces/team/holds")) return undefined;
      return me === admin ? page({ data: [row({ ...rowsOf("team", "ADMIN"), can_reconcile: true })], totals: { count: "9", known_floor_microusd: "999", unbounded_count: "0", oldest_age_hours: 80 } }) : pending.promise;
    });
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    expect(screen.getAllByRole("button", { name: /^Reconcile/ })).toHaveLength(1);
    me = { ...session, workspaces: [personal, member] };
    await client.invalidateQueries({ queryKey: ["session"] });
    await waitFor(() => expect(document.body.textContent).toContain("Only holds from your own keys in Product"));
    expect(screen.queryByRole("button", { name: /^Reconcile/ })).toBeNull();
    expect(bodyText()).not.toContain("model-ADMIN"); expect(bodyText()).not.toContain("999");
    pending.resolve(Response.json(page({ data: [row({ ...rowsOf("team", "OWN"), can_reconcile: false })], totals: { count: "1", known_floor_microusd: "5", unbounded_count: "0", oldest_age_hours: 30 } })));
    await waitFor(() => expect(bodyText()).toContain("model-OWN"));
    expect(bodyText()).not.toContain("model-ADMIN"); expect(screen.queryByRole("button", { name: /^Reconcile/ })).toBeNull();
    client.clear();
  });

  it("revoked access: when the gateway refuses a refresh and the session loses the workspace, the cached rows, totals and Reconcile controls are gone", async () => {
    let revoked = false;
    serve(() => revoked ? { ...session, workspaces: [personal], capabilities: { ...session.capabilities } } : admin, p => {
      if (!p.startsWith("/api/v1/workspaces/team/holds")) return undefined;
      return revoked ? Response.json({ error: { code: "403", message: "Forbidden" } }, { status: 403 }) : page({ data: [row({ ...rowsOf("team", "SECRET"), can_reconcile: true })], totals: { count: "7", known_floor_microusd: "777", unbounded_count: "0", oldest_age_hours: 80 } });
    });
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    expect(screen.getAllByRole("button", { name: /^Reconcile/ })).toHaveLength(1);
    revoked = true;
    await client.invalidateQueries({ queryKey: ["api"] });
    await waitFor(() => expect(screen.queryByRole("table", { name: "Aged unknown holds" })).toBeNull());
    expect(bodyText()).not.toContain("model-SECRET"); expect(bodyText()).not.toContain("777");
    expect(screen.queryByRole("button", { name: /^Reconcile/ })).toBeNull(); expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    // The cache holds nothing of the revoked scope that a later render could show.
    expect(JSON.stringify(client.getQueryCache().getAll().map(q => q.state.data ?? null))).not.toContain("model-SECRET");
    client.clear();
  });

  it("a failed Load more keeps the rows already shown and reports the failure", async () => {
    const user = userEvent.setup();
    serve(session, p => !p.startsWith("/api/v1/workspaces/team/holds") ? undefined : p.includes("cursor=") ? Response.json({ error: { code: "503", message: "The hold queue took too long; try again later" } }, { status: 503 }) : page({ data: [row(rowsOf("team", "KEEP"))], has_more: true, next_cursor: cursor }));
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    await loadedTable();
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await screen.findByText(/took too long/);
    expect(bodyText()).toContain("model-KEEP");
    client.clear();
  });
});

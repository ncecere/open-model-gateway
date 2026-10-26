// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, cleanup, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
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
function serve(me: object | (() => object | Promise<Response>), handler: Handler) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") { const who = typeof me === "function" ? me() : me; return who instanceof Promise ? who : Promise.resolve(Response.json(who)); }
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

  it("names the request link's accessible name with the exact same visible shortened ID (axe label-content-name-mismatch)", async () => {
    // A heterogeneous UUID whose head and tail differ, so truncation is unambiguous and a half-fixed label can't hide behind repeated digits.
    const REALISTIC_ROOT = "a1b2c3d4-e5f6-4789-9abc-def012345678";
    serve(session, p => p.startsWith("/api/v1/workspaces/team/holds") ? page({ data: [row({ root_request_id: REALISTIC_ROOT })] }) : undefined);
    const { client } = await mount("/workspaces/team/costs?tab=holds");
    const table = await loadedTable();
    const link = within(table).getByRole("link", { name: /Request a1b2c3d4/ });
    const visibleText = link.textContent ?? "";
    expect(visibleText).toBe("a1b2c3d4…5678"); // first 8 + ellipsis + last 4, per shortId
    const accessibleName = link.getAttribute("aria-label") ?? "";
    // The accessible name must carry the exact same visible text (including the truncation tail), not just the head.
    expect(accessibleName).toContain(visibleText);
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

  // Automatic eviction (gcTime 0) must not stand in for isolation: keep every query for the whole test.
  const persistent = () => new QueryClient({ defaultOptions: { queries: { retry: false, retryOnMount: false, staleTime: Infinity, gcTime: Infinity } } });
  const totals = () => Object.fromEntries([...screen.getByRole("group", { name: "Aged unknown hold totals" }).querySelectorAll("dl")].map(dl => [dl.querySelector("dt")!.textContent, dl.querySelector("dd")!.textContent]));
  const shown = () => within(screen.getByRole("table", { name: "Aged unknown holds" })).getAllByRole("row").slice(1).map(r => /model-[A-Z0-9]+/.exec(r.textContent ?? "")?.[0]);
  const totalsOf = (count: string, floor: string | null, unbounded: string, oldest: number | null): HoldsPage["totals"] => ({ count, known_floor_microusd: floor, unbounded_count: unbounded, oldest_age_hours: oldest });
  const rowFor = (ws: string, tag: string, canReconcile = false) => row({ ...rowsOf(ws, tag), execution_id: `x-${tag}`, root_request_id: `r-${tag}`, can_reconcile: canReconcile });
  const cacheText = (client: QueryClient) => JSON.stringify(client.getQueryCache().getAll().filter(q => q.queryKey[0] !== "session").map(q => [q.queryKey, q.state.data ?? null]));
  const reconcileButtons = () => screen.queryAllByRole("button", { name: /^Reconcile/ });

  it("workspace A to B: two populated A pages and a deferred third; B's rows, exact totals, cursor and controls hold before and after the late page", async () => {
    const user = userEvent.setup(), late = deferred(), A = totalsOf("5", "111", "2", 50), B = totalsOf("3", "7", "0", 26);
    const fetch = serve(admin, p => {
      if (p.startsWith("/api/v1/workspaces/team/holds")) return p.includes("cursor=cA2") ? late.promise : p.includes("cursor=cA1") ? page({ data: [rowFor("team", "A2", true)], has_more: true, next_cursor: "cA2", totals: A }) : page({ data: [rowFor("team", "A1", true)], has_more: true, next_cursor: "cA1", totals: A });
      if (p.startsWith("/api/v1/workspaces/project/holds")) return p.includes("cursor=cB1") ? page({ data: [rowFor("project", "B2")], has_more: false, next_cursor: null, totals: B }) : page({ data: [rowFor("project", "B1")], has_more: true, next_cursor: "cB1", totals: B });
      return undefined;
    });
    const { router, client } = await mount("/workspaces/team/costs?tab=holds", persistent());
    await loadedTable();
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toEqual(["model-A1", "model-A2"]));
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(urls(fetch).some(u => u.includes("cursor=cA2"))).toBe(true)); // the third page is in flight
    expect(reconcileButtons()).toHaveLength(2);
    expect(totals()).toMatchObject({ "Aged unknown holds": "5", "Known floor (at least)": "$0.000111", "Unbounded": "2", "Oldest": "2 days 2 hours" });
    await router.navigate({ to: "/workspaces/$ws/costs", params: { ws: "project" }, search: { tab: "holds" } as never });
    // Destination, before the late page: only B, B's exact totals, B's cursor, and no Reconcile (B's row isn't reconcilable).
    await waitFor(() => expect(shown()).toEqual(["model-B1"]));
    const expectDestination = (rows: (string | undefined)[]) => {
      expect(shown()).toEqual(rows);
      expect(totals()).toMatchObject({ "Aged unknown holds": "3", "Known floor (at least)": "$0.000007", "Unbounded": "0", "Oldest": "1 day 2 hours" });
      expect(reconcileButtons()).toHaveLength(0);
      expect(bodyText()).not.toContain("Product"); expect(bodyText()).not.toMatch(/model-A\d/);
    };
    expectDestination(["model-B1"]);
    expect(screen.getByRole("button", { name: "Load more" })).toBeTruthy();
    late.resolve(Response.json(page({ data: [rowFor("team", "A3", true)], has_more: true, next_cursor: "cA3", totals: totalsOf("9", "999", "9", 99) })));
    await new Promise(resolve => setTimeout(resolve, 150));
    expectDestination(["model-B1"]);
    expect(screen.getByRole("button", { name: "Load more" })).toBeTruthy();
    // Load more continues B's own position, never A's cursors.
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toEqual(["model-B1", "model-B2"]));
    expect(urls(fetch).filter(u => u.startsWith("/api/v1/workspaces/project/holds"))).toEqual(["/api/v1/workspaces/project/holds?limit=50", "/api/v1/workspaces/project/holds?limit=50&cursor=cB1"]);
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    // B's cache entry holds none of A's data.
    const projectEntry = JSON.stringify(client.getQueryCache().getAll().filter(q => String(q.queryKey[2]).includes("/workspaces/project/holds")).map(q => q.state.data));
    expect(projectEntry).not.toMatch(/model-A\d|cA\d|"count":"5"|"count":"9"/);
    client.clear();
  });

  it("admin to member: two populated admin pages and a deferred third; the member's rows, exact totals, cursor and no Reconcile before and after the late page", async () => {
    const user = userEvent.setup(), late = deferred(), ADMIN = totalsOf("40", "999", "3", 80), OWN = totalsOf("2", "5", "0", 30);
    let me: object = admin;
    const fetch = serve(() => me, p => {
      if (!p.startsWith("/api/v1/workspaces/team/holds")) return undefined;
      if (me === admin) return p.includes("cursor=cAd2") ? late.promise : p.includes("cursor=cAd1") ? page({ data: [rowFor("team", "ADMIN2", true)], has_more: true, next_cursor: "cAd2", totals: ADMIN }) : page({ data: [rowFor("team", "ADMIN1", true)], has_more: true, next_cursor: "cAd1", totals: ADMIN });
      return p.includes("cursor=cM1") ? page({ data: [rowFor("team", "OWN2")], has_more: false, next_cursor: null, totals: OWN }) : page({ data: [rowFor("team", "OWN1")], has_more: true, next_cursor: "cM1", totals: OWN });
    });
    const { client } = await mount("/workspaces/team/costs?tab=holds", persistent());
    await loadedTable();
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toEqual(["model-ADMIN1", "model-ADMIN2"]));
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(urls(fetch).some(u => u.includes("cursor=cAd2"))).toBe(true));
    expect(reconcileButtons()).toHaveLength(2); expect(totals()).toMatchObject({ "Aged unknown holds": "40", "Known floor (at least)": "$0.000999" });
    me = { ...session, workspaces: [personal, member] };
    await client.invalidateQueries({ queryKey: ["session"] });
    const expectMember = (rows: (string | undefined)[]) => {
      expect(shown()).toEqual(rows);
      expect(totals()).toMatchObject({ "Aged unknown holds": "2", "Known floor (at least)": "$0.000005", "Unbounded": "0", "Oldest": "1 day 6 hours" });
      expect(reconcileButtons()).toHaveLength(0);
      expect(bodyText()).toContain("Only holds from your own keys in Product"); expect(bodyText()).not.toMatch(/model-ADMIN\d/);
    };
    await waitFor(() => expect(shown()).toEqual(["model-OWN1"]));
    expectMember(["model-OWN1"]);
    expect(screen.getByRole("button", { name: "Load more" })).toBeTruthy();
    late.resolve(Response.json(page({ data: [rowFor("team", "ADMIN3", true)], has_more: true, next_cursor: "cAd3", totals: totalsOf("77", "7777", "7", 700) })));
    await new Promise(resolve => setTimeout(resolve, 150));
    expectMember(["model-OWN1"]);
    expect(screen.getByRole("button", { name: "Load more" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toEqual(["model-OWN1", "model-OWN2"]));
    expectMember(["model-OWN1", "model-OWN2"]);
    expect(urls(fetch).filter(u => u.includes("cursor=")).filter(u => u.includes("cAd") && me !== admin && !["cursor=cAd1", "cursor=cAd2"].some(c => u.includes(c)))).toEqual([]);
    expect(urls(fetch).at(-1)).toBe("/api/v1/workspaces/team/holds?limit=50&cursor=cM1");
    client.clear();
  });

  it("revoked access: a refused refresh drops the cached rows and entries while /me is still pending, and nothing returns once it answers", async () => {
    const meLate = deferred(), SECRET = totalsOf("7", "777", "0", 80);
    let revoked = false, meCalls = 0;
    serve(() => { if (!revoked) return admin; meCalls++; return meLate.promise; }, p => {
      if (!p.startsWith("/api/v1/workspaces/team/holds")) return undefined;
      if (revoked) return Response.json({ error: { code: "403", message: "Forbidden" } }, { status: 403 });
      return p.includes("cursor=cS1") ? page({ data: [rowFor("team", "SECRET2", true)], has_more: true, next_cursor: "cS2", totals: SECRET }) : page({ data: [rowFor("team", "SECRET1", true)], has_more: true, next_cursor: "cS1", totals: SECRET });
    });
    const user = userEvent.setup();
    const { client } = await mount("/workspaces/team/costs?tab=holds", persistent());
    await loadedTable();
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toEqual(["model-SECRET1", "model-SECRET2"]));
    expect(reconcileButtons()).toHaveLength(2); expect(cacheText(client)).toContain("model-SECRET1");
    revoked = true;
    await client.invalidateQueries({ queryKey: ["api"] });
    // The 403 alone (re-authorization still pending): no rows, totals, cursor or controls on screen, nothing of them cached.
    await waitFor(() => expect(meCalls).toBeGreaterThan(0));
    await waitFor(() => expect(screen.queryByRole("table", { name: "Aged unknown holds" })).toBeNull());
    for (const secret of ["model-SECRET1", "model-SECRET2", "777"]) expect(bodyText(), secret).not.toContain(secret);
    expect(reconcileButtons()).toHaveLength(0); expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    expect(cacheText(client)).not.toMatch(/model-SECRET|cS1|cS2|"count":"7"/);
    meLate.resolve(Response.json({ ...session, workspaces: [personal] }));
    await waitFor(() => expect(screen.queryByRole("tab", { name: "Holds" })).toBeNull());
    for (const secret of ["model-SECRET1", "model-SECRET2", "777"]) expect(bodyText(), secret).not.toContain(secret);
    expect(reconcileButtons()).toHaveLength(0); expect(cacheText(client)).not.toMatch(/model-SECRET|cS1|cS2/);
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

// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { ProviderReconciliations } from "../pages/usage/provider-reconciliations";
import { UsageCosts } from "../pages/usage/usage-costs";
import { DashboardNavigationProvider } from "./navigation-link";
import { dashboardSearch } from "../lib/permissions";
import { admin, auditor, member, session, testClient } from "../lib/test-fixtures";

const base = "/api/v1/platform/provider-reconciliations";
const totals = { settled_estimate_microusd: "9007199254740993", held_floor_microusd: "123", holds_upper_bound_microusd: null, attempts: "4", unresolved_attempts: "1", unbounded_holds: "1", complete: false };
const period = { start: "2026-10-01T00:00:00Z", end: "2026-10-02T00:00:00Z" };
const metadata = { id: "frozen", account_id: "account", import_id: "import", mapping_id: "mapping", observed_at: "2026-10-03T00:00:00Z" };
const imported = { id: "import", provider: "anthropic", period_start: period.start, period_end: period.end };
const personal = { estimates: totals, exclusively_attributed_imported_microusd: "-100", exclusively_attributed_imported_by_category: { credit: "-100" }, reasons: ["incomplete_estimate"] };
const snapshot = { name: "provider-reported cost reconciliation", verified_invoice: false, complete: false, import_id: "import", mapping_id: "mapping", query_version: "aggregate-attempt-start-v1", observed_at: metadata.observed_at, period, imported_microusd: "-9007199254740993", estimates: totals, personal_activity: personal, mapping_provenance: [], category_coverage: [{ category: "credits", status: "supplied" }, { category: "taxes", status: "not_supplied" }], rows: [
  { day: period.start, provider_workspace_id: null, model: "claude-example", service_tier: "standard", category: "tokens", workspace_id: "team", imported_microusd: "1000000", variance_microusd: null, workspace_estimates: [{ workspace_id: "team", totals }], unresolved_coverage: totals, reasons: ["incomplete_estimate", "timing_basis"] },
  { day: period.start, provider_workspace_id: "provider-shared", model: "claude-example", service_tier: "standard", category: "tokens", workspace_id: null, imported_microusd: "1000000", variance_microusd: null, workspace_estimates: [{ workspace_id: "team", totals }], unresolved_coverage: totals, reasons: ["ambiguous_scope", "shared_unallocated"] },
  { day: period.start, workspace_id: "team", model: "estimate-model", service_tier: null, imported_microusd: null, variance_microusd: null, estimate: totals, reasons: ["estimate_only"] },
  { day: period.start, provider_workspace_id: "exclusive", model: "matched-model", service_tier: "standard", category: "tokens", workspace_id: "team", imported_microusd: "1000000", variance_microusd: "-1", workspace_estimates: [{ workspace_id: "team", totals: { ...totals, complete: true } }], reasons: ["timing_basis"] },
  ...["credit", "discount", "web_search", "unknown_fee"].map(category => ({ day: period.start, provider_workspace_id: null, model: null, service_tier: null, category, workspace_id: null, imported_microusd: "-2", variance_microusd: null, workspace_estimates: [], reasons: ["provider_only", "unsupported_category"] })),
] };
function serve(options: { denied?: boolean; empty?: boolean; unsafePersonal?: boolean } = {}) {
  const fetch = vi.fn().mockImplementation((url: string) => {
    if (options.denied) return Promise.resolve(Response.json({ error: { message: "Not authorized" } }, { status: 403 }));
    if (url === `${base}/frozen`) return Promise.resolve(Response.json({ ...metadata, snapshot: options.unsafePersonal ? { ...snapshot, personal_activity: { ...personal, owner_user_id: "private-owner" } } : snapshot }));
    if (url.startsWith("/api/v1/platform/provider-cost-imports?")) return Promise.resolve(Response.json({ data: [imported], limit: 200, offset: 0 }));
    return Promise.resolve(Response.json({ data: options.empty ? [] : [metadata], limit: 20, offset: 0 }));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
function view(viewer = admin, record?: string) {
  render(<QueryClientProvider client={testClient()}><ProviderReconciliations session={viewer} record={record} /></QueryClientProvider>);
  return userEvent.setup();
}
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
describe("provider-reported cost comparison", () => {
  it("lists frozen periods/provider, opens details and goes back using only same-origin reads", async () => {
    const fetch = serve(), user = view();
    await screen.findByRole("button", { name: "Open reconciliation frozen" });
    expect(screen.getByRole("table", { name: "Frozen reconciliations" })).toBeTruthy();
    expect(screen.getByLabelText("Provider")).toBeTruthy(); expect(screen.getByLabelText("Report period")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Open reconciliation frozen" }));
    await screen.findByRole("table", { name: "Provider comparison lines" });
    expect(screen.getByText("aggregate-attempt-start-v1")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Back to reconciliations" }));
    await screen.findByRole("table", { name: "Frozen reconciliations" });
    expect(fetch.mock.calls.every(([url, init]) => url.startsWith("/api/v1/platform/") && (!init.method || init.method === "GET"))).toBe(true);
    expect(fetch.mock.calls.some(([url]) => /\/lines|provider-cost-mappings|\/provider-cost-imports\//.test(url))).toBe(false);
  });
  it("filters loaded periods honestly and pages snapshots without unsupported API filters", async () => {
    const second = { ...imported, id: "second", period_start: "2026-09-01T00:00:00Z", period_end: "2026-09-02T00:00:00Z" };
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(Response.json({ data: url.includes("provider-cost-imports?") ? [imported, second] : url.endsWith("offset=20") ? [] : Array.from({ length: 20 }, (_, i) => ({ ...metadata, id: `frozen-${i}`, import_id: i % 2 ? "second" : "import" })) })));
    vi.stubGlobal("fetch", fetch); const user = view();
    await screen.findByRole("button", { name: "Open reconciliation frozen-0" });
    await user.selectOptions(screen.getByLabelText("Report period"), `${period.start}/${period.end}`);
    expect(within(screen.getByRole("table", { name: "Frozen reconciliations" })).getAllByRole("row")).toHaveLength(11);
    expect(screen.getByText(/Period filters apply to loaded snapshots only/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Load more reconciliations" }));
    await screen.findByRole("table", { name: "Frozen reconciliations" });
    expect(fetch.mock.calls.some(([url]) => url === `${base}?limit=20&offset=20`)).toBe(true);
    expect(fetch.mock.calls.every(([url]) => !url.includes("period=") && !url.includes("provider="))).toBe(true);
  });
  it.each([admin, auditor])("is read-only for $user.platform_role, with honest signed amounts and null variance", async viewer => {
    serve(); const user = view(viewer, "frozen");
    const table = await screen.findByRole("table", { name: "Provider comparison lines" });
    expect(screen.getByText("Read-only")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /upload|create|freeze|map|delete|reconcile|settle|release/i })).toBeNull();
    expect(screen.getByText("-$9,007,199,254.740993")).toBeTruthy();
    const rows = within(table).getAllByRole("row");
    expect(rows[1].textContent).toContain("Incomplete");
    expect(rows[2].textContent).toContain("Ambiguous scope"); expect(rows[2].textContent).toContain("Shared / unallocated");
    expect(within(rows[2]).getAllByRole("cell")[5].textContent).toBe("Unknown");
    expect(within(rows[2]).queryByText("$0.00", { exact: true })).toBeNull();
    expect(rows[3].textContent).toContain("Not supplied"); expect(rows[3].textContent).toContain("Estimate only");
    expect(rows[4].textContent).toContain("Matched"); expect(rows[4].textContent).toContain("-$0.000001");
    for (const category of ["Credit", "Discount", "Web search", "unknown_fee"]) expect(within(table).getByText(category)).toBeTruthy();
    await user.click(screen.getByRole("tab", { name: "Estimates & holds" }));
    expect(screen.getByRole("region", { name: "Holds, separate from settled estimates" }).textContent).toContain("$0.000123");
    await user.click(screen.getByRole("tab", { name: "Unresolved coverage" }));
    expect(screen.getByText(/Do not add unresolved coverage/)).toBeTruthy();
    expect(screen.getByText(/Not a verified invoice/)).toBeTruthy();
  });
  it("filters model/workspace display without allocating a shared actual bucket", async () => {
    serve(); const user = view(auditor, "frozen");
    await screen.findByRole("table", { name: "Provider comparison lines" });
    await user.selectOptions(screen.getByLabelText("Historical model"), "claude-example");
    await user.selectOptions(screen.getByLabelText("OMG workspace"), "team");
    const table = screen.getByRole("table", { name: "Provider comparison lines" });
    expect(within(table).getAllByRole("row")).toHaveLength(3);
    const ambiguous = within(table).getAllByRole("row")[2];
    expect(ambiguous.textContent).toContain("Shared / unallocated");
    expect(within(ambiguous).getAllByRole("cell")[5].textContent).toBe("Unknown");
    expect(screen.getByText("-$9,007,199,254.740993")).toBeTruthy();
    expect(screen.getByText(/Filters change displayed lines only/)).toBeTruthy();
  });
  it("combines personal totals without identities, models or drilldowns", async () => {
    // This is the frozen backend projection (matching.rs), not a workspace directory object.
    expect(Object.keys(personal).sort()).toEqual(["estimates", "exclusively_attributed_imported_by_category", "exclusively_attributed_imported_microusd", "reasons"]);
    expect(JSON.stringify(personal)).not.toMatch(/owner|workspace_id|model|key_id|request|actor|email/);
    serve(); const user = view(auditor, "frozen");
    await screen.findByRole("tab", { name: "Personal totals" });
    await user.click(screen.getByRole("tab", { name: "Personal totals" }));
    const region = await screen.findByRole("region", { name: "Personal workspaces, combined totals only" });
    expect(region.textContent).toContain("-$0.0001"); expect(region.textContent).not.toMatch(/claude|team|me@example/);
    expect(within(region).queryByRole("link")).toBeNull(); expect(within(region).queryByRole("button")).toBeNull();
  });
  it("refuses an unexpected identity-bearing personal API projection instead of rendering it", async () => {
    serve({ unsafePersonal: true }); view(admin, "frozen");
    await screen.findByText("Personal activity must contain identity-free totals only.");
    expect(screen.queryByText("private-owner")).toBeNull(); expect(screen.queryByRole("table", { name: "Provider comparison lines" })).toBeNull();
  });
  it("denies ordinary users before fetching, even with a direct detail selection", () => {
    const fetch = serve(); view(session, "frozen");
    expect(screen.getByText("Access denied")).toBeTruthy(); expect(fetch).not.toHaveBeenCalled();
  });
  it("shows empty metadata without invented financial totals", async () => {
    serve({ empty: true }); view(); await screen.findByText("No frozen reconciliations");
    expect(screen.queryByText("$0.00")).toBeNull();
  });
  it("opens the URL-backed integration tab without fetching unrelated usage/filter options", async () => {
    const fetch = serve();
    const search = dashboardSearch({ page: "platform-costs", tab: "provider-reconciliation", record: "frozen" });
    expect(search.tab).toBe("provider-reconciliation");
    render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={search} navigate={() => {}}><UsageCosts session={auditor} /></DashboardNavigationProvider></QueryClientProvider>);
    await screen.findByRole("table", { name: "Provider comparison lines" });
    expect(screen.getByRole("tab", { name: "Provider reconciliation" }).getAttribute("aria-selected")).toBe("true");
    expect(fetch.mock.calls.map(([url]) => url)).toEqual([`${base}/frozen`]);
  });
  it.each([{ viewer: session, workspace: undefined }, { viewer: admin, workspace: member }])("never offers or fetches the comparison from an unauthorized portal", ({ viewer, workspace }) => {
    const fetch = serve();
    render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={{ page: workspace ? "costs" : "platform-costs", tab: "provider-reconciliation" }} navigate={() => {}}><UsageCosts session={viewer} workspace={workspace} /></DashboardNavigationProvider></QueryClientProvider>);
    expect(screen.queryByRole("tab", { name: "Provider reconciliation" })).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
    expect(screen.queryByRole("table", { name: "Frozen reconciliations" })).toBeNull();
  });
  it("hides cached financial figures after a live read refusal", async () => {
    const fetch = serve(), client = testClient();
    render(<QueryClientProvider client={client}><ProviderReconciliations session={auditor} record="frozen" /></QueryClientProvider>);
    await screen.findByText("-$9,007,199,254.740993");
    fetch.mockImplementation(() => Promise.resolve(Response.json({ error: { message: "Authority revoked" } }, { status: 403 })));
    await act(async () => { await client.invalidateQueries(); });
    await screen.findByText("Authority revoked");
    expect(screen.queryByText("-$9,007,199,254.740993")).toBeNull();
    expect(screen.queryByRole("table", { name: "Provider comparison lines" })).toBeNull();
  });
  it("shows backend refusal, never stale comparison figures", async () => {
    serve({ denied: true }); view(auditor, "frozen"); await screen.findByText("Not authorized");
    expect(screen.queryByRole("table", { name: "Provider comparison lines" })).toBeNull(); expect(screen.queryByText("$0.00")).toBeNull();
  });
});

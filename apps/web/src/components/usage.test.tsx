// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, cleanup, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { session, admin, team, member, personal, report, markup, testClient } from "../lib/test-fixtures";
import { Costs } from "../pages/usage/usage-costs";
import { UsageOverviewTab, showsAccounting } from "../pages/usage/overview";
import { UsageExploreTab } from "../pages/usage/explore";
import { WorkspaceRecords } from "../pages/usage/records";
import { AccountingReport } from "../pages/usage/accounting";
import { dimensionPhrase, previousHasData, tileDelta, usageContext, usagePeriod, type ExploreResponse, type UsageOverview } from "../lib/usage";
import type { DashboardSearch } from "../lib/permissions";

const now = new Date("2026-10-08T12:00:00Z"), period = usagePeriod({}, now), q = "start_date=2026-10-01&end_date=2026-10-09";
const tile = (value: string | null, previous: string | null, daily: [string, string | null][] = [], change_ratio: string | null = null) => ({ value, previous, delta: null, change_ratio, daily: daily.map(([date, v]) => ({ date, value: v })) });
const days: [string, string | null][] = [["2026-10-07", "100"], ["2026-10-08", "2623"]];
const overview = (members: UsageOverview["top"]["members"]): UsageOverview => ({ period: { start_date: "2026-10-01", end_date: "2026-10-09" }, previous_period: { start_date: "2026-09-23", end_date: "2026-10-01" }, observed_at: "2026-10-08T12:00:00Z",
  tiles: { spend: { ...tile("2723", "1000", days), held_microusd: "7600", unresolved_attempts: "5" }, requests: { ...tile("21", "10", [["2026-10-07", "1"], ["2026-10-08", "20"]]), attempts: "23" }, tokens: { ...tile("5000", "4000", days), input_tokens: "3000", output_tokens: "2000", unknown_token_attempts: "2" }, cache_hit_rate: tile("0.25", "0.2", days.map(([d]) => [d, "0.25"]), "0.25"), blended_microusd_per_million: tile(null, null) },
  top: { models: [{ id: "gpt-6-luna", name: "gpt-6-luna", spend_microusd: "2723", requests: "21", tokens: "5000", share: "1", blended_microusd_per_million: "544.6", model_id: "m-luna" }], keys: [{ id: "k", name: "Laptop key", spend_microusd: "81", requests: "1", tokens: "10", share: "0.0297" }], members } });
const explore: ExploreResponse = { metric: "spend", group_by: "model", then_by: null, period: { start_date: "2026-10-02", end_date: "2026-10-09" }, total: { value: "9007199254740996", held_microusd: "0", unresolved_attempts: "0" }, rows: [{ group: { id: "big", name: "big-model" }, then: null, value: "9007199254740993", share: "1", held_microusd: "0", unresolved_attempts: "0" }, { group: { id: "small", name: "small-model" }, then: null, value: "3", share: "0", held_microusd: "0", unresolved_attempts: "0" }], other: null, truncated: false, series: ["2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05", "2026-10-06", "2026-10-07", "2026-10-08"].map((date, i) => ({ date, values: [{ id: "big", value: i === 0 ? "9007199254740993" : "0" }, { id: "small", value: i === 6 ? "3" : "0" }] })) };
const nav = (search: DashboardSearch = { page: "costs", ws: "team" }) => ({ search, navigate: vi.fn() });
const paths = (client: ReturnType<typeof testClient>) => client.getQueryCache().getAll().map(x => String(x.queryKey[2]));

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("Usage & costs overview", () => {
  it("shows three exact sub-cent tiles with deltas, the on-hold amount on Spend (no banner) and a link to unresolved records", () => {
    const html = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, overview([{ id: "u1", name: "alex@example.invalid", spend_microusd: "2723", requests: "21", tokens: "5000", share: "1" }])], ["/api/v1/workspaces/team/policy", { budgets: [{ layer: "local", period: "month", amount_microusd: "100000000", usage_visible: true, used_microusd: "10323", unresolved_usage: true, window_start: "2026-10-01T00:00:00Z", window_end: "2026-11-01T00:00:00Z" }] }]]);
    for (const text of ["$0.0027", 'title="Exactly $0.002723"', "+$0.0076 on hold", "including 2 retries", "At least 5,000", "+172.3%", "+110%", "+25%", "vs previous 8 days", "Top members", "alex@example.invalid", "Laptop key", "$0.000081", "Budgets", "Workspace monthly budget", "$0.01 / $100.00 · Monthly", "Resets Nov 1, 2026", ">Accounting</h2>", "View all in Explore", "Estimates from configured prices, not invoices"]) expect(html).toContain(text);
    // The on-hold explanation is the Spend tile's tooltip (and screen-reader sentence); no "Some costs aren't final yet" banner.
    expect(html).toContain("$0.0076 is on hold for 5 requests whose final cost isn&#x27;t known yet");
    expect(html).not.toContain("Some costs aren"); expect(html).not.toContain('role="alert"');
    expect(html).toMatch(/<a [^>]*href="\/workspaces\/team\/costs\?tab=records&amp;cost_status=on_hold"[^>]*>View unresolved<\/a>/);
    // Only Spend, Requests and Tokens are tiles; cache hit rate and $/1M are in Explore and the Accounting card.
    expect(html.match(/role="group" aria-label="Usage summary"/g)).toHaveLength(1);
    expect(html).not.toContain("Cache hit rate</a>"); expect(html).not.toContain("Cost per 1M tokens</a>"); expect(html).not.toContain("per 1M tokens</");
    expect(html).toContain('href="/workspaces/team/costs?tab=chart&amp;metric=spend"');
    // Top lists: compact rows (name | requests | spend | share); names open the key and the model; no funnel filter button.
    expect(html).toContain('href="/workspaces/team/keys/k"'); expect(html).toContain('href="/workspaces/team/models/m-luna"');
    expect(html).not.toContain("Filter by"); expect(html).not.toContain('href="/workspaces/team/costs?key_id=k"');
    expect(html).not.toMatch(/\$0(?![.,\d])/);
    expect(html).not.toContain("Updated "); expect(html).not.toContain("New");
  });
  it("renders top lists as one-line table rows, top 5 with a link to Explore", () => {
    const o = overview(null); o.top.keys = Array.from({ length: 7 }, (_, i) => ({ id: `k${i}`, name: `Key ${i}`, spend_microusd: String(700 - i * 100), requests: String(i + 1), tokens: "1", share: null }));
    document.body.innerHTML = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, o]]);
    const table = screen.getByRole("table", { name: "Top API keys" });
    expect(within(table).getAllByRole("columnheader").map(h => h.textContent)).toEqual(["API key", "Spend"]); expect(table.textContent).toContain("1 request"); expect(table.textContent).toContain("2 requests");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(5);
    expect(within(rows[0]!).getByRole("rowheader").textContent).toContain("Key 01 request");
    expect(within(rows[0]!).getByRole("link", { name: "Key 0" }).getAttribute("href")).toBe("/workspaces/team/keys/k0");
    expect(rows[0]!.textContent).toContain("$0.0007"); expect(rows[1]!.textContent).toContain("2");
    expect(screen.queryByText("Key 5")).toBeNull();
    const all = screen.getAllByRole("link", { name: "View all in Explore" }).map(a => a.getAttribute("href"));
    expect(all).toContain("/workspaces/team/costs?tab=explore&group=key");
    document.body.innerHTML = "";
  });
  it("shows a tile delta only when the previous period has data (no \"New\", no \"No comparison\")", () => {
    const o = overview(null);
    o.tiles.spend = { ...o.tiles.spend, previous: "0" }; o.tiles.requests = { ...o.tiles.requests, previous: null }; o.tiles.tokens = { ...o.tiles.tokens, previous: "4000" };
    const html = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, o]]);
    expect(html).not.toContain("New"); expect(html).not.toContain("No comparison"); expect(html).not.toContain("+172.3%");
    expect(html.match(/vs previous 8 days/g)).toHaveLength(1); expect(html).toContain("+25%");
    expect(tileDelta("5", "0", "bad", 8)).toBeUndefined(); expect(tileDelta("5", null, "bad", 8)).toBeUndefined(); expect(tileDelta(null, "5", "bad", 8)).toBeUndefined();
    expect(tileDelta("5", "4", "bad", 8)).toEqual({ current: "5", previous: "4", increaseIs: "bad", label: "vs previous 8 days" });
    expect(previousHasData("9007199254740993")).toBe(true); expect(previousHasData("0")).toBe(false); expect(previousHasData("1.5")).toBe(false);
  });
  it("replaces the daily chart with a one-line note when only one day has data, and omits budgets when none are set", () => {
    const o = overview(null), one = (daily: { date: string; value: string | null }[]) => daily.map(d => ({ ...d, value: d.date === "2026-10-08" ? d.value : "0" }));
    o.tiles.spend.daily = one(o.tiles.spend.daily); o.tiles.requests.daily = one(o.tiles.requests.daily); o.tiles.tokens.daily = one(o.tiles.tokens.daily);
    const html = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, o], ["/api/v1/workspaces/team/policy", { budgets: [] }]]);
    expect(html).not.toContain("All activity in this period is on one UTC day"); expect(html).not.toContain("Daily spend"); expect(html).not.toContain("Chart measure");
    expect(html).not.toContain("Budgets"); expect(html).not.toContain("No budget is set");
    // Two days: the chart with Spend / Requests / Tokens only.
    const chart = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, overview(null)]]);
    expect(chart).toContain("Daily spend"); expect(chart).toContain("Chart measure"); expect(chart).not.toContain(">Cache hit<"); expect(chart).not.toContain(">$/1M<");
    // Admin: no installation budget configured means no card at all.
    const admin = markup(<UsageOverviewTab ctx={usageContext()} period={period} nav={nav({ page: "platform-costs" })} />, [[`/api/v1/platform/usage/overview?${q}`, { ...overview([]), installation_budgets: [] }]]);
    expect(admin).not.toContain("Installation budgets"); expect(admin).not.toContain("No installation-wide budget"); expect(admin).not.toContain("Budgets");
  });
  it("orders the page: header, pill tabs, one filter row (Period + More filters), then the tab; no separate period block", () => {
    vi.useFakeTimers({ toFake: ["Date"] }); vi.setSystemTime(now);
    const html = markup(<Costs session={session} workspace={team} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, overview(null)]]);
    const at = (needle: string) => { const i = html.indexOf(needle); expect(i, needle).toBeGreaterThan(-1); return i; };
    expect(at("<h1")).toBeLessThan(at('role="tablist"'));
    expect(at("Oct 1 – Oct 8, 2026 (UTC, today so far)")).toBeLessThan(at('role="tablist"'));
    expect(at('role="tablist"')).toBeLessThan(at(">Period<"));
    expect(at(">Period<")).toBeLessThan(at("More filters"));
    expect(at("More filters")).toBeLessThan(at('aria-label="Usage summary"'));
    expect(html).toContain("This month (UTC)"); expect(html).not.toContain("UTC days"); expect(html).not.toContain("Updated ");
    // Model, API key, Member, Status and Cost center live behind "More filters" (not inline).
    expect(html).not.toContain(">All models<"); expect(html).not.toContain(">Succeeded<");
  });
  it("never shows member breakdowns or workspace budget meters to ordinary members or in Personal", () => {
    const client = testClient();
    const memberHtml = markup(<UsageOverviewTab workspace={member} ctx={usageContext(member)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, overview(null)], ["/api/v1/workspaces/team/policy", { budgets: [{ layer: "local", period: "month", amount_microusd: "100000000", usage_visible: false, used_microusd: null }] }]], client);
    expect(memberHtml).not.toContain("Top members"); expect(memberHtml).not.toContain("Workspace monthly budget"); expect(memberHtml).toContain("Top API keys");
    // Personal: the owner technically sees "workspace-wide" data, but a member list would just be themselves.
    const personalHtml = markup(<UsageOverviewTab workspace={personal} ctx={usageContext(personal)} period={period} nav={nav({ page: "costs", ws: "personal" })} />, [[`/api/v1/workspaces/personal/usage/overview?${q}`, overview([{ id: "me", name: "me@example.invalid", spend_microusd: "1", requests: "1", tokens: "1", share: "1" }])]]);
    expect(personalHtml).not.toContain("Top members"); expect(personalHtml).not.toContain("me@example.invalid");
    // Explore and Records for a member never request or offer the member dimension.
    markup(<><UsageExploreTab workspace={member} ctx={usageContext(member)} period={period} nav={nav({ page: "costs", ws: "team", group: "member", then: "member" })} /><WorkspaceRecords session={session} workspace={member} ctx={usageContext(member)} period={period} nav={nav({ page: "costs", ws: "team", tab: "records" })} /></>, [], client);
    const requested = paths(client);
    expect(requested.some(p => p.includes("/usage/explore?") && p.includes("group_by=model"))).toBe(true);
    expect(requested.filter(p => /member/.test(p))).toEqual([]);
    expect(requested.filter(p => p.includes("/usage/explore?"))).toHaveLength(1);
    client.clear();
  });
  it("sends the URL filters with the overview and explore queries (member filter only with workspace-wide visibility)", () => {
    const client = testClient(), search: DashboardSearch = { page: "costs", ws: "team", model_id: "m1", key_id: "k1", actor_user_id: "u1", status: "failed,cancelled", cost_center_id: "unallocated", service_account_id: "sa1" };
    markup(<><UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav(search)} /><UsageExploreTab workspace={member} ctx={usageContext(member)} period={period} nav={nav(search)} /></>, [], client);
    const requested = paths(client);
    expect(requested).toContain(`/api/v1/workspaces/team/usage/overview?${q}&model_id=m1&key_id=k1&member_user_id=u1&status=failed%2Ccancelled&cost_center_id=unallocated&service_account_id=sa1`);
    const exploreCall = requested.find(p => p.includes("/usage/explore?"))!;
    expect(exploreCall).toContain("model_id=m1"); expect(exploreCall).toContain("key_id=k1"); expect(exploreCall).toContain("status=failed%2Ccancelled"); expect(exploreCall).not.toContain("member_user_id");
    client.clear();
  });
  it("lists records with one request carrying the key, several statuses and the cost state", () => {
    const client = testClient();
    markup(<WorkspaceRecords session={session} workspace={team} ctx={usageContext(team)} period={period} nav={nav({ page: "costs", ws: "team", tab: "records", key_id: "k1", status: "succeeded,failed", cost_status: "cost_unknown" })} />, [], client);
    const calls = paths(client).filter(p => p.includes("/costs?"));
    expect(calls).toEqual([`/api/v1/workspaces/team/costs?${q}&key_id=k1&status=succeeded%2Cfailed&accounting_status=unknown&limit=50&offset=0`]);
    client.clear();
  });
  it("names the cost columns as cost records and links each row to its request", () => {
    const row = { id: "c1", root_request_id: "3cbfb500-0000-4000-8000-000000000001", attempt_number: 1, public_model: "company/smart", provider: "openai", state: "succeeded", workload_kind: "generation", started_at: "2026-10-08T06:30:00Z", input_tokens: "145", output_tokens: "0", billing_usage: null, cost_components: null, price_id: "p", cost_microusd: "8100", reserved_microusd: null, active_held_microusd: null, unresolved_reason: null, cost_status: "settled", unbounded_cost: false, cost_center_id: null, cost_center_name: null, cost_center_code: null, details_redacted_at: null };
    const html = markup(<WorkspaceRecords session={session} workspace={team} ctx={usageContext(team)} period={period} nav={nav({ page: "costs", ws: "team", tab: "records" })} />, [[`/api/v1/workspaces/team/costs?${q}&limit=50&offset=0`, { data: [row], has_more: false }]]);
    expect(html).toContain("Cost status"); expect(html).not.toContain(">Status<"); expect(html).toContain("145 in · 0 out");
    expect(html).toContain(`href="/workspaces/team/requests/${row.root_request_id}"`); expect(html).toContain("Rows 1–1");
  });
  it("says when the Explore chart shows fewer groups than the table (review #40)", () => {
    const client = testClient(), view = () => markup(<UsageExploreTab workspace={team} ctx={usageContext(team)} period={period} nav={nav({ page: "costs", ws: "team", tab: "explore" })} />, [], client);
    view(); const path = paths(client).find(p => p.includes("/usage/explore?"))!;
    const names = Array.from({ length: 9 }, (_, i) => `m${i}`);
    client.setQueryData(["api", undefined, path], { ...explore, rows: names.map(n => ({ group: { id: n, name: n }, then: null, value: "1", share: "0.1", held_microusd: "0", unresolved_attempts: "0" })), series: explore.series.map(d => ({ date: d.date, values: names.map(n => ({ id: n, value: "1" })) })) });
    const html = view();
    expect(html).toContain("Build a breakdown"); expect(html).toContain("The chart shows the top 7 groups; the table shows all 9.");
    client.clear();
  });
  it("keeps acronyms in grouping labels inside sentences (\"model and API key\", not \"api key\")", () => {
    const client = testClient(), view = () => markup(<UsageExploreTab workspace={team} ctx={usageContext(team)} period={period} nav={nav({ page: "costs", ws: "team", tab: "explore", group: "model", then: "key" })} />, [], client);
    view(); const path = paths(client).find(p => p.includes("/usage/explore?"))!;
    client.setQueryData(["api", undefined, path], { ...explore, then_by: "key", rows: explore.rows.map(r => ({ ...r, then: { id: "k", name: "Laptop key" } })) });
    const html = view();
    expect(html).toContain("Spend by model and API key"); expect(html).toContain("top 5 API keys"); expect(html).not.toMatch(/api key/);
    expect(dimensionPhrase("Cost center")).toBe("cost center"); expect(dimensionPhrase("API key")).toBe("API key");
    client.clear();
  });
  it("renders an empty state instead of a flat chart of zeros, and never fabricates $0 while loading", () => {
    const empty = overview(null); empty.tiles.spend = { ...tile("0", "0"), held_microusd: "0", unresolved_attempts: "0" }; empty.tiles.requests = { ...tile("0", "0"), attempts: "0" }; empty.top = { models: [], keys: [], members: null };
    const html = markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, [[`/api/v1/workspaces/team/usage/overview?${q}`, empty]]);
    expect(html).toContain("No requests in this period"); expect(html).not.toContain("Daily spend"); expect(html).not.toContain("Some costs aren");
    vi.useFakeTimers({ toFake: ["Date"] }); vi.setSystemTime(now);
    const loading = markup(<Costs session={session} workspace={team} />);
    expect(loading).toContain("Loading usage"); expect(loading).not.toContain("$0"); expect(loading).toContain("Everyone in Product"); expect(loading).toContain("Oct 1 – Oct 8, 2026 (UTC, today so far)");
    expect(markup(<Costs session={session} workspace={member} />)).toContain("Your usage in Product");
    expect(markup(<Costs session={session} workspace={personal} />)).toContain("Your usage");
  });
  it("summarizes accounting in one compact section: four stats, one status line, non-zero cache rows, coverage on demand", () => {
    const o = overview(null), html = markup(<AccountingReport report={report} rates={o} unresolved={{ page: "costs", ws: "team", tab: "records" }} statusFilter />);
    document.body.innerHTML = html;
    const stats = document.querySelector("[aria-label='Accounting summary']")!;
    expect([...stats.querySelectorAll("dt")].map(d => d.textContent)).toEqual(["Cache hit rate", "Cost per 1M tokens", "Costs pending", "Unpriced"]);
    // Pending + unknown (aged holds are a subset, named in the tooltip but not added again).
    expect(stats.querySelectorAll("dd")[2]!.textContent).toContain("3"); expect(html).toContain("Pending 1 · Unknown 2 · Aged holds 1 (included above)");
    expect(html).toContain("3 requests still have unknown cost");
    expect(screen.getByRole("link", { name: "View records" }).getAttribute("href")).toBe("/workspaces/team/costs?tab=records&cost_status=cost_unknown");
    // Exact money; unknown token counts stay "Unknown"; no explanatory paragraphs or overlapping aggregate row.
    for (const text of ["Legacy pinned pricing", "$9,007,199,254.740993", "Unknown"]) expect(html).toContain(text);
    for (const text of ["don&#x27;t add them up", "Six separate charge categories", "Cache writes (total", "Attempts: 12 for 10 requests", "Period rates", "Accounting health"]) expect(html).not.toContain(text);
    // Coverage numbers stay reachable in a closed disclosure.
    const coverage = screen.getByRole("button", { name: /Coverage/ });
    expect(coverage.getAttribute("aria-expanded")).toBe("false");
    for (const text of ["Unknown-cost attempts", "Attempts missing a reservation", "Incomplete billing", "Complete billing"]) expect(html).toContain(text);
  });
  it("says all costs are known in green, and shows one line instead of an all-zero cache table", () => {
    const zero = { pending_attempts: "0", unknown_attempts: "0", missing_reservation_attempts: "0", unpriced_attempts: "0", aged_hold_attempts: "0", unbounded_attempts: "0" };
    const billing = { total_input_tokens: "49", uncached_input_tokens: "49", cache_read_input_tokens: "0", cache_write_input_tokens: "0", cache_write_default_input_tokens: "0", cache_write_5m_input_tokens: "0", cache_write_1h_input_tokens: "0" };
    const clean = { ...report, health: zero, billing_usage: billing, cost_components: { ...report.cost_components, uncached_input_microusd: "5", output_microusd: "12", legacy_microusd: "0" } };
    const html = markup(<AccountingReport report={clean} />);
    expect(html).toContain("All costs known"); expect(html).not.toContain("unknown cost");
    expect(html).toContain("No prompt caching this period"); expect(html).not.toContain("<table"); expect(html).not.toContain("Legacy pinned pricing");
    // With caching: only non-zero categories, tokens and spend side by side in one table.
    const cached = { ...clean, billing_usage: { ...billing, cache_read_input_tokens: "40", cache_write_input_tokens: "0" }, cost_components: { ...clean.cost_components, cache_read_microusd: "1" } };
    document.body.innerHTML = markup(<AccountingReport report={cached} />);
    const table = screen.getByRole("table", { name: "Charges by category" });
    expect(within(table).getAllByRole("columnheader").map(h => h.textContent)).toEqual(["Category", "Tokens", "Spent"]);
    expect(within(table).getAllByRole("rowheader").map(h => h.textContent)).toEqual(["Uncached input", "Cache read", "Output"]);
    expect(table.textContent).toContain("$0.000001");
  });
  it("shows Accounting only to workspace admins and platform readers; plain members see tiles and charts only", () => {
    const seeded: [string, unknown][] = [[`/api/v1/workspaces/team/usage/overview?${q}`, overview(null)]];
    const asMember = testClient();
    expect(markup(<UsageOverviewTab workspace={member} ctx={usageContext(member)} period={period} nav={nav()} />, seeded, asMember)).not.toContain(">Accounting</h2>");
    expect(paths(asMember).some(p => p.includes("cost-report"))).toBe(false);
    asMember.clear();
    const asAdmin = testClient();
    expect(markup(<UsageOverviewTab workspace={team} ctx={usageContext(team)} period={period} nav={nav()} />, seeded, asAdmin)).toContain(">Accounting</h2>");
    expect(paths(asAdmin).some(p => p.includes("/workspaces/team/cost-report?"))).toBe(true);
    asAdmin.clear();
    // A platform reader who is a plain member still sees it (UsageCosts passes platform_read).
    expect(markup(<UsageOverviewTab workspace={member} ctx={usageContext(member)} period={period} nav={nav()} accounting />, seeded)).toContain(">Accounting</h2>");
    expect(showsAccounting(member)).toBe(false); expect(showsAccounting(team)).toBe(true); expect(showsAccounting()).toBe(true);
  });
});

async function mount(href: string, client = testClient()) { const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) }); await router.load(); const ui = render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>); return { router, client, ...ui }; }
function serve(me: object, handler: (path: string) => unknown) {
  const fetch = vi.fn().mockImplementation((path: string) => { if (path === "/api/v1/me") return Promise.resolve(Response.json(me)); const body = handler(path); return Promise.resolve(body === undefined ? Response.json({ data: [], has_more: false }) : Response.json(body)); });
  vi.stubGlobal("fetch", fetch); return fetch;
}

describe("Usage & costs routing", () => {
  it("opens a tile's routed chart page with a per-model Min/Max/Avg/Total table and goes back", async () => {
    const user = userEvent.setup(), fetch = serve(session, p => p.includes("/usage/explore?") ? explore : p.includes("/usage/overview?") ? overview(null) : undefined);
    const { router, client } = await mount("/workspaces/team/costs?tab=chart&metric=spend&range=7d");
    await screen.findByRole("heading", { name: "Spend", level: 1 });
    const table = await screen.findByRole("table", { name: "Spend by model" });
    for (const header of ["Min (USD per day)", "Max (USD per day)", "Avg (USD per day)", "Total (USD)"]) expect(within(table).getByText(header)).toBeTruthy();
    const big = within(table).getByRole("rowheader", { name: "big-model" }).closest("tr")!;
    expect(within(big).getAllByText("$9,007,199,254.74")).toHaveLength(2); // max and total from decimal strings, in cents
    const small = within(table).getByRole("rowheader", { name: "small-model" }).closest("tr")!;
    expect(within(small).getAllByText("$0.000003")).toHaveLength(2); expect(within(small).getByText("≈ $0.00000042")).toBeTruthy();
    const call = fetch.mock.calls.map(c => String(c[0])).find(p => p.includes("/usage/explore?"))!;
    expect(call).toContain("group_by=model"); expect(call).toContain("metric=spend"); expect(new URLSearchParams(call.split("?")[1]).get("end_date")! > new URLSearchParams(call.split("?")[1]).get("start_date")!).toBe(true);
    await user.click(screen.getByRole("link", { name: /Back to Usage & costs/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/workspaces/team/costs"));
    expect(router.state.location.search).toMatchObject({ range: "7d" }); expect(router.state.location.search).not.toHaveProperty("tab");
    await screen.findByRole("tab", { name: "Overview" });
    client.clear();
  });
  it("keeps Admin › Costs on platform totals when narrowing to a foreign personal workspace", async () => {
    const user = userEvent.setup(), foreign = "00000000-0000-0000-0000-000000000099";
    const fetch = serve(admin, p => p.startsWith("/api/v1/platform/usage/explore?") ? { ...explore, group_by: "workspace", rows: [{ group: { id: foreign, name: "Personal" }, then: null, value: "5", share: "1", held_microusd: "0", unresolved_attempts: "0" }], series: [] } : p.startsWith("/api/v1/platform/usage/overview?") ? { ...overview([]), installation_budgets: [{ period: "month", amount_microusd: "5000000", used_microusd: "1250000", settled_microusd: "1000000", held_microusd: "250000", unresolved_usage: true, exhausted: false, window_start: "2026-10-01T00:00:00Z", window_end: "2026-11-01T00:00:00Z" }] } : undefined);
    const { router, client } = await mount("/admin/costs");
    await screen.findByRole("heading", { name: "Usage & costs" });
    await screen.findByText("Installation monthly budget");
    expect(screen.getByText(/Spent \$1\.00 · on hold \$0\.25 · resets Nov 1, 2026/)).toBeTruthy();
    expect(screen.getByText(/At least this much/)).toBeTruthy();
    const select = await screen.findByRole("combobox", { name: /Workspace/ });
    await waitFor(() => expect(within(select).getByRole("option", { name: "Personal" })).toBeTruthy());
    await user.selectOptions(select, foreign);
    await waitFor(() => expect(router.state.location.search.workspace_id).toBe(foreign));
    await user.click(screen.getByRole("tab", { name: "By workspace" }));
    await waitFor(() => expect(fetch.mock.calls.some(c => String(c[0]).startsWith("/api/v1/platform/cost-report?") && String(c[0]).includes(`workspace_id=${foreign}`))).toBe(true));
    const requested = fetch.mock.calls.map(c => String(c[0]));
    expect(requested.some(p => p.startsWith(`/api/v1/workspaces/${foreign}`))).toBe(false);
    expect(requested.some(p => /platform\/users|\/keys|\/executions|\/costs\?|\/requests/.test(p))).toBe(false);
    expect(requested.filter(p => p.includes(foreign)).every(p => p.startsWith("/api/v1/platform/"))).toBe(true);
    client.clear();
  });
  it("puts By workspace's Columns menu at the end of the one filter row, not on a row of its own", async () => {
    const user = userEvent.setup();
    serve(admin, p => p.startsWith("/api/v1/platform/cost-report?") ? { ...report, scope: "platform", breakdowns: { ...report.breakdowns, workspaces: [{ id: "w1", name: "Research", totals: report.totals }] } } : p.startsWith("/api/v1/platform/usage/explore?") ? { ...explore, group_by: "workspace", rows: [], series: [] } : p.startsWith("/api/v1/platform/usage/overview?") ? overview([]) : undefined);
    const { client } = await mount("/admin/costs?tab=records");
    const table = await screen.findByRole("table", { name: "By workspace" });
    await within(table).findByText("Research");
    const filters = screen.getByRole("group", { name: "Filters" });
    const columns = within(filters).getByRole("button", { name: "Columns" });
    expect(screen.getAllByRole("button", { name: "Columns" })).toHaveLength(1);
    await user.click(columns);
    await user.click(await screen.findByRole("menuitemcheckbox", { name: "On hold" }));
    await waitFor(() => expect(within(table).queryByRole("columnheader", { name: "On hold" })).toBeNull());
    // Other tabs have no Columns on the filter row.
    await user.click(screen.getByRole("tab", { name: "Overview" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Columns" })).toBeNull());
    client.clear();
  });
});

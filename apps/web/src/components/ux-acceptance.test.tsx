// @vitest-environment jsdom
/* Regression tests for the UX program browser acceptance defects D-2…D-12 and its polish notes. */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ApiError } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { auditor, grant, markup, member, personal, session, team, testClient } from "../lib/test-fixtures";
import { reasonText } from "../lib/effective-access";
import { formatRateMicroUsd, resetsAt, usageContext, usagePeriod, utcBoundary, type ExploreResponse, type UsageOverview } from "../lib/usage";
import { draftErrors, draftLimitsValid, draftOf, rateError, type Limits } from "../lib/limits";
import { compactDateTime, tokensText, type RequestDetail, type RequestRow } from "../lib/requests";
import { modelsAcrossWorkspaces } from "../lib/home";
import { countLabel } from "../lib/reports";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { EffectiveAccess } from "./effective-access";
import { BudgetMeters, LimitsTable } from "./scope-limits";
import { FilterToolbar } from "./templates/filter-toolbar";
import { StickySaveBar } from "./templates/sticky-save-bar";
import { TypeTabs } from "./templates/type-tabs";
import { InstallationBudgets, UsageOverviewTab } from "../pages/usage/overview";
import { UsageExploreTab } from "../pages/usage/explore";
import { Requests, requestView, requestViewSearch } from "../pages/requests";
import { RequestDetailPage, isShortRequestId, shortIdLookup } from "../pages/request-detail";
import { KeyUsage } from "../pages/key-detail";
import { WorkspaceModels } from "../pages/model-catalog";
import { HomePortal } from "../pages/home-portal";
import { PlatformOverview } from "../pages/start";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); document.documentElement.style.removeProperty("--save-bar-space"); });

const ws = "/api/v1/workspaces/team";
const nav = (search: DashboardSearch, node: ReactNode) => <DashboardNavigationProvider search={search} navigate={() => {}}>{node}</DashboardNavigationProvider>;
const text = (html: string) => new DOMParser().parseFromString(html, "text/html").body.textContent ?? "";

describe("D-2 blended $/M", () => {
  it("shows at most six decimals, rounded exactly, never a non-zero as $0, with the exact value kept", () => {
    expect(formatRateMicroUsd("3986822.8404")).toEqual({ text: "$3.986823", exact: "$3.9868228404", rounded: true });
    expect(formatRateMicroUsd("7807829.1815").text).toBe("$7.807829");
    expect(formatRateMicroUsd("100000")).toEqual({ text: "$0.10", exact: "$0.10", rounded: false });
    expect(formatRateMicroUsd("1.25").text).toBe("$0.00000125"); // below $1, six digits from the first significant one
    expect(formatRateMicroUsd("0.0001234567891").text).toBe("$0.000000000123457");
    expect(formatRateMicroUsd("544.6").text).toBe("$0.0005446");
    expect(formatRateMicroUsd("999999.9999999").text).toBe("$1.00");
    expect(formatRateMicroUsd(null).text).toBe("Unknown");
  });
  it("moves cost per 1M tokens to Explore (rate tile and model column), with the exact value in the tooltip", () => {
    const tile = (value: string | null) => ({ value, previous: null, delta: null, change_ratio: null, daily: [] });
    const o: UsageOverview = { period: { start_date: "2026-10-01", end_date: "2026-10-09" }, previous_period: { start_date: "2026-09-23", end_date: "2026-10-01" }, tiles: { spend: { ...tile("2723"), held_microusd: "0", unresolved_attempts: "0" }, requests: { ...tile("1"), attempts: "1" }, tokens: { ...tile("5000"), input_tokens: "3000", output_tokens: "2000", unknown_token_attempts: "1" }, cache_hit_rate: tile(null), blended_microusd_per_million: tile("3986822.8404") }, top: { models: [{ id: "m", name: "gpt-6-luna", spend_microusd: "2723", requests: "1", tokens: "5000", share: "1", blended_microusd_per_million: "7807829.1815", model_id: "m" }], keys: [], members: null } };
    const period = usagePeriod({}, new Date("2026-10-08T12:00:00Z")), q = `start_date=${period.start_date}&end_date=${period.end_date}`;
    const explore: ExploreResponse = { metric: "spend", group_by: "model", then_by: null, period: { start_date: period.start_date, end_date: period.end_date }, total: { value: "2723", held_microusd: "0", unresolved_attempts: "0" }, rows: [{ group: { id: "m", name: "gpt-6-luna" }, then: null, value: "2723", share: "1", held_microusd: "0", unresolved_attempts: "0" }], other: null, truncated: false, series: [] };
    const props = { workspace: member, ctx: usageContext(member), period, nav: { search: { page: "costs" as const, ws: "team" }, navigate: () => {} } };
    const seeded: [string, unknown][] = [[`${ws}/usage/overview?${q}`, o], [`${ws}/usage/explore?${q}&metric=spend&group_by=model&top=10`, explore]];
    const html = markup(<UsageOverviewTab {...props} />, seeded);
    // Not an overview tile any more.
    expect(html).not.toContain("Cost per 1M tokens</a>"); expect(html).not.toContain(">$3.986823<"); expect(html).not.toContain("Cache hit rate</a>");
    const exploreHtml = markup(<UsageExploreTab {...props} />, seeded);
    expect(exploreHtml).toContain('title="$3.9868228404 per 1M tokens (exact)">$3.986823<');
    expect(exploreHtml).toContain(">$7.807829<");
    expect(exploreHtml).not.toContain(">$3.9868228404<");
    // Unknown cache hit rate stays Unknown, never 0%.
    expect(text(exploreHtml)).toMatch(/Cache hit rate\s*Unknown/);
    // Plurals: "1 request", never "1 requests".
    expect(text(html)).toContain("1 request didn't report tokens"); expect(text(html)).not.toMatch(/\b1 requests\b/);
    expect(countLabel("2", "request")).toBe("2 requests");
  });
});

const row: RequestRow = { root_request_id: "1a2b3c4d-0000-0000-0000-000000000001", started_at: "2026-10-08T02:30:00Z", completed_at: "2026-10-08T02:30:02Z", model: "demo/a-model-with-a-rather-long-name", key: { id: "key1", name: "CI runner" }, status: "succeeded", attempts: 1, input_tokens: "10", output_tokens: "2", cost_microusd: "14", held_microusd: "0", latency_ms: 2100, cost_center: null, workload_kind: "generation", streamed: false };
describe("D-3 Requests list", () => {
  const html = () => markup(nav({ page: "requests", ws: "team" }, <Requests session={session} workspace={team} />), [[`${ws}/requests?limit=50`, { data: [row, { ...row, root_request_id: "2b2b3c4d-0000-0000-0000-000000000002", workload_kind: "audio_speech", input_tokens: "0", output_tokens: "0" }], next_cursor: null }]]);
  it("puts Started (compact, one line), Model, Status, Cost and Latency first, and makes the whole row one link", () => {
    const doc = new DOMParser().parseFromString(html(), "text/html"), table = doc.querySelector("table")!;
    expect([...table.tHead!.rows[0]!.cells].map(c => c.textContent)).toEqual(["Started", "Model", "Key / app", "Tokens", "Cost", "Latency", "Finish", "Status", "Attempts"]);
    const first = table.tBodies[0]!.rows[0]!, link = first.cells[0]!.querySelector("a")!;
    expect(link.className).toMatch(/rowLink/);
    expect(link.getAttribute("href")).toBe("/workspaces/team/requests/1a2b3c4d-0000-0000-0000-000000000001");
    expect(link.textContent).toMatch(/^Oct \d{1,2}, \d{1,2}:\d{2} [AP]M$/);
    expect(link.querySelector("time")?.getAttribute("title")).toMatch(/2026/);
    expect(first.querySelectorAll("a")).toHaveLength(1); // one tab stop per row; the copy button stays usable
    expect(first.cells[4]!.textContent).toBe("$0.000014");
    expect(first.cells[1]!.querySelector('[title="demo/a-model-with-a-rather-long-name"]')).not.toBeNull();
    expect(table.getAttribute("data-stack")).not.toBeNull(); // rows stack on a phone instead of overflowing
  });
  it("shows audio rows' tokens as Not applicable, like Records", () => {
    expect(text(html())).toContain("Not applicable");
    expect(tokensText("0", "0", "audio_transcriptions")).toBe("Not applicable");
    expect(tokensText("0", "0", "generation")).toBe("0 in · 0 out");
  });
  it("formats the start compactly (24-hour, year only when it isn't this year)", () => {
    const now = new Date("2026-10-08T12:00:00Z");
    expect(compactDateTime("2026-10-08T02:30:00Z", now, "UTC").text).toBe("Oct 8, 2:30 AM");
    expect(compactDateTime("2025-12-31T23:05:00Z", now, "UTC").text).toBe("Dec 31, 2025, 11:05 PM");
    expect(compactDateTime("2026-10-08T02:30:00Z", now, "UTC").full).toBe("Oct 8, 2026, 2:30 AM UTC");
  });
  it("hides low-priority columns on a phone by default, and keeps a phone's default view out of the URL", () => {
    expect(requestView({}, true).hidden).toEqual(expect.arrayContaining(["tokens", "attempts", "key", "request"]));
    expect(requestView({}, true).hidden).not.toContain("cost");
    expect(requestViewSearch(requestView({}, true), true)).toEqual({ cols: undefined, density: undefined });
  });
  it("collapses its filters behind 'Filters' on a phone (D-5) and hides Columns and paging while empty (polish)", () => {
    const empty = markup(nav({ page: "requests", ws: "team", model: "x" }, <Requests session={session} workspace={team} />), [[`${ws}/requests?model=x&limit=50`, { data: [], next_cursor: null }]]);
    expect(empty).toMatch(/aria-expanded="false"[^>]*>.*Filters/);
    expect(text(empty)).toContain("1 active");
    expect(text(empty)).not.toContain("Columns"); expect(text(empty)).not.toContain("Page 1");
  });
});

describe("D-4 / D-12 models that can't serve", () => {
  const catalogRow = (id: string, routes: number, eligibility = "selected") => ({ model_id: id, public_name: `company/${id}`, display_name: id === "model" ? "Smart model" : `Model ${id}`, description: null, protocols: ["chat_completions"], workload: "generation", eligibility, reason: "Added to this workspace from an available catalog", min_input_microusd_per_million: "100000", min_output_microusd_per_million: "400000", routes });
  it("flags workspace models without an enabled route", () => {
    const c = testClient(); c.setQueryData(["api", undefined, `${ws}/catalog`, "choices"], [catalogRow("model", 1), catalogRow("dark", 0), catalogRow("avail", 0, "available_from_catalog")]); c.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]);
    const html = markup(nav({ page: "grants", ws: "team" }, <WorkspaceModels session={session} workspace={team} />), [], c);
    expect(html.match(/<span aria-hidden="true" class="[^"]*"><\/span>Not serving<\/span>/g)).toHaveLength(2); expect(html).not.toContain("Not serving — ");
    expect(html).toContain("No route to a provider is turned on, so requests fail"); // the badge's tooltip, not a sentence in the row
    expect(html).not.toMatch(/<p[^>]*>No route to a provider/);
    const table = markup(nav({ page: "grants", ws: "team", layout: "table" }, <WorkspaceModels session={session} workspace={team} />), [], c);
    expect(table).toContain(">Not serving<"); expect(table).toContain("No route to a provider is turned on");
  });
  it("keeps non-serving models out of Home's 'Models you can use'", () => {
    const r = modelsAcrossWorkspaces([{ workspace: personal, rows: [catalogRow("model", 1), catalogRow("dark", 0), catalogRow("avail", 2, "available_from_catalog")] as never }, { workspace: team, rows: [catalogRow("model", 1)] as never }]);
    expect(r.usable.map(m => m.model.model_id)).toEqual(["model"]);
    expect(r.usable[0]!.workspaces.map(w => w.id)).toEqual(["personal", "team"]);
    expect(r.notServing.map(m => m.model.model_id)).toEqual(["dark"]);
  });
  it("Home flags them and offers Create key only where a workspace has a model", async () => {
    const c = testClient(), user = userEvent.setup();
    c.setQueryData(["api", undefined, "/api/v1/workspaces/personal/catalog", "choices"], []);
    c.setQueryData(["api", undefined, "/api/v1/me/keys?status=active&limit=100"], { data: [], has_more: false });
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: { code: "404", message: "not here" } }, { status: 404 })));
    render(<QueryClientProvider client={c}><ActionProvider>{nav({ page: "home" }, <HomePortal session={auditor} />)}</ActionProvider></QueryClientProvider>);
    await user.click(screen.getByRole("button", { name: "Create key" }));
    const item = await screen.findByRole("menuitem", { name: /Personal · Add a model first/ });
    expect(item.getAttribute("aria-disabled")).toBe("true");
    const shared = testClient();
    shared.setQueryData(["api", undefined, "/api/v1/workspaces/personal/catalog", "choices"], [catalogRow("dark", 0)]);
    shared.setQueryData(["api", undefined, "/api/v1/workspaces/team/catalog", "choices"], [catalogRow("model", 1)]);
    shared.setQueryData(["api", undefined, "/api/v1/workspaces/project/catalog", "choices"], []);
    cleanup();
    render(<QueryClientProvider client={shared}><ActionProvider>{nav({ page: "home" }, <HomePortal session={session} />)}</ActionProvider></QueryClientProvider>);
    const list = screen.getByRole("list", { name: "Models you can use" });
    expect(within(list).getByText("Smart model")).toBeTruthy(); expect(within(list).queryByText("Model dark")).toBeNull();
    expect(screen.getByText(/added, but no enabled route to a provider/)).toBeTruthy();
    shared.clear(); c.clear();
  });
});

describe("D-6 sticky save bar", () => {
  it("reserves its height at the bottom of the page while open, and gives it back when closed", () => {
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ height: 56, width: 400, top: 0, left: 0, right: 400, bottom: 56, x: 0, y: 0, toJSON() {} } as DOMRect);
    const { rerender } = render(<StickySaveBar open message="Unsaved changes"><button>Save</button></StickySaveBar>);
    expect(document.documentElement.style.getPropertyValue("--save-bar-space")).toMatch(/^\d+px$/);
    expect(Number.parseInt(document.documentElement.style.getPropertyValue("--save-bar-space"))).toBeGreaterThanOrEqual(56);
    rerender(<StickySaveBar open={false} message="Unsaved changes"><button>Save</button></StickySaveBar>);
    expect(document.documentElement.style.getPropertyValue("--save-bar-space")).toBe("");
  });
});

describe("D-7 limits editor while some rows are invalid", () => {
  const parent: Limits = { requests_per_minute: 60, tokens_per_minute: 1000, concurrent_requests: null, budgets: [{ period: "month", amount_microusd: "100000000" }] };
  it("says a negative number must be positive", () => {
    expect(rateError("-3")).toBe("Must be a positive whole number.");
    expect(rateError("0")).toBe("Must be a positive whole number.");
    expect(rateError("1.5")).toMatch(/whole number/);
  });
  it("keeps effective values for unchanged valid rows and shows — only for invalid ones", () => {
    const draft = { ...draftOf({ ...parent, requests_per_minute: 30, tokens_per_minute: null, budgets: [{ period: "month", amount_microusd: "5000000" }] }), requests_per_minute: "-3" };
    draft.budgets.push({ key: "new", period: "day", amount: "" });
    const errors = draftErrors(draft, "tighten", [{ label: "platform", limits: parent }]), valid = draftLimitsValid(draft, errors);
    expect(valid.invalid).toEqual({ rates: ["requests_per_minute"], periods: ["day"] });
    expect(valid.limits.budgets).toEqual([{ period: "month", amount_microusd: "5000000" }]);
    const html = markup(<LimitsTable caption="Limits" scopeLabel="This workspace" draft={draft} editing errors={errors} mode="tighten" inherited={{ label: "Inherited", limits: parent }} effective={{ limits: { ...parent, requests_per_minute: null, budgets: [{ period: "month", amount_microusd: "5000000" }] }, invalid: valid.invalid }} />);
    const doc = new DOMParser().parseFromString(html, "text/html"), cells = [...doc.querySelectorAll("tbody tr")].map(r => [...r.querySelectorAll("td")].at(-1)!.textContent);
    // Requests per minute (invalid), Tokens per minute (unchanged), Running at once, Daily (invalid), Monthly (unchanged).
    expect(cells).toEqual(["—", "1,000", "No limit", "—", "$5.00"]);
    expect(html).toContain("Must be a positive whole number.");
  });
});

describe("D-8 one time convention", () => {
  it("shows window boundaries in UTC everywhere", () => {
    expect(utcBoundary("2026-11-01T00:00:00Z")).toBe("Nov 1, 2026, 00:00 UTC");
    expect(resetsAt("2026-11-01T00:00:00Z")).toBe("Resets Nov 1, 2026, 00:00 UTC");
    expect(resetsAt(null)).toBe("Never resets");
    const html = markup(<BudgetMeters kind="personal" windows={[{ layer: "platform", period: "month", amount_microusd: "5000000", monthly_budget_microusd: "5000000", budget_period: "month", window_start: "2026-10-01T00:00:00Z", window_end: "2026-11-01T00:00:00Z", usage_visible: true, used_microusd: "100", unresolved_usage: false }]} />);
    expect(html).toContain("Resets Nov 1, 2026, 00:00 UTC"); expect(html).not.toMatch(/PM|AM/);
  });
  it("labels the key's 30-day chart like the rest of the app (Sep 9), not ISO dates", () => {
    const stats = { key_id: "key1", lineage_id: "key1", status: "active", last_used_at: null, daily: [{ date: "2026-09-09", spend_microusd: "1", held_microusd: "0", requests: "1", unresolved_attempts: "0" }, { date: "2026-09-10", spend_microusd: "2", held_microusd: "0", requests: "1", unresolved_attempts: "0" }], totals: { today: { spend_microusd: "0", held_microusd: "0", requests: "0", unresolved_attempts: "0" }, week: { spend_microusd: "0", held_microusd: "0", requests: "0", unresolved_attempts: "0" }, month: { spend_microusd: "3", held_microusd: "0", requests: "2", unresolved_attempts: "0" } }, budgets: [] };
    const html = markup(<KeyUsage workspace={team} keyId="key1" />, [[`${ws}/keys/key1/stats`, stats]]);
    expect(html).toContain("Sep 9"); expect(html).not.toContain(">2026-09-09<");
  });
});

describe("D-9 request page not found", () => {
  it("says the request doesn't exist or can't be seen, without previous/next", () => {
    const id = "1a2b3c4d-0000-0000-0000-000000000009", client = testClient();
    client.getQueryCache().build(client, { queryKey: ["api", undefined, `${ws}/requests/${id}`] }).setState({ status: "error", error: new ApiError(404, "404", "Not found"), fetchStatus: "idle" });
    const html = markup(nav({ page: "request-detail", ws: "team", record: id }, <RequestDetailPage session={session} workspace={member} id={id} />), [], client);
    expect(html).toContain("This request doesn&#x27;t exist or you can&#x27;t see it");
    expect(html).toContain("Back to Logs");
    expect(html).toContain("Members see only requests made with their own keys.");
    expect(html).toContain("Request not found");
    expect(html).not.toContain("Previous request"); expect(html).not.toContain("No next request");
    expect(html).not.toContain("Resource not found");
  });
  it("short or malformed ids never show parser text: a unique short id resolves, others are not found", () => {
    expect(isShortRequestId("3cbfb500")).toBe(true); expect(isShortRequestId("1a2b3c4d-0000-0000-0000-000000000009")).toBe(false); expect(isShortRequestId("nope")).toBe(false);
    expect(Object.fromEntries(shortIdLookup("3CBFB500", Date.UTC(2026, 9, 8)))).toEqual({ q: "3cbfb500", start_date: "2026-07-08", end_date: "2026-10-09", limit: "2" });
    const lookup = `${ws}/requests?${shortIdLookup("3cbfb500")}`;
    const none = markup(nav({ page: "request-detail", ws: "team", record: "3cbfb500" }, <RequestDetailPage session={session} workspace={member} id="3cbfb500" />), [[lookup, { data: [], next_cursor: null }]]);
    expect(none).toContain("Request not found"); expect(none).not.toContain("Previous request"); expect(none).not.toMatch(/parse|UUID/i);
    const bad = markup(nav({ page: "request-detail", ws: "team", record: "zz" }, <RequestDetailPage session={session} workspace={member} id="zz" />), []);
    expect(bad).toContain("Request not found");
    const one = markup(nav({ page: "request-detail", ws: "team", record: "1a2b3c4d" }, <RequestDetailPage session={session} workspace={member} id="1a2b3c4d" />), [[`${ws}/requests?${shortIdLookup("1a2b3c4d")}`, { data: [row], next_cursor: null }]]);
    expect(one).toContain("Finding request 1a2b3c4d");
  });
  it("still renders a found request normally", () => {
    const detail: RequestDetail = { ...row, workspace_id: "team", attempt_count: 1, attempts: [], prev_id: null, next_id: null };
    const html = markup(nav({ page: "request-detail", ws: "team", record: row.root_request_id }, <RequestDetailPage session={session} workspace={team} id={row.root_request_id} />), [[`${ws}/requests/${row.root_request_id}`, detail]]);
    expect(html).toContain("Request 1a2b3c4d"); expect(html).not.toContain("Request not found");
  });
});

describe("D-10 installation budget", () => {
  it("says when there is no installation-wide budget, on Costs and the Admin overview", () => {
    expect(markup(<InstallationBudgets budgets={[]} />)).toContain("No installation-wide budget");
    expect(markup(<InstallationBudgets budgets={null} />)).toBe("");
    const overview = { setup: { connections: 1, enabled_connections: 1, models: 1, ready_models: 1, enabled_routes: 1, priced_enabled_routes: 1, catalogs: 1, type_defaults: { personal: 1, team: 1, project: 1 }, entitled_users: 1, oidc_mappings: 1 }, glance: { entitled_users: 1, teams: 1, projects: 0, ready_models: 1, attempts_7d: "1", known_cost_7d_microusd: "1" }, installation_budgets: [] };
    const html = markup(<PlatformOverview session={{ ...session, capabilities: { platform_read: true, platform_write: false, create_workspace: false } }} />, [["/api/v1/platform/overview", overview]]);
    // The Admin overview shows the card only when a budget exists ("none" is not worth a card).
    expect(html).not.toContain("No installation-wide budget"); expect(html).not.toContain("Installation budgets");
    // New vocabulary (polish): no "Attempts" or "unresolved charges".
    expect(html).not.toContain("Attempts, last 7 days"); expect(html).not.toContain("unresolved charges"); expect(html).toContain("Spend this month");
  });
});

describe("D-11 revoked key access", () => {
  it("shows that a revoked key can't use any models instead of layer counts, and asks for nothing", () => {
    const client = testClient();
    const html = markup(<EffectiveAccess workspace={team} keyId="key3" keyName="Old token" keyStatus="revoked" />, [], client);
    expect(html).toContain("Revoked keys can&#x27;t use any models");
    expect(html).toContain("0 models are usable");
    expect(html).not.toContain("available ·"); expect(html).not.toContain("Why can&#x27;t I use this model?");
    expect(client.getQueryCache().getAll().every(q => q.state.data === undefined && q.state.fetchStatus === "idle")).toBe(true);
  });
  it("tells a disabled key's holder it can't be used until enabled", () => {
    const html = markup(<EffectiveAccess workspace={team} keyId="key2" keyStatus="disabled" />, [[`${ws}/keys/key2/access`, { workspace_id: "team", key_id: "key2", truncated: false, layers: [], summary: { available: 1, partial: 0, unavailable: 0 }, models: [] }]]);
    expect(html).toContain("Disabled: no models can be used right now");
  });
});

describe("Polish", () => {
  it("names type tabs 'All, 14' (no stray space or comma)", () => {
    render(<TypeTabs label="Model type" value="all" onChange={() => {}} items={[{ value: "all", label: "All", count: 14 }, { value: "audio", label: "Audio", count: null }]} />);
    expect(screen.getByRole("tab", { name: "All, 14" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Audio" })).toBeTruthy();
  });
  it("filter toolbar on a phone: a real 'Filters (n)' toggle with the active count, open on demand", async () => {
    const user = userEvent.setup();
    render(<FilterToolbar start={<p>Controls</p>} extraActive={2} />);
    const toggle = screen.getByRole("button", { name: /Filters/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false"); expect(toggle.textContent).toBe("Filters (2 active)");
    await act(() => user.click(toggle));
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(document.getElementById(toggle.getAttribute("aria-controls")!)?.textContent).toBe("Controls");
  });
  it("tells someone who can add models that they can add it themselves", () => {
    expect(reasonText({ code: "not_selected", layer: "workspace" }, "personal", true)).toContain("You can add it from Models.");
    expect(reasonText({ code: "not_selected", layer: "workspace" }, "team")).toContain("A workspace admin can add it from Models.");
  });
  it("keeps limit help in plain words", () => {
    const html = markup(<LimitsTable caption="Limits" scopeLabel="Here" draft={draftOf({ requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [] })} editing={false} />);
    expect(html).not.toContain("Upstream attempts admitted"); expect(html).toContain("How many requests can start each minute");
  });
});

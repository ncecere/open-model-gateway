// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import type { ReactNode } from "react";
import { DashboardNavigationProvider } from "./navigation-link";
import { LogsPage } from "../pages/logs";
import { PlatformSessionDetailPage, SessionDetailPage } from "../pages/logs-session";
import { PlatformRequestDetailPage, attemptItem } from "../pages/request-detail";
import type { DashboardSearch } from "../lib/permissions";
import { dashboardHref, parseDashboardLocation } from "../lib/locations";
import { dashboardSearch } from "../lib/permissions";
import { navigation } from "../lib/navigation";
import { rateText, requestFilters, requestQuery, servedModel, servedModelText, tpsText, ttftText, validSessionId, type GenerationRow, type LogMetrics, type RequestDetail, type RequestRow, type SessionRow } from "../lib/requests";
import { admin, markup, member, session, team } from "../lib/test-fixtures";

const ws = "/api/v1/workspaces/team";
const nav = (search: DashboardSearch, node: ReactNode) => <DashboardNavigationProvider search={search} navigate={() => {}}>{node}</DashboardNavigationProvider>;
const row: RequestRow = { root_request_id: "1a2b3c4d-0000-0000-0000-000000000001", workspace: { id: "team", name: "Product", kind: "team" }, started_at: "2026-10-08T10:00:00Z", completed_at: "2026-10-08T10:00:02Z", model: "company/smart", upstream_model: "gpt-x", key: { id: "key1", name: "CI runner" }, status: "succeeded", finish_reason: "length", attempts: 1, input_tokens: "10", output_tokens: "5", cached_input_tokens: "3", reasoning_tokens: null, cost_microusd: "14", held_microusd: "0", latency_ms: 2100, time_to_first_token_ms: 120, generation_ms: 620, tokens_per_second: "10.00", cost_center: null, workload_kind: "generation", streamed: true, session_id: "conv 7/a", app: "Agent" };
const metrics: LogMetrics = { requests: "12", completed: "10", failed: "1", error_rate: "0.1000", latency_p50_ms: 850, latency_p95_ms: 3200, avg_time_to_first_token_ms: 140, ttft_requests: "6", tokens_per_second: "41.25", input_tokens: "100", output_tokens: "40", unknown_token_requests: "0", known_cost_microusd: "100", held_microusd: "0", unresolved_requests: "0" };
const generation: GenerationRow = { execution_id: "e1e1e1e1-0000-0000-0000-000000000000", root_request_id: row.root_request_id, attempt_number: 2, started_at: row.started_at, completed_at: row.completed_at, model: row.model, upstream_model: "gpt-x", connection: { id: "c", name: "Backup OpenRouter", provider: "openrouter" }, key: row.key, status: "failed", error_code: "upstream_timeout", finish_reason: "error", streamed: false, workload_kind: "generation", input_tokens: null, output_tokens: null, cost_microusd: "0", held_microusd: "0", latency_ms: 900, time_to_first_token_ms: null, generation_ms: 880, tokens_per_second: null, session_id: null, app: null };
const sessionRow: SessionRow = { session_id: "conv 7/a", workspace: { id: "team", name: "Product", kind: "team" }, requests: "3", attempts: "4", failed_requests: "1", in_progress_requests: "0", input_tokens: "30", output_tokens: "9", cost_microusd: null, known_cost_microusd: "20", held_microusd: "5", unresolved_requests: "1", first_at: "2026-10-08T09:00:00Z", last_at: "2026-10-08T10:00:00Z", models: ["company/smart"], model_count: 1, last_model: "company/smart", app: "Agent", keys: 1 };

describe("Logs page", () => {
  it("shows tiles for the filters, pill tabs and OpenRouter-like request columns with telemetry", () => {
    const search: DashboardSearch = { page: "requests", ws: "team", finish_reason: "length", streamed: "true" };
    const html = markup(nav(search, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/logs/metrics?finish_reason=length&streamed=true`, metrics], [`${ws}/requests?finish_reason=length&streamed=true&limit=50`, { data: [row], next_cursor: null }]]);
    const doc = new DOMParser().parseFromString(html, "text/html");
    expect(doc.querySelector("h1")?.textContent).toBe("Logs");
    expect([...doc.querySelectorAll('[role="tab"]')].map(t => t.textContent)).toEqual(["Requests", "Generations", "Sessions", "Batches"]);
    for (const text of ["12", "10%", "850 ms", "Median · p95 3.2 s", "140 ms", "41.2 tok/s"]) expect(html).toContain(text);
    const table = doc.querySelector("table")!;
    // Streaming telemetry (TTFT, speed) is under Columns by default so rows stay short; the tiles summarize it.
    expect([...table.tHead!.rows[0]!.cells].map(c => c.textContent)).toEqual(["Started", "Model", "Key / app", "Tokens", "Cost", "Latency", "Finish", "Status", "Attempts"]);
    const cells = [...table.tBodies[0]!.rows[0]!.cells].map(c => c.textContent);
    expect(cells).toContain("Length limit"); expect(cells).toContain("CI runnerAgent");
    const all = new DOMParser().parseFromString(markup(nav({ ...search, cols: "none" }, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/logs/metrics?finish_reason=length&streamed=true`, metrics], [`${ws}/requests?finish_reason=length&streamed=true&limit=50`, { data: [row], next_cursor: null }]]), "text/html");
    const allCells = [...all.querySelector("table")!.tBodies[0]!.rows[0]!.cells].map(c => c.textContent);
    expect(allCells).toContain("120 ms"); expect(allCells).toContain("10 tok/s");
    expect(html).toContain('href="/workspaces/team/requests/1a2b3c4d-0000-0000-0000-000000000001?finish_reason=length&amp;streamed=true"');
    expect(html).not.toContain(">Workspace<");
  });
  it("Admin adds the Workspace column and filter, and reads the platform endpoints", () => {
    const html = markup(nav({ page: "platform-logs", workspace_id: "team" }, <LogsPage scope={{ kind: "platform" }} />), [["/api/v1/platform/logs/metrics?workspace_id=team", metrics], ["/api/v1/platform/logs/requests?workspace_id=team&limit=50", { data: [row], next_cursor: null }]]);
    expect(html).toContain("Personal workspaces never appear here");
    expect(new DOMParser().parseFromString(html, "text/html").querySelector("thead")!.textContent).toContain("Workspace");
    expect(html).toContain('href="/admin/logs/1a2b3c4d-0000-0000-0000-000000000001?workspace_id=team"');
    expect(html).not.toContain("Any key");
  });
  it("lists generations (one per attempt) and sessions linking to the session page", () => {
    const g = markup(nav({ page: "requests", ws: "team", tab: "generations" }, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/generations?limit=50`, { data: [generation], next_cursor: null }]]);
    expect(g).toContain("Backup OpenRouter"); expect(g).toContain("2 (fallback)"); expect(g).toContain("Error");
    const s = markup(nav({ page: "requests", ws: "team", tab: "sessions" }, <LogsPage scope={{ kind: "workspace", workspace: member }} />), [[`${ws}/sessions?limit=50`, { data: [sessionRow], next_cursor: null }]]);
    expect(s).toContain('href="/workspaces/team/session?session_id=conv+7%2Fa"');
    expect(s).toContain("At least $0.00002"); expect(s).toContain("conv 7/a");
  });
});

describe("Session and platform request pages", () => {
  it("shows a session's totals and its requests", () => {
    const html = markup(nav({ page: "session-detail", ws: "team", session_id: "conv 7/a" }, <SessionDetailPage session={session} workspace={team} />), [[`${ws}/sessions/conv%207%2Fa`, sessionRow], [`${ws}/requests?session_id=conv+7%2Fa&limit=50`, { data: [row], next_cursor: null }]]);
    for (const text of ["conv 7/a", "At least $0.00002", "$0.000005 on hold", "60 min", "Agent", "Session requests"]) expect(html).toContain(text);
    expect(html).toContain('href="/workspaces/team/logs?tab=sessions"');
    expect(new DOMParser().parseFromString(html, "text/html").querySelector("h1")?.textContent).toBe("Session conv 7/a");
  });
  it("says a missing session isn't visible", () => {
    expect(markup(nav({ page: "platform-log-session" }, <PlatformSessionDetailPage session={admin} />))).toContain("Session not found");
  });
  it("opens a team request on Admin without linking to the key page", () => {
    const detail: RequestDetail = { ...row, workspace_id: "team", attempt_count: 1, attempts: [], prev_id: null, next_id: null };
    const html = markup(nav({ page: "platform-log-detail", record: row.root_request_id }, <PlatformRequestDetailPage session={admin} id={row.root_request_id} />), [[`/api/v1/platform/logs/requests/${row.root_request_id}`, detail]]);
    for (const text of ["Time to first token", "120 ms", "Generation 620 ms · 10 tok/s", "Length limit", "Product · Team", "Agent", "3 cached input"]) expect(html).toContain(text);
    expect(html).toContain('href="/admin/logs/session?workspace_id=team&amp;session_id=conv+7%2Fa"');
    expect(html).not.toContain("/keys/key1");
    expect(html).toContain('href="/admin/logs"');
  });
});

describe("Logs URLs and formatting", () => {
  it("redirects old Requests URLs to Logs and routes Admin logs", () => {
    expect(parseDashboardLocation("/workspaces/team/requests?status=failed")).toMatchObject({ page: "requests", ws: "team", status: "failed" });
    expect(dashboardHref({ page: "requests", ws: "team", status: "failed" })).toBe("/workspaces/team/logs?status=failed");
    expect(parseDashboardLocation("/admin/logs?tab=sessions")).toMatchObject({ page: "platform-logs", tab: "sessions" });
    expect(parseDashboardLocation("/admin/logs/abc")).toMatchObject({ page: "platform-log-detail", record: "abc" });
    expect(parseDashboardLocation("/admin/logs/session?workspace_id=t&session_id=a%20b")).toMatchObject({ page: "platform-log-session", workspace_id: "t", session_id: "a b" });
    expect(navigation.find(n => n.page === "platform-logs")).toMatchObject({ label: "Logs", group: "Usage & spend" });
    expect(navigation.find(n => n.page === "requests")?.label).toBe("Logs");
  });
  it("keeps only valid log filters", () => {
    expect(dashboardSearch({ finish_reason: "length,bogus,stop", streamed: "true", session_id: " x" })).toMatchObject({ finish_reason: "length,stop", streamed: "true" });
    expect(dashboardSearch({ session_id: " x" }).session_id).toBeUndefined();
    expect(requestFilters({ finish_reason: "stop,length", session_id: "s" })).toMatchObject({ finish_reason: "stop,length", session_id: "s" });
    expect(String(requestQuery({ finish_reason: "error", streamed: "false", session_id: "s-1", workspace_id: "w" }).query)).toBe("finish_reason=error&streamed=false&session_id=s-1&workspace_id=w");
    expect(validSessionId("a".repeat(129))).toBe(false);
  });
  it("formats rates and speeds exactly, never inventing zero", () => {
    expect(rateText("0.0250")).toBe("2.5%"); expect(rateText("1.0000")).toBe("100%"); expect(rateText(null)).toBe("Unknown");
    expect(tpsText("41.25")).toBe("41.2 tok/s"); expect(tpsText("1200.00")).toBe("1,200 tok/s"); expect(tpsText(null)).toBe("Unknown");
    expect(ttftText(null, false)).toBe("Not streamed"); expect(ttftText(null, true)).toBe("Unknown");
  });
});

describe("Upstream model (reported by the provider, else configured)", () => {
  it("prefers the reported model and marks a configured fallback", () => {
    expect(servedModel({ upstream_model: "gpt-x", reported_upstream_model: "gpt-x-2026-01-01" })).toEqual({ id: "gpt-x-2026-01-01", configured: false, route: "gpt-x" });
    expect(servedModel({ upstream_model: "gpt-x", reported_upstream_model: "gpt-x" })).toEqual({ id: "gpt-x", configured: false });
    expect(servedModel({ upstream_model: "gpt-x", reported_upstream_model: null })).toEqual({ id: "gpt-x", configured: true });
    expect(servedModel({})).toBeNull();
    expect(servedModelText(servedModel({ upstream_model: "gpt-x" }))).toBe("gpt-x (configured)");
    expect(servedModelText(null)).toBe("Unknown");
  });
  it("shows it in the generations list", () => {
    const rows = [{ ...generation, reported_upstream_model: "openai/gpt-x-2026-01-01" }, { ...generation, execution_id: "e2e2e2e2-0000-0000-0000-000000000000", reported_upstream_model: null }];
    const html = markup(nav({ page: "requests", ws: "team", tab: "generations", cols: "none" }, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/generations?limit=50`, { data: rows, next_cursor: null }]]);
    const table = new DOMParser().parseFromString(html, "text/html").querySelector("table")!;
    const column = [...table.tHead!.rows[0]!.cells].findIndex(c => c.textContent === "Upstream model");
    expect(column).toBeGreaterThan(0);
    expect([...table.tBodies[0]!.rows].map(r => r.cells[column]!.textContent)).toEqual(["openai/gpt-x-2026-01-01", "gpt-x (configured)"]);
  });
  it("shows it on the request page and in each attempt", () => {
    const configured: RequestDetail = { ...row, workspace_id: "team", attempt_count: 1, attempts: [], prev_id: null, next_id: null };
    const html = markup(nav({ page: "platform-log-detail", record: row.root_request_id }, <PlatformRequestDetailPage session={admin} id={row.root_request_id} />), [[`/api/v1/platform/logs/requests/${row.root_request_id}`, configured]]);
    expect(html).toContain("Upstream model"); expect(html).toContain("(configured)");
    const reported = { ...configured, reported_upstream_model: "openai/gpt-x-2026-01-01" };
    const page = markup(nav({ page: "platform-log-detail", record: row.root_request_id }, <PlatformRequestDetailPage session={admin} id={row.root_request_id} />), [[`/api/v1/platform/logs/requests/${row.root_request_id}`, reported]]);
    expect(page).toContain("openai/gpt-x-2026-01-01"); expect(page).toContain("route gpt-x"); expect(page).not.toContain("(configured)");
    const base = { attempt_number: 1, execution_id: "x1", state: "succeeded", error_code: null, started_at: row.started_at, completed_at: row.completed_at, latency_ms: 10, deployment: { id: "d", upstream_model: "claude-route" }, connection: { id: "c", name: "Anthropic", provider: "anthropic" }, input_tokens: "1", output_tokens: "1", billing_usage: null, meter_usage: null, cost_microusd: "0", held_microusd: "0", accounting_state: "settled", unresolved_reason: null, price_id: null, pricing_version: null, failover_reason: null };
    const title = (a: typeof base & { reported_upstream_model?: string | null }) => new DOMParser().parseFromString(markup(<>{attemptItem(a, [a]).title}</>), "text/html").body.textContent;
    expect(title(base)).toBe("Anthropic · claude-route (configured)");
    expect(title({ ...base, reported_upstream_model: "claude-served-1" })).toBe("Anthropic · claude-served-1 · route claude-route");
  });
});

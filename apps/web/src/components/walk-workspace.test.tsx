// @vitest-environment jsdom
/*
 * Workspace portal walk (2026-10-08): regressions for the findings in
 * .local/enterprise-rebuild/walk-workspace.md (few words, one toolbar row,
 * each fact once, cards inside tabs).
 */
import { describe, expect, it } from "vitest";
import type { ReactNode } from "react";
import { DashboardNavigationProvider } from "./navigation-link";
import { countsText } from "./effective-access";
import { LogsPage } from "../pages/logs";
import { addModelText } from "../pages/model-catalog";
import { WorkspaceModelPage } from "../pages/workspace-model";
import { AuditHistory } from "../pages/organization";
import { Overview } from "../pages/overview";
import { Keys } from "../pages/keys";
import { reportQuery } from "../lib/reports";
import type { DashboardSearch } from "../lib/permissions";
import type { LogMetrics } from "../lib/requests";
import { grant, markup, member, model, personal, session, team, testClient } from "../lib/test-fixtures";

const ws = "/api/v1/workspaces/team";
const nav = (search: DashboardSearch, node: ReactNode) => <DashboardNavigationProvider search={search} navigate={() => {}}>{node}</DashboardNavigationProvider>;
const emptyMetrics: LogMetrics = { requests: "0", completed: "0", failed: "0", error_rate: null, latency_p50_ms: null, latency_p95_ms: null, avg_time_to_first_token_ms: null, ttft_requests: "0", tokens_per_second: null, input_tokens: "0", output_tokens: "0", unknown_token_requests: "0", known_cost_microusd: "0", held_microusd: "0", unresolved_requests: "0" };

describe("Logs toolbar and tiles", () => {
  it("keeps one toolbar row: Status is a compact select and Session/Key sit behind More filters", () => {
    const html = markup(nav({ page: "requests", ws: "team" }, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/logs/metrics`, emptyMetrics], [`${ws}/requests?limit=50`, { data: [], next_cursor: null }]]);
    expect(html).toContain("More filters");
    expect(html).toContain("Any status"); // a select, not six segmented buttons
    expect(html).not.toContain(">Unknown result</button>");
    expect(html).not.toContain('placeholder="Session ID"'); // in the More filters popover on a wide screen
    expect(html).not.toContain("Any key");
    expect(html).toContain('placeholder="Request ID"');
  });
  it("shows nothing-to-measure as a dash, not Unknown, and short hints", () => {
    const html = markup(nav({ page: "requests", ws: "team" }, <LogsPage scope={{ kind: "workspace", workspace: team }} />), [[`${ws}/logs/metrics`, emptyMetrics], [`${ws}/requests?limit=50`, { data: [], next_cursor: null }]]);
    expect(html).toContain("No streamed requests");
    expect(html).not.toContain("Average over 0 streamed requests");
    expect(html).not.toContain("Output tokens per second of generation");
    expect(html).not.toMatch(/>Unknown</);
    expect(html).not.toContain("Times are in your time zone");
  });
});

describe("Workspace model page", () => {
  const row = { model_id: model.id, public_name: "demo/local-chat", display_name: "demo/local-chat", description: null, protocols: ["chat_completions"], workload: "generation", eligibility: "selected", reason: "Added to this workspace from an available catalog", min_input_microusd_per_million: null, min_output_microusd_per_million: null, routes: 0, created_at: "2026-10-07T00:00:00Z" };
  const page = () => { const c = testClient(); c.setQueryData(["api", undefined, `${ws}/catalog`, "choices"], [row]); return markup(nav({ page: "workspace-model", ws: "team", record: model.id }, <WorkspaceModelPage session={session} workspace={team} id={model.id} />), [], c); };
  it("says each fact once: no repeated API name, no Pricing card duplicating the tiles, Created (not Added)", () => {
    const html = page(), doc = new DOMParser().parseFromString(html, "text/html");
    expect(doc.querySelector("h1")?.textContent).toBe("demo/local-chat");
    expect(html.match(/demo\/local-chat/g)!.length).toBeLessThan(6);
    expect([...doc.querySelectorAll("h2")].map(h => h.textContent)).not.toContain("Pricing");
    expect(html).not.toContain("In this workspace");
    expect(html).not.toContain(">Added</dt>"); expect(html).toContain("Created");
    expect(html).not.toContain("Unknown · not free");
    expect(html).toContain("Not priced yet");
  });
});

describe("Smaller copy fixes", () => {
  it("doesn't tell a Personal owner about members when adding a model", () => {
    expect(addModelText(personal)).not.toMatch(/member/i);
    expect(addModelText(team)).toMatch(/Members/);
    expect(addModelText(team)).not.toMatch(/allowlist|retired/);
  });
  it("leaves zero 'partly available' counts out", () => {
    expect(countsText({ available: 9, partial: 0, unavailable: 4 })).toBe("9 available · 4 unavailable");
    expect(countsText({ available: 2, partial: 1, unavailable: 0 })).toBe("2 available · 1 partly available");
  });
  it("renders the workspace audit log as a card inside Settings, not a second page title", () => {
    const html = markup(<AuditHistory session={session} workspace={team} />, [[`${ws}/audit?limit=50&offset=0`, { data: [], has_more: false }]]);
    const doc = new DOMParser().parseFromString(html, "text/html");
    expect(doc.querySelector("h1")).toBeNull();
    expect(doc.querySelector("section h2")?.textContent).toBe("Audit log");
  });
  it("API keys: with no keys at all, only the empty state (no search, filters or footer essay)", () => {
    const c = testClient(); c.setQueryData(["api", undefined, `${ws}/keys`, "choices"], []); c.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]); c.setQueryData(["api", undefined, `${ws}/members`, "choices"], []); c.setQueryData(["api", undefined, `${ws}/service-accounts`, "choices"], []);
    const html = markup(nav({ page: "keys", ws: "team" }, <Keys session={session} workspace={team} />), [], c);
    expect(html).toContain("No API keys yet");
    expect(html).not.toContain("Search keys");
    expect(html).not.toContain("revoked and expired keys can&#x27;t");
  });
  it("Overview: no 'Final cost not known yet' under a zero on-hold amount", () => {
    const report = { currency: "USD", totals: { known_cost_microusd: "0", held_microusd: "0", unresolved_attempts: "0", root_requests: "0", attempts: "0" }, breakdowns: { workspaces: [], models: [], keys: [] } };
    const html = markup(<Overview session={session} workspace={member} />, [[`${ws}/cost-report?${reportQuery({ ws: "team" }, false)}`, report], [`${ws}/requests?limit=5`, { data: [], next_cursor: null }]]);
    expect(html).toContain("On hold");
    expect(html).not.toContain("Final cost not known yet");
  });
});

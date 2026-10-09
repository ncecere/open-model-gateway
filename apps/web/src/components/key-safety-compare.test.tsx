// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { KeySafetyPage } from "../pages/key-safety";
import { Keys } from "../pages/workspace";
import { KeyDetail } from "../pages/key-detail";
import { ModelComparePage } from "../pages/model-compare";
import { WorkspaceModels } from "../pages/model-catalog";
import type { DashboardSearch } from "../lib/permissions";
import type { KeyRow, KeyStats } from "../lib/keys";
import type { SafetyReport } from "../lib/key-safety";
import type { CompareResponse } from "../lib/model-compare";
import { admin, grant, markup, model, session, team, testClient } from "../lib/test-fixtures";

afterEach(cleanup);
const ws = "/api/v1/workspaces/team";
const nav = (search: DashboardSearch, node: ReactNode) => <DashboardNavigationProvider search={search} navigate={() => {}}>{node}</DashboardNavigationProvider>;
const thresholds = { unused_days: 30, rotation_days: 180, max_lifetime_days: 90, broad_models: 5, never_used_grace_days: 7 };

describe("Admin › Key safety", () => {
  const report: SafetyReport = { scope: "platform", summary: { keys: 9, flagged: 3, high: 2, medium: 0, low: 1 }, personal: { keys: 40, flagged: 6, high: 1, medium: 2, low: 3 }, truncated: false, thresholds, data: [
    { key: { id: "k1", workspace: { id: "team", name: "Product", kind: "team" }, holder: "member", created_at: "2026-01-01T00:00:00Z", expires_at: null, last_used_at: null, model_restricted: false }, severity: "high", findings: [{ code: "no_limits", severity: "high" }, { code: "no_expiry", severity: "high" }, { code: "not_rotated", severity: "medium", days: 200 }, { code: "never_used", severity: "low", days: 200 }] },
    { key: { id: "k2", workspace: { id: "elsewhere", name: "Moonshot", kind: "project" }, holder: "service_account", created_at: "2026-01-01T00:00:00Z", expires_at: "2027-01-01T00:00:00Z", last_used_at: "2026-10-01T00:00:00Z", model_restricted: true }, severity: "low", findings: [{ code: "unused", severity: "low", days: 37 }] },
  ] };
  const page = (search: DashboardSearch = { page: "key-safety" }) => markup(nav(search, <KeySafetyPage session={admin} />), [["/api/v1/platform/key-safety", report]]);
  it("summarizes by severity, names shared keys by workspace and keeps personal keys as counts", () => {
    const html = page();
    for (const text of ["Key safety", "High", "Medium", "Low", "Personal keys", "of 40 need attention · counts only", "API key in Product", "API key in Moonshot", "Service account", "No limits", "No expiry", "Not rotated", "+1", "Unused"]) expect(html).toContain(text);
    expect(html).toMatch(/>2<\/[^>]+>/); // two high
  });
  it("routes a fix to the key page for workspaces you administer, else to the team/project page", () => {
    const html = page();
    expect(html).toContain('href="/workspaces/team/keys/k1?tab=limits"');
    expect(html).toContain(">Add budget<");
    expect(html).toContain('href="/admin/projects/elsewhere"');
    expect(html).toContain("Open project");
  });
  it("filters by severity from the URL", () => {
    const html = page({ page: "key-safety", severity: "low" });
    expect(html).toContain("API key in Moonshot");
    expect(html).not.toContain("API key in Product");
  });
});

const key: KeyRow = { id: "key1", name: "CI runner", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null, status: "active", last_used_at: null };
const keys: KeyRow[] = [key, { ...key, id: "key2", name: "Laptop" }];
const keyReport: SafetyReport = { scope: "workspace", summary: { keys: 2, flagged: 1, high: 1, medium: 0, low: 0 }, thresholds, data: [{ key: { id: "key1", name: "CI runner", workspace: { id: "team", name: "Product", kind: "team" }, holder: "you", issued_to_user_id: "me", service_account_id: null, created_at: key.created_at, expires_at: key.expires_at, last_used_at: null, model_restricted: false }, severity: "high", findings: [{ code: "expiry_beyond_max", severity: "medium", days: 26000 }, { code: "no_limits", severity: "high" }] }] };
const seed = () => { const c = testClient(); c.setQueryData(["api", undefined, `${ws}/keys`, "choices"], keys); c.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]); c.setQueryData(["api", undefined, `${ws}/service-accounts`, "choices"], []); c.setQueryData(["api", undefined, `${ws}/key-safety`], keyReport); return c; };

describe("API keys: risk badge and Needs attention", () => {
  it("badges keys with findings and offers the filter", () => {
    const html = markup(nav({ page: "keys", ws: "team" }, <Keys session={session} workspace={team} />), [], seed());
    expect(html).toContain("High risk");
    expect(html).toContain("Needs attention");
    expect(html).toContain("Laptop");
  });
  it("shows only keys with findings under Needs attention", () => {
    const html = markup(nav({ page: "keys", ws: "team", risk: "attention" }, <Keys session={session} workspace={team} />), [], seed());
    expect(html).toContain("CI runner");
    expect(html).not.toContain("Laptop");
  });
  it("lists the key's findings with fixes on its Overview", () => {
    const stats: KeyStats = { key_id: "key1", lineage_id: "key1", status: "active", last_used_at: null, daily: [], totals: { today: { spend_microusd: "0", held_microusd: "0", requests: "0", unresolved_attempts: "0" }, week: { spend_microusd: "0", held_microusd: "0", requests: "0", unresolved_attempts: "0" }, month: { spend_microusd: "0", held_microusd: "0", requests: "0", unresolved_attempts: "0" } }, budgets: [] };
    const c = seed(); c.setQueryData(["api", undefined, `${ws}/keys/key1`], key); c.setQueryData(["api", undefined, `${ws}/keys/key1/stats`], stats); c.setQueryData(["api", undefined, `${ws}/key-safety?key_id=key1`], keyReport);
    const html = markup(nav({ page: "key-detail", ws: "team", record: "key1" }, <KeyDetail session={session} workspace={team} id="key1" />), [], c);
    expect(html).toContain("Safety");
    expect(html).toContain("No budget or rate limit at any level.");
    expect(html).toContain("Expires in 26,000 days; new keys can last at most 90 days.");
    expect(html).toContain(">Set expiry<");
    expect(html).toContain('href="/workspaces/team/keys/key1?tab=limits"');
  });
});

describe("Model compare", () => {
  const metrics = (requests: string, error: string | null, latency: number | null) => ({ requests, completed: requests, failed: "0", error_rate: error, latency_p50_ms: latency, ttft_p50_ms: null, ttft_requests: "0", tokens_per_second: null });
  const response: CompareResponse = { scope: "workspace", activity: "own", period: { start: "2026-09-08T00:00:00Z", end: "2026-10-09T00:00:00Z" }, data: [
    { id: "a", public_name: "company/a", display_name: "Model A", enabled: true, protocols: ["chat_completions"], workload: "generation", routes: 1, enabled_routes: 1, priced_enabled_routes: 1, metrics: metrics("12", "0.0833", 900), price: { pricing_version: 3, created_at: "2026-01-01T00:00:00Z", input_token_limit: 200000, output_token_limit: 8000, display_lines: null, lines: [{ meter: "input_tokens", microusd_per_batch: "123457", batch: 1000000 }, { meter: "output_tokens", microusd_per_batch: "9007199254740993", batch: 1000000 }] } },
    { id: "b", public_name: "company/b", display_name: "Model B", enabled: true, protocols: ["chat_completions", "responses"], workload: "generation", routes: 1, enabled_routes: 1, priced_enabled_routes: 0, metrics: metrics("0", null, null), price: null },
  ] };
  const scope = { kind: "workspace" as const, workspace: team };
  it("shows exact prices, unknown (never zero) for unpriced models and marks the best known value", () => {
    const html = markup(nav({ page: "model-compare", ws: "team", ids: "a,b" }, <ModelComparePage scope={scope} ids={["a", "b"]} />), [[`${ws}/models/compare?ids=a,b`, response]]);
    for (const text of ["Compare models", "Model A", "Model B", "$0.123457", "$9,007,199,254.740993", "Unknown", "200,000 tokens", "8.3%", "900 ms", "last 30 days of your requests", "Back to Models"]) expect(html).toContain(text);
    expect(html).not.toContain("$0.00");
    // B has no price and no requests, so nothing is "best" by comparison with an unknown.
    expect(html).not.toContain("(best)");
  });
  it("asks for 2 to 4 models when the URL has fewer", () => {
    const html = markup(nav({ page: "model-compare", ws: "team" }, <ModelComparePage scope={scope} ids={["a"]} />));
    expect(html).toContain("Pick 2 to 4 models");
  });
  it("picks models with checkboxes on the Models page and opens Compare", async () => {
    const catalog = [
      { model_id: model.id, public_name: model.public_name, display_name: model.display_name, description: null, protocols: ["chat_completions"], workload: "generation", eligibility: "selected", reason: "", min_input_microusd_per_million: "100000", min_output_microusd_per_million: "400000", routes: 1 },
      { model_id: "other", public_name: "company/other", display_name: "Other", description: null, protocols: ["chat_completions"], workload: "generation", eligibility: "selected", reason: "", min_input_microusd_per_million: null, min_output_microusd_per_million: null, routes: 1 },
    ];
    const c = testClient(); c.setQueryData(["api", undefined, `${ws}/catalog`, "choices"], catalog); c.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]);
    render(<QueryClientProvider client={c}><ActionProvider><DashboardNavigationProvider search={{ page: "grants", ws: "team" }} navigate={() => {}}><WorkspaceModels session={session} workspace={team} /></DashboardNavigationProvider></ActionProvider></QueryClientProvider>);
    const user = userEvent.setup();
    await user.click(screen.getByRole("checkbox", { name: "Compare Smart model" }));
    expect(screen.getByText("1 selected")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Compare" }) as HTMLButtonElement).disabled).toBe(true);
    await user.click(screen.getByRole("checkbox", { name: "Compare Other" }));
    expect(screen.getByRole("link", { name: "Compare" }).getAttribute("href")).toBe("/workspaces/team/models/compare?ids=model%2Cother");
  });
});

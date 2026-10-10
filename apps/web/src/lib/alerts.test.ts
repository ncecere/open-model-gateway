import { describe, expect, it } from "vitest";
import { conditionText, draftOf, factorToPercent, kindsFor, layersFor, newDraft, parseEmails, parseThresholds, percentToFactor, recipientsText, ruleBody, ruleErrors, unknownCostNote, whereText, type AlertRule, type AlertScope } from "./alerts";
import { dashboardHref, parseDashboardLocation } from "./locations";
import { canView, dashboardSearch, type DashboardSearch } from "./permissions";
import { admin, auditor, member, personal, session, team } from "./test-fixtures";

const platform: AlertScope = { kind: "platform" }, workspace: AlertScope = { kind: "workspace", ws: "team" };
const rule: AlertRule = { id: "r1", scope: "installation", workspace_id: null, kind: "spend_spike", name: "Spike", enabled: true, budget_layers: null, thresholds: null, spike_factor_percent: 250, min_spend_microusd: "1500000", window_minutes: null, error_rate_percent: null, min_requests: null, consecutive_failures: null, provider_connection_id: null, provider_connection: null, notify_workspace_admins: false, notify_platform_admins: true, notify_emails: ["ops@example.com"], firing: 0, last_fired_at: null, created_at: "2026-10-08T00:00:00Z", updated_at: "2026-10-08T00:00:00Z" };

describe("alert rule form", () => {
  it("parses thresholds, multiples and addresses exactly", () => {
    expect(parseThresholds("100, 50 80%,80")).toEqual([50, 80, 100]);
    for (const bad of ["", "0", "101", "50.5", "1,2,3,4,5,6"]) expect(typeof parseThresholds(bad)).toBe("string");
    expect(factorToPercent("3")).toBe(300); expect(factorToPercent("2.5")).toBe(250); expect(factorToPercent("1.1x")).toBe(110); expect(factorToPercent("1000")).toBe(100000);
    for (const bad of ["1", "1.09", "1001", "2.555", "-3", "1e2"]) expect(typeof factorToPercent(bad)).toBe("string");
    for (const p of [110, 125, 250, 300, 100000]) expect(factorToPercent(percentToFactor(p))).toBe(p);
    expect(parseEmails(" A@Example.com, a@example.com;b@example.com ")).toEqual(["a@example.com", "b@example.com"]);
    expect(typeof parseEmails("not an address")).toBe("string");
  });
  it("builds exact bodies per kind and scope (integer micro-USD, no stray fields)", () => {
    const spike = { ...newDraft(platform, "spend_spike"), name: " Spike ", factor: "2.5", minSpend: "1.5" };
    expect(ruleErrors(spike)).toEqual({});
    expect(ruleBody(spike, platform)).toEqual({ name: "Spike", kind: "spend_spike", enabled: true, notify_platform_admins: true, notify_emails: [], spike_factor_percent: 250, min_spend_microusd: "1500000" });
    const budget = { ...newDraft(workspace), name: "Budgets", layers: ["key" as const, "local" as const], thresholds: "80,50" };
    expect(ruleBody(budget, workspace)).toEqual({ name: "Budgets", kind: "budget_threshold", enabled: true, notify_platform_admins: false, notify_workspace_admins: true, notify_emails: [], budget_layers: ["local", "key"], thresholds: [50, 80] });
    const upstream = { ...newDraft(platform, "provider_failing"), name: "Upstream", connection: "c1" };
    expect(ruleBody(upstream, platform)).toMatchObject({ kind: "provider_failing", window_minutes: 15, consecutive_failures: 5, provider_connection_id: "c1" });
    expect(ruleBody(upstream, platform)).not.toHaveProperty("error_rate_percent");
    expect(ruleErrors({ ...upstream, consecutive: "" }).consecutive).toBeTruthy();
    expect(ruleErrors({ ...newDraft(platform, "error_rate"), name: "x", window: "4" }).window).toBeTruthy();
    expect(ruleErrors({ ...spike, minSpend: "0" }).minSpend).toBeTruthy();
    expect(ruleErrors({ ...spike, minSpend: "0.0000001" }).minSpend).toBeTruthy();
    expect(ruleErrors({ ...budget, layers: [] }).layers).toBeTruthy();
    // Installation spend: exact micro-USD from dollars (BigInt), never a float.
    const spend = { ...newDraft(platform, "spend_threshold"), name: "Total", spendPeriod: "lifetime" as const, spendAmount: "9007199254.740993", thresholds: "100,80" };
    expect(ruleErrors(spend)).toEqual({});
    expect(ruleBody(spend, platform)).toEqual({ name: "Total", kind: "spend_threshold", enabled: true, notify_platform_admins: true, notify_emails: [], spend_period: "lifetime", spend_amount_microusd: "9007199254740993", thresholds: [80, 100] });
    expect(ruleErrors({ ...spend, spendAmount: "0" }).spendAmount).toBeTruthy();
    expect(ruleErrors({ ...spend, spendAmount: "" }).spendAmount).toBeTruthy();
    // Only installation rules watch installation spend; no scope offers an installation budget layer.
    expect(kindsFor(workspace)).not.toContain("spend_threshold"); expect(kindsFor(platform)).toContain("spend_threshold");
    expect(layersFor(platform)).toEqual(["type", "override", "local", "key"]);
    expect(conditionText({ ...rule, kind: "spend_threshold", thresholds: [80, 100], spend_period: "month", spend_amount_microusd: "5000000000" })).toBe("80/100% of $5,000.00 monthly spend");
    expect(draftOf({ ...rule, kind: "spend_threshold", thresholds: [80], spend_period: "week", spend_amount_microusd: "1500000" })).toMatchObject({ spendPeriod: "week", spendAmount: "1.50", thresholds: "80" });
  });
  it("round trips a stored rule into the form", () => {
    const d = draftOf(rule);
    expect(d).toMatchObject({ name: "Spike", factor: "2.5", minSpend: "1.50", emails: "ops@example.com", notifyPlatformAdmins: true });
    expect(ruleBody(d, platform)).toMatchObject({ spike_factor_percent: 250, min_spend_microusd: "1500000", notify_emails: ["ops@example.com"] });
  });
  it("describes rules and incidents in a few words, without private details", () => {
    expect(conditionText(rule)).toBe("Last hour ≥ 2.5× hourly average, at least $1.50");
    expect(conditionText({ ...rule, kind: "budget_threshold", budget_layers: ["local", "key"], thresholds: [50, 80, 100] })).toBe("50/80/100% of Workspace, API keys budgets");
    expect(recipientsText(rule)).toBe("Platform admins + 1 address");
    expect(recipientsText({ ...rule, notify_platform_admins: false, notify_emails: [] })).toBe("In-app only");
    expect(whereText({ builtin: true, connection: null, workspace: { id: "p", name: "Alex's workspace", kind: "personal" } })).toBe("Personal workspace");
    expect(whereText({ builtin: false, connection: null, workspace: null })).toBe("Installation");
    expect(unknownCostNote({ details: { unknown_cost_requests: 2 } })).toMatch(/2 requests have unknown cost/);
    expect(unknownCostNote({ details: { unknown_cost_requests: 0 } })).toBeUndefined();
  });
});

describe("alert locations and visibility", () => {
  const pairs: [DashboardSearch, string][] = [
    [{ page: "settings-alerts" }, "/admin/settings/alerts"], [{ page: "settings-alerts", tab: "history", status: "firing" }, "/admin/settings/alerts?tab=history&status=firing"],
    [{ page: "alert-rule-detail", record: "new" }, "/admin/alerts/new"], [{ page: "alert-rule-detail", record: "r1" }, "/admin/alerts/r1"],
    [{ page: "workspace-alert-detail", ws: "team", record: "r1" }, "/workspaces/team/alerts/r1"], [{ page: "notifications", status: "unread" }, "/notifications?status=unread"],
  ];
  it.each(pairs)("round trips %j", (search, href) => { expect(dashboardHref(search)).toBe(href); expect(parseDashboardLocation(href)).toMatchObject(search); });
  it("keeps the new tabs and filters", () => { expect(dashboardSearch({ tab: "alerts", status: "resolved" })).toMatchObject({ tab: "alerts", status: "resolved" }); });
  it("shows rule pages only to those who manage them", () => {
    expect(canView("settings-alerts", admin)).toBe(true); expect(canView("settings-alerts", auditor)).toBe(true); expect(canView("settings-alerts", session)).toBe(false);
    expect(canView("workspace-alert-detail", session, team)).toBe(true);
    expect(canView("workspace-alert-detail", session, member)).toBe(false);
    expect(canView("workspace-alert-detail", session, personal)).toBe(false);
    expect(canView("notifications", session)).toBe(true);
  });
});

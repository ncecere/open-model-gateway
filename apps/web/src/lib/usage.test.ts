import { describe, expect, it } from "vitest";
import { chartLabeler, chartNumber, compareDecimal, customRangeError, decimalChange, dimensionOptions, divideExact, exactInteger, exploreCsv, exploreQuery, formatDecimalMicroUsd, formatMetric, formatRatioPercent, groupStats, meanDecimal, parseDecimal, periodLabel, periodToSearch, pivotFromUsageSearch, pivotToUsageSearch, recordCostText, recordStatusText, recordsQuery, statusList, usageContext, usageFilters, usagePeriod, usageQuery, visibleBudgets, type ExploreResponse } from "./usage";
import { dashboardHref, parseDashboardLocation } from "./locations";
import { personal, team, member } from "./test-fixtures";

const now = new Date("2026-10-08T12:00:00Z");
const explore = (over: Partial<ExploreResponse> = {}): ExploreResponse => ({ metric: "spend", group_by: "model", then_by: null, period: { start_date: "2026-10-06", end_date: "2026-10-09" }, total: { value: "9007199254740996", held_microusd: "0", unresolved_attempts: "0" }, rows: [{ group: { id: "a", name: "model-a" }, then: null, value: "9007199254740993", share: "1", held_microusd: "0", unresolved_attempts: "0" }, { group: { id: "b", name: "model-b" }, then: null, value: "3", share: "0", held_microusd: "0", unresolved_attempts: "0" }], other: null, truncated: false, series: [{ date: "2026-10-06", values: [{ id: "a", value: "9007199254740993" }, { id: "b", value: "1" }] }, { date: "2026-10-07", values: [{ id: "a", value: "0" }, { id: "b", value: "0" }] }, { date: "2026-10-08", values: [{ id: "a", value: "0" }, { id: "b", value: "2" }] }], ...over });

describe("usage period (URL-backed, UTC)", () => {
  it("resolves presets to exclusive API end dates and inclusive labels", () => {
    expect(usagePeriod({}, now)).toMatchObject({ preset: "month", start_date: "2026-10-01", end_date: "2026-10-09", last_date: "2026-10-08", days: 8, partial: true });
    expect(usagePeriod({ range: "7d" }, now)).toMatchObject({ start_date: "2026-10-02", end_date: "2026-10-09", days: 7 });
    expect(usagePeriod({ range: "30d" }, now)).toMatchObject({ start_date: "2026-09-09", days: 30 });
    expect(usagePeriod({ range: "custom", start_date: "2026-09-01", end_date: "2026-10-01" }, now)).toMatchObject({ preset: "custom", last_date: "2026-09-30", partial: false });
    expect(periodLabel(usagePeriod({}, now))).toBe("Oct 1 – Oct 8, 2026 (UTC, today so far)");
    expect(periodLabel(usagePeriod({ range: "custom", start_date: "2025-12-31", end_date: "2026-01-02" }, now))).toBe("Dec 31, 2025 – Jan 1, 2026 (UTC)");
  });
  it("validates custom ranges inline and round-trips through the URL", () => {
    expect(customRangeError("2026-10-05", "2026-10-01", now)).toContain("on or after");
    expect(customRangeError("2026-10-01", "2026-10-09", now)).toContain("future");
    expect(customRangeError("2026-01-01", "2026-06-01", now)).toBe("Pick up to 93 days.");
    expect(() => usagePeriod({ range: "custom", start_date: "2026-01-01", end_date: "2026-06-01" }, now)).toThrow("93");
    expect(periodToSearch("custom", { start: "2026-09-01", last: "2026-09-30" })).toEqual({ range: "custom", start_date: "2026-09-01", end_date: "2026-10-01" });
    expect(periodToSearch("month")).toEqual({ range: undefined, start_date: undefined, end_date: undefined });
    const href = dashboardHref({ page: "costs", ws: "team", tab: "chart", metric: "spend", range: "7d" });
    expect(href).toBe("/workspaces/team/costs?tab=chart&range=7d&metric=spend");
    expect(parseDashboardLocation(href)).toMatchObject({ page: "costs", ws: "team", tab: "chart", metric: "spend", range: "7d" });
    const records = parseDashboardLocation("/workspaces/team/costs?tab=records&cost_status=on_hold&top=25&group=member&then=model&status=failed,succeeded,bogus&model_id=m1&key_id=k1");
    expect(records).toMatchObject({ tab: "records", cost_status: "on_hold", top: "25", group: "member", then: "model", status: "failed,succeeded", model_id: "m1", key_id: "k1" });
    // The former merged "not final yet" view is gone; old links simply drop it.
    expect(parseDashboardLocation("/workspaces/team/costs?cost_status=unresolved")).not.toHaveProperty("cost_status");
    expect(dashboardHref({ page: "costs", ws: "team", model_id: "m1", status: "failed,cancelled" })).toBe("/workspaces/team/costs?model_id=m1&status=failed%2Ccancelled");
    expect(parseDashboardLocation("/workspaces/team/costs?cost_status=bogus&metric=DROP%20TABLE")).not.toHaveProperty("cost_status");
    expect(dashboardHref({ page: "platform-costs", tab: "explore", group: "workspace" })).toBe("/admin/costs?tab=explore&group=workspace");
  });
});

describe("exact money and measures", () => {
  it("formats decimal micro-USD rates and sub-cent amounts exactly, never as $0", () => {
    expect(formatDecimalMicroUsd("1234.5")).toBe("$0.0012345");
    expect(formatDecimalMicroUsd("12400000")).toBe("$12.40");
    expect(formatDecimalMicroUsd("9007199254740993.25")).toBe("$9,007,199,254.74099325");
    expect(formatDecimalMicroUsd("0.5")).toBe("$0.0000005");
    expect(formatDecimalMicroUsd(null)).toBe("Unknown");
    expect(formatDecimalMicroUsd("1e3")).toBe("Unknown");
    expect(formatMetric("spend", "81")).toBe("$0.000081");
    expect(formatMetric("spend", null)).toBe("Unknown");
    expect(formatMetric("tokens", "1234567")).toBe("1,234,567");
  });
  it("formats rates and their changes exactly", () => {
    expect(formatRatioPercent("0.2513")).toBe("25.13%");
    expect(formatRatioPercent("1")).toBe("100%");
    expect(formatRatioPercent("0.0001")).toBe("0.01%");
    expect(formatRatioPercent(null)).toBeNull();
    expect(compareDecimal("0.25", "0.2500")).toBe(0);
    expect(decimalChange("0.3", "0.25", "0.2")).toEqual({ direction: "up", text: "+20%" });
    expect(decimalChange("0.2", "0.25", "-0.2")).toEqual({ direction: "down", text: "−20%" });
    expect(decimalChange("0.2", "0", null)).toEqual({ direction: "up", text: "New" });
    expect(decimalChange(null, "0.25", null)).toBeNull();
    expect(decimalChange("0.25", null, null)).toBeNull();
  });
  it("divides and reads decimal strings without floating point", () => {
    expect(exactInteger("9007199254740993")).toBe(9007199254740993n);
    expect(exactInteger("1.5")).toBeNull(); expect(exactInteger(null)).toBeNull(); expect(exactInteger("1e3")).toBeNull();
    expect(parseDecimal("12.50")).toEqual({ units: 1250n, scale: 2 }); expect(parseDecimal("-1")).toBeNull(); expect(parseDecimal(null)).toBeNull();
    expect(meanDecimal(["0.25", "0.5"], 4)).toEqual({ value: "0.375", exact: true });
    expect(meanDecimal(["0.1", "0.2", "0.2"], 4)).toEqual({ value: "0.1666", exact: false });
    expect(meanDecimal(["9007199254740993", "9007199254740995"], 0)).toEqual({ value: "9007199254740994", exact: true });
    expect(divideExact(10n, 3n, 1)).toEqual({ value: "3.3", exact: false });
    expect(divideExact(9n, 3n, 0)).toEqual({ value: "3", exact: true });
  });
  it("computes per-model Min/Max/Avg/Total exactly from decimal-string series, beyond 2^53", () => {
    const [a, b] = groupStats(explore(), "spend", 3);
    expect(a).toMatchObject({ name: "model-a", min: "$0.00", max: "$9,007,199,254.740993", total: "$9,007,199,254.740993", avg: "$3,002,399,751.580331" });
    expect(b).toMatchObject({ name: "model-b", min: "$0.00", max: "$0.000002", avg: "$0.000001", total: "$0.000003" });
    const [r] = groupStats(explore({ metric: "requests", total: { value: "10", held_microusd: null, unresolved_attempts: null }, rows: [{ group: { id: "a", name: "a" }, then: null, value: "10", share: "1", held_microusd: null, unresolved_attempts: null }], series: [{ date: "d1", values: [{ id: "a", value: "3" }] }, { date: "d2", values: [{ id: "a", value: "7" }] }, { date: "d3", values: [] }] }), "requests", 3);
    expect(r).toMatchObject({ min: "0", max: "7", avg: "≈ 3.3", total: "10" });
    const [rate] = groupStats(explore({ metric: "cache_hit_rate", rows: [{ group: { id: "a", name: "a" }, then: null, value: "0.25", share: null, held_microusd: null, unresolved_attempts: null }], series: [{ date: "d1", values: [{ id: "a", value: "0.5" }] }, { date: "d2", values: [{ id: "a", value: null }] }, { date: "d3", values: [{ id: "a", value: "0.2" }] }] }), "cache_hit_rate", 3);
    expect(rate).toMatchObject({ min: "20%", max: "50%", avg: "35%", total: "25%" });
  });
  it("draws with Numbers but labels drawn values with the exact strings", () => {
    const format = chartLabeler("spend", ["9007199254740993", "3", null]);
    expect(format(chartNumber("spend", "9007199254740993"))).toBe("$9,007,199,254.740993");
    expect(format(3)).toBe("$0.000003");
    expect(format(Number.NaN)).toBe("No data");
    // Two exact values that draw at the same coordinate can't be told apart: no false precision.
    expect(chartLabeler("spend", ["9007199254740993", "9007199254740992"])(9007199254740992)).toBe("Too large to chart exactly");
    expect(chartLabeler("cache_hit_rate", ["0.2513"])(chartNumber("cache_hit_rate", "0.2513"))).toBe("25.13%");
    expect(chartNumber("tokens", null)).toBeNaN();
  });
  it("exports the explore table exactly and neutralizes spreadsheet formulas", () => {
    const csv = exploreCsv(explore({ rows: [{ group: { id: "x", name: "=HYPERLINK(\"evil\")" }, then: null, value: "1", share: "0.0001", held_microusd: "5", unresolved_attempts: "1" }], other: { value: "2", share: "0.5" } }), { start_date: "2026-10-06", last_date: "2026-10-08" });
    expect(csv.split("\r\n")[0]).toBe('"period_start","period_end_inclusive","model","spend_usd","spend_microusd","share_of_total","on_hold_microusd","cost_unknown_attempts"');
    expect(csv).toContain('"\'=HYPERLINK(""evil"")","0.000001","1","0.0001","5","1"');
    expect(csv).toContain('"Other","0.000002","2","0.5"');
  });
});

describe("records and privacy", () => {
  it("shows final cost, holds as floors and unknown, never a fabricated $0", () => {
    expect(recordCostText({ cost_microusd: "9", active_held_microusd: "0" })).toBe("$0.000009");
    expect(recordCostText({ cost_microusd: null, active_held_microusd: "1900" })).toBe("Unknown · $0.0019 on hold");
    expect(recordCostText({ cost_microusd: null, active_held_microusd: "0" })).toBe("Unknown");
    expect(recordCostText({ cost_microusd: "0", active_held_microusd: null })).toBe("$0.00");
    expect(recordStatusText({ cost_status: "unknown", cost_microusd: null, unresolved_reason: "missing_billing_usage" })).toBe("Cost unknown: provider didn't report usage");
    expect(recordStatusText({ cost_status: "pending", cost_microusd: null, unresolved_reason: null })).toBe("On hold");
  });
  it("builds one record query with every filter (key and several statuses included)", () => {
    const period = { start_date: "2026-10-01", end_date: "2026-10-09" };
    const f = { model: "m", member: "u", key_id: "k", status: ["failed" as const, "cancelled" as const], service_account_id: "sa", cost_center_id: "unallocated", workspace_id: "w", cost_status: "on_hold" as const };
    expect(recordsQuery(period, f, { platform: false, limit: 50, offset: 50 })).toBe("start_date=2026-10-01&end_date=2026-10-09&model=m&key_id=k&actor_user_id=u&status=failed%2Ccancelled&cost_center_id=unallocated&service_account_id=sa&accounting_status=pending&limit=50&offset=50");
    expect(recordsQuery(period, f, { platform: true })).toBe("start_date=2026-10-01&end_date=2026-10-09&model=m&key_id=k&actor_user_id=u&status=failed%2Ccancelled&cost_center_id=unallocated&service_account_id=sa&workspace_id=w&accounting_status=pending");
    expect(recordsQuery(period, { status: [] }, { platform: false })).toBe("start_date=2026-10-01&end_date=2026-10-09");
  });
  it("sends overview/explore filters under the API's names and never a member filter without workspace-wide visibility", () => {
    const search = { model_id: "m", key_id: "k", actor_user_id: "u", status: "in_progress,failed", cost_center_id: "cc", service_account_id: "sa" };
    expect(statusList("in_progress,failed,bogus,failed")).toEqual(["failed", "in_progress"]);
    const team_ = usageFilters(search, usageContext(team));
    expect(usageQuery({ start_date: "a", end_date: "b" }, undefined, team_)).toBe("start_date=a&end_date=b&model_id=m&key_id=k&member_user_id=u&status=failed%2Cin_progress&cost_center_id=cc&service_account_id=sa");
    expect(usageFilters(search, usageContext(member)).member).toBeUndefined();
    expect(usageFilters(search, usageContext(personal)).member).toBeUndefined();
    expect(exploreQuery({ start_date: "a", end_date: "b" }, { metric: "spend", groupBy: "model", thenBy: "none", top: 10 }, "w", usageFilters(search, usageContext(member)))).not.toContain("member");
  });
  it("never offers member breakdowns to members or in Personal", () => {
    expect(usageContext(personal).members).toBe(false);
    expect(usageContext(member).members).toBe(false);
    expect(usageContext(team).members).toBe(true);
    expect(usageContext()).toEqual({ platform: true, members: true });
    expect(dimensionOptions(usageContext(member)).map(d => d.value)).toEqual(["model", "key", "provider", "day"]);
    expect(dimensionOptions(usageContext(team)).map(d => d.value)).toEqual(["model", "key", "member", "provider", "day"]);
    const pivot = pivotFromUsageSearch({ group: "member", then: "member", metric: "tokens", top: "25" }, usageContext(member));
    expect(pivot).toEqual({ metric: "tokens", groupBy: "model", thenBy: "none", top: 25 });
    expect(exploreQuery({ start_date: "a", end_date: "b" }, pivot)).not.toContain("member");
    expect(pivotFromUsageSearch({ group: "model", then: "model" }, usageContext(team)).thenBy).toBe("none");
    expect(pivotToUsageSearch({ metric: "spend", groupBy: "model", thenBy: "none", top: 10 })).toEqual({ metric: undefined, group: undefined, then: undefined, top: undefined });
  });
  it("shows budget meters only where workspace-wide usage is visible", () => {
    const windows = [{ layer: "local" as const, period: "month" as const, amount_microusd: "5", usage_visible: true, used_microusd: "1" }, { layer: "platform" as const, period: "day" as const, amount_microusd: "9", usage_visible: true, used_microusd: "1" }, { layer: "platform" as const, period: "week" as const, amount_microusd: "9", usage_visible: false, used_microusd: null }, { layer: "key" as const, period: "day" as const, amount_microusd: "1", usage_visible: true, used_microusd: "0" }];
    expect(visibleBudgets(windows).map(w => `${w.layer}:${w.span}`)).toEqual(["platform:day", "local:month"]);
  });
});

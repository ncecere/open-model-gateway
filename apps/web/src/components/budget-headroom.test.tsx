import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { BudgetMeters } from "./scope-limits";
import { KeyUsage } from "../pages/key-detail";
import { markup, team } from "../lib/test-fixtures";
import { effectiveBudgetHeadroom, windowHeadroom, type BudgetWindow } from "../lib/governance";

const window = (patch: Partial<BudgetWindow> = {}): BudgetWindow => ({ layer: "platform", period: "month", budget_period: "month", amount_microusd: "9007199254740993", monthly_budget_microusd: "9007199254740993", window_start: "2026-01-01T00:00:00Z", window_end: "2026-02-01T00:00:00Z", usage_visible: true, used_microusd: "9007199254740990", unresolved_usage: false, settled_microusd: "9007199254740980", pending_held_microusd: "10", unknown_held_microusd: "0", unknown_cost_requests: "0", remaining_microusd: "3", headroom_status: "known", ...patch });

describe("budget headroom", () => {
  it("takes min remaining, never min limit minus key usage or overlapping sums", () => {
    const windows = [window(), window({ layer: "key", amount_microusd: "100", used_microusd: "5", settled_microusd: "5", pending_held_microusd: "0", remaining_microusd: "95" }), window({ layer: "local", period: "day", budget_period: "day", remaining_microusd: "100" })];
    expect(effectiveBudgetHeadroom(windows)).toEqual([{ period: "day", status: "known", remaining_microusd: "100" }, { period: "month", status: "known", remaining_microusd: "3" }]);
    expect(effectiveBudgetHeadroom([])).toEqual([]);
    expect(effectiveBudgetHeadroom([window({ remaining_microusd: "9007199254740993" }), window({ layer: "key", remaining_microusd: "9007199254740992" })])[0].remaining_microusd).toBe("9007199254740992");
  });
  it("never infers hidden parents or a proven upper bound from retained floors", () => {
    const unknown = window({ headroom_status: "unknown", unknown_cost_requests: "1", unknown_held_microusd: "10", remaining_microusd: null });
    expect(windowHeadroom(unknown).status).toBe("unknown");
    expect(effectiveBudgetHeadroom([unknown, window({ layer: "key" })])[0].status).toBe("unknown");
    expect(effectiveBudgetHeadroom([unknown, window({ layer: "key", headroom_status: undefined })])[0].status).toBe("unknown");
    expect(effectiveBudgetHeadroom([unknown, window({ layer: "local", usage_visible: false, headroom_status: "restricted", remaining_microusd: null })])[0]).toMatchObject({ status: "restricted", remaining_microusd: null });
  });
  it("keeps an explicitly invalid window unknown without fabricating or hiding neighboring facts", () => {
    const invalid = window({ period: "day", budget_period: "day", headroom_status: "unknown", used_microusd: null, settled_microusd: null, pending_held_microusd: null, unknown_held_microusd: null, unknown_cost_requests: null, remaining_microusd: null, unresolved_usage: null, exhausted: null });
    expect(windowHeadroom(invalid)).toEqual({ status: "unknown", remaining_microusd: null });
    expect(effectiveBudgetHeadroom([invalid, window()])).toEqual([{ period: "day", status: "unknown", remaining_microusd: null }, { period: "month", status: "known", remaining_microusd: "3" }]);
    expect(effectiveBudgetHeadroom([invalid, window({ period: "day", budget_period: "day", layer: "key" })])[0].status).toBe("unknown");
    const html = renderToStaticMarkup(<BudgetMeters kind="team" windows={[invalid, window()]} />);
    expect(html).toContain("Daily · Remaining: Unknown");
    expect(html).toContain("Monthly · Remaining: $0.000003");
    expect(html).not.toContain("Restricted");
    expect(html).not.toContain("Headroom unavailable");
    expect(html).not.toContain("Settled: Unknown");
    expect(html).not.toContain("Unknown-cost requests: </p>");
    const invalidOnly = renderToStaticMarkup(<BudgetMeters kind="team" windows={[invalid]} />);
    for (const untrusted of ["Settled:", "Pending holds:", "Retained unknown-cost floors:", "Unknown-cost requests:", "Remaining: $"]) expect(invalidOnly).not.toContain(untrusted);
    const key = markup(<KeyUsage workspace={team} keyId="key" part="budgets" />, [["/api/v1/workspaces/team/keys/key/stats", { daily: [], budgets: [invalid, window()] }]]);
    expect(key).toContain("Daily · Remaining: Unknown");
    expect(key).toContain("Monthly · Remaining: $0.000003");
    expect(key).not.toContain("Settled: Unknown");
  });
  it("old or incomplete gateway facts are unavailable, never reconstructed", () => {
    expect(windowHeadroom(window({ headroom_status: undefined })).status).toBe("unavailable");
    expect(windowHeadroom(window({ pending_held_microusd: undefined })).status).toBe("unavailable");
    expect(windowHeadroom(window({ remaining_microusd: null })).status).toBe("unavailable");
    expect(windowHeadroom(window({ unknown_cost_requests: "1" })).status).toBe("unknown");
    expect(windowHeadroom(window({ settled_microusd: "1e3" })).status).toBe("unavailable");
    expect(windowHeadroom(window({ remaining_microusd: "-1" })).status).toBe("unavailable");
    expect(effectiveBudgetHeadroom([window(), window({ layer: "key", pending_held_microusd: undefined })])[0].status).toBe("unavailable");
  });
  it("shows the same facts and effective result beside the key's existing budget rings", () => {
    const html = markup(<KeyUsage workspace={team} keyId="key" part="budgets" />, [["/api/v1/workspaces/team/keys/key/stats", { daily: [], budgets: [window()] }]]);
    expect(html).toContain("Settled:");
    expect(html).toContain("Remaining: $0.000003");
    expect(html).toContain("Monthly · Remaining: $0.000003");
    expect(html).toContain('role="meter"');
  });
  it("renders exact breakdowns, finite floors, restricted and unavailable states", () => {
    const html = renderToStaticMarkup(<BudgetMeters kind="team" windows={[window(), window({ layer: "key", period: "week", budget_period: "week", headroom_status: "unknown", unknown_cost_requests: "9007199254740993", unknown_held_microusd: "10", remaining_microusd: null }), window({ layer: "local", period: "day", budget_period: "day", usage_visible: false, headroom_status: "restricted" })]} />);
    for (const label of ["Settled", "Pending holds", "Retained unknown-cost floors", "$9,007,199,254.74098", "Remaining: $0.000003", "Remaining: Unknown", "Remaining: Restricted", "9007199254740993", "Effective headroom", "not an upper bound"]) expect(html).toContain(label);
    const old = renderToStaticMarkup(<BudgetMeters kind="team" windows={[window({ headroom_status: undefined })]} />);
    expect(old).toContain("Headroom unavailable");
    expect(old).not.toContain("Remaining: $0.000003");
    expect(renderToStaticMarkup(<BudgetMeters kind="team" windows={[]} />)).toBe("");
    const restricted = renderToStaticMarkup(<BudgetMeters kind="team" windows={[window({ usage_visible: false, settled_microusd: "123456789", remaining_microusd: "987654321" })]} />);
    expect(restricted).toContain("Remaining: Restricted");
    expect(restricted).not.toContain("$123.456789");
    expect(restricted).not.toContain("$987.654321");
  });
});

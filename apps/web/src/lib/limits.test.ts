import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { composeLimits, draftErrors, draftLimits, draftOf, hasErrors, limitsBody, limitsOf, limitsSaveError, limitsSummary, lockedBudget, mergeErrors, noLimits, policyBudgets, policyRejection, rateRows, rejectionErrors, resetText, type Limits, type LimitsDraft } from "./limits";

const base = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null };
const limits = (patch: Partial<Limits>): Limits => ({ ...noLimits, ...patch });
const draft = (budgets: [string, "day" | "week" | "month" | "lifetime", string][], rates: Partial<Record<"requests_per_minute" | "tokens_per_minute" | "concurrent_requests", string>> = {}): LimitsDraft => ({ requests_per_minute: "", tokens_per_minute: "", concurrent_requests: "", concurrent_jobs: "", ...rates, budgets: budgets.map(([key, period, amount]) => ({ key, period, amount })) });

describe("stacked budgets", () => {
  it("reads stacked budgets in period order, and the legacy single budget of older gateways", () => {
    expect(policyBudgets({ ...base, monthly_budget_microusd: "1", budgets: [{ period: "lifetime", amount_microusd: "9" }, { period: "day", amount_microusd: "1" }] })).toEqual([{ period: "day", amount_microusd: "1" }, { period: "lifetime", amount_microusd: "9" }]);
    expect(policyBudgets({ ...base, monthly_budget_microusd: "5", budget_period: "week" })).toEqual([{ period: "week", amount_microusd: "5" }]);
    expect(policyBudgets(base)).toEqual([]);
  });
  it("composes rates by minimum and budgets by minimum per period; other periods all apply", () => {
    const platform = limits({ requests_per_minute: 60, budgets: [{ period: "month", amount_microusd: "100000000" }] });
    const local = limits({ requests_per_minute: 80, budgets: [{ period: "month", amount_microusd: "9007199254740993" }, { period: "day", amount_microusd: "5000000" }] });
    expect(composeLimits(platform, local)).toEqual({ requests_per_minute: 60, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, budgets: [{ period: "day", amount_microusd: "5000000" }, { period: "month", amount_microusd: "100000000" }] });
    expect(composeLimits(limits({ budgets: [{ period: "month", amount_microusd: "9007199254740993" }] }), limits({ budgets: [{ period: "month", amount_microusd: "9007199254740992" }] })).budgets[0]!.amount_microusd).toBe("9007199254740992");
  });
  it("sends the full set with exact BigInt money, never the legacy fields", () => {
    const body = limitsBody(draftLimits(draft([["a", "lifetime", "0.000001"], ["b", "week", "9007199254.740993"]], { requests_per_minute: "5" })));
    expect(body).toEqual({ requests_per_minute: 5, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, budgets: [{ period: "week", amount_microusd: "9007199254740993" }, { period: "lifetime", amount_microusd: "1" }] });
    expect(body).not.toHaveProperty("monthly_budget_microusd");
    expect(draftOf(limitsOf({ ...base, budgets: [{ period: "day", amount_microusd: "1500000" }] })).budgets).toEqual([{ key: "saved-day", period: "day", amount: "1.50" }]);
  });
  it("uses plain-language reset rules", () => {
    expect(["day", "week", "month", "lifetime"].map(p => resetText(p as "day"))).toEqual(["Resets daily at 00:00 UTC", "Resets weekly on Monday at 00:00 UTC", "Resets monthly on the 1st at 00:00 UTC", "Never resets"]);
    expect(limitsSummary(limits({ requests_per_minute: 60, budgets: [{ period: "day", amount_microusd: "5000000" }] }))).toBe("60 RPM · $5.00 daily");
  });
});
describe("stacked budget validation (mirrors the server's tighten-only rules)", () => {
  const parents = [{ label: "Team default", limits: limits({ requests_per_minute: 60, budgets: [{ period: "month", amount_microusd: "100000000" }] }) }];
  it("checks syntax and one budget per period in free mode", () => {
    const e = draftErrors(draft([["a", "month", "0"], ["b", "month", "5"], ["c", "day", ""], ["d", "week", "1.1234567"]], { requests_per_minute: "1e3" }), "free");
    expect(e.rates.requests_per_minute).toBeDefined();
    expect(e.budgets.a).toContain("more than");
    expect(e.budgets.b).toContain("Only one monthly budget");
    expect(e.budgets.c).toContain("Enter an amount");
    expect(e.budgets.d).toBeDefined();
    expect(hasErrors(draftErrors(draft([["a", "month", "500"]]), "free", parents))).toBe(false);
  });
  it("rejects a child above its parent for the same period only; different periods are independent", () => {
    expect(draftErrors(draft([["a", "month", "100.000001"]]), "tighten", parents).budgets.a).toBe("Can't be higher than the Team default monthly budget ($100.00).");
    expect(hasErrors(draftErrors(draft([["a", "month", "100"]]), "tighten", parents))).toBe(false);
    expect(hasErrors(draftErrors(draft([["a", "day", "1000"], ["b", "lifetime", "5000"]]), "tighten", parents))).toBe(false);
    expect(draftErrors(draft([], { requests_per_minute: "61" }), "tighten", parents).rates.requests_per_minute).toBe("Can't be higher than the Team default limit (60).");
  });
  it("never raises or removes a saved cap", () => {
    const stored = limits({ requests_per_minute: 10, budgets: [{ period: "week", amount_microusd: "5000000" }] });
    const raised = draftErrors(draft([["saved-week", "week", "5.01"]], { requests_per_minute: "11" }), "tighten", [], stored);
    expect(raised.budgets["saved-week"]).toContain("only be lowered");
    expect(raised.rates.requests_per_minute).toContain("only be lowered");
    const removed = draftErrors(draft([], { requests_per_minute: "" }), "tighten", [], stored);
    expect(removed.form.join(" ")).toContain("weekly budget ($5.00) can't be removed");
    expect(removed.rates.requests_per_minute).toContain("can't be removed");
    expect(hasErrors(draftErrors(draft([["saved-week", "week", "4"]], { requests_per_minute: "9" }), "tighten", [], stored))).toBe(false);
    expect(lockedBudget("tighten", stored, { key: "saved-week", period: "week", amount: "5" })).toBe(true);
    expect(lockedBudget("free", stored, { key: "saved-week", period: "week", amount: "5" })).toBe(false);
  });
  it("explains the server's tighten-only and personal-limit rejections in plain words", () => {
    expect((limitsSaveError(new ApiError(400, "400", "Invalid request")) as Error).message).toContain("higher than an inherited limit for the same period");
    expect((limitsSaveError(new ApiError(403, "403", "Access denied")) as Error).message).toContain("only be lowered");
    expect((limitsSaveError(new ApiError(403, "403", "Personal workspace limits are set by the platform", "personal_limits_platform_controlled")) as Error).message).toContain("Ask a Platform Admin");
    expect((limitsSaveError(new ApiError(409, "409", "x", "stacked_budgets_require_budgets_field")) as Error).message).toContain("several budgets");
    const other = new Error("network"); expect(limitsSaveError(other)).toBe(other);
  });
  it("maps each named policy rejection (with its period or limit) to a specific message on the right field", () => {
    const reject = (status: number, reason: string, detail?: { period?: string; limit?: string }) => new ApiError(status, String(status), "server text", reason, detail);
    const d: LimitsDraft = { requests_per_minute: "100", tokens_per_minute: "", concurrent_requests: "", concurrent_jobs: "", budgets: [{ key: "b1", period: "day", amount: "9" }, { key: "b2", period: "month", amount: "90" }] };
    const parent = rejectionErrors(reject(400, "exceeds_parent_budget", { period: "day" }), d)!;
    expect(parent.budgets.b1).toBe("The daily budget is higher than an inherited daily budget for the same period. Lower it to at most the inherited amount.");
    expect(parent.budgets.b2).toBeUndefined(); expect(parent.form).toEqual([]);
    expect(rejectionErrors(reject(400, "exceeds_parent_rate", { limit: "requests_per_minute" }), d)!.rates.requests_per_minute).toBe("Requests per minute is higher than an inherited limit. Lower it to at most the inherited value.");
    expect(rejectionErrors(reject(403, "stored_budget_raise_not_allowed", { period: "month" }), d)!.budgets.b2).toBe("The saved monthly budget can only be lowered, not raised.");
    expect(rejectionErrors(reject(403, "period_change_not_allowed", { period: "week" }), d)!.form).toEqual(["The saved weekly budget can't be removed or moved to another period. Keep it; you can lower its amount."]);
    expect(rejectionErrors(reject(403, "stored_rate_loosen_not_allowed", { limit: "concurrent_requests" }), d)!.rates.concurrent_requests).toBe("The saved requests running at once limit can only be lowered, never raised or removed.");
    expect(rejectionErrors(reject(403, "personal_limits_platform_controlled"), d)!.form[0]).toContain("Ask a Platform Admin");
    // A period not in the draft lands on the form; unknown reasons and plain errors aren't placed at all.
    expect(rejectionErrors(reject(400, "exceeds_parent_budget", { period: "lifetime" }), d)!.form[0]).toContain("lifetime budget");
    expect(rejectionErrors(reject(400, "something_new"), d)).toBeUndefined();
    expect(rejectionErrors(new ApiError(400, "400", "Invalid request"), d)).toBeUndefined();
    expect(policyRejection(reject(400, "exceeds_parent_budget", { period: "decade" }))!.message).toContain("The budget is higher");
    expect((limitsSaveError(reject(403, "stored_rate_loosen_not_allowed", { limit: "tokens_per_minute" })) as Error).message).toBe("The saved tokens per minute limit can only be lowered, never raised or removed.");
    // Client errors win over a server rejection on the same field.
    expect(mergeErrors({ rates: { requests_per_minute: "client" }, budgets: {}, form: [] }, parent)).toEqual({ rates: { requests_per_minute: "client" }, budgets: { b1: parent.budgets.b1 }, form: [] });
  });
});

describe("jobs at once", () => {
  it("is one more stacked limit: shown, composed by minimum, tighten-only, and read from older gateways as no limit", () => {
    expect(rateRows.map(r => r.label)).toContain("Jobs at once");
    expect(limitsOf(base).concurrent_jobs).toBeNull();
    expect(limitsOf({ ...base, concurrent_jobs: 2 }).concurrent_jobs).toBe(2);
    expect(composeLimits(limits({ concurrent_jobs: 2 }), limits({ concurrent_jobs: 5 }), limits({})).concurrent_jobs).toBe(2);
    expect(limitsSummary(limits({ concurrent_requests: 8, concurrent_jobs: 2 }))).toBe("8 at once · 2 jobs at once");
    expect(limitsSummary(limits({ concurrent_jobs: 1 }))).toBe("1 job at once");
    const d = { ...draftOf(limits({ concurrent_jobs: 2 })), concurrent_jobs: "3" };
    expect(draftErrors(d, "tighten", [{ label: "platform", limits: limits({ concurrent_jobs: 2 }) }]).rates.concurrent_jobs).toMatch(/only be lowered|higher than the platform/);
    expect(draftErrors({ ...d, concurrent_jobs: "" }, "tighten", [], limits({ concurrent_jobs: 2 })).rates.concurrent_jobs).toMatch(/can't be removed/);
    expect(limitsBody(draftLimits({ ...d, concurrent_jobs: "1" })).concurrent_jobs).toBe(1);
    const reject = new ApiError(400, "400", "server text", "exceeds_parent_rate", { limit: "concurrent_jobs" });
    expect(policyRejection(reject)).toEqual({ rate: "concurrent_jobs", message: "Jobs at once is higher than an inherited limit. Lower it to at most the inherited value." });
  });
});

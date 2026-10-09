import { describe, expect, it } from "vitest";
import { budgetTightenError, effectivePolicy, periodNests, canReconcile, deploymentRoutingBody, dollarsToMicroUsd, formatMicroUsd, formatUsd, integerField, microUsdError, microUsdToDollars, modelRoutingBody, passiveHealth, policyBody, policyFields, priceBody, priceFields, residencyError, priceLinesText, scalarRate, type Cost, type Policy, type Price } from "./governance";
import { validateFields } from "./forms";

const cacheDefaults = { cache_read_status: "unknown", cache_write_status: "unknown", cache_write_5m_status: "unknown", cache_write_1h_status: "unknown" };
const cachePricing = { read: { status: "unknown" }, write: { status: "unknown" }, write_5m: { status: "unknown" }, write_1h: { status: "unknown" } };
const unset: Policy = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null };
describe("budget periods", () => {
  it("compares budget amounts only where windows nest, like the server", () => {
    expect(periodNests("day", "week")).toBe(true); expect(periodNests("day", "month")).toBe(true);
    expect(periodNests("week", "month")).toBe(false); expect(periodNests("month", "day")).toBe(false);
    const parent: Policy = { ...unset, monthly_budget_microusd: "10", budget_period: "month" };
    expect(budgetTightenError(11n, "day", [parent])).toContain("Can't exceed");
    expect(budgetTightenError(11n, "week", [parent])).toBeUndefined();
    expect(budgetTightenError(5n, "day", [parent])).toBeUndefined();
    const stored: Policy = { ...unset, monthly_budget_microusd: "10", budget_period: "day" };
    expect(budgetTightenError(10n, "week", [], stored)).toBeUndefined();
    expect(budgetTightenError(11n, "day", [], stored)).toContain("lowered");
    expect(budgetTightenError(null, "day", [], stored)).toContain("can't be removed");
    expect(budgetTightenError(1n, "day", [], { ...stored, budget_period: "week" })).toContain("can't switch");
    expect(budgetTightenError(10n, "month", [], { ...stored, budget_period: "week" })).toContain("can't switch");
  });
  it("summarizes the smallest budget with its own period, defaulting legacy policies to monthly", () => {
    expect(effectivePolicy({ ...unset, monthly_budget_microusd: "5", budget_period: "day" }, { ...unset, monthly_budget_microusd: "100", budget_period: "month" })).toMatchObject({ monthly_budget_microusd: "5", budget_period: "day" });
    expect(effectivePolicy({ ...unset, monthly_budget_microusd: "500" , budget_period: "week" }, { ...unset, monthly_budget_microusd: "100" })).toMatchObject({ monthly_budget_microusd: "100", budget_period: "month" });
    expect(policyBody({ budget_period: "week" })).toEqual({ ...unset, budget_period: "week" });
    expect(policyFields(unset).find(f => f.name === "budget_period")?.value).toBe("month");
  });
});
describe("exact micro-USD and governance forms", () => {
  it("formats above JavaScript's safe integer without rounding", () => {
    expect(formatMicroUsd("9007199254740993")).toBe("$9,007,199,254.740993");
    expect(formatMicroUsd("9223372036854775807")).toBe("$9,223,372,036,854.775807");
    expect(formatMicroUsd("18446744073709551614")).toBe("$18,446,744,073,709.551614");
    expect(formatMicroUsd("1")).toBe("$0.000001");
    expect(formatMicroUsd("0")).toBe("$0.00");
    expect(formatMicroUsd("1000000")).toBe("$1.00");
    expect(formatMicroUsd("12340000")).toBe("$12.34");
    expect(formatMicroUsd("1200000")).toBe("$1.20");
    expect(formatMicroUsd("1001000")).toBe("$1.001");
    expect(formatMicroUsd("1000010")).toBe("$1.00001");
    expect(formatMicroUsd(null)).toBe("Unknown");
    expect(formatMicroUsd("bogus")).toBe("Unknown");
  });
  it("rounds display money to cents, keeps sub-cent amounts non-zero, and leaves unknown unknown", () => {
    expect(formatUsd("8271628")).toBe("$8.27");
    expect(formatUsd("8275000")).toBe("$8.28"); // half-up
    expect(formatUsd("999995")).toBe("$1.00");
    expect(formatUsd("10000")).toBe("$0.01");
    expect(formatUsd("1234567891234")).toBe("$1,234,567.89");
    expect(formatUsd("9223372036854775807")).toBe("$9,223,372,036,854.78");
    expect(formatUsd("0")).toBe("$0.00");
    expect(formatUsd("8100")).toBe("$0.0081");
    expect(formatUsd("8149")).toBe("$0.0081");
    expect(formatUsd("8150")).toBe("$0.0082");
    expect(formatUsd("9999")).toBe("$0.01");
    expect(formatUsd("120")).toBe("$0.00012");
    // Two significant digits keep a trailing zero (seen live: "$0.007", "$0.009").
    expect(formatUsd("7000")).toBe("$0.0070");
    expect(formatUsd("8960")).toBe("$0.0090");
    expect(formatUsd("500")).toBe("$0.00050");
    expect(formatUsd("10")).toBe("$0.000010");
    expect(formatUsd("5")).toBe("$0.000005");
    expect(formatUsd("1")).toBe("$0.000001");
    for (const v of ["1", "49", "4999", "9949"]) expect(formatUsd(v)).not.toBe("$0.00");
    expect(formatUsd(null)).toBe("Unknown");
    expect(formatUsd(undefined)).toBe("Unknown");
    expect(formatUsd("-1")).toBe("Unknown");
    expect(formatUsd("1.5")).toBe("Unknown");
  });
  it("converts exact decimal dollars using BigInt, rejecting silent precision loss", () => {
    expect(dollarsToMicroUsd("9007199254.740993")).toBe("9007199254740993");
    expect(dollarsToMicroUsd("12.34")).toBe("12340000");
    expect(dollarsToMicroUsd("0.000001")).toBe("1");
    expect(dollarsToMicroUsd("9223372036854.775807")).toBe("9223372036854775807");
    expect(dollarsToMicroUsd("0")).toBe("0");
    expect(dollarsToMicroUsd(" 0012.340000 ")).toBe("12340000");
    for (const bad of ["", "1e6", "1E-6", "-1", "-0", "+1", ".1", "1.", "0.0000001", "1.0000001", "NaN", "Infinity", "-Infinity", "1,000", "$1", "€1", "1 000", "9223372036854.775808", "9223372036854775807", "0".repeat(100000)]) expect(() => dollarsToMicroUsd(bad)).toThrow();
  });
  it("round-trips every meaningful decimal digit without grouping or currency symbols", () => {
    for (const [micro, dollars] of [["0", "0.00"], ["1", "0.000001"], ["10", "0.00001"], ["1000", "0.001"], ["10000", "0.01"], ["100000", "0.10"], ["1000000", "1.00"], ["1234567", "1.234567"], ["9007199254740993", "9007199254.740993"], ["9223372036854775807", "9223372036854.775807"]]) {
      expect(microUsdToDollars(micro)).toBe(dollars);
      expect(dollarsToMicroUsd(microUsdToDollars(micro))).toBe(micro);
    }
    for (const bad of ["-1", "1.1", "9223372036854775808", "0".repeat(100000)]) expect(() => microUsdToDollars(bad)).toThrow();
  });
  it("accepts only bounded raw integer money", () => {
    for (const bad of ["1.2", "1e6", "-1", "+1", "Infinity", "9223372036854775808", "0".repeat(100000)]) expect(microUsdError(bad)).toBeTruthy();
    expect(microUsdError("9007199254740993")).toBeUndefined();
    expect(microUsdError("0")).toBeUndefined();
  });
  it("sends all policy fields and distinguishes blank from positive budgets", () => {
    expect(policyBody({})).toEqual(unset);
    expect(policyBody({ monthly_budget_usd: "" })).toEqual(unset);
    expect(policyBody({ monthly_budget_usd: "   " })).toEqual(unset);
    expect(policyBody({ requests_per_minute: "10", monthly_budget_usd: "9007199254.740993" })).toEqual({ ...unset, requests_per_minute: 10, monthly_budget_microusd: "9007199254740993" });
    const fields = policyFields(unset);
    expect(validateFields(fields, {})).toEqual({});
    expect(validateFields(fields, { requests_per_minute: "0" })).toHaveProperty("requests_per_minute");
    for (const zero of ["0", "0.00", "0.000000"]) expect(validateFields(fields, { monthly_budget_usd: zero })).toHaveProperty("monthly_budget_usd");
    expect(validateFields(fields, { monthly_budget_usd: "0.000001" })).toEqual({});
    for (const bad of ["-1", "1e6", "1.0000001", "9223372036854.775808", "9".repeat(100000)]) expect(validateFields(fields, { monthly_budget_usd: bad })).toHaveProperty("monthly_budget_usd");
    expect(fields[3]).toMatchObject({ name: "monthly_budget_usd", label: "Budget (USD)", type: "text", inputMode: "decimal", maxLength: 32, value: "" });
    expect(validateFields(fields, { concurrent_requests: "2147483648" })).toHaveProperty("concurrent_requests");
    expect(validateFields(fields, { tokens_per_minute: "1e3" })).toHaveProperty("tokens_per_minute");
  });
  it("labels a stored tighten-only cap honestly instead of Optional (live acceptance F5)", () => {
    const stored = { ...unset, requests_per_minute: 10, monthly_budget_microusd: "10" };
    const fields = policyFields(stored, undefined, true);
    expect(fields[3]).toMatchObject({ required: true, hint: "Stored cap · can only be lowered" });
    expect(fields[3].help).toContain("not raise or remove it");
    expect(fields[0].hint).toBe("Stored cap · can only be lowered");
    expect(fields[1].hint).toBeUndefined();
    expect(validateFields(fields, { requests_per_minute: "10", monthly_budget_usd: "" })).toHaveProperty("monthly_budget_usd");
    expect(validateFields(fields, { requests_per_minute: "10", monthly_budget_usd: "0.000005" })).toEqual({});
    // Replacement overrides and unset caps keep the ordinary optional semantics.
    expect(policyFields(stored)[3].hint).toBeUndefined();
    expect(policyFields(unset, undefined, true)[3].hint).toBeUndefined();
  });
  it("compares USD budgets to inherited micro-USD ceilings exactly", () => {
    const fields = policyFields(unset, { ...unset, requests_per_minute: 10, monthly_budget_microusd: "9007199254740993" });
    expect(validateFields(fields, { monthly_budget_usd: "9007199254.740993" })).toEqual({});
    expect(validateFields(fields, { monthly_budget_usd: "9007199254.740994" }).monthly_budget_usd).toContain("$9,007,199,254.740993 USD");
    expect(validateFields(fields, { requests_per_minute: "11" })).toHaveProperty("requests_per_minute");
    expect(validateFields(fields, { monthly_budget_usd: "" })).toEqual({});
    expect(fields[3].validate?.("", {})).toBeUndefined();
    const tinyCeiling = policyFields(unset, { ...unset, monthly_budget_microusd: "1" });
    expect(validateFields(tinyCeiling, { monthly_budget_usd: "0.000001" })).toEqual({});
    expect(validateFields(tinyCeiling, { monthly_budget_usd: "0.000002" })).toHaveProperty("monthly_budget_usd");
  });
  it("preserves API money through policy edit/save roundtrips", () => {
    for (const monthly_budget_microusd of [null, "1", "12340000", "9007199254740993", "9223372036854775807"]) {
      const policy = { ...unset, requests_per_minute: 10, monthly_budget_microusd };
      const fields = policyFields(policy);
      const values = Object.fromEntries(fields.map(field => [field.name, field.value ?? ""]));
      expect(validateFields(fields, values)).toEqual({});
      expect(policyBody(values)).toEqual({ ...policy, budget_period: "month" });
    }
  });
  it("rejects unsafe numeric counters and overflowed full-ceiling reservations", () => {
    expect(validateFields([integerField("count", "Count")], { count: "9007199254740993" })).toHaveProperty("count");
    const values = { ...cacheDefaults, input_usd_per_million: "9223372036854.775807", output_usd_per_million: "0", input_token_limit: "1000001", output_token_limit: "1" };
    expect(validateFields(priceFields(), values).output_token_limit).toContain("USD");
    expect(validateFields(priceFields(), { ...values, input_token_limit: "1000000" })).toEqual({});
    // Rounding each side up adds a single micro-dollar beyond the ledger maximum.
    expect(validateFields(priceFields(), { ...values, input_token_limit: "1000000", output_usd_per_million: "0.000001" })).toHaveProperty("output_token_limit");
    expect(validateFields(priceFields(), { ...values, input_usd_per_million: "9223372036854.775806", input_token_limit: "1000000", output_usd_per_million: "0.000001" })).toEqual({});
  });
  it("accepts zero USD prices, requires rates, and emits only exact API keys", () => {
    const values = { ...cacheDefaults, input_usd_per_million: "0", output_usd_per_million: "0.000001", input_token_limit: "1000000", output_token_limit: "1" };
    expect(validateFields(priceFields(), values)).toEqual({});
    expect(priceBody(values)).toEqual({ input_microusd_per_million: "0", output_microusd_per_million: "1", input_token_limit: 1000000, output_token_limit: 1, pricing_version: 2, cache_pricing: cachePricing });
    for (const name of ["input_usd_per_million", "output_usd_per_million"]) {
      for (const bad of ["", "-1", "1e3", "Infinity", "0.0000001", "9223372036854.775808", "9".repeat(100000)]) expect(validateFields(priceFields(), { ...values, [name]: bad })).toHaveProperty(name);
    }
    for (const field of priceFields().slice(0, 2)) expect(field).toMatchObject({ type: "text", inputMode: "decimal", maxLength: 32 });
    expect(priceBody({ ...values, input_usd_per_million: " 0012.340000 " }).input_microusd_per_million).toBe("12340000");
  });
  it("round-trips USD price prefills without losing precision", () => {
    const price: Price = { id: "price", deployment_id: "deployment", pricing_version: 1, cache_pricing: null, created_at: "2026-01-01T00:00:00Z", input_microusd_per_million: "9007199254740993", output_microusd_per_million: "1", input_token_limit: 1000000, output_token_limit: 1 };
    const fields = priceFields(price);
    const values = Object.fromEntries(fields.map(field => [field.name, field.value ?? ""]));
    expect(values.input_usd_per_million).toBe("9007199254.740993");
    expect(values.output_usd_per_million).toBe("0.000001");
    expect(validateFields(fields, values)).toEqual({});
    expect(priceBody(values)).toEqual({ input_microusd_per_million: price.input_microusd_per_million, output_microusd_per_million: price.output_microusd_per_million, input_token_limit: price.input_token_limit, output_token_limit: price.output_token_limit, pricing_version: 2, cache_pricing: cachePricing });
  });
  it("preserves operator-only residency and disabled state for org-admin edits", () => {
    const current = { priority: 0, weight: 1, residency: "us-east", failure_threshold: 3, cooldown_seconds: 30 };
    const values = { priority: "-3", weight: "10", residency: "eu-west", failure_threshold: "3", cooldown_seconds: "30" };
    expect(deploymentRoutingBody(values, current, false)).toEqual({ ...current, priority: -3, weight: 10 });
    expect(deploymentRoutingBody(values, current, true).residency).toBe("eu-west");
    expect(modelRoutingBody({ strategy: "priority", max_attempts: "1", allow_ambiguous_failover: "false", failure_threshold: "3", cooldown_seconds: "30", required_residency: "" }).required_residency).toBeNull();
    expect(residencyError("US East")).toBeTruthy();
    expect(residencyError("us-east_1.a")).toBeUndefined();
  });
  it("does not confuse an unobserved deployment with a healthy one", () => {
    expect(passiveHealth({ consecutive_failures: 0, open_until: null })).toContain("Unknown");
    expect(passiveHealth({ consecutive_failures: 0, open_until: null, last_observed_at: null })).toContain("Unknown");
    expect(passiveHealth({ consecutive_failures: 3, open_until: "2999-01-01T00:00:00Z", last_observed_at: "2026-01-01T00:00:00Z" })).toContain("Cooldown");
  });
  it("only offers reconciliation for recognized unresolved terminal records with pinned prices", () => {
    const row = { state: "failed", cost_microusd: null, price_id: "price" } as Cost;
    expect(canReconcile(row)).toBe(true);
    expect(canReconcile({ ...row, state: "started" })).toBe(false);
    expect(canReconcile({ ...row, state: "new-unknown-state" })).toBe(false);
    expect(canReconcile({ ...row, price_id: null })).toBe(false);
    expect(canReconcile({ ...row, cost_microusd: "0" })).toBe(false);
  });
});

describe("pricing v3 read-only display", () => {
  const v3: Price = { id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 1, output_token_limit: 0, cache_pricing: null, price_lines: [{ meter: "input_characters", microusd_per_batch: "15000000", batch: 1000000, unit_label: "/M characters", sku_label: "Characters" }, { meter: "requests", not_applicable: true }], display_lines: ["$15/M characters", "Requests: not applicable"], created_at: "2026-01-01T00:00:00Z" };
  it("uses server display strings and never shows v3 scalar rates as unknown", () => {
    expect(priceLinesText(v3)).toEqual(["$15/M characters", "Requests: not applicable"]);
    expect(scalarRate(v3, "input")).toBe("See price lines");
    expect(priceLinesText({ ...v3, display_lines: null })).toEqual(["15000000 µUSD per 1000000 input_characters", "requests: not applicable"]);
    expect(priceLinesText({ ...v3, pricing_version: 2 })).toEqual([]);
    expect(scalarRate({ ...v3, pricing_version: 2, input_microusd_per_million: "100000" }, "input")).toBe("$0.10");
  });
});

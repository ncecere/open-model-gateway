import type { Field, Values } from "./forms";

/** Budget window: UTC day, ISO week (Monday 00:00 UTC), UTC calendar month, or lifetime (since the scope was created). */
export type BudgetPeriod = "day" | "week" | "month" | "lifetime";
/** One stacked budget (contract "stacked budgets"): at most one per period per scope, each enforced over its own window. */
export type PolicyBudget = { period: BudgetPeriod; amount_microusd: string };
/**
 * `budgets` is the stacked set (sorted day, week, month, lifetime). `monthly_budget_microusd`/`budget_period` are the
 * deprecated mirror of the smallest budget; older gateways only send those.
 */
export type Policy = { requests_per_minute: number | null; tokens_per_minute: number | null; concurrent_requests: number | null; monthly_budget_microusd: string | null; budget_period?: BudgetPeriod; budgets?: PolicyBudget[] };
/** One applicable budget (layer and period) and its current UTC window; usage only where the caller may see that scope's activity. */
export type BudgetWindow = { layer: "platform" | "local" | "key"; period?: BudgetPeriod; amount_microusd?: string; monthly_budget_microusd: string; budget_period: BudgetPeriod; window_start: string; window_end: string | null; usage_visible: boolean; used_microusd: string | null; unresolved_usage: boolean | null; exhausted?: boolean | null };
export const budgetPeriods: { value: BudgetPeriod; label: string; unit: string; window: string }[] = [
  { value: "day", label: "Daily", unit: "USD / day", window: "UTC day" },
  { value: "week", label: "Weekly", unit: "USD / week", window: "ISO week from Monday 00:00 UTC" },
  { value: "month", label: "Monthly", unit: "USD / month", window: "UTC calendar month" },
  { value: "lifetime", label: "Lifetime", unit: "USD in total", window: "since the scope was created" },
];
export const periodOf = (policy: Pick<Policy, "budget_period">): BudgetPeriod => policy.budget_period ?? "month";
export const periodLabel = (period: BudgetPeriod) => budgetPeriods.find(p => p.value === period)!.label;
export const periodUnit = (period: BudgetPeriod) => budgetPeriods.find(p => p.value === period)!.unit;
/** Every `inner` window lies inside one `outer` window: same period, or a day in its week/month. ISO weeks straddle months. */
export const periodNests = (inner: BudgetPeriod, outer: BudgetPeriod) => inner === outer || outer === "lifetime" || inner === "day";
/** "$10.00 / week" or the empty text. */
export function budgetText(policy: Policy, empty = "No cap"): string { return policy.monthly_budget_microusd === null ? empty : `${formatMicroUsd(policy.monthly_budget_microusd)} / ${periodOf(policy)}`; }
/**
 * Tighten-only budget rules shared with the server (management/governance/policies.rs). A child amount is compared
 * with a parent only when its period nests in the parent's; otherwise each layer is enforced over its own window, so
 * the parent still applies. A stored budget may only keep or lower its amount, and change period only from a period
 * that nests in the new one (daily to weekly/monthly).
 */
export function budgetTightenError(amount: bigint | null, period: BudgetPeriod, parents: Policy[], stored?: Policy): string | undefined {
  if (stored && stored.monthly_budget_microusd !== null) {
    if (amount === null) return "A stored budget can't be removed here. Lower it instead.";
    if (amount > BigInt(stored.monthly_budget_microusd)) return `A stored budget can only be lowered (now ${budgetText(stored)}).`;
    if (!periodNests(periodOf(stored), period)) return `A ${periodLabel(periodOf(stored)).toLowerCase()} budget can't switch to ${periodLabel(period).toLowerCase()}; that would loosen it.`;
  }
  if (amount === null) return;
  for (const parent of parents) if (parent.monthly_budget_microusd !== null && periodNests(period, periodOf(parent)) && amount > BigInt(parent.monthly_budget_microusd)) return `Can't exceed the inherited ${budgetText(parent)}.`;
}
export type CacheRate = { status: "priced"; microusd_per_million: string } | { status: "unknown" } | { status: "not_applicable" };
export type CachePricing = { read: CacheRate; write: CacheRate; write_5m: CacheRate; write_1h: CacheRate };
export type Meter = "input_tokens" | "output_tokens" | "cache_read_tokens" | "cache_write_tokens" | "cache_write_5m_tokens" | "cache_write_1h_tokens" | "output_images" | "input_characters" | "input_audio_seconds_ms" | "output_audio_seconds_ms" | "search_units" | "requests";
export type UnitMeter = "output_images" | "input_characters" | "input_audio_seconds_ms" | "output_audio_seconds_ms" | "search_units" | "requests";
/** Pricing-v3 line: integer micro-USD per batch, or an explicit not-applicable assertion. A missing meter is unknown, never free. */
export type PriceLine = { meter: Meter; microusd_per_batch: string; batch: number; unit_label: string; sku_label: string; variant?: string; min_prompt_tokens?: number } | { meter: Meter; not_applicable: true };
/** v1/v2 carry scalar token rates; v3 rates live in `price_lines` (scalar rates are null) with server-computed exact display strings. */
export type Price = { id: string; deployment_id: string; pricing_version: 1 | 2 | 3; input_microusd_per_million: string | null; output_microusd_per_million: string | null; input_token_limit: number; output_token_limit: number; cache_pricing: CachePricing | null; price_lines?: PriceLine[] | null; max_units?: Partial<Record<UnitMeter, string>> | null; display_lines?: string[] | null; display_summary?: string | null; created_at: string };
export type MeterUsage = Record<UnitMeter, string | null>;
export type MeterCostComponents = { output_images_microusd: string; input_characters_microusd: string; input_audio_microusd: string; output_audio_microusd: string; search_units_microusd: string; requests_microusd: string };
export type WorkloadKind = "generation" | "embeddings" | "images" | "audio_transcriptions" | "audio_speech" | "rerank" | "systemone";
export type PolicyResponse = { policy: Policy; effective: Policy; provenance: { platform_source: "type_default" | "workspace_override"; platform: Policy; local: Policy; key: Policy | null; type_default?: Policy }; mode?: "inherit" | "replace"; budgets?: BudgetWindow[] };
export type ModelRouting = { strategy: "priority" | "weighted"; max_attempts: number; allow_ambiguous_failover: boolean; required_residency: string | null };
export type DeploymentRouting = { routing: { priority: number; weight: number; residency: string | null; failure_threshold: number; cooldown_seconds: number }; health: { consecutive_failures: number; open_until: string | null; last_observed_at?: string | null } };
export type CostSummary = { currency: "USD"; known_cost_microusd: string; held_microusd: string; unknown_cost_requests: number; requests: number };
export type BillingUsage = { total_input_tokens: string | null; uncached_input_tokens: string | null; cache_read_input_tokens: string | null; cache_write_input_tokens: string | null; cache_write_default_input_tokens: string | null; cache_write_5m_input_tokens: string | null; cache_write_1h_input_tokens: string | null };
export type CostComponents = { uncached_input_microusd: string; cache_read_microusd: string; cache_write_default_microusd: string; cache_write_5m_microusd: string; cache_write_1h_microusd: string; output_microusd: string };
export type Cost = { id: string; root_request_id: string; attempt_number: number; public_model: string; provider: string; state: string; workload_kind: WorkloadKind; started_at: string; input_tokens: string | null; output_tokens: string | null; billing_usage: BillingUsage | null; cost_components: (CostComponents & Partial<MeterCostComponents>) | null; meter_usage?: MeterUsage | null; output_image_variant?: string | null; provider_cost_microusd?: string | null; price_id: string | null; cost_microusd: string | null; reserved_microusd: string | null; active_held_microusd: string | null; unresolved_reason: string | null; cost_status: "pending" | "unknown" | "settled"; unbounded_cost: boolean; cost_center_id: string | null; cost_center_name: string | null; cost_center_code: string | null; details_redacted_at: string | null };
export type Totals = { known_cost_microusd: string; held_microusd: string; attempts: string; root_requests: string; unresolved_attempts: string };
export type Health = { pending_attempts: string; unknown_attempts: string; missing_reservation_attempts: string; unpriced_attempts: string; aged_hold_attempts: string; unbounded_attempts: string };
export type Period = { start_date: string; end_date: string; timezone: "UTC"; includes_partial_day: boolean };
export type Breakdown = { id: string | null; name: string; totals: Totals };
export type CostReport = { period: Period; scope: "platform" | "workspace"; observed_at: string; basis: "configured_rate_estimate"; currency: "USD"; totals: Totals; health: Health; daily: { date: string; totals: Totals }[]; breakdowns: { models: Breakdown[]; providers: Breakdown[]; workspaces: Breakdown[]; cost_centers: Breakdown[]; service_accounts: Breakdown[] }; comparison: null | { period: Period; totals: Totals }; coverage: { priced_attempts: string; settled_attempts: string; total_attempts: string; complete_billing_attempts: string; legacy_pricing_attempts: string; incomplete_billing_attempts: string }; cost_components: CostComponents & Partial<MeterCostComponents> & { legacy_microusd: string }; billing_usage: BillingUsage; meter_usage?: MeterUsage; /** Attempts whose workload can produce each meter (decimal strings). */ meter_relevant_attempts?: Record<UnitMeter, string>; /** Relevant attempts where the meter was not observed: the total is then a lower bound. */ meter_unknown_attempts?: Record<UnitMeter, string>; provider_reported_cost_microusd?: string | null; breakdowns_truncated: boolean };

// Integer-string money never passes through Number, parseFloat or toFixed.
export const MAX_MICROUSD = 9223372036854775807n;
const MAX_MONEY_INPUT_LENGTH = 32;
const USD_RANGE_ERROR = "Amount cannot exceed $9,223,372,036,854.775807 USD.";
export function microUsdError(value: string): string | undefined {
  if (value.length > MAX_MONEY_INPUT_LENGTH) return USD_RANGE_ERROR;
  if (!/^\d+$/.test(value)) return "Enter a non-negative integer micro-USD amount (no decimals or exponent).";
  if (BigInt(value) > MAX_MICROUSD) return USD_RANGE_ERROR;
}
// Trim surrounding whitespace and normalize leading zeros; never accept signs,
// grouping, currency symbols, exponent notation, or more than six decimals.
export function dollarsToMicroUsd(value: string): string {
  if (value.length > MAX_MONEY_INPUT_LENGTH) throw new Error("Enter a USD amount using at most 32 characters.");
  const dollars = value.trim();
  if (!/^\d+(?:\.\d{1,6})?$/.test(dollars)) throw new Error("Enter a non-negative USD amount with up to 6 decimal places, without commas, currency symbols, or exponent notation.");
  const [whole, fraction = ""] = dollars.split(".");
  const amount = BigInt(whole) * 1000000n + BigInt(fraction.padEnd(6, "0"));
  if (amount > MAX_MICROUSD) throw new Error(USD_RANGE_ERROR);
  return amount.toString();
}
function dollarsError(value: string): string | undefined {
  try { dollarsToMicroUsd(value); } catch (error) { return (error as Error).message; }
}
// Editable exact dollars: no currency symbol/grouping, at least two decimals.
export function microUsdToDollars(value: string): string {
  const error = microUsdError(value);
  if (error) throw new Error(error);
  return decimalDollars(BigInt(value));
}
function decimalDollars(amount: bigint): string {
  const fraction = (amount % 1000000n).toString().padStart(6, "0").replace(/0+$/, "").padEnd(2, "0");
  return `${amount / 1000000n}.${fraction}`;
}
export function formatMicroUsd(value: string | null | undefined): string {
  // SQL SUM(bigint) is numeric: read-only totals may exceed a single-entry
  // int64 limit. Do not mislabel those known totals as unknown.
  if (value == null || value.length > 128 || !/^\d+$/.test(value)) return "Unknown";
  const [whole, fraction] = decimalDollars(BigInt(value)).split(".");
  return `$${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${fraction}`;
}
export const integerField = (name: string, label: string, min = 1, max = 2147483647): Field => ({ name, label, type: "number", required: true, min, max, validate: value => /^-?\d+$/.test(value) && Number.isSafeInteger(Number(value)) ? undefined : "Enter an exact safe integer, without exponent notation." });
/**
 * Local/key restrictions are tighten-only: once a cap is stored at this scope, blank cannot remove it and a
 * higher value cannot replace it. Say so instead of marking the field Optional.
 */
const storedCap = { hint: "Stored cap · can only be lowered", help: "This scope already stores a cap. You can lower it, but not raise or remove it here; leaving it blank is not allowed. Inherited platform limits still apply." };
export function policyFields(policy: Policy, ceiling?: Policy, preserveLocalCaps = false): Field[] {
  return [
    ...([['requests_per_minute', 'Upstream attempts per minute'], ['tokens_per_minute', 'Tokens per minute'], ['concurrent_requests', 'Concurrent requests']] as const).map(([name, label]): Field => ({
      ...integerField(name, label), required: preserveLocalCaps && policy[name] !== null, value: policy[name]?.toString() ?? "", ...(preserveLocalCaps && policy[name] !== null ? storedCap : { help: "Blank means shared parent allowance; inherited limits still apply." }),
      validate: (value, values) => {
        if (!value.trim()) return;
        const error = integerField(name, label).validate?.(value, values);
        if (error) return error;
        if (preserveLocalCaps && policy[name] !== null && Number(value) > policy[name]!) return "A previously stored local restriction can only tighten.";
        const parent = ceiling?.[name];
        if (parent != null && Number(value) > parent) return "This limit cannot exceed the inherited ceiling. Leave blank to share the parent allowance.";
      },
    })),
    {
      name: "monthly_budget_usd", label: "Budget (USD)", type: "text", required: preserveLocalCaps && policy.monthly_budget_microusd !== null, inputMode: "decimal", maxLength: MAX_MONEY_INPUT_LENGTH,
      value: policy.monthly_budget_microusd == null ? "" : microUsdToDollars(policy.monthly_budget_microusd),
      ...(preserveLocalCaps && policy.monthly_budget_microusd !== null ? { ...storedCap, help: `Enter dollars (up to 6 decimal places). ${storedCap.help}` } : { help: "Enter dollars (up to 6 decimal places). Blank means shared parent allowance, not a reserved allocation." }),
      validate: (value, values) => {
        if (!value.trim()) return;
        const error = dollarsError(value);
        if (error) return error;
        const amount = BigInt(dollarsToMicroUsd(value));
        if (amount === 0n) return "Budget must be greater than $0.00 USD, or blank to inherit the parent allowance.";
        if (preserveLocalCaps && policy.monthly_budget_microusd !== null && amount > BigInt(policy.monthly_budget_microusd)) return "A previously stored local restriction can only tighten.";
        const parent = ceiling?.monthly_budget_microusd;
        if (parent != null && amount > BigInt(parent) && periodNests((values?.budget_period || periodOf(policy)) as BudgetPeriod, periodOf(ceiling!))) return `Budget cannot exceed the inherited ceiling of ${formatMicroUsd(parent)} USD. Leave blank to share the parent allowance.`;
      },
    },
    { name: "budget_period", label: "Budget period", type: "select", value: periodOf(policy), options: budgetPeriods.map(p => ({ value: p.value, label: `${p.label} · ${p.window}` })), help: "Each budget is enforced over its own current UTC window. Blank keeps the stored period (monthly when new). Changing the period never resets consumption." },
  ];
}
/** Client mirror of the server's composed summary: minimum rate caps; the smallest budget amount with its period (ties prefer the nested, shorter period). Budgets with different periods all apply independently. */
export function effectivePolicy(local: Policy, parent: Policy): Policy {
  const minimum = (a: number | null, b: number | null) => a === null ? b : b === null ? a : Math.min(a, b);
  const a = local.monthly_budget_microusd, b = parent.monthly_budget_microusd;
  const pickLocal = a !== null && (b === null || BigInt(a) < BigInt(b) || BigInt(a) === BigInt(b) && periodNests(periodOf(local), periodOf(parent)));
  return { requests_per_minute: minimum(local.requests_per_minute, parent.requests_per_minute), tokens_per_minute: minimum(local.tokens_per_minute, parent.tokens_per_minute), concurrent_requests: minimum(local.concurrent_requests, parent.concurrent_requests), monthly_budget_microusd: pickLocal ? a : b, ...(local.budget_period || parent.budget_period ? { budget_period: pickLocal || b === null ? periodOf(local) : periodOf(parent) } : {}) };
}
export function policyBody(values: Values): Policy {
  return { requests_per_minute: values.requests_per_minute ? Number(values.requests_per_minute) : null, tokens_per_minute: values.tokens_per_minute ? Number(values.tokens_per_minute) : null, concurrent_requests: values.concurrent_requests ? Number(values.concurrent_requests) : null, monthly_budget_microusd: values.monthly_budget_usd?.trim() ? dollarsToMicroUsd(values.monthly_budget_usd) : null, ...(values.budget_period ? { budget_period: values.budget_period as BudgetPeriod } : {}) };
}
export function modelRoutingBody(values: Values): ModelRouting {
  return { strategy: values.strategy as ModelRouting["strategy"], max_attempts: Number(values.max_attempts), allow_ambiguous_failover: values.allow_ambiguous_failover === "true", required_residency: values.required_residency || null };
}
export function deploymentRoutingBody(values: Values, current: DeploymentRouting["routing"], operator: boolean): DeploymentRouting["routing"] {
  return { priority: Number(values.priority), weight: Number(values.weight), residency: operator ? values.residency || null : current.residency, failure_threshold: Number(values.failure_threshold), cooldown_seconds: Number(values.cooldown_seconds) };
}
export function priceBody(values: Values): Omit<Price, "id" | "deployment_id" | "created_at"> {
  return {
    input_microusd_per_million: dollarsToMicroUsd(values.input_usd_per_million),
    output_microusd_per_million: dollarsToMicroUsd(values.output_usd_per_million),
    input_token_limit: Number(values.input_token_limit),
    output_token_limit: Number(values.output_token_limit),
    pricing_version: 2,
    cache_pricing: Object.fromEntries(cacheCategories.map(([key]) => [key, values[`cache_${key}_status`] === "priced" ? { status: "priced", microusd_per_million: dollarsToMicroUsd(values[`cache_${key}_usd`]) } : values[`cache_${key}_status`] === "not_applicable" ? { status: "not_applicable" } : { status: "unknown" }])) as CachePricing,
  };
}
export const cacheCategories = [["read", "Cache read"], ["write", "Cache write · default"], ["write_5m", "Cache write · 5-minute"], ["write_1h", "Cache write · 1-hour"]] as const;
export function priceFields(price?: Price): Field[] {
  return [
    { name: "input_usd_per_million", label: "Input rate (USD per million tokens)", type: "text", inputMode: "decimal", required: true, maxLength: MAX_MONEY_INPUT_LENGTH, validate: dollarsError, value: price?.input_microusd_per_million ? microUsdToDollars(price.input_microusd_per_million) : "", help: "Enter dollars per million tokens (up to 6 decimal places). Zero is allowed." },
    { name: "output_usd_per_million", label: "Output rate (USD per million tokens)", type: "text", inputMode: "decimal", required: true, maxLength: MAX_MONEY_INPUT_LENGTH, validate: dollarsError, value: price?.output_microusd_per_million ? microUsdToDollars(price.output_microusd_per_million) : "" },
    { ...integerField("input_token_limit", "Hard upstream input token ceiling"), value: price?.input_token_limit.toString() ?? "" },
    { ...integerField("output_token_limit", "Hard upstream output token ceiling", 0), help: "Zero is valid for embeddings-only deployments. Generation requires a positive explicit output bound.", value: price?.output_token_limit.toString() ?? "", validate: (value, values) => {
      const validInteger = integerField("", "").validate?.(value, values);
      if (validInteger) return validInteger;
      const inputTokens = values.input_token_limit?.trim() ?? "";
      if (integerField("", "").validate?.(inputTokens, values) || !/^\d+$/.test(inputTokens) || !/^\d+$/.test(value)) return;
      const input = values.input_usd_per_million ?? "", output = values.output_usd_per_million ?? "";
      if (dollarsError(input) || dollarsError(output)) return;
      const rates = [BigInt(dollarsToMicroUsd(input)), ...cacheCategories.flatMap(([key]) => values[`cache_${key}_status`] === "priced" && !dollarsError(values[`cache_${key}_usd`] ?? "") ? [BigInt(dollarsToMicroUsd(values[`cache_${key}_usd`]))] : [])];
      const inputRate = rates.reduce((a, b) => a > b ? a : b), outputRate = BigInt(dollarsToMicroUsd(output));
      // Counters were checked as safe integers above; money never uses Number.
      const inputLimit = BigInt(Number(inputTokens)), outputLimit = BigInt(Number(value));
      if ((inputRate * inputLimit + 999999n) / 1000000n + (outputRate * outputLimit + 999999n) / 1000000n > MAX_MICROUSD) return "Maximum reservation exceeds $9,223,372,036,854.775807 USD. Reduce rates or token ceilings.";
    } },
    ...cacheCategories.flatMap(([key, label]): Field[] => {
      const rate = price?.cache_pricing?.[key];
      return [{ name: `cache_${key}_status`, label: `${label} pricing`, type: "select", required: true, value: rate?.status ?? "unknown", options: [{ value: "unknown", label: "Unknown · cannot assume free" }, { value: "priced", label: "Priced · explicit rate" }, { value: "not_applicable", label: "Not applicable · certified profile only" }], help: "A configured cache rate does not enable caching. TTL allocations overlap the cache-write aggregate and are never charged twice." }, { name: `cache_${key}_usd`, label: `${label} (USD per million tokens)`, type: "text", inputMode: "decimal", required: true, maxLength: MAX_MONEY_INPUT_LENGTH, value: rate?.status === "priced" ? microUsdToDollars(rate.microusd_per_million) : "", validate: dollarsError, visibleWhen: values => values[`cache_${key}_status`] === "priced" }];
    }),
  ];
}
/** Read-only v3 rendering: the server computes exact display strings from integers. */
export function priceLinesText(price: Price): string[] {
  if (price.pricing_version !== 3) return [];
  return price.display_lines ?? (price.price_lines ?? []).map(l => "not_applicable" in l ? `${l.meter}: not applicable` : `${l.microusd_per_batch} µUSD per ${l.batch} ${l.meter}`);
}
/** Scalar token rate for v1/v2; v3 versions point at their price lines instead of showing "Unknown". */
export function scalarRate(price: Price, which: "input" | "output"): string {
  if (price.pricing_version === 3) return "See price lines";
  return formatMicroUsd(which === "input" ? price.input_microusd_per_million : price.output_microusd_per_million);
}
export function canReconcile(cost: Cost): boolean {
  return cost.cost_microusd === null && cost.price_id !== null && ["succeeded", "failed", "cancelled"].includes(cost.state);
}
export function residencyError(value: string): string | undefined {
  return /^[a-z0-9][a-z0-9._-]{0,63}$/.test(value) ? undefined : "Use 1–64 lowercase letters, digits, dots, underscores or hyphens; start with a letter or digit.";
}
export function passiveHealth(health: DeploymentRouting["health"]): string {
  if (!health.last_observed_at) return "Unknown · no recorded observation";
  if (health.open_until && Date.parse(health.open_until) > Date.now()) return "Cooldown · passive observation";
  return `${health.consecutive_failures} consecutive observed failures`;
}

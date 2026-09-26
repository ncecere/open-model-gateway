import type { Field, Values } from "./forms";

export type Policy = { requests_per_minute: number | null; tokens_per_minute: number | null; concurrent_requests: number | null; monthly_budget_microusd: string | null };
export type Price = { id: string; input_microusd_per_million: string; output_microusd_per_million: string; input_token_limit: number; output_token_limit: number; created_at: string };
export type ModelRouting = { strategy: "priority" | "weighted"; max_attempts: number; allow_ambiguous_failover: boolean; failure_threshold: number; cooldown_seconds: number; required_residency: string | null };
export type DeploymentRouting = { routing: { priority: number; weight: number; residency: string; operator_disabled?: boolean }; health: { consecutive_failures: number; open_until: string | null; last_observed_at?: string | null } };
export type CostSummary = { currency: "USD"; known_cost_microusd: string; held_microusd: string; unknown_cost_requests: number; requests: number };
export type Cost = { id: string; public_model: string; provider: string; state: string; started_at: string; input_tokens: number | null; output_tokens: number | null; price_id: string | null; cost_microusd: string | null; reserved_microusd: string | null; cost_status: string };

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
export function policyFields(policy: Policy, ceiling?: Policy): Field[] {
  return [
    ...([['requests_per_minute', 'Upstream attempts per minute'], ['tokens_per_minute', 'Tokens per minute'], ['concurrent_requests', 'Concurrent requests']] as const).map(([name, label]): Field => ({
      ...integerField(name, label), required: false, value: policy[name]?.toString() ?? "", help: "Blank means shared parent allowance; inherited limits still apply.",
      validate: (value, values) => {
        if (!value.trim()) return;
        const error = integerField(name, label).validate?.(value, values);
        if (error) return error;
        const parent = ceiling?.[name];
        if (parent != null && Number(value) > parent) return "This limit cannot exceed the inherited ceiling. Leave blank to share the parent allowance.";
      },
    })),
    {
      name: "monthly_budget_usd", label: "Monthly budget (USD)", type: "text", inputMode: "decimal", maxLength: MAX_MONEY_INPUT_LENGTH,
      value: policy.monthly_budget_microusd == null ? "" : microUsdToDollars(policy.monthly_budget_microusd),
      help: "Enter dollars (up to 6 decimal places). Blank means shared parent allowance, not a reserved allocation.",
      validate: value => {
        if (!value.trim()) return;
        const error = dollarsError(value);
        if (error) return error;
        const amount = BigInt(dollarsToMicroUsd(value));
        if (amount === 0n) return "Budget must be greater than $0.00 USD, or blank to inherit the parent allowance.";
        const parent = ceiling?.monthly_budget_microusd;
        if (parent != null && amount > BigInt(parent)) return `Budget cannot exceed the inherited ceiling of ${formatMicroUsd(parent)} USD. Leave blank to share the parent allowance.`;
      },
    },
  ];
}
export function effectivePolicy(local: Policy, parent: Policy): Policy {
  const minimum = (a: number | null, b: number | null) => a === null ? b : b === null ? a : Math.min(a, b);
  const a = local.monthly_budget_microusd, b = parent.monthly_budget_microusd;
  return { requests_per_minute: minimum(local.requests_per_minute, parent.requests_per_minute), tokens_per_minute: minimum(local.tokens_per_minute, parent.tokens_per_minute), concurrent_requests: minimum(local.concurrent_requests, parent.concurrent_requests), monthly_budget_microusd: a === null ? b : b === null ? a : BigInt(a) < BigInt(b) ? a : b };
}
export function policyBody(values: Values): Policy {
  return { requests_per_minute: values.requests_per_minute ? Number(values.requests_per_minute) : null, tokens_per_minute: values.tokens_per_minute ? Number(values.tokens_per_minute) : null, concurrent_requests: values.concurrent_requests ? Number(values.concurrent_requests) : null, monthly_budget_microusd: values.monthly_budget_usd?.trim() ? dollarsToMicroUsd(values.monthly_budget_usd) : null };
}
export function modelRoutingBody(values: Values): ModelRouting {
  return { strategy: values.strategy as ModelRouting["strategy"], max_attempts: Number(values.max_attempts), allow_ambiguous_failover: values.allow_ambiguous_failover === "true", failure_threshold: Number(values.failure_threshold), cooldown_seconds: Number(values.cooldown_seconds), required_residency: values.required_residency || null };
}
export function deploymentRoutingBody(values: Values, current: DeploymentRouting["routing"], operator: boolean): DeploymentRouting["routing"] {
  return { priority: Number(values.priority), weight: Number(values.weight), residency: operator ? values.residency : current.residency, ...(current.operator_disabled !== undefined ? { operator_disabled: current.operator_disabled } : {}) };
}
export function priceBody(values: Values): Omit<Price, "id" | "created_at"> {
  return {
    input_microusd_per_million: dollarsToMicroUsd(values.input_usd_per_million),
    output_microusd_per_million: dollarsToMicroUsd(values.output_usd_per_million),
    input_token_limit: Number(values.input_token_limit),
    output_token_limit: Number(values.output_token_limit),
  };
}
export function priceFields(price?: Price): Field[] {
  return [
    { name: "input_usd_per_million", label: "Input rate (USD per million tokens)", type: "text", inputMode: "decimal", required: true, maxLength: MAX_MONEY_INPUT_LENGTH, validate: dollarsError, value: price ? microUsdToDollars(price.input_microusd_per_million) : "", help: "Enter dollars per million tokens (up to 6 decimal places). Zero is allowed." },
    { name: "output_usd_per_million", label: "Output rate (USD per million tokens)", type: "text", inputMode: "decimal", required: true, maxLength: MAX_MONEY_INPUT_LENGTH, validate: dollarsError, value: price ? microUsdToDollars(price.output_microusd_per_million) : "" },
    { ...integerField("input_token_limit", "Hard upstream input token ceiling"), value: price?.input_token_limit.toString() ?? "" },
    { ...integerField("output_token_limit", "Hard upstream output token ceiling"), value: price?.output_token_limit.toString() ?? "", validate: (value, values) => {
      const validInteger = integerField("", "").validate?.(value, values);
      if (validInteger) return validInteger;
      const inputTokens = values.input_token_limit?.trim() ?? "";
      if (integerField("", "").validate?.(inputTokens, values) || !/^\d+$/.test(inputTokens) || !/^\d+$/.test(value)) return;
      const input = values.input_usd_per_million ?? "", output = values.output_usd_per_million ?? "";
      if (dollarsError(input) || dollarsError(output)) return;
      const inputRate = BigInt(dollarsToMicroUsd(input)), outputRate = BigInt(dollarsToMicroUsd(output));
      // Counters were checked as safe integers above; money never uses Number.
      const inputLimit = BigInt(Number(inputTokens)), outputLimit = BigInt(Number(value));
      if ((inputRate * inputLimit + 999999n) / 1000000n + (outputRate * outputLimit + 999999n) / 1000000n > MAX_MICROUSD) return "Maximum reservation exceeds $9,223,372,036,854.775807 USD. Reduce rates or token ceilings.";
    } },
  ];
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

/*
 * Stacked limits (ux-api-contract "Stacked budgets"): every scope holds three
 * rate limits and at most one budget per period (day, week, month, lifetime).
 * All applicable limits compose: rates take the minimum, budgets take the
 * minimum per period, and budgets of different periods are all enforced, each
 * over its own UTC window.
 *
 * Local and key layers are tighten-only, mirroring the server
 * (management/governance/policies.rs validate_tighten): a rate can't exceed a
 * parent's rate, a budget can't exceed a parent's budget for the same period,
 * and a saved cap (rate or budget for a period) can't be raised or removed.
 *
 * Money is an integer micro-USD string, converted from dollars with BigInt only.
 */
import { ApiError } from "./api";
import { dollarsToMicroUsd, formatMicroUsd, microUsdToDollars, type BudgetPeriod, type Policy, type PolicyBudget } from "./governance";

export type RateKey = "requests_per_minute" | "tokens_per_minute" | "concurrent_requests";
export type Limits = Record<RateKey, number | null> & { budgets: PolicyBudget[] };
export const stackPeriods: BudgetPeriod[] = ["day", "week", "month", "lifetime"];
export const rateRows: { key: RateKey; label: string; description: string; unit: string }[] = [
  { key: "requests_per_minute", label: "Requests per minute", description: "How many requests can start each minute. Fallback attempts count too.", unit: "per min" },
  { key: "tokens_per_minute", label: "Tokens per minute", description: "Input and output tokens a minute's requests can use, counted when each request starts.", unit: "per min" },
  { key: "concurrent_requests", label: "Requests running at once", description: "How many requests can be in progress at the same time.", unit: "at once" },
];
export const periodName: Record<BudgetPeriod, string> = { day: "Daily", week: "Weekly", month: "Monthly", lifetime: "Lifetime" };
/** Plain-language reset rule of a budget period. */
export function resetText(period: BudgetPeriod): string {
  return { day: "Resets daily at 00:00 UTC", week: "Resets weekly on Monday at 00:00 UTC", month: "Resets monthly on the 1st at 00:00 UTC", lifetime: "Never resets" }[period];
}
const periodRank = (p: BudgetPeriod) => stackPeriods.indexOf(p);
export const sortBudgets = (budgets: PolicyBudget[]) => [...budgets].sort((a, b) => periodRank(a.period) - periodRank(b.period));
/** Stacked budgets of a policy, or the legacy single budget of an older gateway. */
export function policyBudgets(policy: Pick<Policy, "budgets" | "monthly_budget_microusd" | "budget_period">): PolicyBudget[] {
  if (Array.isArray(policy.budgets)) return sortBudgets(policy.budgets.filter(b => stackPeriods.includes(b.period)));
  return policy.monthly_budget_microusd === null ? [] : [{ period: policy.budget_period ?? "month", amount_microusd: policy.monthly_budget_microusd }];
}
export function limitsOf(policy: Policy): Limits {
  return { requests_per_minute: policy.requests_per_minute, tokens_per_minute: policy.tokens_per_minute, concurrent_requests: policy.concurrent_requests, budgets: policyBudgets(policy) };
}
export const noLimits: Limits = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [] };
export const budgetFor = (limits: Pick<Limits, "budgets"> | undefined, period: BudgetPeriod) => limits?.budgets.find(b => b.period === period)?.amount_microusd ?? null;
const minAmount = (a: string | null, b: string | null) => a === null ? b : b === null ? a : BigInt(a) <= BigInt(b) ? a : b;
/** All layers apply: minimum rates, and the minimum budget per period. */
export function composeLimits(...layers: (Limits | undefined)[]): Limits {
  const known = layers.filter((l): l is Limits => !!l);
  const minimum = (key: RateKey) => known.reduce<number | null>((acc, l) => l[key] === null ? acc : acc === null ? l[key] : Math.min(acc, l[key]!), null);
  const budgets = stackPeriods.flatMap(period => { const amount = known.reduce<string | null>((acc, l) => minAmount(acc, budgetFor(l, period)), null); return amount === null ? [] : [{ period, amount_microusd: amount }]; });
  return { requests_per_minute: minimum("requests_per_minute"), tokens_per_minute: minimum("tokens_per_minute"), concurrent_requests: minimum("concurrent_requests"), budgets };
}
/** The full PUT body: three explicit rate fields plus the complete stacked budget set (never the legacy fields). */
export function limitsBody(limits: Limits) {
  return { requests_per_minute: limits.requests_per_minute, tokens_per_minute: limits.tokens_per_minute, concurrent_requests: limits.concurrent_requests, budgets: sortBudgets(limits.budgets).map(b => ({ period: b.period, amount_microusd: b.amount_microusd })) };
}
export const rateText = (value: number | null, unit = "") => value === null ? "No limit" : `${value.toLocaleString("en-US")}${unit ? ` ${unit}` : ""}`;
/** "$5.00 daily · $100.00 monthly", or the empty text. */
export function budgetsText(budgets: PolicyBudget[], empty = "No budget"): string {
  return budgets.length ? sortBudgets(budgets).map(b => `${formatMicroUsd(b.amount_microusd)} ${periodName[b.period].toLowerCase()}`).join(" · ") : empty;
}
/** One-line summary of a layer: "60 RPM · $5.00 daily", or "No limits". */
export function limitsSummary(limits: Partial<Limits> | null | undefined, empty = "No limits"): string {
  if (!limits) return empty;
  const parts = [limits.requests_per_minute != null ? `${limits.requests_per_minute.toLocaleString("en-US")} RPM` : "", limits.tokens_per_minute != null ? `${limits.tokens_per_minute.toLocaleString("en-US")} TPM` : "", limits.concurrent_requests != null ? `${limits.concurrent_requests.toLocaleString("en-US")} at once` : "", limits.budgets?.length ? budgetsText(limits.budgets) : ""].filter(Boolean);
  return parts.length ? parts.join(" · ") : empty;
}

/* Editable drafts. */
export type BudgetDraft = { key: string; period: BudgetPeriod; amount: string };
export type LimitsDraft = Record<RateKey, string> & { budgets: BudgetDraft[] };
let draftKeys = 0;
export const newBudgetKey = () => `b${++draftKeys}`;
export function draftOf(limits: Limits): LimitsDraft {
  return { requests_per_minute: limits.requests_per_minute?.toString() ?? "", tokens_per_minute: limits.tokens_per_minute?.toString() ?? "", concurrent_requests: limits.concurrent_requests?.toString() ?? "", budgets: limits.budgets.map(b => ({ key: `saved-${b.period}`, period: b.period, amount: microUsdToDollars(b.amount_microusd) })) };
}
export function sameDraft(a: LimitsDraft, b: LimitsDraft) {
  const budgets = (d: LimitsDraft) => JSON.stringify(sortBudgets(d.budgets.map(x => ({ period: x.period, amount_microusd: x.amount.trim() }))));
  return rateRows.every(r => a[r.key].trim() === b[r.key].trim()) && budgets(a) === budgets(b);
}
/** The first period not used by a row (to add a budget). */
export const nextPeriod = (draft: LimitsDraft): BudgetPeriod | undefined => (["month", "day", "week", "lifetime"] as BudgetPeriod[]).find(p => !draft.budgets.some(b => b.period === p));
const MAX_INT = 2147483647;
/** Why a typed rate can't be saved; blank is valid (no limit at this scope). */
export function rateError(raw: string): string | undefined {
  const value = raw.trim();
  if (!value) return;
  if (/^[-\u2212]\s*\d/.test(value) || /^0+$/.test(value)) return "Must be a positive whole number.";
  if (!/^\d+$/.test(value) || !Number.isSafeInteger(Number(value))) return "Enter a whole number.";
  const n = Number(value);
  return n > MAX_INT ? `Enter 1 to ${MAX_INT.toLocaleString("en-US")}, or leave blank for no limit.` : undefined;
}
/** Why a typed budget amount can't be saved. A budget row needs an amount above $0 (remove the row for no budget). */
export function budgetAmountError(raw: string): string | undefined {
  if (!raw.trim()) return "Enter an amount, or remove this budget.";
  let amount: bigint;
  try { amount = BigInt(dollarsToMicroUsd(raw)); } catch (error) { return (error as Error).message; }
  return amount === 0n ? "Enter more than $0.00, or remove this budget." : undefined;
}
/** Draft to limits; call only on a draft without errors. */
export function draftLimits(draft: LimitsDraft): Limits {
  const int = (v: string) => v.trim() ? Number(v.trim()) : null;
  return { requests_per_minute: int(draft.requests_per_minute), tokens_per_minute: int(draft.tokens_per_minute), concurrent_requests: int(draft.concurrent_requests), budgets: sortBudgets(draft.budgets.map(b => ({ period: b.period, amount_microusd: dollarsToMicroUsd(b.amount) }))) };
}
/** Rows whose typed value can't be used yet: their Effective cell shows "—" while the other rows keep theirs. */
export type InvalidRows = { rates: RateKey[]; periods: BudgetPeriod[] };
/**
 * The draft's valid part as limits, for a live Effective preview while other rows are being fixed: a rate with an
 * error and a budget with an error are left out and named in `invalid`. A saved tighten-only budget missing from the
 * draft (a form error) is marked invalid too, so its period never previews as uncapped.
 */
export function draftLimitsValid(draft: LimitsDraft, errors: DraftErrors, stored: Limits = noLimits): { limits: Limits; invalid: InvalidRows } {
  const invalid: InvalidRows = { rates: [], periods: [] };
  const rate = (key: RateKey) => { if (errors.rates[key]) { invalid.rates.push(key); return null; } const v = draft[key].trim(); return v ? Number(v) : null; };
  const budgets: PolicyBudget[] = [];
  for (const b of draft.budgets) {
    if (errors.budgets[b.key] || invalid.periods.includes(b.period)) { if (!invalid.periods.includes(b.period)) invalid.periods.push(b.period); continue; }
    budgets.push({ period: b.period, amount_microusd: dollarsToMicroUsd(b.amount) });
  }
  for (const b of draft.budgets) if (invalid.periods.includes(b.period)) { const i = budgets.findIndex(x => x.period === b.period); if (i >= 0) budgets.splice(i, 1); }
  if (errors.form.length) for (const s of stored.budgets) if (!draft.budgets.some(b => b.period === s.period) && !invalid.periods.includes(s.period)) invalid.periods.push(s.period);
  return { limits: { requests_per_minute: rate("requests_per_minute"), tokens_per_minute: rate("tokens_per_minute"), concurrent_requests: rate("concurrent_requests"), budgets: sortBudgets(budgets) }, invalid };
}
export type Parent = { label: string; limits: Limits };
export type DraftErrors = { rates: Partial<Record<RateKey, string>>; budgets: Record<string, string>; form: string[] };
export const hasErrors = (e: DraftErrors) => Object.keys(e.rates).length > 0 || Object.keys(e.budgets).length > 0 || e.form.length > 0;
const money = (amount: string) => formatMicroUsd(amount);
/**
 * Field errors. `free` (platform scopes, replacement overrides) checks syntax and one budget per period;
 * `tighten` (workspace and key layers) also enforces the server's tighten-only rules.
 */
export function draftErrors(draft: LimitsDraft, mode: "free" | "tighten", parents: Parent[] = [], stored: Limits = noLimits): DraftErrors {
  const errors: DraftErrors = { rates: {}, budgets: {}, form: [] };
  for (const r of rateRows) {
    const raw = draft[r.key].trim(), syntax = rateError(raw);
    if (syntax) { errors.rates[r.key] = syntax; continue; }
    if (mode === "free") continue;
    const n = raw ? Number(raw) : null, old = stored[r.key];
    if (old !== null && n === null) { errors.rates[r.key] = `A saved limit can't be removed here, only lowered (now ${old.toLocaleString("en-US")}).`; continue; }
    if (old !== null && n !== null && n > old) { errors.rates[r.key] = `A saved limit can only be lowered (now ${old.toLocaleString("en-US")}).`; continue; }
    for (const parent of parents) { const cap = parent.limits[r.key]; if (n !== null && cap !== null && n > cap) { errors.rates[r.key] = `Can't be higher than the ${parent.label} limit (${cap.toLocaleString("en-US")}).`; break; } }
  }
  const seen = new Set<BudgetPeriod>();
  for (const b of draft.budgets) {
    if (seen.has(b.period)) { errors.budgets[b.key] = `Only one ${periodName[b.period].toLowerCase()} budget is allowed.`; continue; }
    seen.add(b.period);
    const syntax = budgetAmountError(b.amount);
    if (syntax) { errors.budgets[b.key] = syntax; continue; }
    if (mode === "free") continue;
    const amount = BigInt(dollarsToMicroUsd(b.amount)), old = budgetFor(stored, b.period);
    if (old !== null && amount > BigInt(old)) { errors.budgets[b.key] = `A saved budget can only be lowered (now ${money(old)}).`; continue; }
    for (const parent of parents) { const cap = budgetFor(parent.limits, b.period); if (cap !== null && amount > BigInt(cap)) { errors.budgets[b.key] = `Can't be higher than the ${parent.label} ${periodName[b.period].toLowerCase()} budget (${money(cap)}).`; break; } }
  }
  if (draft.budgets.length > stackPeriods.length) errors.form.push("A scope can have at most one budget per period.");
  if (mode === "tighten") for (const b of stored.budgets) if (!seen.has(b.period)) errors.form.push(`The saved ${periodName[b.period].toLowerCase()} budget (${money(b.amount_microusd)}) can't be removed here, only lowered.`);
  return errors;
}
/** A stored budget row in a tighten-only layer: its period is fixed and it can't be removed. */
export const lockedBudget = (mode: "free" | "tighten", stored: Limits, row: BudgetDraft) => mode === "tighten" && row.key === `saved-${row.period}` && budgetFor(stored, row.period) !== null;

/* Server rejections (ux-api-contract "Policy rejection reasons"): `reason` plus the period or limit it concerns. */

const isPeriod = (value: string | undefined): value is BudgetPeriod => !!value && (stackPeriods as string[]).includes(value);
const rateLabel = (value: string | undefined) => rateRows.find(r => r.key === value)?.label;
type PolicyRejection = { message: string; rate?: RateKey; period?: BudgetPeriod; form?: boolean };
/** The specific, plain explanation of a policy rejection with a known `reason`, and which field it concerns. */
export function policyRejection(error: unknown): PolicyRejection | undefined {
  if (!(error instanceof ApiError) || !error.reason) return;
  const detail = error.detail?.period, period = isPeriod(detail) ? detail : undefined, budget = period ? `${periodName[period].toLowerCase()} budget` : "budget";
  const limit = rateRows.find(r => r.key === error.detail?.limit)?.key, rate = rateLabel(limit);
  switch (error.reason) {
    case "exceeds_parent_budget": return { period, message: `The ${budget} is higher than an inherited ${budget} for the same period. Lower it to at most the inherited amount.` };
    case "exceeds_parent_rate": return { rate: limit, message: `${rate ?? "A rate limit"} is higher than an inherited limit. Lower it to at most the inherited value.` };
    case "stored_budget_raise_not_allowed": return { period, message: `The saved ${budget} can only be lowered, not raised.` };
    case "period_change_not_allowed": return { form: true, message: `The saved ${budget} can't be removed or moved to another period. Keep it; you can lower its amount.` };
    case "stored_rate_loosen_not_allowed": return { rate: limit, message: `The saved ${rate ? `${rate.toLowerCase()} ` : ""}limit can only be lowered, never raised or removed.` };
    case "personal_limits_platform_controlled": return { form: true, message: "Personal workspace limits are set by the platform. Ask a Platform Admin to change them." };
    case "stacked_budgets_require_budgets_field": return { form: true, message: "This scope has several budgets. Reload the page and save them together." };
  }
}
/**
 * A plain message for a rejected policy save: the specific reason when the server names one, else a general
 * explanation of the tighten-only rules (malformed bodies are plain 400s without a reason).
 */
export function limitsSaveError(error: unknown): unknown {
  if (!(error instanceof ApiError)) return error;
  const known = policyRejection(error);
  if (known) return new ApiError(error.status, error.code, known.message, error.reason, error.detail);
  if (error.status === 400) return new ApiError(400, error.code, "Not saved: a limit is higher than an inherited limit for the same period, or a value isn't valid. Lower it and try again.", error.reason);
  if (error.status === 403) return new ApiError(403, error.code, "Not saved: saved caps can only be lowered, never raised or removed, and only people who manage this scope can change it.", error.reason);
  return error;
}
/**
 * A server rejection placed on the field it concerns: the rate input of `limit`, the budget row of `period` (or the
 * form when that row isn't in the draft). Undefined when the error has no known reason (show it as a notice instead).
 */
export function rejectionErrors(error: unknown, draft: Pick<LimitsDraft, "budgets">): DraftErrors | undefined {
  const known = policyRejection(error);
  if (!known) return;
  const errors: DraftErrors = { rates: {}, budgets: {}, form: [] };
  const row = known.period ? draft.budgets.find(b => b.period === known.period) : undefined;
  if (known.rate) errors.rates[known.rate] = known.message;
  else if (row && !known.form) errors.budgets[row.key] = known.message;
  else errors.form.push(known.message);
  return errors;
}
/** Client errors first; a server rejection fills only fields the client found nothing wrong with. */
export function mergeErrors(client: DraftErrors, server: DraftErrors | undefined): DraftErrors {
  if (!server) return client;
  return { rates: { ...server.rates, ...client.rates }, budgets: { ...server.budgets, ...client.budgets }, form: [...client.form, ...server.form.filter(f => !client.form.includes(f))] };
}

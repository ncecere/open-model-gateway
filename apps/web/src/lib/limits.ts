/*
 * Stacked limits (ux-api-contract "Stacked budgets"): every scope holds four
 * rate limits (including "Jobs at once" for video and batch jobs) and at most one budget per period (day, week, month, lifetime).
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
 *
 * "Storage" (`storage_bytes`, file store quota) exists on the workspace layers
 * only (type default, platform override, workspace local); installation and
 * key layers have none. It is typed with a unit ("5 GB", "500 MB"; a plain
 * number is GB, 1 GB = 2^30 bytes) and follows the same tighten-only rules.
 */
import { ApiError } from "./api";
import { dollarsToMicroUsd, formatMicroUsd, microUsdToDollars, type BudgetPeriod, type Policy, type PolicyBudget } from "./governance";

export type RateKey = "requests_per_minute" | "tokens_per_minute" | "concurrent_requests" | "concurrent_jobs";
export type Limits = Record<RateKey, number | null> & { budgets: PolicyBudget[]; /** Workspace layers only; absent = none. */ storage_bytes?: number | null };
export const stackPeriods: BudgetPeriod[] = ["day", "week", "month", "lifetime"];
export const rateRows: { key: RateKey; label: string; description: string; unit: string }[] = [
  { key: "requests_per_minute", label: "Requests per minute", description: "How many requests can start each minute. Fallback attempts count too.", unit: "per min" },
  { key: "tokens_per_minute", label: "Tokens per minute", description: "Input and output tokens a minute's requests can use, counted when each request starts.", unit: "per min" },
  { key: "concurrent_requests", label: "Requests running at once", description: "How many requests can be in progress at the same time.", unit: "at once" },
  { key: "concurrent_jobs", label: "Jobs at once", description: "How many video and batch jobs can run at the same time. Jobs don't use the per-minute or requests-at-once limits.", unit: "at once" },
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
  return { requests_per_minute: policy.requests_per_minute, tokens_per_minute: policy.tokens_per_minute, concurrent_requests: policy.concurrent_requests, concurrent_jobs: policy.concurrent_jobs ?? null, budgets: policyBudgets(policy), storage_bytes: policy.storage_bytes ?? null };
}
export const noLimits: Limits = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, budgets: [] };
export const budgetFor = (limits: Pick<Limits, "budgets"> | undefined, period: BudgetPeriod) => limits?.budgets.find(b => b.period === period)?.amount_microusd ?? null;
const minAmount = (a: string | null, b: string | null) => a === null ? b : b === null ? a : BigInt(a) <= BigInt(b) ? a : b;
/** All layers apply: minimum rates, and the minimum budget per period. */
export function composeLimits(...layers: (Limits | undefined)[]): Limits {
  const known = layers.filter((l): l is Limits => !!l);
  const minimum = (key: RateKey) => known.reduce<number | null>((acc, l) => l[key] === null ? acc : acc === null ? l[key] : Math.min(acc, l[key]!), null);
  const budgets = stackPeriods.flatMap(period => { const amount = known.reduce<string | null>((acc, l) => minAmount(acc, budgetFor(l, period)), null); return amount === null ? [] : [{ period, amount_microusd: amount }]; });
  const storage = known.reduce<number | null>((acc, l) => l.storage_bytes == null ? acc : acc === null ? l.storage_bytes : Math.min(acc, l.storage_bytes), null);
  return { requests_per_minute: minimum("requests_per_minute"), tokens_per_minute: minimum("tokens_per_minute"), concurrent_requests: minimum("concurrent_requests"), concurrent_jobs: minimum("concurrent_jobs"), budgets, storage_bytes: storage };
}
/**
 * The full PUT body: four explicit rate fields plus the complete stacked budget set (never the legacy fields).
 * `storage_bytes` is sent only for workspace layers (`storage`); elsewhere it is omitted, which keeps the stored value.
 */
export function limitsBody(limits: Limits, storage = false) {
  return { requests_per_minute: limits.requests_per_minute, tokens_per_minute: limits.tokens_per_minute, concurrent_requests: limits.concurrent_requests, concurrent_jobs: limits.concurrent_jobs, ...storage ? { storage_bytes: limits.storage_bytes ?? null } : {}, budgets: sortBudgets(limits.budgets).map(b => ({ period: b.period, amount_microusd: b.amount_microusd })) };
}

/* Storage quota (bytes): exact text with a unit, and parsing. */
const GB = 1073741824, MB = 1048576;
/** Largest storage quota the server accepts (1 PiB). */
export const MAX_STORAGE_BYTES = 2 ** 50;
/** "1 GB", "1.5 GB", "500 MB"; exact (a size that isn't whole MB reads in bytes). */
export function storageText(bytes: number | null | undefined, empty = "No limit"): string {
  if (bytes == null) return empty;
  if (bytes % MB !== 0) return `${bytes.toLocaleString("en-US")} B`;
  const gb = bytes / GB;
  if (bytes >= GB && Number.isInteger(gb * 1000)) return `${gb.toLocaleString("en-US", { maximumFractionDigits: 3 })} GB`;
  return `${(bytes / MB).toLocaleString("en-US")} MB`;
}
/** Draft text of a stored quota (no thousands separators, so it parses back exactly). */
export const storageDraft = (bytes: number | null | undefined) => bytes == null ? "" : storageText(bytes).replace(/,/g, "");
/** Bytes of a typed quota ("5", "5 GB", "1.5gb", "500 MB", "2 TB", "1048576 B"); a plain number is GB. */
export function parseStorage(raw: string): number | undefined {
  const m = /^(\d+(?:\.\d{1,3})?)\s*(tb|gb|mb|b)?$/i.exec(raw.trim().replace(/,/g, ""));
  if (!m) return;
  const unit = (m[2] ?? "gb").toLowerCase(), scale = unit === "tb" ? GB * 1024 : unit === "gb" ? GB : unit === "mb" ? MB : 1;
  const [whole, frac = ""] = m[1]!.split(".");
  if (unit === "b" && frac) return;
  // Exact: thousandths of the unit, in integer arithmetic.
  const milli = BigInt(whole!) * 1000n + BigInt(frac.padEnd(3, "0") || "0"), bytes = milli * BigInt(scale) / 1000n;
  return milli * BigInt(scale) % 1000n === 0n && bytes <= BigInt(MAX_STORAGE_BYTES) ? Number(bytes) : undefined;
}
/** Why a typed quota can't be saved; blank is valid (no limit at this scope). */
export function storageError(raw: string): string | undefined {
  if (!raw.trim()) return;
  const bytes = parseStorage(raw);
  if (bytes === undefined) return "Enter a size like 5 GB or 500 MB.";
  return bytes === 0 ? "Must be more than 0." : undefined;
}
export const rateText = (value: number | null, unit = "") => value === null ? "No limit" : `${value.toLocaleString("en-US")}${unit ? ` ${unit}` : ""}`;
/** "$5.00 daily · $100.00 monthly", or the empty text. */
export function budgetsText(budgets: PolicyBudget[], empty = "No budget"): string {
  return budgets.length ? sortBudgets(budgets).map(b => `${formatMicroUsd(b.amount_microusd)} ${periodName[b.period].toLowerCase()}`).join(" · ") : empty;
}
/** One-line summary of a layer: "60 RPM · $5.00 daily", or "No limits". */
export function limitsSummary(limits: Partial<Limits> | null | undefined, empty = "No limits"): string {
  if (!limits) return empty;
  const parts = [limits.storage_bytes != null ? `${storageText(limits.storage_bytes)} storage` : "", limits.requests_per_minute != null ? `${limits.requests_per_minute.toLocaleString("en-US")} RPM` : "", limits.tokens_per_minute != null ? `${limits.tokens_per_minute.toLocaleString("en-US")} TPM` : "", limits.concurrent_requests != null ? `${limits.concurrent_requests.toLocaleString("en-US")} at once` : "", limits.concurrent_jobs != null ? `${limits.concurrent_jobs.toLocaleString("en-US")} ${limits.concurrent_jobs === 1 ? "job" : "jobs"} at once` : "", limits.budgets?.length ? budgetsText(limits.budgets) : ""].filter(Boolean);
  return parts.length ? parts.join(" · ") : empty;
}

/* Editable drafts. */
export type BudgetDraft = { key: string; period: BudgetPeriod; amount: string };
export type LimitsDraft = Record<RateKey, string> & { budgets: BudgetDraft[]; /** Storage quota text ("5 GB"); workspace layers only. */ storage?: string };
let draftKeys = 0;
export const newBudgetKey = () => `b${++draftKeys}`;
export function draftOf(limits: Limits): LimitsDraft {
  return { requests_per_minute: limits.requests_per_minute?.toString() ?? "", tokens_per_minute: limits.tokens_per_minute?.toString() ?? "", concurrent_requests: limits.concurrent_requests?.toString() ?? "", concurrent_jobs: limits.concurrent_jobs?.toString() ?? "", budgets: limits.budgets.map(b => ({ key: `saved-${b.period}`, period: b.period, amount: microUsdToDollars(b.amount_microusd) })), storage: storageDraft(limits.storage_bytes) };
}
export function sameDraft(a: LimitsDraft, b: LimitsDraft) {
  const budgets = (d: LimitsDraft) => JSON.stringify(sortBudgets(d.budgets.map(x => ({ period: x.period, amount_microusd: x.amount.trim() }))));
  return rateRows.every(r => a[r.key].trim() === b[r.key].trim()) && budgets(a) === budgets(b) && (a.storage ?? "").trim() === (b.storage ?? "").trim();
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
  return { requests_per_minute: int(draft.requests_per_minute), tokens_per_minute: int(draft.tokens_per_minute), concurrent_requests: int(draft.concurrent_requests), concurrent_jobs: int(draft.concurrent_jobs), budgets: sortBudgets(draft.budgets.map(b => ({ period: b.period, amount_microusd: dollarsToMicroUsd(b.amount) }))), storage_bytes: draft.storage?.trim() ? parseStorage(draft.storage) ?? null : null };
}
/** Rows whose typed value can't be used yet: their Effective cell shows "—" while the other rows keep theirs. */
export type InvalidRows = { rates: RateKey[]; periods: BudgetPeriod[]; storage?: boolean };
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
  if (errors.storage) invalid.storage = true;
  const storage = errors.storage || !draft.storage?.trim() ? null : parseStorage(draft.storage) ?? null;
  return { limits: { requests_per_minute: rate("requests_per_minute"), tokens_per_minute: rate("tokens_per_minute"), concurrent_requests: rate("concurrent_requests"), concurrent_jobs: rate("concurrent_jobs"), budgets: sortBudgets(budgets), storage_bytes: storage }, invalid };
}
export type Parent = { label: string; limits: Limits };
export type DraftErrors = { rates: Partial<Record<RateKey, string>>; budgets: Record<string, string>; form: string[]; storage?: string };
export const hasErrors = (e: DraftErrors) => Object.keys(e.rates).length > 0 || Object.keys(e.budgets).length > 0 || e.form.length > 0 || !!e.storage;
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
  if (draft.storage !== undefined) {
    const raw = draft.storage.trim(), syntax = storageError(raw), n = raw && !syntax ? parseStorage(raw)! : null, old = stored.storage_bytes ?? null;
    if (syntax) errors.storage = syntax;
    else if (mode === "tighten") {
      if (old !== null && n === null) errors.storage = `A saved quota can't be removed here, only lowered (now ${storageText(old)}).`;
      else if (old !== null && n !== null && n > old) errors.storage = `A saved quota can only be lowered (now ${storageText(old)}).`;
      else for (const parent of parents) { const cap = parent.limits.storage_bytes ?? null; if (n !== null && cap !== null && n > cap) { errors.storage = `Can't be higher than the ${parent.label} quota (${storageText(cap)}).`; break; } }
    }
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
const rateLabel = (value: string | undefined) => value === "storage_bytes" ? "Storage" : rateRows.find(r => r.key === value)?.label;
type PolicyRejection = { message: string; rate?: RateKey; period?: BudgetPeriod; form?: boolean; storage?: boolean };
/** The specific, plain explanation of a policy rejection with a known `reason`, and which field it concerns. */
export function policyRejection(error: unknown): PolicyRejection | undefined {
  if (!(error instanceof ApiError) || !error.reason) return;
  const detail = error.detail?.period, period = isPeriod(detail) ? detail : undefined, budget = period ? `${periodName[period].toLowerCase()} budget` : "budget";
  const limit = rateRows.find(r => r.key === error.detail?.limit)?.key, rate = rateLabel(error.detail?.limit), storage = error.detail?.limit === "storage_bytes";
  switch (error.reason) {
    case "exceeds_parent_rate": if (storage) return { storage, message: "Storage is higher than an inherited quota. Lower it to at most the inherited value." }; break;
    case "stored_rate_loosen_not_allowed": if (storage) return { storage, message: "The saved storage quota can only be lowered, never raised or removed." }; break;
  }
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
  if (known.storage) errors.storage = known.message;
  else if (known.rate) errors.rates[known.rate] = known.message;
  else if (row && !known.form) errors.budgets[row.key] = known.message;
  else errors.form.push(known.message);
  return errors;
}
/** Client errors first; a server rejection fills only fields the client found nothing wrong with. */
export function mergeErrors(client: DraftErrors, server: DraftErrors | undefined): DraftErrors {
  if (!server) return client;
  return { rates: { ...server.rates, ...client.rates }, budgets: { ...server.budgets, ...client.budgets }, form: [...client.form, ...server.form.filter(f => !client.form.includes(f))], storage: client.storage ?? server.storage };
}

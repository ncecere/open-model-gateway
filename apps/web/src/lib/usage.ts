/*
 * Usage & costs: pure helpers for the Overview / Explore / Records pages and the
 * routed chart page (pages/usage/*). API contract: .local/enterprise-rebuild/ux-api-contract.md §5.
 *
 * Money is an integer micro-USD decimal string end to end: formatting, totals,
 * minima/maxima and CSV cells use strings and BigInt, never Number/parseFloat.
 * Every series value is a decimal string too (`null` = undefined rate); Number
 * only positions chart marks, and chart labels map back to the exact strings.
 * `null` (unknown) never becomes zero.
 * Periods are whole UTC days; the API end date is exclusive, the UI shows the
 * last included day.
 */
import type { CostStatusFilter, DashboardSearch } from "./permissions";
import type { InstallationBudget } from "./api";
import { formatMicroUsd, type Cost, type WorkloadKind } from "./governance";
import { dateError, formatCount } from "./reports";
import { requestStatuses, type RequestStatus } from "./requests";

/* ---------------- API shapes (contract §5) ---------------- */

export type DailyValue = { date: string; value: string | null };
export type Tile = { value: string | null; previous: string | null; delta?: string | null; change_ratio?: string | null; daily: DailyValue[] };
export type UsageOverview = {
  scope?: "workspace" | "platform";
  period: { start_date: string; end_date: string; timezone?: "UTC" };
  previous_period: { start_date: string; end_date: string };
  observed_at?: string;
  tiles: {
    spend: Tile & { held_microusd: string | null; unresolved_attempts: string | null };
    requests: Tile & { attempts: string | null };
    tokens: Tile & { input_tokens: string | null; output_tokens: string | null; unknown_token_attempts: string | null };
    cache_hit_rate: Tile;
    blended_microusd_per_million: Tile;
  };
  top: { models: TopRow[]; keys: TopRow[]; members: TopRow[] | null };
  /** Platform scope only (null for workspaces); ignores filters. */
  installation_budgets?: InstallationBudget[] | null;
};
/**
 * A top-10 row. `blended_microusd_per_million`: the row's settled cost per 1M tokens of its settled attempts (decimal
 * string, null when unknown). Model rows also carry `model_id` (null when the alias mapped to several models).
 */
export type TopRow = { id: string | null; name: string | null; spend_microusd: string; requests: string; tokens: string; share: string | null; blended_microusd_per_million?: string | null; model_id?: string | null };
export type ExploreGroup = { id: string | null; name: string | null };
export type ExploreRow = { group: ExploreGroup; then: ExploreGroup | null; value: string | null; share: string | null; held_microusd: string | null; unresolved_attempts: string | null };
export type ExploreResponse = {
  metric: ExploreMetric; group_by: Dimension; then_by: Dimension | null;
  period: { start_date: string; end_date: string };
  total: { value: string | null; held_microusd: string | null; unresolved_attempts: string | null };
  rows: ExploreRow[];
  other: { value: string | null; share: string | null } | null;
  truncated: boolean;
  /** Daily values of the top first-level groups: decimal strings ("0" on empty days), null for an undefined rate. */
  series: { date: string; values: { id: string | null; value: string | null }[] }[];
};
/** A budget window from GET …/policy (contract §8), tolerant of the legacy field names. */
export type UsageBudgetWindow = { layer: "platform" | "local" | "key"; period?: BudgetSpan; budget_period?: BudgetSpan; amount_microusd?: string; monthly_budget_microusd?: string; window_start?: string | null; window_end?: string | null; usage_visible?: boolean; used_microusd?: string | null; unresolved_usage?: boolean | null; exhausted?: boolean | null };
export type BudgetSpan = "day" | "week" | "month" | "lifetime";

/* ---------------- Period (URL-backed, UTC) ---------------- */

export type UsageRange = "month" | "7d" | "30d" | "custom";
export const usageRanges: { value: UsageRange; label: string }[] = [{ value: "month", label: "This month" }, { value: "7d", label: "Last 7 days" }, { value: "30d", label: "Last 30 days" }, { value: "custom", label: "Custom" }];
export const MAX_PERIOD_DAYS = 93;
export type UsagePeriod = { preset: UsageRange; start_date: string; /** Exclusive (API). */ end_date: string; /** Inclusive (UI). */ last_date: string; days: number; /** Ends today: today's numbers are still growing. */ partial: boolean };

const isoDay = (d: Date) => d.toISOString().slice(0, 10);
export function addDays(iso: string, n: number): string { const d = new Date(`${iso}T00:00:00Z`); d.setUTCDate(d.getUTCDate() + n); return isoDay(d); }
export const daysBetween = (start: string, end: string) => Math.round((Date.parse(`${end}T00:00:00Z`) - Date.parse(`${start}T00:00:00Z`)) / 86400000);

/** Why a custom inclusive range can't be used, or undefined. */
export function customRangeError(start: string, last: string, now = new Date()): string | undefined {
  if (!start || !last) return "Choose a start and an end date.";
  if (dateError(start) || dateError(last)) return "Enter real dates (YYYY-MM-DD).";
  if (last < start) return "The end date must be on or after the start date.";
  if (last > isoDay(now)) return "The end date can't be in the future.";
  if (daysBetween(start, last) + 1 > MAX_PERIOD_DAYS) return `Pick up to ${MAX_PERIOD_DAYS} days.`;
}

/** The period in the URL: `range` preset (default this month) or `range=custom` with `start_date`/`end_date` (exclusive). */
export function usagePeriod(search: Pick<DashboardSearch, "range" | "start_date" | "end_date">, now = new Date()): UsagePeriod {
  const today = isoDay(now), tomorrow = addDays(today, 1);
  const make = (preset: UsageRange, start_date: string, end_date: string): UsagePeriod => ({ preset, start_date, end_date, last_date: addDays(end_date, -1), days: daysBetween(start_date, end_date), partial: end_date === tomorrow });
  const custom = search.range === "custom" || (search.range === undefined && !!search.start_date && !!search.end_date);
  if (custom && search.start_date && search.end_date) {
    const error = customRangeError(search.start_date, addDays(search.end_date, -1), now);
    if (error) throw new Error(error);
    return make("custom", search.start_date, search.end_date);
  }
  if (search.range === "7d") return make("7d", addDays(tomorrow, -7), tomorrow);
  if (search.range === "30d") return make("30d", addDays(tomorrow, -30), tomorrow);
  return make(custom ? "custom" : "month", `${today.slice(0, 7)}-01`, tomorrow);
}

/** URL entries for a period choice; this month (the default) leaves the URL clean. */
export function periodToSearch(preset: UsageRange, custom?: { start: string; last: string }): Pick<DashboardSearch, "range" | "start_date" | "end_date"> {
  if (preset === "custom" && custom) return { range: "custom", start_date: custom.start, end_date: addDays(custom.last, 1) };
  return { range: preset === "month" ? undefined : preset, start_date: undefined, end_date: undefined };
}

const monthDay = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
const fullDay = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" });
const utc = (iso: string) => new Date(`${iso}T00:00:00Z`);
export const shortDate = (iso: string) => monthDay.format(utc(iso));
export const longDate = (iso: string) => fullDay.format(utc(iso));
const boundaryTime = new Intl.DateTimeFormat("en-US", { hour: "2-digit", minute: "2-digit", hourCycle: "h23", timeZone: "UTC" });
/**
 * A budget or period boundary, always in UTC (the one convention for windows): "Nov 1, 2026, 00:00 UTC".
 * Unparseable values are returned as given rather than guessed.
 */
export function utcBoundary(iso: string): string {
  const d = new Date(iso);
  return Number.isFinite(d.getTime()) ? `${fullDay.format(d)}, ${boundaryTime.format(d)} UTC` : iso;
}
/** "Resets Nov 1, 2026, 00:00 UTC", or "Never resets" for a window without an end (lifetime). */
export function resetsAt(windowEnd: string | null | undefined, lowercase = false): string {
  const text = windowEnd ? `Resets ${utcBoundary(windowEnd)}` : "Never resets";
  return lowercase ? text.charAt(0).toLowerCase() + text.slice(1) : text;
}
/** "Oct 1 – Oct 8, 2026 (UTC, today so far)". */
export function periodLabel(p: Pick<UsagePeriod, "start_date" | "last_date" | "partial">): string {
  const sameYear = p.start_date.slice(0, 4) === p.last_date.slice(0, 4);
  const range = p.start_date === p.last_date ? longDate(p.start_date) : `${sameYear ? shortDate(p.start_date) : longDate(p.start_date)} – ${longDate(p.last_date)}`;
  return `${range} (UTC${p.partial ? ", today so far" : ""})`;
}
export const comparisonLabel = (days: number) => `vs previous ${days} day${days === 1 ? "" : "s"}`;

/* ---------------- Exact formatting ---------------- */

const DECIMAL = /^\d{1,64}(?:\.\d{1,64})?$/;
const group = (digits: string) => digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
/** A non-negative decimal micro-USD string (e.g. a blended rate "1234.5") as exact dollars: "$0.0012345". */
export function formatDecimalMicroUsd(value: string | null | undefined): string {
  if (value == null || !DECIMAL.test(value)) return "Unknown";
  const [int, frac = ""] = value.split(".");
  const digits = int.replace(/^0+(?=\d)/, "").padStart(7, "0");
  const whole = digits.slice(0, -6).replace(/^0+(?=\d)/, ""), fraction = `${digits.slice(-6)}${frac}`.replace(/0+$/, "").padEnd(2, "0");
  return `$${group(whole)}.${fraction}`;
}
/**
 * A decimal micro-USD rate (blended $/1M tokens) for tight spaces: at most `maxDecimals` significant decimal digits,
 * rounded half up with BigInt (never floats). From $1 up that is `maxDecimals` decimal places ("$3.986823"); below $1
 * the digits count from the first non-zero one ("$0.00000125" stays exact), so a non-zero value never becomes $0.
 * `exact` is the full value (for a tooltip); `rounded` tells whether digits were dropped.
 */
export function formatRateMicroUsd(value: string | null | undefined, maxDecimals = 6): { text: string; exact: string; rounded: boolean } {
  const exact = formatDecimalMicroUsd(value);
  if (value == null || !DECIMAL.test(value)) return { text: exact, exact, rounded: false };
  const [int, frac = ""] = value.split(".");
  const digits = int.replace(/^0+(?=\d)/, "").padStart(7, "0"), whole = digits.slice(0, -6).replace(/^0+(?=\d)/, ""), fraction = `${digits.slice(-6)}${frac}`.replace(/0+$/, "");
  const first = fraction.search(/[1-9]/), keep = whole !== "0" || first === -1 ? maxDecimals : first + maxDecimals;
  if (fraction.length <= keep) return { text: exact, exact, rounded: false };
  const units = BigInt(`${whole}${fraction.slice(0, keep)}`) + (fraction.charCodeAt(keep) >= 53 /* "5" */ ? 1n : 0n);
  const s = units.toString().padStart(keep + 1, "0"), w = s.slice(0, -keep), f = s.slice(-keep).replace(/0+$/, "").padEnd(2, "0");
  return { text: `$${group(w)}.${f}`, exact, rounded: true };
}
/** A ratio decimal string ("0.2513") as an exact percentage ("25.13%"); null stays null. */
export function formatRatioPercent(value: string | null | undefined): string | null {
  if (value == null || !DECIMAL.test(value)) return null;
  const [int, frac = ""] = value.split("."), padded = frac.padEnd(2, "0");
  const whole = BigInt(int) * 100n + BigInt(padded.slice(0, 2)), rest = padded.slice(2).replace(/0+$/, "");
  return `${whole.toLocaleString("en-US")}${rest ? `.${rest}` : ""}%`;
}
/** Compares two non-negative decimal strings exactly. */
export function compareDecimal(a: string, b: string): -1 | 0 | 1 {
  const [ai, af = ""] = a.split("."), [bi, bf = ""] = b.split("."), len = Math.max(af.length, bf.length);
  const x = BigInt(ai + af.padEnd(len, "0")), y = BigInt(bi + bf.padEnd(len, "0"));
  return x === y ? 0 : x > y ? 1 : -1;
}
export type Change = { direction: "up" | "down" | "flat"; text: string };
/**
 * Δ for decimal tiles (cache hit rate, $/M) from the server's exact `change_ratio`. No comparison when either side is
 * unknown; "New" when the previous value was zero.
 */
export function decimalChange(current: string | null | undefined, previous: string | null | undefined, ratio: string | null | undefined): Change | null {
  if (current == null || previous == null || !DECIMAL.test(current) || !DECIMAL.test(previous)) return null;
  const order = compareDecimal(current, previous);
  if (order === 0) return { direction: "flat", text: "0%" };
  if (/^0+(?:\.0+)?$/.test(previous)) return { direction: "up", text: "New" };
  if (ratio == null || !/^-?\d+(?:\.\d+)?$/.test(ratio)) return null;
  const pct = formatRatioPercent(ratio.replace(/^-/, ""))!;
  return { direction: order > 0 ? "up" : "down", text: pct === "0%" ? (order > 0 ? "<+0.01%" : "<−0.01%") : `${order > 0 ? "+" : "−"}${pct}` };
}
/** An integer decimal string as an exact BigInt, else null (fractions, numbers and malformed values are unknown). */
export function exactInteger(value: string | null | undefined): bigint | null {
  return typeof value === "string" && /^\d{1,128}$/.test(value) ? BigInt(value) : null;
}
/** A non-negative decimal string as an exact scaled integer ("12.50" → 1250n at scale 2), else null. */
export function parseDecimal(value: string | null | undefined): { units: bigint; scale: number } | null {
  if (typeof value !== "string" || !DECIMAL.test(value)) return null;
  const [int, frac = ""] = value.split(".");
  return { units: BigInt(int + frac), scale: frac.length };
}
/** The exact mean of decimal strings, truncated to `scale` places; `exact` is false when digits were cut. */
export function meanDecimal(values: string[], scale: number): { value: string; exact: boolean } | null {
  const parsed = values.map(parseDecimal);
  if (!parsed.length || parsed.some(p => p === null)) return null;
  const common = Math.max(...parsed.map(p => p!.scale));
  const sum = parsed.reduce((acc, p) => acc + p!.units * 10n ** BigInt(common - p!.scale), 0n);
  return divideExact(sum, BigInt(parsed.length) * 10n ** BigInt(common), scale);
}
/** `numerator / denominator` as a decimal string truncated to `scale` places; `exact` is false when digits were cut. */
export function divideExact(numerator: bigint, denominator: bigint, scale: number): { value: string; exact: boolean } {
  const factor = 10n ** BigInt(scale), scaled = numerator * factor, q = scaled / denominator, exact = scaled % denominator === 0n;
  if (scale === 0) return { value: q.toString(), exact };
  const s = q.toString().padStart(scale + 1, "0"), whole = s.slice(0, -scale), frac = s.slice(-scale).replace(/0+$/, "");
  return { value: frac ? `${whole}.${frac}` : whole, exact };
}
/** Integer micro-USD in dollars without symbol or grouping ("0.000001"), for CSV cells. */
export function microUsdDecimal(value: string): string { const v = BigInt(value), frac = (v % 1000000n).toString().padStart(6, "0").replace(/0+$/, "").padEnd(2, "0"); return `${v / 1000000n}.${frac}`; }

/* ---------------- Metrics and dimensions ---------------- */

export type ExploreMetric = "spend" | "requests" | "tokens" | "cache_hit_rate";
export type ChartMetric = ExploreMetric | "blended";
export type Dimension = "model" | "key" | "member" | "workspace" | "cost_center" | "provider" | "day";
export const metricInfo: Record<ChartMetric, { label: string; unit: string; additive: boolean; increaseIs: "good" | "bad" | "neutral" }> = {
  spend: { label: "Spend", unit: "USD", additive: true, increaseIs: "bad" },
  requests: { label: "Requests", unit: "requests", additive: true, increaseIs: "neutral" },
  tokens: { label: "Tokens", unit: "tokens", additive: true, increaseIs: "neutral" },
  cache_hit_rate: { label: "Cache hit rate", unit: "% of input tokens", additive: false, increaseIs: "good" },
  blended: { label: "Cost per million tokens", unit: "USD per 1M tokens", additive: false, increaseIs: "bad" },
};
export const chartMetrics = Object.keys(metricInfo) as ChartMetric[];
export const exploreMetrics: ExploreMetric[] = ["spend", "requests", "tokens", "cache_hit_rate"];
/** Who may see what: member breakdowns need workspace-wide visibility and never apply to Personal. */
export type UsageContext = { platform: boolean; members: boolean };
export function usageContext(workspace?: { kind: string; capabilities: { view_all_activity: boolean } }): UsageContext {
  return { platform: !workspace, members: !workspace || (workspace.kind !== "personal" && workspace.capabilities.view_all_activity) };
}
export function dimensionOptions(ctx: UsageContext): { value: Dimension; label: string }[] {
  return [{ value: "model", label: "Model" }, { value: "key", label: "API key" }, ...(ctx.members ? [{ value: "member" as const, label: "Member" }] : []), { value: "provider", label: "Provider" }, ...(ctx.platform ? [{ value: "workspace" as const, label: "Workspace" }, { value: "cost_center" as const, label: "Cost center" }] : []), { value: "day", label: "Day" }];
}
export const exploreTopN = [5, 10, 25];

/** One measure, exactly: money with `formatMicroUsd`, counts grouped, rates as percentages; unknown stays "Unknown". */
export function formatMetric(metric: ChartMetric, value: string | null | undefined): string {
  if (value == null) return "Unknown";
  const v = value;
  if (metric === "spend") return formatMicroUsd(v);
  if (metric === "blended") return formatDecimalMicroUsd(v);
  if (metric === "cache_hit_rate") return formatRatioPercent(v) ?? "Unknown";
  return formatCount(v);
}
/** Display name of a group: named rows as given; unnamed rows by dimension (never a bare UUID). */
export function groupName(g: ExploreGroup | null | undefined, dimension: Dimension | null): string {
  if (g?.name) return dimension === "day" ? longDate(g.name) : g.name;
  if (dimension === "cost_center") return "Unallocated";
  if (dimension === "member") return "Service accounts";
  return "Unknown";
}

/* ---------------- Filters (URL-backed, shared by every tab) ---------------- */

export const usageStatuses = requestStatuses;
/**
 * Overview/Explore/Records filters: one model (UUID), key and member (that user's human keys), several attempt
 * statuses, one cost center (UUID or `unallocated`) and one service account. In the URL as `model_id`, `key_id`,
 * `actor_user_id`, `status` (comma list), `cost_center_id` and `service_account_id`.
 */
export type UsageFilters = { model_id?: string; key_id?: string; member?: string; status: RequestStatus[]; cost_center_id?: string; service_account_id?: string };
/** Request (attempt) statuses in a comma list, deduplicated, in display order. */
export function statusList(value: string | undefined): RequestStatus[] {
  const picked = new Set((value ?? "").split(","));
  return usageStatuses.map(s => s.value).filter(v => picked.has(v));
}
/** The filters in the URL that this caller may use: no member filter without workspace-wide visibility. */
export function usageFilters(search: Pick<DashboardSearch, "model_id" | "key_id" | "actor_user_id" | "status" | "cost_center_id" | "service_account_id">, ctx: UsageContext): UsageFilters {
  return { model_id: search.model_id, key_id: search.key_id, member: ctx.members ? search.actor_user_id : undefined, status: statusList(search.status), cost_center_id: search.cost_center_id, service_account_id: search.service_account_id };
}
export const filterCount = (f: UsageFilters) => [f.model_id, f.key_id, f.member, f.status.length ? "status" : undefined, f.cost_center_id, f.service_account_id].filter(Boolean).length;
/** URL entries that clear every filter (and paging). */
export const clearedFilters: Pick<DashboardSearch, "model_id" | "key_id" | "actor_user_id" | "status" | "cost_center_id" | "service_account_id" | "cost_status" | "offset"> = { model_id: undefined, key_id: undefined, actor_user_id: undefined, status: undefined, cost_center_id: undefined, service_account_id: undefined, cost_status: undefined, offset: undefined };
function setFilters(q: URLSearchParams, f: UsageFilters | undefined, memberKey: "member_user_id" | "actor_user_id") {
  if (!f) return;
  if (f.key_id) q.set("key_id", f.key_id);
  if (f.member) q.set(memberKey, f.member);
  if (f.status.length) q.set("status", f.status.join(","));
  if (f.cost_center_id) q.set("cost_center_id", f.cost_center_id);
  if (f.service_account_id) q.set("service_account_id", f.service_account_id);
}

/* ---------------- Queries ---------------- */

export function usageQuery(period: Pick<UsagePeriod, "start_date" | "end_date">, workspaceId?: string, filters?: UsageFilters): string {
  const q = new URLSearchParams({ start_date: period.start_date, end_date: period.end_date });
  if (workspaceId) q.set("workspace_id", workspaceId);
  if (filters?.model_id) q.set("model_id", filters.model_id);
  setFilters(q, filters, "member_user_id");
  return q.toString();
}
export type Pivot = { metric: ExploreMetric; groupBy: Dimension; thenBy: Dimension | "none"; top: number };
export function exploreQuery(period: Pick<UsagePeriod, "start_date" | "end_date">, pivot: Pivot, workspaceId?: string, filters?: UsageFilters): string {
  const q = new URLSearchParams(usageQuery(period, workspaceId, filters));
  q.set("metric", pivot.metric); q.set("group_by", pivot.groupBy);
  if (pivot.thenBy !== "none") q.set("then_by", pivot.thenBy);
  if (pivot.groupBy !== "day") q.set("top", String(pivot.top));
  return q.toString();
}
/** Explore state from the URL (`metric`, `group`, `then`, `top`), dropping anything this caller isn't offered. */
export function pivotFromUsageSearch(search: Pick<DashboardSearch, "metric" | "group" | "then" | "top">, ctx: UsageContext): Pivot {
  const dims = dimensionOptions(ctx).map(d => d.value);
  const metric = exploreMetrics.includes(search.metric as ExploreMetric) ? search.metric as ExploreMetric : "spend";
  const groupBy = dims.includes(search.group as Dimension) ? search.group as Dimension : "model";
  const thenBy = dims.includes(search.then as Dimension) && search.then !== groupBy ? search.then as Dimension : "none";
  const top = exploreTopN.includes(Number(search.top)) ? Number(search.top) : 10;
  return { metric, groupBy, thenBy, top };
}
export function pivotToUsageSearch(p: Pivot): Pick<DashboardSearch, "metric" | "group" | "then" | "top"> {
  return { metric: p.metric === "spend" ? undefined : p.metric, group: p.groupBy === "model" ? undefined : p.groupBy, then: p.thenBy === "none" ? undefined : p.thenBy, top: p.top === 10 ? undefined : String(p.top) };
}

/* ---------------- Records (cost records) ---------------- */

export type RecordStatus = CostStatusFilter;
/** Plain words for one accounting state (the "Cost" filter); attempt statuses are the separate multi-choice filter. */
export const recordStatuses: { value: RecordStatus; label: string; state: "settled" | "pending" | "unknown" | "missing" }[] = [
  { value: "final", label: "Final", state: "settled" },
  { value: "on_hold", label: "On hold", state: "pending" },
  { value: "cost_unknown", label: "Cost unknown", state: "unknown" },
  { value: "not_recorded", label: "No cost record", state: "missing" },
];
/** Records filters: the shared filters, the model's API name (records match on it), one accounting state, and Admin's workspace. */
export type RecordFilters = UsageFilters & { model?: string; cost_status?: CostStatusFilter; workspace_id?: string };
/** Records/CSV/cost-report query: every filter in one request (the server takes a comma list of statuses). */
export function recordsQuery(period: Pick<UsagePeriod, "start_date" | "end_date">, filters: RecordFilters, opts: { platform: boolean; limit?: number; offset?: number }): string {
  const q = new URLSearchParams({ start_date: period.start_date, end_date: period.end_date });
  if (filters.model) q.set("model", filters.model);
  setFilters(q, filters, "actor_user_id");
  if (opts.platform && filters.workspace_id) q.set("workspace_id", filters.workspace_id);
  const state = recordStatuses.find(r => r.value === filters.cost_status)?.state;
  if (state) q.set("accounting_status", state);
  if (opts.limit !== undefined) q.set("limit", String(opts.limit));
  if (opts.offset !== undefined) q.set("offset", String(opts.offset));
  return q.toString();
}
const reasons: Record<string, string> = { missing_billing_usage: "provider didn't report usage", incomplete_usage_or_cache_allocation: "usage incomplete", incomplete_or_unpriced_meter_usage: "usage or price incomplete", unbounded_cost_or_unknown_rate: "no upper limit known", unpriced: "no price set", missing_reservation: "no cost record", incomplete_raw_usage: "usage incomplete" };
export function recordStatusText(c: Pick<Cost, "cost_status" | "cost_microusd" | "unresolved_reason">): string {
  if (c.cost_microusd != null || c.cost_status === "settled") return "Final";
  if (c.cost_status === "pending") return "On hold";
  return c.unresolved_reason && reasons[c.unresolved_reason] ? `Cost unknown: ${reasons[c.unresolved_reason]}` : "Cost unknown";
}
/** Final cost, else the amount on hold (a floor, not a cap), else Unknown. */
export function recordCostText(c: Pick<Cost, "cost_microusd" | "active_held_microusd">): string {
  if (c.cost_microusd != null) return formatMicroUsd(c.cost_microusd);
  if (c.active_held_microusd != null && /^\d+$/.test(c.active_held_microusd) && BigInt(c.active_held_microusd) > 0n) return `Unknown · ${formatMicroUsd(c.active_held_microusd)} on hold`;
  return "Unknown";
}
export const workloadLabels: Record<WorkloadKind, string> = { generation: "Text", embeddings: "Embeddings", images: "Images", audio_transcriptions: "Speech to text", audio_speech: "Text to speech", rerank: "Rerank", systemone: "System One" };

/* ---------------- Charts and per-group statistics ---------------- */

/** Days with a known, non-zero value (the overview hides its chart below two). */
export const activeDays = (daily: DailyValue[]) => daily.filter(d => d.value != null && !/^0+(?:\.0+)?$/.test(d.value)).length;
/** A series value of one group on one day (absent = no value). */
export function seriesValue(day: ExploreResponse["series"][number], id: string | null | undefined): string | null {
  return day.values.find(v => (v.id ?? null) === (id ?? null))?.value ?? null;
}
/** Chart coordinate only, for drawing (Number is never used for displayed amounts); rates become percentages. */
export function chartNumber(metric: ChartMetric, value: string | null | undefined): number {
  if (value == null || !DECIMAL.test(value)) return Number.NaN;
  const n = Number(value);
  return metric === "cache_hit_rate" ? n * 100 : n;
}
/**
 * Labels for drawn values (hover titles, peak, "Show data"): each coordinate maps back to the exact decimal string
 * it was drawn from. A coordinate shared by different exact values (beyond 2^53), or a stack total, falls back to
 * `chartLabel`.
 */
export function chartLabeler(metric: ChartMetric, values: (string | null | undefined)[]): (v: number) => string {
  const exact = new Map<number, string | null>();
  for (const value of values) {
    if (value == null || !DECIMAL.test(value)) continue;
    const n = chartNumber(metric, value), label = formatMetric(metric, value), seen = exact.get(n);
    exact.set(n, seen === undefined || seen === label ? label : null);
  }
  return v => exact.get(v) ?? chartLabel(metric, v);
}
/** Axis/hover text for a chart coordinate: exact for safe integers (micro-USD, counts), rounded for rates. */
export function chartLabel(metric: ChartMetric, v: number): string {
  if (!Number.isFinite(v)) return "No data";
  if (metric === "cache_hit_rate") return `${Math.round(v * 100) / 100}%`;
  if (metric === "blended") return formatDecimalMicroUsd(String(Math.max(0, Math.round(v))));
  const n = Math.max(0, Math.round(v));
  return Number.isSafeInteger(n) ? formatMetric(metric, String(n)) : "Too large to chart exactly";
}
export type GroupStats = { key: string; name: string; min: string; max: string; avg: string; total: string };
/**
 * Per-group Min / Max / Avg per day and Total over the period for the chart page. Additive metrics: exact BigInt
 * minima and maxima over every day (days without use count as zero), Avg = Total / days truncated (marked "≈" when
 * inexact; money to 1/100 micro-dollar), Total = the group's exact total. Rates: over days with data; Total is the group's overall rate.
 */
export function groupStats(res: ExploreResponse, metric: ExploreMetric, days: number): GroupStats[] {
  const firstLevel = res.rows.filter(r => r.then === null);
  return firstLevel.map((row, index) => {
    const values = res.series.map(d => seriesValue(d, row.group.id));
    const key = `g${index}`, name = groupName(row.group, res.group_by);
    if (metric === "cache_hit_rate") {
      const known = values.filter((v): v is string => v != null && DECIMAL.test(v));
      if (!known.length) return { key, name, min: "—", max: "—", avg: "—", total: formatMetric(metric, row.value) };
      // Exact mean of the daily rates, truncated to 1/100 of a percent ("≈" when digits were cut).
      const sorted = [...known].sort(compareDecimal), mean = meanDecimal(known, 4)!;
      return { key, name, min: formatMetric(metric, sorted[0]), max: formatMetric(metric, sorted[sorted.length - 1]), avg: `${mean.exact ? "" : "≈ "}${formatMetric(metric, mean.value)}`, total: formatMetric(metric, row.value) };
    }
    const exact = values.map(v => v == null ? 0n : exactInteger(v));
    const total = exactInteger(row.value);
    if (exact.some(v => v === null) || !exact.length) return { key, name, min: "Unknown", max: "Unknown", avg: total === null || days < 1 ? "Unknown" : avgText(metric, total, days), total: formatMetric(metric, row.value) };
    const list = exact as bigint[], min = list.reduce((a, b) => b < a ? b : a), max = list.reduce((a, b) => b > a ? b : a);
    return { key, name, min: formatMetric(metric, min.toString()), max: formatMetric(metric, max.toString()), avg: total === null || days < 1 ? "Unknown" : avgText(metric, total, days), total: formatMetric(metric, row.value) };
  });
}
function avgText(metric: ExploreMetric, total: bigint, days: number): string {
  // Money keeps two digits below the micro-dollar, so a sub-cent average never reads $0.00.
  const r = divideExact(total, BigInt(days), metric === "spend" ? 2 : 1);
  const text = metric === "spend" ? formatDecimalMicroUsd(r.value) : `${group(r.value.split(".")[0]!)}${r.value.includes(".") ? `.${r.value.split(".")[1]}` : ""}`;
  return r.exact ? text : `≈ ${text}`;
}

/* ---------------- CSV ---------------- */

/** One quoted CSV cell; leading formula characters are neutralized (spreadsheet injection). */
export function csvCell(value: string): string {
  const risky = /^[=+\-@\t\r\n]/.test(value.replace(/^[\s\u0000-\u001f]+/, "")) || /^[\t\r\n]/.test(value);
  return `"${risky ? "'" : ""}${value.replaceAll('"', '""')}"`;
}
export const csvLine = (cells: string[]) => cells.map(csvCell).join(",");
/** The Explore table exactly as shown (bounded by the pivot's Top N), with exact values and units in the headers. */
export function exploreCsv(res: ExploreResponse, period: Pick<UsagePeriod, "start_date" | "last_date">): string {
  const money = res.metric === "spend", then = res.then_by;
  const header = ["period_start", "period_end_inclusive", res.group_by, ...(then ? [then] : []), money ? "spend_usd" : res.metric, ...(money ? ["spend_microusd"] : []), "share_of_total", "on_hold_microusd", "cost_unknown_attempts"];
  const value = (v: string | null) => v == null ? "unknown" : money ? microUsdDecimal(v) : v;
  const lines = [csvLine(header), ...res.rows.map(r => csvLine([period.start_date, period.last_date, groupName(r.group, res.group_by), ...(then ? [groupName(r.then, then)] : []), value(r.value), ...(money ? [r.value ?? "unknown"] : []), r.share ?? "", r.held_microusd ?? "unknown", r.unresolved_attempts ?? "unknown"]))];
  if (res.other) lines.push(csvLine([period.start_date, period.last_date, "Other", ...(then ? [""] : []), value(res.other.value), ...(money ? [res.other.value ?? "unknown"] : []), res.other.share ?? "", "", ""]));
  return `${lines.join("\r\n")}\r\n`;
}
export function downloadText(text: string, filename: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/csv;charset=utf-8" })), link = document.createElement("a");
  link.href = url; link.download = filename; document.body.append(link); link.click(); link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/* ---------------- Budgets ---------------- */

/** Workspace-wide budget windows the caller may see usage for, smallest period first. Members get none (no workspace money). */
export function visibleBudgets(windows: UsageBudgetWindow[] | undefined): (UsageBudgetWindow & { span: BudgetSpan; amount: string })[] {
  const order: BudgetSpan[] = ["day", "week", "month", "lifetime"];
  return (windows ?? []).flatMap(w => { const span = w.period ?? w.budget_period ?? "month", amount = w.amount_microusd ?? w.monthly_budget_microusd; return w.layer !== "key" && w.usage_visible && amount ? [{ ...w, span, amount }] : []; })
    .sort((a, b) => order.indexOf(a.span) - order.indexOf(b.span) || (a.layer === b.layer ? 0 : a.layer === "platform" ? -1 : 1));
}

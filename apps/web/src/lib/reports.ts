import type { DashboardSearch } from "./permissions";
import type { CostReport, Period, Totals, BillingUsage, Cost } from "./governance";
import { formatMicroUsd } from "./governance";
import { uuidError, type Field, type Values } from "./forms";
export const reportFilterKeys = ["model", "provider", "workspace_id", "cost_center_id", "actor_user_id", "service_account_id", "accounting_status"] as const;
type DateRange = Pick<Period, "start_date" | "end_date">;
export function utcMonth(now = new Date()): DateRange { return { start_date: `${now.getUTCFullYear()}-${String(now.getUTCMonth() + 1).padStart(2, "0")}-01`, end_date: new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1)).toISOString().slice(0, 10) }; }
export function dateError(value: string): string | undefined { const date = new Date(`${value}T00:00:00Z`); return !/^\d{4}-\d{2}-\d{2}$/.test(value) || value < "0001-01-01" || !Number.isFinite(date.getTime()) || date.toISOString().slice(0, 10) !== value ? "Enter a real UTC date (YYYY-MM-DD)." : undefined; }
export function reportPeriod(search: Pick<DashboardSearch, "start_date" | "end_date">, now?: Date): DateRange {
  const current = utcMonth(now), start_date = search.start_date ?? current.start_date, end_date = search.end_date ?? current.end_date;
  if (dateError(start_date) || dateError(end_date)) throw new Error("Choose valid UTC start and end dates.");
  const days = (Date.parse(end_date) - Date.parse(start_date)) / 86400000;
  if (days < 1 || days > 93) throw new Error("Choose a UTC period of 1–93 days; the end date is exclusive.");
  if (end_date > utcMonth(now).end_date) throw new Error("The exclusive end date cannot exceed tomorrow UTC.");
  return { start_date, end_date };
}
export function reportQuery(search: DashboardSearch, platform: boolean, pagination?: { limit: number; offset: number }): string {
  const period = reportPeriod(search), query = new URLSearchParams(period);
  if (search.compare === "previous_period" && Date.parse(period.start_date) - (Date.parse(period.end_date) - Date.parse(period.start_date)) < Date.parse("0001-01-01T00:00:00Z")) throw new Error("The previous comparison period underflows supported dates.");
  query.set("compare", search.compare ?? "none");
  for (const key of reportFilterKeys) {
    const value = search[key]; if (value === undefined || value === "") continue;
    if (key === "workspace_id" && !platform && value !== search.ws) throw new Error("Workspace filter must match the current workspace.");
    if (key === "service_account_id" && platform) throw new Error("Platform totals do not expose private service-account dimensions.");
    if (["workspace_id", "actor_user_id", "service_account_id"].includes(key) && uuidError(value)) throw new Error(`Invalid ${key.replaceAll("_", " ")}.`);
    if (key === "cost_center_id" && value !== "unallocated" && uuidError(value)) throw new Error("Invalid cost center.");
    query.set(key, value);
  }
  if (pagination) { query.set("limit", String(pagination.limit)); query.set("offset", String(pagination.offset)); }
  return query.toString();
}
export function formatCount(value: string | number | null | undefined): string {
  if (value == null || typeof value === "number" && !Number.isSafeInteger(value) || !/^\d+$/.test(String(value))) return "Unknown";
  return BigInt(value).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}
/** "1 request", "2 requests", "Unknown requests": a count with its noun in the right number. */
export function countLabel(value: string | number | null | undefined, singular: string, plural = `${singular}s`): string {
  return `${formatCount(value)} ${value != null && String(value) === "1" ? singular : plural}`;
}
/**
 * Known spend beside its unresolved attempts: a known zero with attempts whose cost isn't known yet is "Unknown",
 * never "$0.00" (unknown is not zero); otherwise the exact amount (a lower bound the row's unknown count qualifies).
 */
export function knownSpendText(known: string | null | undefined, unresolved: string | null | undefined): string {
  return known != null && /^0+$/.test(known) && unresolved != null && /^\d+$/.test(unresolved) && BigInt(unresolved) > 0n ? "Unknown" : formatMicroUsd(known);
}
export function exactDifference(a: string, b: string, money = false): string {
  if (!/^\d+$/.test(a) || !/^\d+$/.test(b)) return "Unknown";
  const value = BigInt(a) - BigInt(b), abs = value < 0n ? -value : value;
  return `${value > 0n ? "+" : value < 0n ? "−" : ""}${money ? formatMicroUsd(abs.toString()) : formatCount(abs.toString())}`;
}
// Only the small normalized graph coordinates become Number. Money/count labels stay exact.
export function trendPoints(report: CostReport) {
  const max = report.daily.reduce((peak, d) => { const n = BigInt(d.totals.known_cost_microusd); return peak > n ? peak : n; }, 0n);
  return report.daily.map(d => ({ label: `${d.date} · ${formatMicroUsd(d.totals.known_cost_microusd)}`, values: { known: max === 0n ? 0 : Number(BigInt(d.totals.known_cost_microusd) * 10000n / max) / 100 } }));
}
export const totalLabels: { key: keyof Totals; label: string; money?: boolean }[] = [{ key: "known_cost_microusd", label: "Known estimated cost", money: true }, { key: "held_microusd", label: "Held reservations", money: true }, { key: "attempts", label: "Upstream attempts" }, { key: "root_requests", label: "Root requests" }, { key: "unresolved_attempts", label: "Unresolved attempts" }];
export const billingLabels: { key: keyof BillingUsage; label: string }[] = [{ key: "total_input_tokens", label: "Total input · inclusive" }, { key: "uncached_input_tokens", label: "Uncached input" }, { key: "cache_read_input_tokens", label: "Cache read" }, { key: "cache_write_input_tokens", label: "Cache write · overlapping aggregate" }, { key: "cache_write_default_input_tokens", label: "Cache write · default" }, { key: "cache_write_5m_input_tokens", label: "Cache write · 5-minute" }, { key: "cache_write_1h_input_tokens", label: "Cache write · 1-hour" }];
export function reportFields(search: DashboardSearch, report?: CostReport, platform = false): Field[] {
  const period = reportPeriod(search);
  return [{ name: "start_date", label: "Start date (UTC)", type: "date", required: true, value: period.start_date, validate: dateError }, { name: "end_date", label: "End date (UTC · exclusive)", type: "date", required: true, value: period.end_date, validate: (v, values) => { const invalid = dateError(v); if (invalid) return invalid; try { reportPeriod(values); } catch (error) { return (error as Error).message; } } }, { name: "compare", label: "Comparison", type: "select", required: true, value: search.compare ?? "none", options: [{ value: "none", label: "No comparison" }, { value: "previous_period", label: "Previous equal UTC period" }] },
    { name: "model", label: "Model", type: "select", value: search.model ?? "", options: report?.breakdowns.models.map(b => ({ value: b.id ?? b.name, label: b.name })) ?? [] }, { name: "provider", label: "Provider", type: "select", value: search.provider ?? "", options: report?.breakdowns.providers.map(b => ({ value: b.id ?? b.name, label: b.name })) ?? [] },
    ...(platform ? [{ name: "workspace_id", label: "Workspace · totals only", type: "select", value: search.workspace_id ?? "", options: report?.breakdowns.workspaces.filter(b => b.id).map(b => ({ value: b.id!, label: b.name })) ?? [], help: "Options come only from authorized aggregate dimensions. No private keys, requests or user-directory metadata is fetched." } satisfies Field] : []),
    { name: "cost_center_id", label: "Cost center", type: "select", value: search.cost_center_id ?? "", options: [{ value: "unallocated", label: "Unallocated" }, ...(report?.breakdowns.cost_centers.filter(b => b.id).map(b => ({ value: b.id!, label: b.name })) ?? [])] }, { name: "accounting_status", label: "Accounting status", type: "select", value: search.accounting_status ?? "", options: ["pending", "unknown", "settled", "missing"].map(value => ({ value, label: value })) },
    { name: "actor_user_id", label: "Actor user UUID", value: search.actor_user_id ?? "", validate: uuidError, help: "Authorization is applied before filtering. A filter never enlarges the activity visible to you." }, ...(!platform ? [{ name: "service_account_id", label: "Service account", type: "select", value: search.service_account_id ?? "", options: report?.breakdowns.service_accounts.filter(b => b.id).map(b => ({ value: b.id!, label: b.name })) ?? [] } satisfies Field] : [])];
}
export function reportSearch(values: Values): Partial<DashboardSearch> { reportPeriod(values); return Object.fromEntries(["start_date", "end_date", "compare", ...reportFilterKeys].map(key => [key, values[key] || undefined])) as Partial<DashboardSearch>; }
export function reconciliationFields(cost: Cost): Field[] {
  const counter = (name: string, label: string, value?: string | null, required = false): Field => ({ name, label, required, value: value ?? "", help: value != null ? "Pinned observed count · must remain unchanged." : "Blank is unknown, not zero.", validate: v => value != null && v !== value ? "Preserve the pinned original observation." : /^\d{1,20}$/.test(v) && BigInt(v) <= (required ? 9223372036854775807n : 18446744073709551615n) ? undefined : "Enter an exact non-negative integer within the supported range, without exponent notation." });
  return [counter("input_tokens", "Original raw input tokens", cost.input_tokens, true), counter("output_tokens", "Original raw output tokens", cost.output_tokens, true), ...billingLabels.map(b => counter(b.key, b.label, cost.billing_usage?.[b.key])), { name: "evidence", label: "Authoritative evidence reference", required: true, maxLength: 200, help: "No prompt, response or secret. Repeated identical evidence and counters are idempotent; changed evidence is a conflict." }];
}
export function reconciliationBody(values: Values) {
  const billing_usage = Object.fromEntries(billingLabels.map(b => [b.key, values[b.key] || null])) as BillingUsage;
  return { input_tokens: values.input_tokens, output_tokens: values.output_tokens, billing_usage: Object.values(billing_usage).some(v => v !== null) ? billing_usage : null, evidence: values.evidence };
}

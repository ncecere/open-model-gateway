/*
 * Request logs (ux-api-contract "Requests"): root requests with their upstream
 * attempts. Metadata only, never prompts or bodies. The server scopes rows:
 * members see their own human-key requests, shared-workspace admins the whole
 * workspace, personal workspaces only their owner; everyone else gets 403/404.
 *
 * Filters live in the URL (model, key_id, status, q, range or start/end dates,
 * cursor) so the list, the request page and its previous/next links share them.
 */
import type { DashboardSearch, RangePreset } from "./permissions";
import { formatMicroUsd } from "./governance";
import type { DataPolicy } from "../components/templates/data-policy-badge";
import type { TimelineStatus } from "../components/templates/timeline";
import { detailTime, tableTime } from "./format";

export type RequestStatus = "succeeded" | "failed" | "cancelled" | "indeterminate" | "in_progress";
export const requestStatuses: { value: RequestStatus; label: string }[] = [
  { value: "succeeded", label: "Succeeded" }, { value: "failed", label: "Failed" }, { value: "cancelled", label: "Cancelled" }, { value: "indeterminate", label: "Unknown result" }, { value: "in_progress", label: "In progress" },
];
export const requestStatusLabel = (status: string) => requestStatuses.find(s => s.value === status)?.label ?? status;
export const requestStatusTone = (status: string) => status === "succeeded" ? "success" : status === "failed" ? "danger" : status === "cancelled" ? "neutral" : status === "in_progress" ? "info" : "warning";
export type RequestRow = {
  root_request_id: string; started_at: string; completed_at: string | null; model: string; key: { id: string; name: string }; status: string; attempts: number;
  input_tokens: string | null; output_tokens: string | null; cost_microusd: string | null; held_microusd: string | null; latency_ms: number | null;
  cost_center: { id: string; name: string; code: string } | null; workload_kind: string; streamed: boolean;
};
export type RequestAttempt = {
  attempt_number: number; execution_id: string; state: string; error_code: string | null; started_at: string; completed_at: string | null; latency_ms: number | null; streamed?: boolean; workload_kind?: string;
  deployment: { id: string; upstream_model: string }; connection: { id: string; name: string; provider: string };
  input_tokens: string | null; output_tokens: string | null; billing_usage: Record<string, string | null> | null; meter_usage: Record<string, string | null> | null;
  cost_microusd: string | null; held_microusd: string | null; accounting_state: string; unresolved_reason: string | null; price_id: string | null; pricing_version: number | null;
  failover_reason: string | null; data_policy?: { data_collection: "allow" | "deny" | "unknown"; basis: string }; details_redacted_at?: string | null;
};
export type RequestDetail = Omit<RequestRow, "attempts"> & { workspace_id: string; attempt_count: number; attempts: RequestAttempt[]; prev_id: string | null; next_id: string | null };
export type RequestPage = { data: RequestRow[]; next_cursor: string | null };

/** The URL filters shared by the list and the request page (not the cursor or table view). */
export type RequestFilters = Pick<DashboardSearch, "model" | "key_id" | "status" | "q" | "range" | "start_date" | "end_date">;
export function requestFilters(search: DashboardSearch): RequestFilters {
  // One or several statuses (comma list), deduplicated in display order; the server takes the same list.
  const picked = new Set((search.status ?? "").split(",")), statuses = requestStatuses.filter(s => picked.has(s.value)).map(s => s.value);
  const status = statuses.length ? statuses.join(",") : undefined;
  // Custom dates: `range=custom` (or no range) with start and exclusive end dates, as Usage & costs writes them.
  const custom = (search.range === "custom" || !search.range) && !!search.start_date && !!search.end_date;
  return { model: search.model, key_id: search.key_id, status, q: search.q, range: custom ? "custom" : search.range === "custom" ? undefined : search.range, start_date: custom ? search.start_date : undefined, end_date: custom ? search.end_date : undefined };
}
export const activeFilterCount = (f: RequestFilters) => [f.model, f.key_id, f.status, f.q, f.range && f.range !== "30d" ? f.range : undefined].filter(Boolean).length;
/** A request ID search: 4 to 36 hex digits or hyphens (the server's rule). */
export const requestIdQuery = (q: string) => /^[0-9a-fA-F-]{4,36}$/.test(q.trim());
const DAY = 86_400_000;
export const utcDate = (ms: number) => new Date(ms).toISOString().slice(0, 10);
const utcMidnight = (now: number) => Math.floor(now / DAY) * DAY;
/** UTC dates (start inclusive, end exclusive) of a period preset. 30 days is the server default. */
export function rangeDates(range: Exclude<RangePreset, "custom">, now = Date.now()): { start_date: string; end_date: string } {
  const tomorrow = utcMidnight(now) + DAY;
  if (range === "month") return { start_date: `${utcDate(now).slice(0, 7)}-01`, end_date: utcDate(tomorrow) };
  const days = range === "today" ? 1 : range === "7d" ? 7 : range === "90d" ? 90 : 30;
  return { start_date: utcDate(tomorrow - days * DAY), end_date: utcDate(tomorrow) };
}
/** Period presets offered on Requests (custom dates are separate). */
export const rangeLabels: Record<Exclude<RangePreset, "custom">, string> = { today: "Today", "7d": "Last 7 days", "30d": "Last 30 days", month: "This month", "90d": "Last 90 days" };
/** Why custom dates can't be used: the server accepts 1 to 93 UTC days ending no later than tomorrow. */
export function customRangeError(start?: string, end?: string, now = Date.now()): string | undefined {
  if (!start || !end) return "Choose both dates.";
  const a = Date.parse(`${start}T00:00:00Z`), b = Date.parse(`${end}T00:00:00Z`), days = (b - a) / DAY;
  if (!Number.isFinite(a) || !Number.isFinite(b)) return "Choose valid dates.";
  if (days < 1) return "The end date must be on or after the start date.";
  if (days > 93) return "Pick up to 93 days.";
  if (b > utcMidnight(now) + DAY) return "The end date can't be after today.";
}
/**
 * API query for the current filters (without cursor/limit), or the reason it can't be sent. An incomplete request ID
 * search is not sent (the server would reject it); the list says what to type instead.
 */
export function requestQuery(filters: RequestFilters, now = Date.now()): { query: URLSearchParams; error?: string } {
  const query = new URLSearchParams();
  if (filters.range && filters.range !== "30d" && filters.range !== "custom") { const d = rangeDates(filters.range, now); query.set("start_date", d.start_date); query.set("end_date", d.end_date); }
  else if (filters.range === "custom") {
    const error = customRangeError(filters.start_date, filters.end_date, now);
    if (error) return { query, error };
    query.set("start_date", filters.start_date!); query.set("end_date", filters.end_date!);
  }
  if (filters.model) query.set("model", filters.model);
  if (filters.key_id) query.set("key_id", filters.key_id);
  if (filters.status) query.set("status", filters.status);
  if (filters.q?.trim()) { if (!requestIdQuery(filters.q)) return { query, error: "Search by at least 4 characters of a request ID (0–9, a–f)." }; query.set("q", filters.q.trim().toLowerCase()); }
  return { query };
}
export function timelineStatus(state: string): TimelineStatus {
  return state === "succeeded" ? "success" : state === "failed" ? "failed" : state === "cancelled" ? "cancelled" : state === "started" || state === "in_progress" ? "pending" : "unknown";
}
/** Integer decimal strings with grouping; null or invalid is "Unknown" (never zero). */
export function countText(value: string | number | null | undefined): string {
  if (value == null || !/^\d+$/.test(String(value))) return "Unknown";
  return BigInt(value).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}
/**
 * "10 in · 2 out" everywhere (review rule 5); speech and transcription requests that report no tokens read "Not applicable"
 * (as Records does). An unreported count makes the whole value "Unknown", never "Unknown in · 0 out".
 */
export function tokensText(input: string | number | null | undefined, output: string | number | null | undefined, workload?: string): string {
  if ((workload === "audio_speech" || workload === "audio_transcriptions") && input != null && String(input) === "0" && (output == null || String(output) === "0")) return "Not applicable";
  if (countText(input) === "Unknown" || countText(output) === "Unknown") return "Unknown";
  return `${countText(input)} in · ${countText(output)} out`;
}
/**
 * A request's start for a dense list (review rule 4, lib/format): "Oct 8, 2:30 AM" (12-hour, the viewer's time zone;
 * the year only when it isn't this year) and "Oct 8, 2026, 2:30 AM EDT" for the tooltip. Invalid values are returned as given.
 */
export function compactDateTime(iso: string, now = new Date(), timeZone?: string): { text: string; full: string } {
  return { text: tableTime(iso, now, timeZone), full: detailTime(iso, timeZone) };
}
/** Known cost, or "Unknown" with what's on hold (a hold is a floor, not the price). */
export function costText(cost: string | null, held: string | null): string {
  if (cost !== null) return formatMicroUsd(cost);
  return held && /^\d+$/.test(held) && BigInt(held) > 0n ? `Unknown · ${formatMicroUsd(held)} on hold` : "Unknown";
}
export const latencyText = (ms: number | null | undefined) => ms == null ? "Unknown" : ms < 1000 ? `${ms.toLocaleString("en-US")} ms` : `${(Math.round(ms / 100) / 10).toLocaleString("en-US")} s`;
/** OpenRouter's server setting: deny = providers that don't collect data only; others are unknown, never "doesn't keep". */
export function dataPolicyOf(policy: RequestAttempt["data_policy"]): DataPolicy {
  return policy?.data_collection === "deny" ? "no_keep" : policy?.data_collection === "allow" ? "keeps" : "unknown";
}
const reasons: Record<string, string> = {
  missing_reservation: "No cost reservation was recorded", unpriced: "This route had no price", unbounded_cost_or_unknown_rate: "No upper limit known for the cost",
  in_progress: "Still running", incomplete_usage: "The provider didn't report complete usage",
};
export const unresolvedText = (reason: string | null) => reason ? reasons[reason] ?? reason.replaceAll("_", " ") : null;
const workloads: Record<string, string> = { generation: "Text generation", embeddings: "Embeddings", images: "Images", audio_transcriptions: "Speech to text", audio_speech: "Text to speech", rerank: "Rerank", systemone: "System One" };
export const workloadText = (kind: string) => workloads[kind] ?? kind;

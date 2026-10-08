/*
 * Request logs (ux-api-contract "Requests"): root requests with their upstream
 * attempts. Metadata only, never prompts or bodies. The server scopes rows:
 * members see their own human-key requests, shared-workspace admins the whole
 * workspace, personal workspaces only their owner; everyone else gets 403/404.
 *
 * Filters live in the URL (model, key_id, status, finish_reason, streamed,
 * session_id, q, range or start/end dates, workspace_id on Admin, cursor) so
 * the Logs tabs, the request page and its previous/next links share them.
 *
 * Logs telemetry (migration 0009): finish reason, time to first token
 * (streams), generation time, tokens per second, cached/reasoning tokens and
 * the optional client session id / app name. Unknown stays null, never zero.
 */
import type { DashboardSearch, RangePreset } from "./permissions";
import { formatMicroUsd } from "./governance";
import type { DataPolicy } from "../components/templates/data-policy-badge";
import type { TimelineStatus } from "../components/templates/timeline";
import { detailTime, tableTime } from "./format";
import { platformPath, wsPath, type Workspace } from "./api";

/**
 * Where Logs live: a workspace (Workspace › Logs; privacy by membership) or the platform
 * (Admin › Usage & spend › Logs; Team/Project workspaces only, never personal rows).
 */
export type LogsScope = { kind: "workspace"; workspace: Workspace } | { kind: "platform" };
export const logTabs = ["requests", "generations", "sessions"] as const;
export type LogTab = typeof logTabs[number];
export const logTab = (tab?: string): LogTab => logTabs.includes(tab as LogTab) ? tab as LogTab : "requests";
/** API collections of a scope. */
export function logPaths(scope: LogsScope) {
  if (scope.kind === "platform") { const base = `${platformPath}/logs`; return { requests: `${base}/requests`, generations: `${base}/generations`, sessions: `${base}/sessions`, metrics: `${base}/metrics`, session: (ws: string, id: string) => `${base}/sessions/${encodeURIComponent(ws)}/${encodeURIComponent(id)}` }; }
  const base = wsPath(scope.workspace.id);
  return { requests: `${base}/requests`, generations: `${base}/generations`, sessions: `${base}/sessions`, metrics: `${base}/logs/metrics`, session: (_ws: string, id: string) => `${base}/sessions/${encodeURIComponent(id)}` };
}
type View = Pick<DashboardSearch, "cols" | "density">;
/** The Logs list (a tab) with the given filters. */
export function logsSearch(scope: LogsScope, filters: RequestFilters, tab: LogTab = "requests", view: View = {}): DashboardSearch {
  const base: DashboardSearch = scope.kind === "platform" ? { page: "platform-logs" } : { page: "requests", ws: scope.workspace.id };
  return { ...base, ...filters, ...(scope.kind === "workspace" ? { workspace_id: undefined } : {}), tab: tab === "requests" ? undefined : tab, ...view };
}
/** A request's own page, keeping the list's filters for previous/next. */
export function requestTarget(scope: LogsScope, id: string, filters: RequestFilters, view: View = {}): DashboardSearch {
  return scope.kind === "platform" ? { page: "platform-log-detail", record: id, ...filters, ...view } : { page: "request-detail", ws: scope.workspace.id, record: id, ...filters, workspace_id: undefined, ...view };
}
/** A session's page: its summary and requests, within the period. */
export function sessionTarget(scope: LogsScope, session: string, workspace: string, filters: RequestFilters): DashboardSearch {
  const period = { range: filters.range, start_date: filters.start_date, end_date: filters.end_date };
  return scope.kind === "platform" ? { page: "platform-log-session", workspace_id: workspace, session_id: session, ...period } : { page: "session-detail", ws: scope.workspace.id, session_id: session, ...period };
}

export type RequestStatus = "succeeded" | "failed" | "cancelled" | "indeterminate" | "in_progress";
export const requestStatuses: { value: RequestStatus; label: string }[] = [
  { value: "succeeded", label: "Succeeded" }, { value: "failed", label: "Failed" }, { value: "cancelled", label: "Cancelled" }, { value: "indeterminate", label: "Unknown result" }, { value: "in_progress", label: "In progress" },
];
export const requestStatusLabel = (status: string) => requestStatuses.find(s => s.value === status)?.label ?? status;
export const requestStatusTone = (status: string) => status === "succeeded" ? "success" : status === "failed" ? "danger" : status === "cancelled" ? "neutral" : status === "in_progress" ? "info" : "warning";
export type FinishReason = "stop" | "length" | "tool_calls" | "content_filter" | "error" | "cancelled" | "unknown";
export const finishReasons: { value: FinishReason; label: string }[] = [
  { value: "stop", label: "Stop" }, { value: "length", label: "Length limit" }, { value: "tool_calls", label: "Tool calls" }, { value: "content_filter", label: "Content filter" },
  { value: "error", label: "Error" }, { value: "cancelled", label: "Cancelled" }, { value: "unknown", label: "Unknown" },
];
/** A finish reason for display; null = still running or not applicable (e.g. embeddings). */
export const finishReasonLabel = (reason: string | null | undefined) => reason == null ? "None" : finishReasons.find(f => f.value === reason)?.label ?? reason;
export const finishReasonTone = (reason: string | null | undefined) => reason === "stop" || reason === "tool_calls" ? "success" : reason === "error" ? "danger" : reason === "length" || reason === "content_filter" ? "warning" : "neutral";
export type LogWorkspace = { id: string; name: string; kind: "personal" | "team" | "project" };
/** Telemetry shared by request and generation rows (optional: older servers omit them). */
export type LogTelemetry = {
  workspace?: LogWorkspace; upstream_model?: string | null; finish_reason?: string | null; cached_input_tokens?: string | null; reasoning_tokens?: string | null;
  time_to_first_token_ms?: number | null; generation_ms?: number | null; tokens_per_second?: string | null; session_id?: string | null; app?: string | null;
};
export type RequestRow = {
  root_request_id: string; started_at: string; completed_at: string | null; model: string; key: { id: string; name: string }; status: string; attempts: number;
  input_tokens: string | null; output_tokens: string | null; cost_microusd: string | null; held_microusd: string | null; latency_ms: number | null;
  cost_center: { id: string; name: string; code: string } | null; workload_kind: string; streamed: boolean; provider?: string | null;
} & LogTelemetry;
/** One upstream attempt (Logs › Generations). */
export type GenerationRow = {
  execution_id: string; root_request_id: string; attempt_number: number; started_at: string; completed_at: string | null; model: string;
  connection: { id: string; name: string; provider: string }; key: { id: string; name: string }; status: string; error_code: string | null; streamed: boolean; workload_kind: string;
  input_tokens: string | null; output_tokens: string | null; cost_microusd: string | null; held_microusd: string | null; latency_ms: number | null;
} & LogTelemetry;
export type GenerationPage = { data: GenerationRow[]; next_cursor: string | null };
/** Requests grouped by client session id (Logs › Sessions). */
export type SessionRow = {
  session_id: string; workspace: LogWorkspace; requests: string; attempts: string; failed_requests: string; in_progress_requests: string;
  input_tokens: string | null; output_tokens: string | null; cost_microusd: string | null; known_cost_microusd: string; held_microusd: string; unresolved_requests: string;
  first_at: string; last_at: string; models: string[]; model_count: number; last_model: string | null; app: string | null; keys: number;
};
export type SessionPage = { data: SessionRow[]; next_cursor: string | null };
/** Summary of the filtered root requests (Logs tiles). */
export type LogMetrics = {
  requests: string; completed: string; failed: string; error_rate: string | null; latency_p50_ms: number | null; latency_p95_ms: number | null;
  avg_time_to_first_token_ms: number | null; ttft_requests: string; tokens_per_second: string | null; input_tokens: string; output_tokens: string; unknown_token_requests: string;
  known_cost_microusd: string; held_microusd: string; unresolved_requests: string;
};
export type RequestAttempt = {
  attempt_number: number; execution_id: string; state: string; error_code: string | null; started_at: string; completed_at: string | null; latency_ms: number | null; streamed?: boolean; workload_kind?: string;
  deployment: { id: string; upstream_model: string }; connection: { id: string; name: string; provider: string };
  input_tokens: string | null; output_tokens: string | null; billing_usage: Record<string, string | null> | null; meter_usage: Record<string, string | null> | null;
  cost_microusd: string | null; held_microusd: string | null; accounting_state: string; unresolved_reason: string | null; price_id: string | null; pricing_version: number | null;
  failover_reason: string | null; data_policy?: { data_collection: "allow" | "deny" | "unknown"; basis: string }; details_redacted_at?: string | null;
  finish_reason?: string | null; time_to_first_token_ms?: number | null; generation_ms?: number | null; cached_input_tokens?: string | null; reasoning_tokens?: string | null;
};
export type RequestDetail = Omit<RequestRow, "attempts"> & { workspace_id: string; attempt_count: number; attempts: RequestAttempt[]; prev_id: string | null; next_id: string | null };
export type RequestPage = { data: RequestRow[]; next_cursor: string | null };

/** The URL filters shared by the Logs tabs and the request page (not the cursor, tab or table view). */
export type RequestFilters = Pick<DashboardSearch, "model" | "key_id" | "status" | "q" | "range" | "start_date" | "end_date" | "finish_reason" | "streamed" | "session_id" | "workspace_id">;
export function requestFilters(search: DashboardSearch): RequestFilters {
  const picked = new Set((search.finish_reason ?? "").split(",")), reasons = finishReasons.filter(f => picked.has(f.value)).map(f => f.value);
  const extra = { finish_reason: reasons.length ? reasons.join(",") : undefined, streamed: search.streamed, session_id: search.session_id, workspace_id: search.workspace_id };
  return Object.fromEntries(Object.entries({ ...baseFilters(search), ...extra }).filter(([, v]) => v !== undefined)) as RequestFilters;
}
function baseFilters(search: DashboardSearch): RequestFilters {
  // One or several statuses (comma list), deduplicated in display order; the server takes the same list.
  const picked = new Set((search.status ?? "").split(",")), statuses = requestStatuses.filter(s => picked.has(s.value)).map(s => s.value);
  const status = statuses.length ? statuses.join(",") : undefined;
  // Custom dates: `range=custom` (or no range) with start and exclusive end dates, as Usage & costs writes them.
  const custom = (search.range === "custom" || !search.range) && !!search.start_date && !!search.end_date;
  return { model: search.model, key_id: search.key_id, status, q: search.q, range: custom ? "custom" : search.range === "custom" ? undefined : search.range, start_date: custom ? search.start_date : undefined, end_date: custom ? search.end_date : undefined };
}
export const activeFilterCount = (f: RequestFilters) => [f.model, f.key_id, f.status, f.q, f.finish_reason, f.streamed, f.session_id, f.workspace_id, f.range && f.range !== "30d" ? f.range : undefined].filter(Boolean).length;
/** Client session ids: 1–128 characters, no control characters or surrounding spaces (the server's rule). */
export const validSessionId = (id: string) => id.length >= 1 && [...id].length <= 128 && id.trim() === id && !/[\u0000-\u001f\u007f-\u009f]/.test(id);
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
  if (filters.finish_reason) query.set("finish_reason", filters.finish_reason);
  if (filters.streamed) query.set("streamed", filters.streamed);
  if (filters.session_id) { if (!validSessionId(filters.session_id)) return { query, error: "A session ID has 1 to 128 characters without leading or trailing spaces." }; query.set("session_id", filters.session_id); }
  if (filters.workspace_id) query.set("workspace_id", filters.workspace_id);
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
/** Tokens per second (a decimal string from the server); unknown is "Unknown". */
export function tpsText(value: string | null | undefined): string {
  if (value == null || !/^\d+(?:\.\d+)?$/.test(value)) return "Unknown";
  const [whole, fraction = ""] = value.split("."), tenths = fraction.slice(0, 1);
  return `${countText(whole)}${tenths && tenths !== "0" ? `.${tenths}` : ""} tok/s`;
}
/** A duration that may legitimately be absent: "Not streamed" for TTFT of a non-streamed request. */
export const ttftText = (ms: number | null | undefined, streamed: boolean) => ms != null ? latencyText(ms) : streamed ? "Unknown" : "Not streamed";
/** Error rate as a percentage with one decimal ("2.5%"); null is "Unknown". */
export function rateText(ratio: string | null | undefined): string {
  if (ratio == null || !/^\d+(?:\.\d+)?$/.test(ratio)) return "Unknown";
  const [whole, fraction = ""] = ratio.split("."), basis = BigInt(whole + fraction.padEnd(4, "0").slice(0, 4)); // ten-thousandths
  const tenths = (basis + 5n) / 10n; // percent with one decimal = basis / 10, rounded half up
  return `${tenths / 10n}${tenths % 10n ? `.${tenths % 10n}` : ""}%`;
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

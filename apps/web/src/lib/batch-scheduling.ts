/*
 * Batch scheduling of a route (Admin › model › route): when gateway-run batch
 * lines may start. GET/PUT /api/v1/platform/deployments/{id}/batch-scheduling.
 * Native provider batches are unaffected. The server validates everything
 * again (approved metrics origins, vLLM-only priority hints, IANA zones).
 */
import { platformPath } from "./api";
import type { Field, Values } from "./forms";
import { parseCheckboxValues } from "./forms";

export type Day = "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun";
export type MetricsGate = { url: string; max_waiting?: number | null; max_running?: number | null; max_kv_cache_percent?: number | null };
export type TimeWindow = { timezone: string; days: Day[]; start: string; end: string };
export type BatchScheduling = { max_concurrency: number; yield_live_threshold?: number | null; metrics?: MetricsGate | null; priority?: number | null; window?: TimeWindow | null };
export type PauseReason = "outside_window" | "live_traffic" | "server_busy" | "metrics_unavailable" | "concurrency" | "fair_share" | "workers" | "rate_limited";
export type MetricsReading = { checked_at: string; ok: boolean | null; waiting: number | null; running: number | null; kv_cache_permille: number | null; error: string | null };
export type RouteBatchStatus = { queued_lines: number; waiting_batches: number; running_lines: number; paused_reason: PauseReason | null; checked_at: string | null; live_in_flight: number | null; metrics: MetricsReading | null };
export type BatchSchedulingDocument = { settings: BatchScheduling; defaults: BatchScheduling; priority_supported: boolean; status: RouteBatchStatus };
/** A batch's wait on one route (batch page). */
export type BatchRouteWait = { model: string; reason: PauseReason | null; waiting_lines: number; running_lines: number; position: number; queue: number; since: string };

export const batchSchedulingPath = (deployment: string) => `${platformPath}/deployments/${encodeURIComponent(deployment)}/batch-scheduling`;

export const DAYS: { value: Day; label: string }[] = [
  { value: "mon", label: "Mon" }, { value: "tue", label: "Tue" }, { value: "wed", label: "Wed" }, { value: "thu", label: "Thu" },
  { value: "fri", label: "Fri" }, { value: "sat", label: "Sat" }, { value: "sun", label: "Sun" },
];

const pauseLabels: Record<PauseReason, string> = {
  outside_window: "Outside its time window",
  live_traffic: "Yielding to live traffic",
  server_busy: "Server busy",
  metrics_unavailable: "Server metrics unavailable",
  concurrency: "At its batch limit",
  fair_share: "Other batches' turn",
  workers: "Gateway batch workers busy",
  rate_limited: "Rate limited · backing off",
};
export const pauseLabel = (reason: string | null | undefined) => reason ? pauseLabels[reason as PauseReason] ?? reason.replace(/_/g, " ") : null;
/** Pauses that are a configured gate or capacity (not a fault). */
export const isFault = (reason: string | null | undefined) => reason === "metrics_unavailable";

/** "Mon–Fri", "Mon, Wed", "Every day". */
export function daysText(days: Day[]): string {
  const idx = DAYS.map(d => d.value).filter(d => days.includes(d)).map(d => DAYS.findIndex(x => x.value === d));
  if (idx.length === 7) return "Every day";
  const runs: string[] = [];
  for (let i = 0; i < idx.length;) {
    let j = i;
    while (j + 1 < idx.length && idx[j + 1] === idx[j]! + 1) j++;
    const a = DAYS[idx[i]!]!.label, b = DAYS[idx[j]!]!.label;
    runs.push(j - i >= 2 ? `${a}–${b}` : j > i ? `${a}, ${b}` : a);
    i = j + 1;
  }
  return runs.join(", ");
}
export function windowText(w: TimeWindow): string {
  return `${daysText(w.days)} ${w.start === w.end ? "all day" : `${w.start}–${w.end}`} (${w.timezone})`;
}
export function metricsGateText(m: MetricsGate): string {
  const limits = [
    m.max_waiting != null ? `waiting > ${m.max_waiting}` : null,
    m.max_running != null ? `running > ${m.max_running}` : null,
    m.max_kv_cache_percent != null ? `KV cache > ${m.max_kv_cache_percent}%` : null,
  ].filter(Boolean);
  return `Pause if ${limits.join(" or ")}`;
}
/** The last server reading: "2 waiting · 4 running · KV cache 91.3%", or why it failed. */
export function readingText(r: MetricsReading | null): string | null {
  if (!r) return null;
  if (r.ok === false) return `Unavailable${r.error ? ` (${r.error.replace(/_/g, " ")})` : ""} · treated as busy`;
  const parts = [r.waiting != null ? `${r.waiting} waiting` : null, r.running != null ? `${r.running} running` : null, r.kv_cache_permille != null ? `KV cache ${(r.kv_cache_permille / 10).toFixed(1)}%` : null].filter(Boolean);
  return parts.length ? parts.join(" · ") : "No vLLM gauges";
}
/** One status line for the route's batch queue. */
export function queueText(s: RouteBatchStatus): string {
  const lines = (n: number) => `${n.toLocaleString("en-US")} line${n === 1 ? "" : "s"}`;
  if (!s.queued_lines && !s.running_lines) return "No batch lines waiting";
  const paused = s.paused_reason ? ` · Paused: ${pauseLabel(s.paused_reason)?.toLowerCase()}` : "";
  return `${lines(s.running_lines)} running · ${lines(s.queued_lines)} queued${s.waiting_batches ? ` (${s.waiting_batches} batch${s.waiting_batches === 1 ? "" : "es"})` : ""}${paused}`;
}

/** Settings as label/value rows (read view). */
export function settingsRows(s: BatchScheduling, prioritySupported: boolean): { label: string; value: string }[] {
  return [
    { label: "Batch lines at once", value: String(s.max_concurrency) },
    { label: "Yield to live traffic", value: s.yield_live_threshold ? `Pause when ${s.yield_live_threshold}+ live requests run` : "Off" },
    { label: "Server load signal", value: s.metrics ? metricsGateText(s.metrics) : "Off" },
    ...(prioritySupported || s.priority ? [{ label: "Priority hint", value: s.priority ? `priority ${s.priority} on batch lines` : "Off" }] : []),
    { label: "Time window", value: s.window ? windowText(s.window) : "Any time" },
  ];
}

const optionalInt = (min: number, max: number) => (value: string) => !value ? undefined : /^\d+$/.test(value) && Number(value) >= min && Number(value) <= max ? undefined : `Enter a whole number from ${min} to ${max}.`;
const hhmm = (value: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(value) ? undefined : "Use HH:MM (24-hour).";
const hasUrl = (v: Values) => !!v.metrics_url?.trim();
const hasDays = (v: Values) => { try { return parseCheckboxValues(v.window_days || "[]").length > 0; } catch { return false; } };
const localZone = () => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC"; } catch { return "UTC"; } };
const str = (n: number | null | undefined) => n == null ? "" : String(n);

/** Edit dialog fields. */
export function schedulingFields(s: BatchScheduling, prioritySupported: boolean): Field[] {
  const fields: Field[] = [
    { name: "max_concurrency", label: "Batch lines at once", type: "number", required: true, min: 1, max: 256, value: String(s.max_concurrency), help: "Across all batches and gateway instances." },
    { name: "yield_live_threshold", label: "Pause while live requests ≥", type: "number", min: 1, max: 100000, value: str(s.yield_live_threshold), placeholder: "Off", validate: optionalInt(1, 100000) },
    { name: "metrics_url", label: "Server metrics URL", value: s.metrics?.url ?? "", placeholder: "http://gpu-host:8000/metrics", help: "vLLM /metrics on an approved local endpoint. Unreadable counts as busy.", maxLength: 2048,
      validate: (value, v) => !value ? undefined : !/^https?:\/\/[^\s?#@]+\/metrics$/.test(value) ? "Use http(s)://host[:port][/prefix]/metrics." : !v.metrics_max_waiting && !v.metrics_max_running && !v.metrics_max_kv_cache_percent ? "Set at least one limit below." : undefined },
    { name: "metrics_max_waiting", label: "Pause if waiting requests >", type: "number", min: 0, max: 100000, value: str(s.metrics?.max_waiting), placeholder: "No limit", visibleWhen: hasUrl, validate: optionalInt(0, 100000) },
    { name: "metrics_max_running", label: "Pause if running requests >", type: "number", min: 0, max: 100000, value: str(s.metrics?.max_running), placeholder: "No limit", visibleWhen: hasUrl, validate: optionalInt(0, 100000) },
    { name: "metrics_max_kv_cache_percent", label: "Pause if KV cache use > (%)", type: "number", min: 1, max: 100, value: str(s.metrics?.max_kv_cache_percent), placeholder: "No limit", visibleWhen: hasUrl, validate: optionalInt(1, 100) },
  ];
  if (prioritySupported || s.priority) fields.push({ name: "priority", label: "vLLM priority for batch lines", type: "number", min: 1, max: 1000000, value: str(s.priority), placeholder: "Off", help: "Needs --scheduling-policy priority. Lower runs first; live traffic is 0.", validate: optionalInt(1, 1000000) });
  fields.push(
    { name: "window_days", label: "Time window", type: "checkboxes", options: DAYS, value: JSON.stringify(s.window?.days ?? []), help: "Days a window starts. None: any time." },
    { name: "window_start", label: "From", value: s.window?.start ?? "19:00", placeholder: "19:00", visibleWhen: hasDays, requiredWhen: hasDays, validate: hhmm },
    { name: "window_end", label: "Until", value: s.window?.end ?? "07:00", placeholder: "07:00", visibleWhen: hasDays, requiredWhen: hasDays, validate: hhmm, help: "Earlier than From spans midnight." },
    { name: "window_timezone", label: "Time zone", value: s.window?.timezone ?? localZone(), placeholder: "America/New_York", visibleWhen: hasDays, requiredWhen: hasDays, maxLength: 64 },
  );
  return fields;
}

const intOrNull = (value: string | undefined) => value?.trim() ? Number(value.trim()) : null;
/** PUT body from dialog values (validated by the fields first). */
export function schedulingBody(v: Values): BatchScheduling {
  const days = parseCheckboxValues(v.window_days || "[]") as Day[];
  const url = v.metrics_url?.trim();
  return {
    max_concurrency: Number(v.max_concurrency),
    yield_live_threshold: intOrNull(v.yield_live_threshold),
    metrics: url ? { url, max_waiting: intOrNull(v.metrics_max_waiting), max_running: intOrNull(v.metrics_max_running), max_kv_cache_percent: intOrNull(v.metrics_max_kv_cache_percent) } : null,
    priority: intOrNull(v.priority),
    window: days.length ? { timezone: v.window_timezone!.trim(), days: DAYS.map(d => d.value).filter(d => days.includes(d)), start: v.window_start!.trim(), end: v.window_end!.trim() } : null,
  };
}

import { platformPath, wsPath, type Session, type Workspace } from "./api";
import { uuidError, type Field, type Values } from "./forms";
import { formatCount, totalLabels } from "./reports";
import { healthLabels } from "./people";
import type { Tone } from "./bitop-utils";
export type Schedule = { id: string; scope_kind: string; workspace_id: string | null; cost_center_id: string | null; cadence: "weekly" | "monthly"; timezone: string; delivery_time: string; state: "active" | "paused"; recipient_user_ids: string[]; next_run_at: string | null; configuration_revision: string };
export type Aggregate = { known_cost_microusd?: string | null; held_microusd?: string | null; attempts?: string | null; unresolved_attempts?: string | null; root_requests?: string | null };
export type Snapshot = { basis: string; currency: string; totals: Aggregate; coverage: Record<string, string | null>; breakdowns: Record<string, { id: string | null; name: string; totals: Aggregate }[]>; breakdowns_truncated: boolean; truncated_dimensions: string[]; unavailable: Record<string, string> };
export type ReportRun = { id: string; schedule_id: string; period_start: string; period_end: string; observed_at: string; expires_at: string; expired: boolean; configuration_revision: string; skipped_intervals?: string; snapshot: Snapshot | null; delivery: { status: string; recipients: number; sent: number; failed: number; waiting: number } | null };
export type ScheduleList = { data: Schedule[]; has_more: boolean; configuration: { smtp_configured: boolean; public_url_configured: boolean } };
export const reportsPath = (workspace?: Workspace) => `${workspace ? wsPath(workspace.id) : platformPath}/report-schedules`;
export function canReadReports(session: Session, workspace?: Workspace) {
  if (workspace?.disabled_at) return false;
  return workspace ? workspace.kind === "personal" ? workspace.owner_user_id === session.user.id : workspace.role === "owner" || workspace.role === "admin" : session.capabilities.platform_read;
}
const recipientIds = (text: string) => text.split(/[,\s]+/).filter(Boolean);
export function scheduleFields(session: Session, workspace?: Workspace, existing?: Schedule): Field[] {
  const fields: Field[] = [
    { name: "cadence", label: "Cadence", type: "select", required: true, value: existing?.cadence ?? "weekly", options: [{ value: "weekly", label: "Weekly" }, { value: "monthly", label: "Monthly" }] },
    { name: "timezone", label: "IANA time zone", required: true, maxLength: 64, value: existing?.timezone ?? "UTC", help: "Controls delivery only; accounting periods are always UTC.", validate: value => { try { new Intl.DateTimeFormat("en", { timeZone: value }); } catch { return "Enter an IANA time zone, such as America/New_York."; } } },
    { name: "delivery_time", label: "Local delivery time", required: true, value: existing?.delivery_time ?? "09:00", help: "HH:MM, 24-hour time. First configured local time after period closure.", validate: value => /^(?:[01]\d|2[0-3]):[0-5]\d$/.test(value) ? undefined : "Enter HH:MM from 00:00 to 23:59." },
  ];
  if (workspace?.kind !== "personal") fields.push({ name: "recipient_user_ids", label: "Recipient user IDs", type: "textarea", required: true, value: existing?.recipient_user_ids.join(", ") ?? session.user.id, help: workspace ? "1–50 internal workspace owners/admins. Separate UUIDs with commas or new lines; no email addresses." : "1–50 internal Platform Admins/Auditors. Separate UUIDs with commas or new lines; no email addresses.", validate: text => { const ids = recipientIds(text); return !ids.length || ids.length > 50 || new Set(ids).size !== ids.length || ids.some(id => uuidError(id)) ? "Enter 1–50 unique internal user UUIDs." : undefined; } });
  if (!existing && workspace?.kind !== "personal") fields.push({ name: "cost_center_id", label: "Historical cost center ID", help: "Optional UUID. Narrows admission-time attribution, including archived assignments. Scope cannot be changed later.", validate: value => uuidError(value) });
  return fields;
}
export function scheduleBody(values: Values, workspace?: Workspace, session?: Session, editing = false) {
  return { cadence: values.cadence, timezone: values.timezone, delivery_time: values.delivery_time, recipient_user_ids: workspace?.kind === "personal" && session ? [session.user.id] : recipientIds(values.recipient_user_ids ?? ""), ...editing ? {} : { state: "paused", ...values.cost_center_id ? { cost_center_id: values.cost_center_id } : {} } };
}

/*
 * Human labels for every machine key a scheduled-report snapshot exposes. Unknown keys fall back to
 * `humanizeKey` (title-cased words, never raw snake_case, never a bare "microusd" suffix) so a gateway
 * returning a measure this UI doesn't yet name still reads as words, not a wire key.
 */
export function humanizeKey(key: string): string {
  const words = key.replace(/_microusd$/, "").split("_").filter(Boolean);
  if (!words.length) return key;
  return words.map(w => `${w[0]!.toUpperCase()}${w.slice(1)}`).join(" ");
}
const totalLabelMap: Record<string, string> = Object.fromEntries(totalLabels.map(t => [t.key, t.label]));
/** A snapshot aggregate's non-money measure (attempts, unresolved_attempts, root_requests), shared with the live cost report's labels. */
export const aggregateMeasureLabel = (key: string): string => totalLabelMap[key] ?? humanizeKey(key);
/** A count, or "Unavailable" (never a synthesized zero) for a null/non-numeric value. */
export const countOrUnavailable = (value: string | null | undefined): string => value != null && /^\d+$/.test(value) ? formatCount(value) : "Unavailable";
const dimensionLabels: Record<string, string> = { workspaces: "Workspaces", cost_centers: "Cost centers", models: "Models", providers: "Providers" };
/** A breakdown dimension's heading ("cost_centers" → "Cost centers"). */
export const dimensionLabel = (key: string): string => dimensionLabels[key] ?? humanizeKey(key);
const coverageLabels: Record<string, string> = { priced_attempts: "Priced attempts", total_attempts: "Total attempts", settled_attempts: "Settled attempts", pending_attempts: healthLabels.pending_attempts!, unknown_attempts: healthLabels.unknown_attempts!, missing_reservation_attempts: healthLabels.missing_reservation_attempts!, unresolved_attempts: "Unresolved attempts" };
/** A snapshot's coverage measure, reusing the live cost report's accounting-health wording where they overlap. */
export const coverageLabel = (key: string): string => coverageLabels[key] ?? humanizeKey(key);
const unavailableLabels: Record<string, string> = { root_requests: "Root requests", cost_components: "Cost components", detailed_health: "Detailed health", pricing_coverage: "Pricing coverage", billing_usage: "Billing usage", meter_usage: "Meter usage", provider_reported_cost_microusd: "Provider-reported cost" };
/** A measure a historical snapshot can't compute (never shown as a raw key, never with "microusd"). */
export const unavailableLabel = (key: string): string => unavailableLabels[key] ?? humanizeKey(key);
const unavailableReasonLabels: Record<string, string> = { non_additive: "Not additive across a period", not_in_rollups: "Not kept in historical rollups" };
export const unavailableReasonLabel = (reason: string): string => unavailableReasonLabels[reason] ?? humanizeKey(reason);
const deliveryStatusLabels: Record<string, string> = { pending: "Pending", report_pending: "Delivery pending", sent: "Delivered", partial: "Partially delivered", failed: "Delivery failed", not_configured: "Email not configured", no_recipients: "No recipients", suppressed: "Suppressed" };
const deliveryStatusTones: Record<string, Tone> = { pending: "info", report_pending: "info", sent: "success", partial: "warning", failed: "danger", not_configured: "warning", no_recipients: "neutral", suppressed: "neutral" };
export const deliveryStatusLabel = (status: string): string => deliveryStatusLabels[status] ?? humanizeKey(status);
export const deliveryStatusTone = (status: string): Tone => deliveryStatusTones[status] ?? "neutral";
/** "1 recipient", "2 recipients". */
export const recipientLabel = (count: number): string => `${count} ${count === 1 ? "recipient" : "recipients"}`;

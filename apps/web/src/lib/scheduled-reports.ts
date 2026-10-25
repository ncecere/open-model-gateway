import { platformPath, wsPath, type Session, type Workspace } from "./api";
import { uuidError, type Field, type Values } from "./forms";
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

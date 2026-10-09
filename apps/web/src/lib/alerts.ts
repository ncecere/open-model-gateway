/*
 * Alerts (docs/alerts.md): API shapes, plain-language labels and the rule
 * form's exact conversions. Money travels as integer micro-USD strings (BigInt
 * conversion, never floats); the spike factor is sent as an integer percent.
 * The server stays authoritative for validation and authority.
 */
import { API, platformPath, wsPath } from "./api";
import { dollarsToMicroUsd, formatMicroUsd, microUsdToDollars } from "./governance";

export type AlertKind = "budget_threshold" | "spend_spike" | "error_rate" | "provider_failing";
export type BudgetLayer = "installation" | "type" | "override" | "local" | "key";
export type AlertRule = {
  id: string; scope: "installation" | "workspace"; workspace_id: string | null; kind: AlertKind; name: string; enabled: boolean;
  budget_layers: BudgetLayer[] | null; thresholds: number[] | null;
  spike_factor_percent: number | null; min_spend_microusd: string | null;
  window_minutes: number | null; error_rate_percent: number | null; min_requests: number | null; consecutive_failures: number | null;
  provider_connection_id: string | null; provider_connection: { id: string; name: string; provider: string } | null;
  notify_workspace_admins: boolean; notify_platform_admins: boolean; notify_emails: string[];
  firing: number; last_fired_at: string | null; created_at: string; updated_at: string;
};
export type RuleList = { data: AlertRule[]; /** Personal workspaces: the built-in budget alert (nothing configurable). */ builtin?: { thresholds: number[]; budget_layers: BudgetLayer[] } | null; writable?: boolean };
export type EmailOutcome = { status: "pending" | "sent" | "partial" | "failed" | "not_configured" | "no_recipients"; recipients: number; sent: number; failed: number; error: string | null };
export type AlertEvent = {
  id: string; rule: { id: string; name: string; scope: "installation" | "workspace"; deleted: boolean } | null; builtin: boolean; kind: AlertKind;
  state: "firing" | "resolved"; severity: "warning" | "critical"; level: number; summary: string; details: Record<string, unknown>;
  workspace: { id: string; name: string; kind: "personal" | "team" | "project" } | null; connection: { id: string; name: string; provider: string } | null;
  fired_at: string; resolved_at: string | null; resolution: "cleared" | "superseded" | "rule_disabled" | null; email: EmailOutcome | null;
};
export type Notification = AlertEvent & { read: boolean };
export type NotificationSummary = { unread: number; firing: number };

export type AlertScope = { kind: "platform" } | { kind: "workspace"; ws: string };
const base = (scope: AlertScope) => scope.kind === "platform" ? `${platformPath}/alerts` : `${wsPath(scope.ws)}/alerts`;
export const rulesPath = (scope: AlertScope) => `${base(scope)}/rules`;
export const rulePath = (scope: AlertScope, id: string) => `${rulesPath(scope)}/${encodeURIComponent(id)}`;
export const eventsPath = (scope: AlertScope) => `${base(scope)}/events`;
export const notificationsPath = `${API}/me/notifications`;
export const notificationSummaryPath = `${notificationsPath}/summary`;

export const kindLabels: Record<AlertKind, string> = { budget_threshold: "Budget", spend_spike: "Spend spike", error_rate: "Error rate", provider_failing: "Failing connection" };
export const kindHints: Record<AlertKind, string> = {
  budget_threshold: "Spend reaches a share of a budget.",
  spend_spike: "Last hour's spend is far above the 7-day hourly average.",
  error_rate: "Too many requests fail.",
  provider_failing: "A connection keeps failing upstream.",
};
export const layerLabels: Record<BudgetLayer, string> = { installation: "Installation", type: "Type default", override: "Platform override", local: "Workspace", key: "API keys" };
export const kindsFor = (scope: AlertScope): AlertKind[] => scope.kind === "platform" ? ["budget_threshold", "spend_spike", "error_rate", "provider_failing"] : ["budget_threshold", "spend_spike", "error_rate"];
export const layersFor = (scope: AlertScope): BudgetLayer[] => scope.kind === "platform" ? ["installation", "type", "override", "local", "key"] : ["type", "override", "local", "key"];

export type RuleDraft = {
  name: string; kind: AlertKind; enabled: boolean; layers: BudgetLayer[]; thresholds: string;
  factor: string; minSpend: string; window: string; rate: string; minRequests: string; consecutive: string; connection: string;
  notifyWorkspaceAdmins: boolean; notifyPlatformAdmins: boolean; emails: string;
};
export function newDraft(scope: AlertScope, kind: AlertKind = "budget_threshold"): RuleDraft {
  const platform = scope.kind === "platform";
  return { name: "", kind, enabled: true, layers: platform ? ["installation", "local"] : ["local", "key"], thresholds: "50, 80, 100", factor: "3", minSpend: "1.00", window: "15", rate: kind === "provider_failing" ? "" : "20", minRequests: kind === "provider_failing" ? "" : "20", consecutive: "5", connection: "", notifyWorkspaceAdmins: !platform, notifyPlatformAdmins: platform, emails: "" };
}
export function draftOf(rule: AlertRule): RuleDraft {
  const scope: AlertScope = rule.workspace_id ? { kind: "workspace", ws: rule.workspace_id } : { kind: "platform" };
  const d = newDraft(scope, rule.kind);
  return {
    ...d, name: rule.name, enabled: rule.enabled, layers: rule.budget_layers ?? d.layers, thresholds: rule.thresholds?.join(", ") ?? d.thresholds,
    factor: rule.spike_factor_percent ? percentToFactor(rule.spike_factor_percent) : d.factor, minSpend: rule.min_spend_microusd ? microUsdToDollars(rule.min_spend_microusd) : d.minSpend,
    window: rule.window_minutes?.toString() ?? d.window, rate: rule.error_rate_percent?.toString() ?? (rule.kind === "provider_failing" ? "" : d.rate),
    minRequests: rule.min_requests?.toString() ?? (rule.kind === "provider_failing" ? "" : d.minRequests),
    consecutive: rule.kind === "provider_failing" ? rule.consecutive_failures?.toString() ?? "" : d.consecutive, connection: rule.provider_connection_id ?? "",
    notifyWorkspaceAdmins: rule.notify_workspace_admins, notifyPlatformAdmins: rule.notify_platform_admins, emails: rule.notify_emails.join(", "),
  };
}

/** "50, 80, 100" → [50, 80, 100] (sorted, distinct, 1–100, at most five). */
export function parseThresholds(text: string): number[] | string {
  const parts = text.split(/[\s,%]+/).filter(Boolean);
  if (!parts.length) return "Enter at least one percentage.";
  if (parts.some(p => !/^\d{1,3}$/.test(p))) return "Enter whole percentages, such as 50, 80, 100.";
  const values = [...new Set(parts.map(Number))].sort((a, b) => a - b);
  if (values.some(v => v < 1 || v > 100)) return "Use percentages from 1 to 100.";
  return values.length > 5 ? "Use at most five percentages." : values;
}
/** "3" → 300, "2.5" → 250 (exact; up to two decimals); 1.1×–1000×. */
export function factorToPercent(text: string): number | string {
  const v = text.trim().replace(/[x×]$/i, "");
  if (!/^\d{1,4}(?:\.\d{1,2})?$/.test(v)) return "Enter a multiple such as 3 or 2.5.";
  const [whole, fraction = ""] = v.split(".");
  const percent = Number(whole) * 100 + Number(fraction.padEnd(2, "0"));
  return percent < 110 || percent > 100000 ? "Enter a multiple from 1.1 to 1000." : percent;
}
export function percentToFactor(percent: number): string {
  const whole = Math.floor(percent / 100), fraction = String(percent % 100).padStart(2, "0").replace(/0+$/, "");
  return fraction ? `${whole}.${fraction}` : String(whole);
}
const intIn = (text: string, min: number, max: number) => /^\d{1,6}$/.test(text.trim()) && Number(text) >= min && Number(text) <= max;
export function parseEmails(text: string): string[] | string {
  const list = [...new Set(text.split(/[\s,;]+/).map(e => e.trim().toLowerCase()).filter(Boolean))];
  if (list.some(e => e.length > 320 || !/^[^\s@]+@[^\s@]+$/.test(e))) return "Enter email addresses separated by commas.";
  return list.length > 10 ? "Use at most 10 addresses." : list;
}

export type RuleErrors = Partial<Record<keyof RuleDraft, string>>;
export function ruleErrors(d: RuleDraft): RuleErrors {
  const e: RuleErrors = {};
  const name = d.name.trim();
  if (!name) e.name = "Enter a name."; else if (name.length > 120) e.name = "Use at most 120 characters.";
  if (d.kind === "budget_threshold") {
    if (!d.layers.length) e.layers = "Choose at least one budget.";
    const t = parseThresholds(d.thresholds); if (typeof t === "string") e.thresholds = t;
  }
  if (d.kind === "spend_spike") {
    const f = factorToPercent(d.factor); if (typeof f === "string") e.factor = f;
    try { if (BigInt(dollarsToMicroUsd(d.minSpend)) < 1n) e.minSpend = "Enter more than $0.00."; } catch (error) { e.minSpend = (error as Error).message; }
  }
  if (d.kind === "error_rate" || d.kind === "provider_failing") {
    if (!intIn(d.window, 5, 1440)) e.window = "Enter 5–1440 minutes.";
    const rateSet = !!d.rate.trim() || !!d.minRequests.trim();
    if (d.kind === "error_rate" || rateSet) {
      if (!intIn(d.rate, 1, 100)) e.rate = "Enter 1–100%.";
      if (!intIn(d.minRequests, 1, 100000)) e.minRequests = "Enter 1–100,000 requests.";
    }
    if (d.kind === "provider_failing") {
      if (d.consecutive.trim() && !intIn(d.consecutive, 1, 100)) e.consecutive = "Enter 1–100 failures.";
      if (!d.consecutive.trim() && !rateSet) e.consecutive = "Set failures in a row, an error rate, or both.";
    }
  }
  const emails = parseEmails(d.emails); if (typeof emails === "string") e.emails = emails;
  return e;
}
/** The exact request body for a valid draft (fields of other kinds are omitted). */
export function ruleBody(d: RuleDraft, scope: AlertScope) {
  const emails = parseEmails(d.emails);
  const body: Record<string, unknown> = { name: d.name.trim(), kind: d.kind, enabled: d.enabled, notify_platform_admins: d.notifyPlatformAdmins, notify_emails: Array.isArray(emails) ? emails : [] };
  if (scope.kind === "workspace") body.notify_workspace_admins = d.notifyWorkspaceAdmins;
  if (d.kind === "budget_threshold") Object.assign(body, { budget_layers: layersFor(scope).filter(l => d.layers.includes(l)), thresholds: parseThresholds(d.thresholds) });
  if (d.kind === "spend_spike") Object.assign(body, { spike_factor_percent: factorToPercent(d.factor), min_spend_microusd: dollarsToMicroUsd(d.minSpend) });
  if (d.kind === "error_rate" || d.kind === "provider_failing") {
    body.window_minutes = Number(d.window);
    if (d.kind === "error_rate" || d.rate.trim()) Object.assign(body, { error_rate_percent: Number(d.rate), min_requests: Number(d.minRequests) });
  }
  if (d.kind === "provider_failing") {
    if (d.consecutive.trim()) body.consecutive_failures = Number(d.consecutive);
    if (d.connection) body.provider_connection_id = d.connection;
  }
  return body;
}

/** One short line: what the rule watches. */
export function conditionText(r: Pick<AlertRule, "kind" | "budget_layers" | "thresholds" | "spike_factor_percent" | "min_spend_microusd" | "window_minutes" | "error_rate_percent" | "min_requests" | "consecutive_failures" | "provider_connection">): string {
  switch (r.kind) {
    case "budget_threshold": return `${(r.thresholds ?? []).join("/")}% of ${(r.budget_layers ?? []).map(l => layerLabels[l]).join(", ")} budgets`;
    case "spend_spike": return `Last hour ≥ ${percentToFactor(r.spike_factor_percent ?? 0)}× hourly average, at least ${formatMicroUsd(r.min_spend_microusd)}`;
    case "error_rate": return `≥ ${r.error_rate_percent}% failed over ${r.window_minutes} min (at least ${r.min_requests})`;
    case "provider_failing": {
      const parts = [r.consecutive_failures ? `${r.consecutive_failures} failures in a row` : "", r.error_rate_percent ? `≥ ${r.error_rate_percent}% failed over ${r.window_minutes} min` : ""].filter(Boolean);
      return `${r.provider_connection?.name ?? "Any connection"}: ${parts.join(" or ")}`;
    }
  }
}
export function recipientsText(r: Pick<AlertRule, "notify_workspace_admins" | "notify_platform_admins" | "notify_emails">): string {
  const parts = [r.notify_workspace_admins ? "Workspace admins" : "", r.notify_platform_admins ? "Platform admins" : "", r.notify_emails.length ? `${r.notify_emails.length} ${r.notify_emails.length === 1 ? "address" : "addresses"}` : ""].filter(Boolean);
  return parts.length ? parts.join(" + ") : "In-app only";
}
/** Where an incident happened, without private details. */
export function whereText(e: Pick<AlertEvent, "workspace" | "connection" | "builtin">): string {
  if (e.connection) return e.connection.name;
  if (e.workspace) return e.workspace.kind === "personal" ? "Personal workspace" : e.workspace.name;
  return "Installation";
}
export const resolutionLabels: Record<NonNullable<AlertEvent["resolution"]>, string> = { cleared: "Cleared", superseded: "Replaced by a higher level", rule_disabled: "Rule turned off" };
export const emailLabels: Record<EmailOutcome["status"], string> = { pending: "Sending", sent: "Sent", partial: "Partly sent", failed: "Failed", not_configured: "Email not set up", no_recipients: "No recipients" };
/** Unknown cost is shown as a caveat, never folded into the spend. */
export function unknownCostNote(e: Pick<AlertEvent, "details">): string | undefined {
  const n = Number(e.details.unknown_cost_requests ?? 0);
  return Number.isSafeInteger(n) && n > 0 ? `${n} ${n === 1 ? "request has" : "requests have"} unknown cost, not included; spend may be higher.` : undefined;
}

/*
 * Key safety audit (docs/key-safety.md): findings per active key from
 * GET …/key-safety (workspace: the keys you can list; platform: Team/Project
 * keys without names, personal keys only as counts). Read-only: every fix
 * opens an existing key action (rotate with a new expiry, limits, access,
 * disable, revoke) on the key's own page or its confirmation dialog.
 */
import type { WorkspaceKind } from "./api";

export type Severity = "high" | "medium" | "low";
export type FindingCode = "no_expiry" | "expiry_beyond_max" | "no_limits" | "no_budget" | "owner_lost_access" | "unused" | "never_used" | "broad_model_access" | "not_rotated";
export type Finding = { code: FindingCode; severity: Severity; days?: number; models?: number };
export type SafetyKey = { id: string; /** Workspace scope only: admin views name shared keys "API key in <workspace>". */ name?: string; workspace: { id: string; name: string; kind: WorkspaceKind }; holder: "you" | "member" | "service_account"; issued_to_user_id?: string | null; service_account_id?: string | null; created_at: string; expires_at: string | null; last_used_at: string | null; model_restricted: boolean };
export type SafetyRow = { key: SafetyKey; severity: Severity; findings: Finding[] };
export type SafetySummary = { keys: number; flagged: number; high: number; medium: number; low: number };
export type SafetyThresholds = { unused_days: number; rotation_days: number; max_lifetime_days: number; broad_models: number; never_used_grace_days: number };
export type SafetyReport = { scope: "workspace" | "platform"; summary: SafetySummary; /** Platform only: personal keys as counts (null when filtered to one workspace). */ personal?: SafetySummary | null; data: SafetyRow[]; truncated?: boolean; thresholds: SafetyThresholds };

export const severities: Severity[] = ["high", "medium", "low"];
export const severityLabels: Record<Severity, string> = { high: "High", medium: "Medium", low: "Low" };
export const severityTone: Record<Severity, "danger" | "warning" | "neutral"> = { high: "danger", medium: "warning", low: "neutral" };
const rank: Record<Severity, number> = { high: 3, medium: 2, low: 1 };
export const compareSeverity = (a: Severity, b: Severity) => rank[b] - rank[a];

export const findingLabels: Record<FindingCode, string> = { no_expiry: "No expiry", expiry_beyond_max: "Expiry too long", no_limits: "No limits", no_budget: "No budget", owner_lost_access: "Holder left", unused: "Unused", never_used: "Never used", broad_model_access: "All models", not_rotated: "Not rotated" };
const plural = (n: number, noun: string) => `${n.toLocaleString("en-US")} ${noun}${n === 1 ? "" : "s"}`;
/** One short sentence: the tooltip behind a finding badge. */
export function findingHint(f: Finding, t?: Pick<SafetyThresholds, "max_lifetime_days">): string {
  switch (f.code) {
    case "no_expiry": return "Never expires.";
    case "expiry_beyond_max": return `Expires in ${plural(f.days ?? 0, "day")}${t ? `; new keys can last at most ${plural(t.max_lifetime_days, "day")}` : ""}.`;
    case "no_limits": return "No budget or rate limit at any level.";
    case "no_budget": return "No spending budget at any level, only rate limits.";
    case "owner_lost_access": return "Its holder is no longer a member here.";
    case "unused": return `Last used ${plural(f.days ?? 0, "day")} ago.`;
    case "never_used": return `Never used in ${plural(f.days ?? 0, "day")}.`;
    case "broad_model_access": return `Can call all ${plural(f.models ?? 0, "model")} here.`;
    case "not_rotated": return `Secret is ${plural(f.days ?? 0, "day")} old.`;
  }
}

/** Existing key actions a finding is fixed with. Expiry is fixed at creation, so "Set expiry" rotates. */
export type Fix = "set_expiry" | "rotate" | "add_budget" | "restrict_models" | "disable" | "revoke";
export const findingFix: Record<FindingCode, Fix> = { no_expiry: "set_expiry", expiry_beyond_max: "set_expiry", no_limits: "add_budget", no_budget: "add_budget", owner_lost_access: "revoke", unused: "disable", never_used: "disable", broad_model_access: "restrict_models", not_rotated: "rotate" };
export const fixLabels: Record<Fix, string> = { set_expiry: "Set expiry", rotate: "Rotate", add_budget: "Add budget", restrict_models: "Restrict models", disable: "Disable", revoke: "Revoke" };
/** Fixes that open a key page tab rather than a confirmation dialog. */
export const fixTab: Partial<Record<Fix, "limits" | "access">> = { add_budget: "limits", restrict_models: "access" };
export const fixHint: Partial<Record<Fix, string>> = { set_expiry: "Rotates the key with a new expiry.", restrict_models: "A key's models can't change: create a restricted key, then revoke this one." };
/** Distinct fixes for a row's findings, most severe first. */
export function rowFixes(row: Pick<SafetyRow, "findings">): Fix[] {
  return [...new Set([...row.findings].sort((a, b) => compareSeverity(a.severity, b.severity)).map(f => findingFix[f.code]))];
}

/** Findings by key id (workspace lists and the key page). */
export function findingsByKey(report?: SafetyReport): Map<string, SafetyRow> { return new Map((report?.data ?? []).map(r => [r.key.id, r])); }
/** "API key in Product" (admin views never show shared key names). */
export const adminKeyLabel = (row: SafetyRow) => `API key in ${row.key.workspace.name}`;
export const holderLabel: Record<SafetyKey["holder"], string> = { you: "You", member: "Member", service_account: "Service account" };
/** Platform rows matching the page's search (workspace name) and severity filter. */
export function filterSafetyRows(rows: SafetyRow[], q?: string, severity?: string): SafetyRow[] {
  const text = q?.trim().toLowerCase(), picked = severity?.split(",").filter((s): s is Severity => severities.includes(s as Severity));
  return rows.filter(r => (!picked?.length || picked.includes(r.severity)) && (!text || r.key.workspace.name.toLowerCase().includes(text) || r.findings.some(f => findingLabels[f.code].toLowerCase().includes(text))));
}
export const keySafetyPath = (base: string, params?: { key_id?: string; workspace_id?: string }) => {
  const q = new URLSearchParams(Object.entries(params ?? {}).filter((e): e is [string, string] => !!e[1]));
  return `${base}/key-safety${q.size ? `?${q}` : ""}`;
};

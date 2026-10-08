/*
 * API key lifecycle and usage (ux-api-contract "Key detail, disable vs
 * revoke"). Display status: revoked (permanent) > expired > disabled
 * (reversible) > active. Enable is offered only for disabled keys; revoked or
 * expired keys never come back. Money stays integer micro-USD strings.
 */
import type { Key, Session, Workspace } from "./api";
import type { BudgetPeriod, BudgetWindow } from "./governance";
import type { LifecycleStatus } from "../components/templates/status-pill";
import { canManageKey, canRotateKey } from "./permissions";

export type KeyStatus = "active" | "disabled" | "revoked" | "expired";
/** The key lineage's primary (smallest) key budget and its use in the current window; `limit_microusd` null = no key budget. */
export type KeyUsage = { period: BudgetPeriod; used_microusd: string | null; limit_microusd: string | null; window_start: string; window_end: string | null; unresolved_usage: boolean };
export type KeyRow = Key & { status?: KeyStatus; disabled_at?: string | null; last_used_at?: string | null; lineage_id?: string; usage?: KeyUsage };
export type KeyTotals = { spend_microusd: string; held_microusd: string; requests: string; attempts?: string; unresolved_attempts: string };
export type KeyStats = { key_id: string; lineage_id: string; status: KeyStatus; last_used_at: string | null; daily: (KeyTotals & { date: string })[]; totals: { today: KeyTotals; week: KeyTotals; month: KeyTotals }; budgets: BudgetWindow[] };

/** Server status when present (newer gateways), else derived the same way. */
export function keyStatus(key: Pick<KeyRow, "status" | "revoked_at" | "expires_at" | "disabled_at">, now = Date.now()): KeyStatus {
  if (key.status === "active" || key.status === "disabled" || key.status === "revoked" || key.status === "expired") return key.status;
  if (key.revoked_at) return "revoked";
  if (key.expires_at && Date.parse(key.expires_at) <= now) return "expired";
  return key.disabled_at ? "disabled" : "active";
}
/** StatusPill props: expired reads "Expired" with the permanent (revoked) tone. */
export function keyPill(status: KeyStatus): { status: LifecycleStatus; label?: string } {
  return status === "expired" ? { status: "revoked", label: "Expired" } : { status };
}
export const keyStatusLabel: Record<KeyStatus, string> = { active: "Active", disabled: "Disabled", revoked: "Revoked", expired: "Expired" };
/** Disable: active keys the caller manages. Enable: disabled keys only (never revoked or expired). */
export const canDisable = (session: Session, workspace: Workspace, key: KeyRow) => keyStatus(key) === "active" && canManageKey(session, workspace, key);
export const canEnable = (session: Session, workspace: Workspace, key: KeyRow) => keyStatus(key) === "disabled" && canManageKey(session, workspace, key);
export const canRevoke = (session: Session, workspace: Workspace, key: KeyRow) => keyStatus(key) !== "revoked" && canManageKey(session, workspace, key);
/** Rotation needs an active key (the server refuses disabled keys with `key_disabled`). */
export const canRotate = (session: Session, workspace: Workspace, key: KeyRow) => keyStatus(key) === "active" && canRotateKey(session, workspace, key);
/** Key limits: the key holder or a workspace admin may tighten them while the key can still be used again. */
export const canEditKeyLimits = (session: Session, workspace: Workspace, key: KeyRow) => (keyStatus(key) === "active" || keyStatus(key) === "disabled") && canManageKey(session, workspace, key);
/** Who holds a key: "You", a member's email when the viewer can see the member list (workspace admins), else "Another member". */
export function issuedTo(key: KeyRow, session: Session, accounts?: { id: string; name: string }[], members?: { user_id: string; email: string | null }[]): string {
  if (key.service_account_id) return `Service account · ${accounts?.find(a => a.id === key.service_account_id)?.name ?? "unnamed"}`;
  if (key.issued_to_user_id === session.user.id) return "You";
  return members?.find(m => m.user_id === key.issued_to_user_id)?.email ?? "Another member";
}
const keyStatusValues = ["active", "disabled", "revoked", "expired"];
/** The API keys list's status filter: Active by default (revoked keys don't crowd the list); `all` shows every key. */
export const keyListStatus = (status?: string): string | undefined => status === "all" ? undefined : status && keyStatusValues.includes(status) ? status : "active";
/** Sort: active, disabled, expired, revoked; newest first within each. */
export function sortKeys(keys: KeyRow[]): KeyRow[] {
  const rank: Record<KeyStatus, number> = { active: 0, disabled: 1, expired: 2, revoked: 3 };
  return [...keys].sort((a, b) => rank[keyStatus(a)] - rank[keyStatus(b)] || Date.parse(b.created_at) - Date.parse(a.created_at) || a.id.localeCompare(b.id));
}
/** Keys matching the list's URL filters: status and a name or ID search. */
export function filterKeys(keys: KeyRow[], status?: string, q?: string): KeyRow[] {
  const text = q?.trim().toLowerCase();
  return sortKeys(keys).filter(k => (!status || !["active", "disabled", "revoked", "expired"].includes(status) || keyStatus(k) === status) && (!text || k.name.toLowerCase().includes(text) || k.id.toLowerCase().includes(text)));
}
export const expiryPresets = [7, 30, 90, 180, 365];
/** Custom expiry in whole days (1 to 365). */
export function expiryError(raw: string): string | undefined {
  const value = raw.trim();
  if (!/^\d{1,3}$/.test(value)) return "Enter a whole number of days.";
  const n = Number(value);
  return n < 1 || n > 365 ? "Keys must expire within 1 to 365 days." : undefined;
}

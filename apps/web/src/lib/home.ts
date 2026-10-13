/*
 * Home (user-level) data: the caller's own usage and keys across their workspaces
 * (`GET /me/summary`, `GET /me/keys`, ux-api-contract §4). Home never reads
 * workspace-wide reports, other people's keys or request details.
 */
import { ApiError, type Session, type Workspace, type WorkspaceKind, type Role } from "./api";
import { personalWorkspace, portalWorkspaces } from "./navigation";
/** Totals only: Home doesn't show the per-workspace breakdown, whose cost grows with membership count. */
export const ME_SUMMARY = "/api/v1/me/summary?totals_only=true";
/** "Models you can use" across your workspaces in one bounded request (never one per workspace). */
export const ME_MODELS = "/api/v1/me/models";
export type MeModel = { model_id: string; public_name: string; display_name: string; workspace_count: number; workspaces: { id: string; name: string }[] };
export type MeModels = { models: MeModel[]; usable_count: number; not_serving: Pick<MeModel, "model_id" | "public_name" | "display_name">[]; not_serving_count: number; workspaces_without_models: string[] };
export const ME_KEYS = "/api/v1/me/keys?status=active&limit=100";
export type MeUsage = { known_cost_microusd: string | null; held_microusd: string | null; requests: string | null; attempts?: string | null; unresolved_attempts: string | null; input_tokens?: string | null; output_tokens?: string | null; tokens: string | null; unknown_token_attempts: string | null };
export type MeSummary = {
  currency: "USD"; basis?: string; observed_at?: string;
  period: { current: { start_date: string; end_date: string }; previous: { start_date: string; end_date: string } };
  totals: { current: MeUsage; previous: MeUsage | null };
  workspaces: { workspace_id: string; name: string; kind: WorkspaceKind; role: Role | null; active_keys?: string; current: MeUsage; previous: MeUsage | null }[];
};
export type KeyUsage = { period: "day" | "week" | "month" | "lifetime"; used_microusd: string | null; limit_microusd: string | null; window_start?: string | null; window_end?: string | null; unresolved_usage?: boolean | null };
export type MeKey = { id: string; name: string; workspace: { id: string; name: string; kind: WorkspaceKind }; status: "active" | "disabled" | "revoked" | "expired"; created_at: string; expires_at: string; revoked_at: string | null; disabled_at?: string | null; last_used_at: string | null; model_ids: string[] | null; usage: KeyUsage | null };
/** An endpoint the running gateway doesn't serve yet (feature detection, not an error). */
export const notAvailable = (error: unknown) => error instanceof ApiError && (error.status === 404 || error.status === 405 || error.status === 501);
/** "Welcome, alex" from the email's local part; the gateway has no display names. */
/** "Welcome, {first name}" from the identity provider's display name; without one just "Welcome" (never a guess from the email). */
export function welcomeTitle(session: Session) { const name = session.user.display_name?.trim(); return name ? `Welcome, ${name.split(/\s+/)[0]!}` : "Welcome"; }
/** Shared workspaces for "Your teams & projects": memberships only, teams first, then projects, by name. */
export function sharedMemberships(session: Session): Workspace[] {
  const order = { team: 0, project: 1, personal: 2 } as const;
  return portalWorkspaces(session).filter(w => w.kind !== "personal").sort((a, b) => order[a.kind] - order[b.kind] || a.name.localeCompare(b.name));
}
/** No enabled route (connection and route both on): requests fail. Unknown route counts are not flagged. */
export const notServing = (row: { routes?: number | string | null }) => row.routes != null && Number(row.routes) === 0;
/** The keys card: active keys with the most spend in their current budget window first. */
export function topKeys(keys: MeKey[], limit = 5): MeKey[] {
  const used = (k: MeKey) => { const v = k.usage?.used_microusd; return v && /^\d+$/.test(v) ? BigInt(v) : -1n; };
  return keys.filter(k => k.status === "active").sort((a, b) => { const d = used(b) - used(a); return d > 0n ? 1 : d < 0n ? -1 : a.name.localeCompare(b.name); }).slice(0, limit);
}
/** Where "Create key" goes: workspaces where you can issue your own key, Personal first. */
export function keyWorkspaces(session: Session): Workspace[] {
  const personal = personalWorkspace(session);
  return portalWorkspaces(session).filter(w => w.capabilities.issue_own_key).sort((a, b) => (a.id === personal?.id ? -1 : b.id === personal?.id ? 1 : 0));
}
/** "September" from a YYYY-MM-DD date (UTC). */
export function monthName(date: string | undefined) { if (!date || !/^\d{4}-\d{2}-\d{2}$/.test(date)) return undefined; return new Date(`${date}T00:00:00Z`).toLocaleString("en-US", { month: "long", timeZone: "UTC" }); }

/*
 * Home (user-level) data: the caller's own usage and keys across their workspaces
 * (`GET /me/summary`, `GET /me/keys`, ux-api-contract §4). Home never reads
 * workspace-wide reports, other people's keys or request details.
 */
import { ApiError, type Session, type Workspace, type WorkspaceKind, type Role } from "./api";
import type { WorkspaceCatalogModel } from "./model-setup";
import { personalWorkspace, portalWorkspaces } from "./navigation";
export const ME_SUMMARY = "/api/v1/me/summary";
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
/** A row of GET /workspaces/{ws}/catalog as Home uses it: eligibility and the number of enabled routes. */
export type HomeCatalogRow = Pick<WorkspaceCatalogModel, "model_id" | "public_name" | "display_name" | "eligibility"> & { routes?: number | string | null };
/** Added (selected or assigned) in that workspace, as opposed to only available to add. */
export const addedModel = (row: Pick<HomeCatalogRow, "eligibility">) => row.eligibility !== "available_from_catalog";
/** No enabled route (connection and route both on): requests fail. Unknown route counts are not flagged. */
export const notServing = (row: Pick<HomeCatalogRow, "routes">) => row.routes != null && Number(row.routes) === 0;
/** Home model cards: one per model, listing every workspace of yours where it is added (Personal first). */
export type HomeModel = { model: HomeCatalogRow; workspaces: Workspace[] };
/**
 * "Models you can use": added models that can serve. Added models without an enabled route are returned separately
 * (`notServing`), matching the access reasons ("No route to a provider is turned on"), never listed as usable.
 */
export function modelsAcrossWorkspaces(entries: { workspace: Workspace; rows: HomeCatalogRow[] | undefined }[]): { usable: HomeModel[]; notServing: HomeModel[] } {
  const byId = new Map<string, HomeModel>();
  for (const { workspace, rows } of entries) for (const r of rows ?? []) {
    if (!addedModel(r)) continue;
    const found = byId.get(r.model_id);
    if (found) { if (!found.workspaces.some(w => w.id === workspace.id)) found.workspaces.push(workspace); }
    else byId.set(r.model_id, { model: r, workspaces: [workspace] });
  }
  const all = [...byId.values()].sort((a, b) => b.workspaces.length - a.workspaces.length || (a.model.display_name || a.model.public_name).localeCompare(b.model.display_name || b.model.public_name));
  return { usable: all.filter(m => !notServing(m.model)), notServing: all.filter(m => notServing(m.model)) };
}
/** Workspaces where a key could call something: at least one model added. Unknown (still loading) counts as yes. */
export function addedModelCount(rows: HomeCatalogRow[] | undefined): number | undefined { return rows?.filter(addedModel).length; }
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

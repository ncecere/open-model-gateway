/*
 * Usage & costs › Holds: the aged unknown-hold review queue (#101).
 *
 * A hold is the amount a gateway keeps reserved for an attempt whose cost is still unknown. Its
 * `held_microusd` is a floor ("at least"), never an upper bound, and may be null; an `unbounded_cost` hold has no
 * known limit at all. Amounts are decimal micro-USD strings and are only ever formatted with BigInt, never Number.
 * Nothing here refunds, releases or settles a hold: reconciling uses the existing evidence-backed cost flow.
 */
import { ApiError, platformPath, wsPath, type Session, type Workspace } from "./api";
import { formatMicroUsd } from "./governance";
import { canView, inWorkspacePortal, type DashboardSearch } from "./permissions";

export type HoldWorkspace = { id: string; name: string; kind: "personal" | "team" | "project" };
export type HoldRow = {
  execution_id: string; root_request_id: string; workspace: HoldWorkspace; public_model: string; provider: string; workload_kind: string;
  unknown_since: string; /** Whole hours; at least 24. */ age_hours: number; /** A retained floor: decimal micro-USD, or null when unknown. */ held_microusd: string | null; unbounded_cost: boolean;
  /** The `/costs` unresolved-reason vocabulary. */ unresolved_reason: string | null; /** Server-derived (live Platform Admin authority, enabled workspace, pinned price, terminal non-realtime execution). */ can_reconcile: boolean;
};
/** Counts and money are decimal strings; ages are whole hours, null when there are no rows. */
export type HoldTotals = { count: string; known_floor_microusd: string | null; unbounded_count: string; oldest_age_hours: number | null };
export type HoldsPage = { observed_at: string; data: HoldRow[]; totals: HoldTotals; /** Platform route only: every personal workspace combined, identity-free. */ personal: HoldTotals | null; has_more: boolean; next_cursor: string | null };

export const HOLDS_LIMIT = 50;
export const holdsPath = (workspaceId?: string) => workspaceId ? `${wsPath(workspaceId)}/holds` : `${platformPath}/holds`;
export const holdsQuery = (cursor?: string | null, limit = HOLDS_LIMIT) => `limit=${limit}${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`;
/** A gateway without the review queue answers 404 for its route; nothing else means "not available". */
export const holdsUnavailable = (error: unknown) => error instanceof ApiError && error.status === 404;

const exact = (value: string | null | undefined): value is string => typeof value === "string" && value.length <= 128 && /^\d+$/.test(value);
/** "At least $1.500000": the retained floor, exact to the micro-USD. Null or malformed is "No floor known", never zero. */
/** The bare exact amount (for a tile already labelled "at least"), or "No floor known". */
export const floorAmount = (value: string | null | undefined) => exact(value) ? formatMicroUsd(value) : "No floor known";
export const floorText = (value: string | null | undefined) => exact(value) ? `At least ${formatMicroUsd(value)}` : "No floor known";
const plural = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"}`;
/** Whole hours as "1 day 2 hours". */
export function ageText(hours: number | null | undefined): string {
  if (hours == null || !Number.isSafeInteger(hours) || hours < 0) return "Unknown";
  if (hours < 1) return "Under 1 hour";
  const days = Math.floor(hours / 24), rest = hours % 24;
  return [days > 0 && plural(days, "day"), (rest > 0 || days === 0) && plural(rest, "hour")].filter(Boolean).join(" ");
}

const reasons: Record<string, string> = { missing_billing_usage: "Provider didn't report usage", incomplete_usage_or_cache_allocation: "Usage or cache allocation incomplete", incomplete_or_unpriced_meter_usage: "Usage or price incomplete", incomplete_raw_usage: "Usage incomplete", unpriced: "No price set", unbounded_cost_or_unknown_rate: "No upper limit known" };
/** Why the cost is still unknown (the `/costs` vocabulary), in words; an unrecognised code stays readable. */
export const reasonText = (reason: string | null | undefined) => reason ? reasons[reason] ?? `${reason.charAt(0).toUpperCase()}${reason.slice(1).replaceAll("_", " ")}` : "Cost unknown";

/** Every loaded page as one list: a row repeated across pages shows once; totals, personal aggregate, `has_more` and the cursor are the most recently fetched (last) page's (totals cover the whole scope, not the page). */
export function mergeHoldPages(pages: HoldsPage[]) {
  const seen = new Set<string>(), rows: HoldRow[] = [];
  for (const page of pages) for (const row of page.data) if (!seen.has(row.execution_id)) { seen.add(row.execution_id); rows.push(row); }
  const last = pages[pages.length - 1];
  return { rows, totals: last?.totals ?? { count: "0", known_floor_microusd: null, unbounded_count: "0", oldest_age_hours: null } as HoldTotals, personal: last?.personal ?? null, hasMore: !!last?.has_more && !!last.next_cursor, nextCursor: last?.has_more ? last.next_cursor : null, observedAt: last?.observed_at };
}

/** The request behind a hold, only where the existing request page is open to this viewer; Admin never opens a personal workspace's request. */
export function holdRequestLink(session: Session, row: HoldRow, workspace?: Workspace): DashboardSearch | undefined {
  if (!workspace) return session.capabilities.platform_read && row.workspace.kind !== "personal" ? { page: "platform-log-detail", record: row.root_request_id } : undefined;
  return canView("request-detail", session, workspace) ? { page: "request-detail", ws: workspace.id, record: row.root_request_id } : undefined;
}
/** The existing reconcile action is Platform Admin only; the server's `can_reconcile` says whether this row can take it. */
export const canReconcileHold = (session: Session, row: HoldRow) => row.can_reconcile && session.capabilities.platform_write;

/** Who the list covers: a member sees only their own keys' holds. */
export function holdsScopeText(workspace?: Workspace): string {
  if (!workspace) return "Team and Project holds on this installation · Personal workspaces appear only as one combined total";
  if (workspace.kind === "personal") return "Your holds";
  return workspace.capabilities.view_all_activity ? `Everyone in ${workspace.name}` : `Only holds from your own keys in ${workspace.name}`;
}

/**
 * "Open holds" on an aged unknown-hold notification: the viewer's own workspace tab when they belong to that workspace,
 * else Admin's queue for platform readers (the workspace route would refuse them, and the personal incident has no
 * workspace at all). Never a rule, a key or an execution.
 */
export function agedHoldLink(session: Session, event: { kind: string; workspace: { id: string } | null }): DashboardSearch | undefined {
  if (event.kind !== "aged_unknown_hold") return;
  const own = event.workspace && session.workspaces.find(w => w.id === event.workspace!.id);
  if (own && inWorkspacePortal(session, own)) return { page: "costs", ws: own.id, tab: "holds" };
  return session.capabilities.platform_read ? { page: "platform-costs", tab: "holds" } : undefined;
}

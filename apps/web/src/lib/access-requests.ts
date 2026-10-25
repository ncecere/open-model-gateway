/*
 * Self-service membership requests (#83, slice A): join a discoverable Team/Project as
 * `member`. Browser-only routes (docs/management-api.md#self-service-membership-requests-83-slice-a).
 * Reasons are bounded plain text and are always rendered as text, never HTML. Server
 * capabilities are presentation only: Rust rechecks authority, discoverability, membership
 * and stale-request conflicts on every write.
 */
import { ApiError, API, type WorkspaceKind } from "./api";
import { retryText } from "./key-issuance";

export type AccessRequestStatus = "pending" | "approved" | "denied" | "withdrawn" | "expired";
/** Only Teams/Projects are ever offered; personal workspaces never appear. */
export type AccessRequestTarget = { id: string; name: string; kind: Exclude<WorkspaceKind, "personal"> };
/**
 * `requester`/`workspace_id`/`decided_by` stay opaque IDs; the server never adds newly disclosed
 * live workspace/membership data to history (docs/management-api.md). Two UX-fix fields narrow
 * that rule, by viewer:
 * - `workspace_name_at_request`: the name captured once, server-side, at submission. Present on
 *   every read (requester and approver), it is a frozen reference, never the workspace's current
 *   (possibly renamed, possibly now-hidden) name.
 * - `requester_display_name`/`requester_email`: present **only** on the workspace- and
 *   platform-scoped approval queues (never on the requester's own `mine`/detail reads). Approvers
 *   there already see the same pair for any entitled user, through the Members "Add member"
 *   picker (owner/admin) or the platform user directory (Platform Admin/Auditor); this only
 *   avoids forcing them to look a raw requester ID up separately. Both fields are server-redacted
 *   to `null` once the requester is no longer eligible for that picker (disabled, cleaned or no
 *   current platform entitlement), so a queue never discloses more about someone than the
 *   matching picker currently would.
 */
export type AccessRequest = {
  id: string;
  requester: string;
  requester_display_name?: string | null;
  requester_email?: string | null;
  target_kind: "workspace";
  workspace_id: string;
  workspace_name_at_request: string;
  requested_role: "member";
  status: AccessRequestStatus;
  reason: string | null;
  created_at: string;
  expires_at: string;
  decided_by: string | null;
  decided_at: string | null;
  membership_grant_id: string | null;
};

export const accessRequestTargetsPath = `${API}/access-request-targets`;
export const myAccessRequestsPath = `${API}/me/access-requests`;
export const myAccessRequestPath = (id: string) => `${myAccessRequestsPath}/${encodeURIComponent(id)}`;
export const withdrawAccessRequestPath = (id: string) => `${myAccessRequestPath(id)}/withdraw`;
export const workspaceAccessRequestsPath = (ws: string) => `${API}/workspaces/${encodeURIComponent(ws)}/access-requests`;
export const workspaceAccessRequestDecisionPath = (ws: string, id: string, approve: boolean) => `${workspaceAccessRequestsPath(ws)}/${encodeURIComponent(id)}/${approve ? "approve" : "deny"}`;
export const platformAccessRequestsPath = `${API}/platform/access-requests`;
export const platformAccessRequestDecisionPath = (id: string, approve: boolean) => `${platformAccessRequestsPath}/${encodeURIComponent(id)}/${approve ? "approve" : "deny"}`;
export const discoverablePath = (ws: string) => `${API}/platform/workspaces/${encodeURIComponent(ws)}/discoverable`;

export const accessRequestStatusLabels: Record<AccessRequestStatus, string> = { pending: "Pending", approved: "Approved", denied: "Denied", withdrawn: "Withdrawn", expired: "Expired" };
export const accessRequestStatusTone: Record<AccessRequestStatus, "warning" | "success" | "danger" | "neutral"> = { pending: "warning", approved: "success", denied: "danger", withdrawn: "neutral", expired: "neutral" };

/** A fresh requester-scoped idempotency key: visible ASCII, well within the server's 1–128-byte bound. */
export const newIdempotencyKey = () => crypto.randomUUID();

/**
 * Mirrors the server's bounded plain-text check (docs/management-api.md): at most 1,000
 * Unicode scalar values, no control characters (so no line breaks either). Comparing by
 * code point, not UTF-16 length, avoids rejecting valid text astride a surrogate pair.
 */
export function reasonError(value: string): string | undefined {
  if (!value) return undefined;
  if ([...value].length > 1000) return "Use at most 1,000 characters.";
  if (/[\u0000-\u001f\u007f-\u009f]/.test(value)) return "Line breaks and control characters are not allowed.";
  return undefined;
}

/** A fixed 429 message naming the wait, from the server's `retry_after_seconds`/`Retry-After`. */
export function accessRequestRefusal(error: unknown): { title: string; message: string; tone: "warning" } | undefined {
  if (!(error instanceof ApiError) || error.status !== 429 || error.reason !== "access_request_rate_limited") return;
  const wait = error.retryAfter;
  return { title: "Too many requests", message: `You've sent too many access requests. ${wait === undefined ? "Try again later." : `Try again ${retryText(wait)}.`}`, tone: "warning" };
}

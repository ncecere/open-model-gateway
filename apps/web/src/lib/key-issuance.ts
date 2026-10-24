/*
 * Key issuance budget (#253): each key a person creates or rotates counts in a rolling 24 hours against their own
 * budget and the workspace's (`GET /workspaces/{ws}/key-issuance`). Workspace admins also get the workspace's block;
 * members see only their own. A refusal is a 429 `key_issuance_limit_exceeded` naming the scope and the wait.
 */
import { ApiError } from "./api";

export type IssuanceBlock = { used: number; limit: number; next_available_at: string | null };
export type KeyIssuance = { window_seconds: number; holder: IssuanceBlock; /** Workspace admins only. */ workspace?: IssuanceBlock };

const plural = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"}`;
/** "in 3 hours 5 minutes": the two largest units, rounded up to the minute above a minute. */
export function retryText(seconds: number): string {
  const total = Math.max(1, Math.ceil(seconds));
  if (total < 60) return `in ${plural(total, "second")}`;
  const minutes = Math.ceil(total / 60);
  if (minutes < 60) return `in ${plural(minutes, "minute")}`;
  const hours = Math.floor(minutes / 60), rest = minutes % 60;
  return `in ${plural(hours, "hour")}${rest ? ` ${plural(rest, "minute")}` : ""}`;
}
export const issuanceUsageText = (block: IssuanceBlock) => `${block.used.toLocaleString("en-US")} of ${block.limit.toLocaleString("en-US")} issued in the last 24 h`;
const waitFrom = (block: IssuanceBlock, now: number) => retryText(block.next_available_at ? (Date.parse(block.next_available_at) - now) / 1000 : 1);
/** Which budget stops Create right now (the person's before the workspace's), with a plain explanation; undefined while there is room. */
export function issuanceBlock(view: KeyIssuance, now = Date.now()): { scope: "holder" | "workspace"; message: string } | undefined {
  const { holder, workspace } = view;
  if (holder.used >= holder.limit) return { scope: "holder", message: `You have created or rotated ${plural(holder.limit, "key")} in the last 24 hours, the most this workspace allows. You can issue another ${waitFrom(holder, now)}.` };
  if (workspace && workspace.used >= workspace.limit) return { scope: "workspace", message: `This workspace has issued ${plural(workspace.limit, "key")} in the last 24 hours, the most it allows. Another can be issued ${waitFrom(workspace, now)}.` };
}
export type IssuanceRefusal = { scope: "holder" | "workspace"; operation: string; retrySeconds: number | undefined; message: string };
/** A 429 `key_issuance_limit_exceeded` as a named message; undefined for any other error. */
export function issuanceRefusal(error: unknown): IssuanceRefusal | undefined {
  if (!(error instanceof ApiError) || error.status !== 429 || error.reason !== "key_issuance_limit_exceeded") return;
  const scope = error.detail?.scope === "workspace" ? "workspace" : "holder", operation = error.detail?.operation ?? "create", wait = error.retryAfter;
  const when = wait === undefined ? "try again later" : `try again ${retryText(wait)}`;
  const message = scope === "workspace" ? `This workspace has reached its limit on creating or rotating keys across all members. ${when[0]!.toUpperCase()}${when.slice(1)}.` : `You've reached this workspace's limit on creating or rotating keys. You can ${when}.`;
  return { scope, operation, retrySeconds: wait, message };
}

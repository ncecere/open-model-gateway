/*
 * Batches (Logs › Batches and a batch's own page): the same batches as
 * `GET /v1/batches`, from GET /api/v1/workspaces/{ws}/batches (members see their
 * own, admins the workspace's) or /api/v1/platform/batches (Team/Project rows;
 * personal workspaces as totals only). Metadata only: no lines or results.
 * Money is exact integer micro-USD strings; an unknown cost is never zero.
 */
import { platformPath, wsPath } from "./api";
import { formatMicroUsd, formatUsd } from "./governance";
import type { BatchRouteWait } from "./batch-scheduling";

export type BatchState = "queued" | "in_progress" | "completed" | "failed" | "cancelled" | "expired";
export type BatchMode = "native" | "gateway";
export type BatchRow = {
  id: string; state: BatchState; upstream_status: string | null; mode: BatchMode; endpoint: string; model: string; provider: string;
  price_tier: "batch" | "standard" | null; batch_price: boolean | null;
  total: number | null; completed: number; failed: number; error_code: string | null;
  created_at: string; in_progress_at: string | null; finalizing_at: string | null; completed_at: string | null; cancel_requested_at: string | null; last_progress_at: string | null;
  input_file_id: string | null; output_file_id: string | null; error_file_id: string | null;
  settled_microusd: string; held_microusd: string; cost_unknown: boolean; /** Attempts whose cost is unknown (decimal string; absent from older gateways). */ cost_unknown_attempts?: string;
  workspace_id: string; workspace_name: string; workspace_kind: string; key_name?: string | null; mine?: boolean;
  /** Gateway-run scheduling (0022): lines running now, and why lines wait (null: not waiting). */
  running_lines?: number; waiting_reason?: string | null;
};
export type BatchPage = { data: BatchRow[]; has_more: boolean; scope?: "workspace" | "own"; personal?: { active: number; finished: number; failed: number; lines: number } };
export type BatchDetail = { batch: BatchRow; outcomes: { state: string; code: string | null; lines: number }[]; scheduling?: BatchRouteWait[] };
export const batchStatuses = [{ value: "active", label: "Running" }, { value: "finished", label: "Finished" }] as const;
export type BatchStatusFilter = typeof batchStatuses[number]["value"];
export const isBatchStatus = (v: unknown): v is BatchStatusFilter => batchStatuses.some(s => s.value === v);

export function batchesPath(scope: { kind: "platform" } | { kind: "workspace"; ws: string }, status?: string, offset = 0) {
  const params = new URLSearchParams({ limit: "50" });
  if (status && isBatchStatus(status)) params.set("status", status);
  if (offset) params.set("offset", String(offset));
  return scope.kind === "platform" ? `${platformPath}/batches?${params}` : `${wsPath(scope.ws)}/batches?${params}`;
}
export const batchPath = (ws: string, id: string) => `${wsPath(ws)}/batches/${encodeURIComponent(id)}`;
export const batchCancelPath = (ws: string, id: string) => `${batchPath(ws, id)}/cancel`;

/** The OpenAI status shown for a batch (the provider's step while running); a gateway-run batch with nothing running and lines held back by its routes is waiting for capacity. */
export function batchStatus(b: Pick<BatchRow, "state" | "upstream_status" | "cancel_requested_at"> & Partial<Pick<BatchRow, "running_lines" | "waiting_reason">>): string {
  if (b.state === "queued" || b.state === "in_progress") {
    if (b.cancel_requested_at) return "cancelling";
    if (b.waiting_reason && !b.running_lines && b.upstream_status !== "finalizing") return "waiting_capacity";
    return b.upstream_status ?? (b.state === "queued" ? "validating" : "in_progress");
  }
  return b.state;
}
const statusLabels: Record<string, string> = { waiting_capacity: "Queued — waiting for capacity", validating: "Validating", in_progress: "Running", finalizing: "Finalizing", cancelling: "Cancelling", completed: "Completed", failed: "Failed", cancelled: "Cancelled", expired: "Expired" };
export const batchStatusLabel = (status: string) => statusLabels[status] ?? status.replace(/_/g, " ");
export function batchStatusTone(status: string): "success" | "danger" | "warning" | "info" | "neutral" {
  if (status === "completed") return "success";
  if (status === "failed" || status === "expired") return "danger";
  if (status === "cancelling" || status === "cancelled") return "warning";
  return "info";
}
export const isActive = (b: Pick<BatchRow, "state">) => b.state === "queued" || b.state === "in_progress";
export const modeLabel = (mode: BatchMode) => mode === "native" ? "Native" : "Gateway";
export const modeHint = (mode: BatchMode) => mode === "native" ? "Runs on the provider's batch API" : "Runs line by line through the gateway";
/** Native batches show which price list applied; gateway-run lines always use standard prices. */
export function priceListLabel(b: Pick<BatchRow, "mode" | "batch_price">): { text: string; tone: "success" | "warning" | "neutral"; hint: string } | null {
  if (b.mode !== "native" || b.batch_price === null) return null;
  return b.batch_price ? { text: "Batch prices", tone: "success", hint: "The provider's published batch prices apply" } : { text: "No batch price", tone: "warning", hint: "No batch price is published for this route: standard prices apply" };
}
/** Lines finished (completed + failed) of the total. */
export function progress(b: Pick<BatchRow, "total" | "completed" | "failed">) {
  const total = b.total ?? 0, done = Math.min(total, b.completed + b.failed);
  return { done, total, text: `${done.toLocaleString("en-US")} / ${total.toLocaleString("en-US")}` };
}
/**
 * Cost so far: settled cost plus what's on hold, rounded for reading (`formatUsd`). `hint` is a short rounded
 * footnote (settled/on hold split or the unknown-cost caveat); `title` carries the exact amounts for a tooltip.
 * Unknown is never shown as zero.
 */
export function costSoFar(b: Pick<BatchRow, "settled_microusd" | "held_microusd" | "cost_unknown"> & Partial<Pick<BatchRow, "cost_unknown_attempts">>) {
  const holding = /^\d+$/.test(b.held_microusd) && BigInt(b.held_microusd) > 0n;
  const held = holding ? formatUsd(b.held_microusd) : null, heldExact = holding ? formatMicroUsd(b.held_microusd) : null;
  const settled = formatUsd(b.settled_microusd), settledExact = formatMicroUsd(b.settled_microusd);
  // Some attempts' cost is unknown (e.g. a failed line on a priced route): the settled amount is a lower bound, so the
  // headline says so instead of collapsing the whole total to "Unknown"; the caveat and exact amounts are the detail.
  if (b.cost_unknown) {
    const n = b.cost_unknown_attempts && /^\d+$/.test(b.cost_unknown_attempts) && b.cost_unknown_attempts !== "0" ? b.cost_unknown_attempts : null;
    const why = n ? `${n} attempt${n === "1" ? "" : "s"} with unknown cost` : "Some attempts' cost is unknown";
    return { text: `At least ${settled}${held ? ` + ${held} on hold` : ""}`, hint: `${why} · at least ${settled} settled${held ? ` · ${held} on hold` : ""}`, title: `${why} · at least ${settledExact} settled${heldExact ? ` · ${heldExact} on hold` : ""}` };
  }
  // A running batch's hold is part of its cost so far: never show it as a bare "$0.00".
  if (held) return { text: `${settled} + ${held} on hold`, hint: `${settled} settled · ${held} on hold`, title: settled === settledExact && held === heldExact ? null : `Exactly ${settledExact} settled · ${heldExact} on hold` };
  return { text: settled, hint: null, title: settled === settledExact ? null : `Exactly ${settledExact}` };
}
const lineStates: Record<string, string> = { running: "Running", succeeded: "Succeeded", failed: "Failed", interrupted: "Interrupted" };
/**
 * A batch-line outcome row: the state once, the error code only when it adds something (an interrupted line's code
 * is "interrupted" too), then the count: "Interrupted · 2 lines", "Failed · rate limited · 1 line".
 */
export function lineOutcomeLabel(o: { state: string; code: string | null; lines: number }): string {
  const state = lineStates[o.state] ?? batchStatusLabel(o.state);
  const code = o.code && o.code !== o.state ? o.code.replace(/_/g, " ") : null;
  return `${state}${code ? ` · ${code}` : ""} · ${o.lines.toLocaleString("en-US")} line${o.lines === 1 ? "" : "s"}`;
}
const reasons: Record<string, string> = { budget_exceeded: "A budget was exhausted", submission_failed: "The provider refused the batch", submission_interrupted: "The submission was interrupted", batch_failed: "The provider failed the batch" };
export const stopReason = (code: string | null) => code ? reasons[code] ?? code.replace(/_/g, " ") : null;
export const endpointLabel = (endpoint: string) => ({ "/v1/chat/completions": "Chat Completions", "/v1/responses": "Responses", "/v1/embeddings": "Embeddings", "/v1/messages": "Messages" } as Record<string, string>)[endpoint] ?? endpoint;

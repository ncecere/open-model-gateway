import { ApiError } from "./api";

export type BatchResult = { added: string[]; failed: { id: string; message: string }[]; stopped: boolean };
const reason = (error: unknown) => error instanceof ApiError ? error.message : error instanceof Error ? error.message : "Request failed";
/**
 * Add several models one request at a time (the API selects one model per POST). Each item succeeds or fails on
 * its own; an abort stops before the next request. `onProgress` reports completed/total after every item.
 */
export async function addEach(ids: string[], post: (id: string) => Promise<unknown>, signal?: AbortSignal, onProgress?: (done: number, total: number) => void): Promise<BatchResult> {
  const result: BatchResult = { added: [], failed: [], stopped: false };
  for (const [index, id] of ids.entries()) {
    if (signal?.aborted) { result.stopped = true; break; }
    try { await post(id); result.added.push(id); }
    catch (error) { if (signal?.aborted) { result.stopped = true; break; } result.failed.push({ id, message: reason(error) }); }
    onProgress?.(index + 1, ids.length);
  }
  return result;
}
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
/** Human summary of a partial batch; names come from the caller's labels. */
export function batchFailureMessage(result: BatchResult, total: number, label: (id: string) => string): string {
  const failed = result.failed.map(f => `${label(f.id)} (${f.message})`).join("; ");
  return `Added ${result.added.length} of ${plural(total, "model")}.${failed ? ` Not added: ${failed}.` : ""}${result.stopped ? " Stopped before the remaining models." : ""} Submitting again retries only the models not yet added.`;
}

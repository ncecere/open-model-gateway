import { ApiError, wsPath } from "./api";

export const EXPORT_MAX_ROWS = 1000;
export const EXPORT_MAX_BYTES = 10 * 1024 * 1024;
/** One bounded page, never cached. No off-origin redirects or arbitrary download URLs. */
export async function usageCsv(workspace: string, limit: number, offset: number, signal?: AbortSignal, reportFilters?: string): Promise<Blob> {
  if (!Number.isInteger(limit) || limit < 1 || limit > EXPORT_MAX_ROWS || !Number.isInteger(offset) || offset < 0 || offset > 100000) throw new Error("Invalid export page.");
  const params = new URLSearchParams(reportFilters);
  params.delete("compare"); params.set("limit", String(limit)); params.set("offset", String(offset));
  if (!params.has("start_date") || !params.has("end_date")) throw new Error("CSV export requires an explicit UTC period.");
  const response = await fetch(`${wsPath(workspace)}/usage-export?${params}`, { credentials: "same-origin", cache: "no-store", redirect: "error", headers: { Accept: "text/csv" }, signal });
  if (response.status === 401 && typeof window !== "undefined") window.dispatchEvent(new Event("omg:unauthorized"));
  if (!response.ok) throw new ApiError(response.status, "export_failed", `CSV export failed (${response.status}).`);
  if (!response.headers.get("content-type")?.toLowerCase().startsWith("text/csv")) throw new Error("Gateway did not return CSV; nothing was downloaded.");
  const reader = response.body?.getReader();
  if (!reader) throw new Error("CSV response has no body.");
  const chunks: Uint8Array<ArrayBuffer>[] = [];
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > EXPORT_MAX_BYTES) { await reader.cancel(); throw new Error("CSV page exceeds 10 MiB. Request fewer rows."); }
      chunks.push(new Uint8Array(value));
    }
  } finally { reader.releaseLock(); }
  signal?.throwIfAborted();
  return new Blob(chunks, { type: "text/csv;charset=utf-8" });
}
export function saveCsv(blob: Blob, offset: number) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url; link.download = `gateway-costs-offset-${offset}.csv`;
  document.body.append(link); link.click(); link.remove();
  // Allow the browser to consume the Blob before revoking its transient URL.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

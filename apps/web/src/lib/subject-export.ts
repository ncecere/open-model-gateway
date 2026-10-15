import { API, ApiError } from "./api";

/** The largest export the browser accepts (the server caps every list). */
const MAX_BYTES = 64 * 1024 * 1024;
export const MAX_EXPORT_DAYS = 93;

/** Days in an inclusive YYYY-MM-DD range, or NaN when either date is invalid. */
export function rangeDays(from: string, to: string): number {
  const a = Date.parse(`${from}T00:00:00Z`), b = Date.parse(`${to}T00:00:00Z`);
  return Number.isNaN(a) || Number.isNaN(b) ? NaN : (b - a) / 86_400_000 + 1;
}

/** Download a JSON export (#74) to a file. Same origin only, never cached or redirected. */
export async function downloadExport(path: string, filename: string, signal?: AbortSignal): Promise<void> {
  if (!path.startsWith(`${API}/`)) throw new Error("Invalid export path.");
  const response = await fetch(path, { credentials: "same-origin", cache: "no-store", redirect: "error", headers: { Accept: "application/json" }, signal });
  if (response.status === 401 && typeof window !== "undefined") window.dispatchEvent(new Event("omg:unauthorized"));
  if (response.status === 429) throw new ApiError(429, "rate_limited", "You can export 3 times an hour. Try again later.");
  if (!response.ok) {
    // Keep the server's short explanation (a bad range, busy) when it sent one.
    const message = await response.json().then((b: { error?: string }) => typeof b?.error === "string" ? b.error : undefined, () => undefined);
    throw new ApiError(response.status, "export_failed", message ?? `Export failed (${response.status}).`);
  }
  const length = Number(response.headers.get("content-length") ?? "0");
  if (length > MAX_BYTES) { await response.body?.cancel(); throw new Error("The export is larger than 64 MiB. Choose a shorter range."); }
  if (!response.headers.get("content-type")?.toLowerCase().startsWith("application/json")) throw new Error("The gateway did not return an export; nothing was downloaded.");
  const blob = await response.blob();
  if (blob.size > MAX_BYTES) throw new Error("The export is larger than 64 MiB.");
  signal?.throwIfAborted();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url; link.download = filename;
  document.body.append(link); link.click(); link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

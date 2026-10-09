/*
 * Workspace › Files: the gateway Files API store as the dashboard sees it
 * (GET /api/v1/workspaces/{ws}/files). Same files as `GET /v1/files`: batch
 * inputs and outputs, and user files (stored now; usable in requests later).
 * Contents are never shown in the page; downloads are opaque attachments.
 */
import { ApiError, csrfCookie, wsPath } from "./api";

export type FilePurpose = "batch" | "batch_output" | "user_data" | "vision" | "assistants" | "evals";
export type GatewayFile = { id: string; filename: string; purpose: FilePurpose; bytes: number; content_type: string | null; created_at: string; expires_at: string | null; status: "processed"; mine: boolean; via_key: boolean };
export type FilesResponse = { data: GatewayFile[]; has_more: boolean; scope: "workspace" | "own"; storage: { quota_bytes: number | null; used_bytes: number | null }; store: { configured: boolean; batch: boolean; user_files: boolean }; max_bytes: number };

export const filePurposes: { value: FilePurpose; label: string; upload: boolean; group: "batch" | "user_files" }[] = [
  { value: "batch", label: "Batch input", upload: true, group: "batch" },
  { value: "batch_output", label: "Batch output", upload: false, group: "batch" },
  { value: "user_data", label: "User data", upload: true, group: "user_files" },
  { value: "vision", label: "Vision", upload: true, group: "user_files" },
  { value: "assistants", label: "Assistants", upload: true, group: "user_files" },
  { value: "evals", label: "Evals", upload: true, group: "user_files" },
];
export const purposeLabel = (purpose: string) => filePurposes.find(p => p.value === purpose)?.label ?? purpose;
export const isFilePurpose = (value: unknown): value is FilePurpose => typeof value === "string" && filePurposes.some(p => p.value === value);
/** Purposes a person can upload now: the store is on and the purpose's Settings group is allowed. */
export function uploadPurposes(store: FilesResponse["store"] | undefined) {
  if (!store?.configured) return [];
  return filePurposes.filter(p => p.upload && store[p.group]);
}
export function filesQuery(ws: string, filters: { q?: string; purpose?: string; after?: string }) {
  const params = new URLSearchParams({ limit: "50" });
  if (filters.q) params.set("search", filters.q);
  if (filters.purpose && isFilePurpose(filters.purpose)) params.set("purpose", filters.purpose);
  if (filters.after) params.set("after", filters.after);
  return `${wsPath(ws)}/files?${params}`;
}
export const filePath = (ws: string, id: string) => `${wsPath(ws)}/files/${encodeURIComponent(id)}`;
export const fileContentPath = (ws: string, id: string) => `${filePath(ws, id)}/content`;
/** Why an upload was refused, in plain words (server `error.reason`). */
export function uploadErrorText(error: unknown): string {
  if (!(error instanceof ApiError)) return "The upload failed. Check your connection and try again.";
  switch (error.reason) {
    case "storage_quota_exceeded": return "Not enough storage left in this workspace. Delete files or ask an admin for a larger quota.";
    case "file_too_large": return "The file is larger than the upload limit.";
    case "file_purpose_disabled": return "Files for this purpose are turned off by the platform administrator.";
    case "file_storage_not_configured": return "File storage isn't set up on this gateway.";
    default: return error.message;
  }
}
/**
 * Multipart upload (purpose first, then the file) with the session's CSRF token. The body streams from the
 * browser; the gateway stores it encrypted as it arrives.
 */
export async function uploadFile(ws: string, purpose: FilePurpose, file: File, signal?: AbortSignal): Promise<GatewayFile> {
  const csrf = csrfCookie(document.cookie);
  if (!csrf) throw new ApiError(403, "csrf_missing", "Your security token is missing. Reload this page and sign in again.");
  const form = new FormData();
  form.append("purpose", purpose);
  form.append("file", file, file.name);
  let response: Response;
  try { response = await fetch(`${wsPath(ws)}/files/upload`, { method: "POST", headers: { Accept: "application/json", "X-CSRF-Token": csrf }, body: form, credentials: "same-origin", cache: "no-store", redirect: "error", signal }); }
  catch (error) { if (signal?.aborted) throw error; throw new ApiError(0, "network_error", "Cannot reach the gateway. Check your connection and try again."); }
  let body: unknown; try { body = await response.json(); } catch { body = undefined; }
  if (response.status === 401) window.dispatchEvent(new Event("omg:unauthorized"));
  if (!response.ok) {
    const error = body && typeof body === "object" && "error" in body && body.error && typeof body.error === "object" ? body.error as Record<string, unknown> : {};
    throw new ApiError(response.status, typeof error.code === "string" ? error.code : "request_failed", typeof error.message === "string" ? error.message : `Upload failed (${response.status}).`, typeof error.reason === "string" ? error.reason : undefined);
  }
  return body as GatewayFile;
}

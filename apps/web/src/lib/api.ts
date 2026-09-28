export const API = "/api/v1";

export class ApiError extends Error {
  constructor(public status: number, public code: string, message: string) {
    super(message);
    this.name = "ApiError";
  }
}

export function csrfCookie(cookie: string): string | undefined {
  const value = cookie.split(";").map((part) => part.trim()).find((part) => part.startsWith("omg_csrf="))?.slice(9);
  if (!value) return undefined;
  try { return decodeURIComponent(value); } catch { return undefined; }
}

// Keep all API requests relative; the browser supplies Origin and session cookies.
// Never put tokens in URLs, persistent storage, Query caches, or error messages.
export async function api<T>(path: string, options: { method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE"; body?: unknown; signal?: AbortSignal } = {}): Promise<T> {
  if (!path.startsWith(`${API}/`) || path.includes("://") || path.includes("#")) throw new Error("Invalid API path");
  const method = options.method ?? "GET";
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method !== "GET") {
    const csrf = csrfCookie(document.cookie);
    if (!csrf) throw new ApiError(403, "csrf_missing", "Your security token is missing. Reload this page and sign in again.");
    headers["X-CSRF-Token"] = csrf;
  }
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  let response: Response;
  try {
    response = await fetch(path, { method, headers, body: options.body === undefined ? undefined : JSON.stringify(options.body), credentials: "same-origin", cache: "no-store", signal: options.signal });
  } catch (error) {
    if (options.signal?.aborted) throw error;
    throw new ApiError(0, "network_error", "Cannot reach the gateway. Check your connection and try again.");
  }
  if (response.status === 401 && path !== `${API}/me` && typeof window !== "undefined") window.dispatchEvent(new Event("omg:unauthorized"));
  const text = response.status === 204 ? "" : await response.text();
  let body: unknown;
  try { body = text ? JSON.parse(text) : undefined; } catch { /* Axum extractors may return plain validation text. */ }
  if (!response.ok) {
    const error = body && typeof body === "object" && "error" in body ? body.error : undefined;
    const code = error && typeof error === "object" && "code" in error && typeof error.code === "string" ? error.code : "request_failed";
    const plain = response.headers.get("content-type")?.includes("text/plain") && text.length <= 2000 ? text.trim() : "";
    const message = error && typeof error === "object" && "message" in error && typeof error.message === "string" ? error.message : plain || `Request failed (${response.status}).`;
    throw new ApiError(response.status, code, message);
  }
  if (body === undefined && response.status !== 204) throw new ApiError(response.status, "invalid_response", "The gateway returned an invalid response.");
  return body as T;
}

export type Role = "owner" | "admin" | "member";
export type AuthoritySource = "platform" | "organization" | "direct" | "personal";
export type OrganizationCapabilities = { create_workspace: boolean; create_personal_workspace: boolean; manage_members: boolean; manage_owners: boolean; delegate_models: boolean; manage_policy: boolean };
export type WorkspaceCapabilities = { issue_own_key: boolean; manage_members: boolean; manage_owners: boolean; manage_service_accounts: boolean; delegate_models: boolean; manage_policy: boolean; view_all_activity: boolean };
// role remains the effective authority. Actual membership is independent: an
// inherited administrator is not automatically a member or a human key issuer.
export type Organization = { id: string; name: string; slug: string; role: Role | "operator"; membership_role?: Role | null; authority_source?: AuthoritySource; capabilities?: OrganizationCapabilities };
export type Workspace = { id: string; organization_id: string; name: string; kind: "personal" | "team" | "project"; role: Role; membership_role?: Role | null; authority_source?: AuthoritySource; capabilities?: WorkspaceCapabilities; own_key_denial_reason?: "organization_membership_required" | "workspace_membership_required" | null };
export type Session = { user: { id: string; email: string; platform_admin: boolean }; organizations: Organization[]; workspaces: Workspace[] };
export type Member = { user_id: string; email: string; role: Role; disabled_at: string | null };
export type Key = { model_ids?: string[] | null; id: string; name: string; issued_to_user_id: string | null; service_account_id: string | null; created_at: string; expires_at: string; revoked_at: string | null };
export type ServiceAccount = { id: string; name: string; disabled_at: string | null };
export type Invitation = { id: string; email: string; workspace_id: string | null; organization_role: Role; workspace_role: Role; expires_at: string; accepted_at: string | null; revoked_at: string | null };
export type Provider = { id: string; name: string; provider: string; endpoint: string | null; region: string | null; enabled: boolean };
export type Model = { id: string; public_name: string; display_name: string; enabled: boolean; personal_enabled: boolean };
export type Deployment = { id: string; model_id: string; provider_connection_id: string; upstream_model: string; enabled: boolean };
export type Grant = Pick<Model, "public_name" | "display_name"> & { model_id: string; workspace_granted?: boolean; individual_granted?: boolean };
export type Execution = { id: string; public_model: string; provider: string; state: string; streamed: boolean; input_tokens: number | null; output_tokens: number | null; elapsed_ms: number | null; started_at: string; error_code: string | null };
export type Usage = { requests: number; input_tokens: number | null; output_tokens: number | null; unknown_usage_requests: number };
export type Audit = { id: string; actor_user_id: string | null; action: string; target_id: string | null; workspace_id: string | null; created_at: string };
export type Collection<T> = { data: T[] };
export type OneTimeToken = { id: string; token: string };
export const platformPath = `${API}/platform`;
export const orgPath = (id: string) => `${API}/orgs/${encodeURIComponent(id)}`;
export const wsPath = (id: string) => `${API}/workspaces/${encodeURIComponent(id)}`;

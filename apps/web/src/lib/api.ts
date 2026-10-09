export const API = "/api/v1";
export class ApiError extends Error {
  /**
   * `reason`: the server's stable machine code for selected errors (`error.reason`), when present. `detail`: the one
   * non-sensitive field that accompanies a policy rejection (`error.period` or `error.limit`), never an amount.
   */
  constructor(public status: number, public code: string, message: string, public reason?: string, public detail?: { period?: string; limit?: string }) { super(message); this.name = "ApiError"; }
}
export function csrfCookie(cookie: string): string | undefined {
  const value = cookie.split(";").map(part => part.trim()).find(part => part.startsWith("omg_csrf="))?.slice(9);
  if (!value) return;
  try { return decodeURIComponent(value); } catch { return; }
}
const requests = new Set<AbortController>();
export function abortRequests() { for (const request of requests) request.abort(); requests.clear(); }
export type RequestOptions = { method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE"; body?: unknown; signal?: AbortSignal };
// Relative paths, browser Origin, exact CSRF cookie, no redirects or response cache.
export async function api<T>(path: string, options: RequestOptions = {}): Promise<T> {
  if (!path.startsWith(`${API}/`) || path.includes("://") || path.includes("#") || path.includes("\\") || /(?:^|\/)\.\.(?:\/|$)/.test(path)) throw new Error("Invalid API path");
  const method = options.method ?? "GET";
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method !== "GET") {
    const csrf = csrfCookie(document.cookie);
    if (!csrf) throw new ApiError(403, "csrf_missing", "Your security token is missing. Reload this page and sign in again.");
    headers["X-CSRF-Token"] = csrf;
  }
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const controller = new AbortController(); requests.add(controller);
  const signal = options.signal ? AbortSignal.any([controller.signal, options.signal]) : controller.signal;
  try {
    let response: Response;
    try { response = await fetch(path, { method, headers, body: options.body === undefined ? undefined : JSON.stringify(options.body), credentials: "same-origin", cache: "no-store", redirect: "error", signal }); }
    catch (error) { if (signal.aborted) throw error; throw new ApiError(0, "network_error", "Cannot reach the gateway. Check your connection and try again."); }
    if (response.status === 401 && path !== `${API}/me` && typeof window !== "undefined") window.dispatchEvent(new Event("omg:unauthorized"));
    const text = response.status === 204 ? "" : await response.text();
    let body: unknown;
    try { body = text ? JSON.parse(text) : undefined; } catch { /* Extractor validation may be plain text. */ }
    if (!response.ok) {
      const error = body && typeof body === "object" && "error" in body ? body.error : undefined;
      const code = error && typeof error === "object" && "code" in error && typeof error.code === "string" ? error.code : "request_failed";
      const plain = response.headers.get("content-type")?.includes("text/plain") && text.length <= 2000 ? text.trim() : "";
      const message = error && typeof error === "object" && "message" in error && typeof error.message === "string" ? error.message : plain || `Request failed (${response.status}).`;
      if (response.status === 403 && method === "GET" && path !== `${API}/me` && typeof window !== "undefined") window.dispatchEvent(new Event("omg:access-refresh"));
      const reason = error && typeof error === "object" && "reason" in error && typeof error.reason === "string" ? error.reason : undefined;
      const field = (name: "period" | "limit") => error && typeof error === "object" && name in error && typeof (error as Record<string, unknown>)[name] === "string" && /^[a-z_]{1,40}$/.test((error as Record<string, string>)[name]!) ? (error as Record<string, string>)[name] : undefined;
      const period = field("period"), limit = field("limit");
      throw new ApiError(response.status, code, message, reason, period || limit ? { ...(period ? { period } : {}), ...(limit ? { limit } : {}) } : undefined);
    }
    signal.throwIfAborted();
    if (body === undefined && response.status !== 204) throw new ApiError(response.status, "invalid_response", "The gateway returned an invalid response.");
    return body as T;
  } finally { requests.delete(controller); }
}
export type Role = "owner" | "admin" | "member";
export type PlatformRole = "user" | "auditor" | "admin";
export type WorkspaceKind = "personal" | "team" | "project";
export type MembershipSource = "manual" | "group" | "mixed";
export type WorkspaceCapabilities = { issue_own_key: boolean; manage_members: boolean; manage_service_accounts: boolean; delegate_models: boolean; manage_policy: boolean; view_all_activity: boolean };
export type Workspace = { id: string; name: string; kind: WorkspaceKind; owner_user_id: string | null; role: Role | null; membership_source: MembershipSource | null; capabilities: WorkspaceCapabilities; cost_center_id?: string | null; /** Named on GET /workspaces/{ws} (not in /me). */ cost_center?: { id: string; name: string; code: string } | null; disabled_at?: string | null; created_at?: string };
/** The uploaded installation logo, served same-origin (`url` is `/api/v1/branding/logo?v=…`). */
export type InstallationLogo = { url: string; updated_at: string };
/** Public sign-in configuration (no session). `installation_name` is sent only with a logo (its alt text). */
export type AuthConfig = { enabled: boolean; logo?: InstallationLogo | null; installation_name?: string | null };
export type Session = { installation: { id: string; name: string; /** Admin › Settings › General (absent from older gateways). `logo_url` is deprecated and never shown. */ support_url?: string | null; logo_url?: string | null; logo?: InstallationLogo | null; key_max_lifetime_days?: number }; user: { id: string; email: string; /** From the last verified sign-in (`name` claim); presentation only. */ display_name?: string | null; platform_role: PlatformRole }; workspaces: Workspace[]; capabilities: { platform_read: boolean; platform_write: boolean; create_workspace: boolean } };
export type RoleGrant = { id?: string; role: PlatformRole | Role; source: "manual" | "group" | "bootstrap"; mapping_id?: string | null; revoked_at?: string | null };
export type PlatformUser = { id: string; email: string | null; display_name?: string | null; platform_role?: PlatformRole | null; role_grants?: RoleGrant[]; disabled_at: string | null; cleanup_due_at?: string | null; cleaned_at?: string | null; created_at?: string };
export type Member = { user_id: string; email: string; display_name?: string | null; role: Role | null; membership_source?: MembershipSource; grants?: RoleGrant[]; disabled_at?: string | null };
export type Key = { model_ids?: string[] | null; id: string; name: string; issued_to_user_id: string | null; service_account_id: string | null; created_at: string; expires_at: string; revoked_at: string | null };
export type ServiceAccount = { id: string; name: string; disabled_at: string | null };
export type Invitation = { id: string; email: string; workspace_id: string; role: Role; expires_at: string; accepted_at: string | null; revoked_at: string | null };
export type GroupMapping = { id: string; issuer: string; group_value: string; target_kind: "platform" | "workspace"; platform_role: PlatformRole | null; workspace_id: string | null; workspace_role: "admin" | "member" | null; enabled: boolean };
export type Provider = { id: string; name: string; provider: string; endpoint: string | null; region: string | null; enabled: boolean; auth_mode: "none" | "credential"; /** Bedrock identity mode; the reference itself is never returned. */ aws_auth?: "default" | "profile" | "role" | null; model_count?: number };
export type ModelProtocol = "chat_completions" | "responses" | "messages" | "embeddings" | "images" | "audio_transcriptions" | "audio_speech" | "rerank" | "systemone" | "realtime" | "videos" | "batches";
/** Server-computed aggregate counts (contract §2). Absent from older gateways: never assume zero. */
export type ModelReadiness = { routes: number; enabled_routes: number; priced_enabled_routes: number; catalogs: number; direct_workspaces: number; connections: { id: string; name: string }[]; /** Configuration check: smallest applicable tokens-per-minute default (null when none). */ type_tokens_per_minute?: number | null; /** Enabled routes whose latest input+output token ceilings exceed it. */ routes_over_token_limit?: number; /** Enabled routes on OpenRouter `:free` endpoints. */ openrouter_free_routes?: number };
/** Server-controlled provider policy (Admin/Auditor readable; no secrets). */
export type ServerPolicy = { openrouter: { data_collection: "deny" | "allow"; free_models_available: boolean } };
export type Model = { id: string; public_name: string; display_name: string; description?: string | null; enabled: boolean; supported_protocols: ModelProtocol[]; readiness?: ModelReadiness; /** When the model was added (wave 2; absent from older gateways). */ created_at?: string };
/** POST /platform/model-setup (contract §1): one transaction; any failure creates nothing. */
export type ModelSetupBody = { model: { public_name: string; display_name: string; description: string | null; supported_protocols: ModelProtocol[]; enabled: boolean }; route: { provider_connection_id: string; upstream_model: string; enabled: boolean }; price: unknown | null; catalog_ids: string[] };
export type ModelSetupResult = { model_id: string; deployment_id: string; price_id: string | null };
/** GET /platform/overview (contract §4): aggregate counts only; the two 7-day totals are decimal strings. */
export type PlatformOverviewData = {
  setup: { connections: number; enabled_connections: number; models: number; ready_models: number; enabled_routes: number; priced_enabled_routes: number; catalogs: number; type_defaults: { personal: number; team: number; project: number }; entitled_users: number; oidc_mappings: number };
  glance: { entitled_users: number; teams: number; projects: number; ready_models: number; attempts_7d: string; known_cost_7d_microusd: string };
  /** Installation-wide budgets with their current usage (wave 2; absent from older gateways). */
  installation_budgets?: InstallationBudget[] | null;
};
/**
 * One installation budget and its current window (contract §5): `used = settled + held` by the admission rule, a lower
 * bound when `unresolved_usage`. Lifetime windows start at installation creation and have no end.
 */
export type InstallationBudget = { period: "day" | "week" | "month" | "lifetime"; amount_microusd: string; used_microusd: string | null; settled_microusd: string | null; held_microusd: string | null; unresolved_usage: boolean | null; exhausted: boolean | null; window_start: string; window_end: string | null };
export type Deployment = { id: string; model_id: string; model_public_name?: string; provider_connection_id: string; provider_name?: string; upstream_model: string; enabled: boolean };
export type Catalog = { id: string; name: string; description: string | null; created_at?: string };
export type CatalogAvailability = { mode: "inherit" | "replace"; catalog_ids: string[]; effective_catalog_ids: string[] };
export type Grant = Pick<Model, "public_name" | "display_name"> & { model_id: string; sources?: ("catalog" | "direct")[]; source?: "catalog" | "direct"; catalog_ids?: string[]; selected: boolean; enabled: boolean; catalog_granted: boolean; direct_granted: boolean; available_from_catalog: boolean; supported_protocols: ModelProtocol[] };
export type CostCenter = { id: string; name: string; code: string; archived_at: string | null };
export type Execution = { id: string; public_model: string; provider: string; state: string; streamed: boolean; input_tokens: string | number | null; output_tokens: string | number | null; elapsed_ms: number | null; started_at: string; error_code: string | null };
export type Usage = { requests: string | number; input_tokens: string | number | null; output_tokens: string | number | null; unknown_usage_requests: string | number };
export type Audit = { id: string; actor_user_id: string | null; action: string; resource_type?: string; resource_id?: string | null; workspace_id: string | null; created_at: string; metadata?: Record<string, unknown>; /** Platform audit only: server-derived name for unnamed targets (routes, prices). */ target_name?: string | null };
export type Collection<T> = { data: T[]; has_more?: boolean };
export type OneTimeToken = { id: string; token: string };
export const platformPath = `${API}/platform`;
export const wsPath = (id: string) => `${API}/workspaces/${encodeURIComponent(id)}`;
export const platformWorkspacePath = (id: string) => `${platformPath}/workspaces/${encodeURIComponent(id)}`;

export const API = "/api/v1";
export class ApiError extends Error {
  /**
   * `code`: `error.code`, the HTTP status as a string for management errors.
   * `reason`: the server's stable machine code for selected errors (`error.reason`), when present. `detail`: the one
   * non-sensitive field that accompanies a policy rejection (`error.period` or `error.limit`), never an amount; a key
   * issuance refusal (429) names its `scope` and `operation` the same way. `retryAfter`: whole seconds until a 429 may
   * succeed (`error.retry_after_seconds`, else the `Retry-After` header).
   */
  constructor(public status: number, public code: string, message: string, public reason?: string, public detail?: { period?: string; limit?: string; scope?: string; operation?: string }, public retryAfter?: number) { super(message); this.name = "ApiError"; }
}
export function csrfCookie(cookie: string): string | undefined {
  const value = cookie.split(";").map(part => part.trim()).find(part => part.startsWith("omg_csrf="))?.slice(9);
  if (!value) return;
  try { return decodeURIComponent(value); } catch { return; }
}
const requests = new Set<AbortController>();
export function abortRequests() { for (const request of requests) request.abort(); requests.clear(); }
export type RequestOptions = ({ body?: unknown; rawJson?: never } | { body?: never; /** Byte-preserving JSON evidence; sent directly, never parsed/stringified. */ rawJson: Blob }) & { method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE"; signal?: AbortSignal; /** Extra request headers (for example `If-Match`); never a way to set `Content-Type`, `Accept` or the CSRF header, which the client always controls itself. */ headers?: Record<string, string> };
// Relative paths, browser Origin, exact CSRF cookie, no redirects or response cache.
export async function api<T>(path: string, options: RequestOptions = {}): Promise<T> {
  if (!path.startsWith(`${API}/`) || path.includes("://") || path.includes("#") || path.includes("\\") || /(?:^|\/)\.\.(?:\/|$)/.test(path)) throw new Error("Invalid API path");
  if (options.body !== undefined && options.rawJson !== undefined) throw new Error("Choose one request body");
  const method = options.method ?? "GET";
  const headers: Record<string, string> = { Accept: "application/json", ...options.headers };
  if (method !== "GET") {
    const csrf = csrfCookie(document.cookie);
    if (!csrf) throw new ApiError(403, "csrf_missing", "Your security token is missing. Reload this page and sign in again.");
    headers["X-CSRF-Token"] = csrf;
  }
  if (options.body !== undefined || options.rawJson !== undefined) headers["Content-Type"] = "application/json";
  const controller = new AbortController(); requests.add(controller);
  const signal = options.signal ? AbortSignal.any([controller.signal, options.signal]) : controller.signal;
  try {
    let response: Response;
    try { response = await fetch(path, { method, headers, body: options.rawJson ?? (options.body === undefined ? undefined : JSON.stringify(options.body)), credentials: "same-origin", cache: "no-store", redirect: "error", signal }); }
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
      // Automatic refusals are distinct from bare manual authority-change events.
      // Carry only the cause and resource path, never query values or response text.
      if (response.status === 403 && method === "GET" && path !== `${API}/me` && typeof window !== "undefined") window.dispatchEvent(new CustomEvent("omg:access-refresh", { detail: { cause: "forbidden", path: path.split("?")[0] } }));
      const reason = error && typeof error === "object" && "reason" in error && typeof error.reason === "string" ? error.reason : undefined;
      const field = (name: "period" | "limit" | "scope" | "operation") => error && typeof error === "object" && name in error && typeof (error as Record<string, unknown>)[name] === "string" && /^[a-z_]{1,40}$/.test((error as Record<string, string>)[name]!) ? (error as Record<string, string>)[name] : undefined;
      const period = field("period"), limit = field("limit"), scope = field("scope"), operation = field("operation");
      const waited = error && typeof error === "object" && "retry_after_seconds" in error ? Number(error.retry_after_seconds) : Number(response.headers.get("retry-after"));
      const retryAfter = response.status === 429 && Number.isSafeInteger(waited) && waited > 0 ? waited : undefined;
      throw new ApiError(response.status, code, message, reason, period || limit || scope || operation ? { ...(period ? { period } : {}), ...(limit ? { limit } : {}), ...(scope ? { scope } : {}), ...(operation ? { operation } : {}) } : undefined, retryAfter);
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
export type Workspace = { id: string; name: string; kind: WorkspaceKind; owner_user_id: string | null; role: Role | null; membership_source: MembershipSource | null; capabilities: WorkspaceCapabilities; cost_center_id?: string | null; /** Named on GET /workspaces/{ws} (not in /me). */ cost_center?: { id: string; name: string; code: string } | null; disabled_at?: string | null; created_at?: string; /** Teams and Projects only, on GET /workspaces/{ws}: who sees files, batches and videos (#141). */ object_visibility?: ObjectVisibility };
/** `member`: members' keys see only their own files, batches and videos; `workspace`: every member's key sees all of them. */
export type ObjectVisibility = "member" | "workspace";
/** The uploaded installation logo, served same-origin (`url` is `/api/v1/branding/logo?v=…`). */
export type InstallationLogo = { url: string; updated_at: string };
/** Public sign-in configuration (no session). `installation_name` is sent only with a logo (its alt text). */
export type AuthConfig = { enabled: boolean; logo?: InstallationLogo | null; installation_name?: string | null };
export type Session = { installation: { id: string; name: string; /** Display-only SDK base URL; null means current origin + /v1. */ inference_base_url?: string | null; /** Admin › Settings › General (absent from older gateways). `logo_url` is deprecated and never shown. */ support_url?: string | null; logo_url?: string | null; logo?: InstallationLogo | null; key_max_lifetime_days?: number }; user: { id: string; email: string; /** From the last verified sign-in (`name` claim); presentation only. */ display_name?: string | null; platform_role: PlatformRole }; workspaces: Workspace[]; capabilities: { platform_read: boolean; platform_write: boolean; create_workspace: boolean } };
export type RoleGrant = { id?: string; role: PlatformRole | Role; source: "manual" | "group" | "bootstrap"; mapping_id?: string | null; revoked_at?: string | null };
export type PlatformUser = { id: string; email: string | null; display_name?: string | null; platform_role?: PlatformRole | null; role_grants?: RoleGrant[]; disabled_at: string | null; /** Why the account is disabled: `entitlement_loss` (no platform role) or `admin_suspension`. */ disable_reason?: string | null; cleanup_due_at?: string | null; cleaned_at?: string | null; created_at?: string };
export type Member = { user_id: string; email: string; display_name?: string | null; role: Role | null; membership_source?: MembershipSource; grants?: RoleGrant[]; disabled_at?: string | null };
export type Key = { model_ids?: string[] | null; id: string; name: string; issued_to_user_id: string | null; service_account_id: string | null; created_at: string; expires_at: string; revoked_at: string | null };
export type ServiceAccount = { id: string; name: string; disabled_at: string | null };
export type Invitation = { id: string; email: string; workspace_id: string; role: Role; expires_at: string; accepted_at: string | null; revoked_at: string | null };
export type GroupMapping = { id: string; issuer: string; group_value: string; target_kind: "platform" | "workspace"; platform_role: PlatformRole | null; workspace_id: string | null; workspace_role: "admin" | "member" | null; enabled: boolean; /** The target Team or Project, named by the gateway. */ workspace_name?: string | null; workspace_kind?: WorkspaceKind | null };
/**
 * POST .../oidc/group-mappings/preview or .../{id}/preview (#89): a
 * read-only dry-run, never an apply authorization. `immediate` is what the
 * real PATCH/DELETE does synchronously the instant it commits (every
 * grant the existing mapping currently produces, revoked unconditionally,
 * whatever the new definition would be); `diff` is the net effective-role
 * change (gained: an upgrade or new entitlement; lost: a downgrade or total
 * loss) once sign-in/SCIM reconciliation has caught up. `digest`, present
 * only for an existing mapping, identifies its reviewed definition for an
 * `If-Match` header on the following PATCH/DELETE; `complete: false` means
 * the group's membership exceeded the roster evaluation cap and this
 * preview may undercount.
 */
export type MappingPreviewEntry = { user_id: string; email: string | null; display_name: string | null; before: string | null; after: string | null; workspace_id: string | null; reactivated?: boolean; would_be_suspended?: boolean };
export type MappingPreviewImmediate = {
  applies: boolean;
  blocked_by_last_admin: boolean;
  platform_role_revoked: { user_id: string; email: string | null; display_name: string | null; role: string }[];
  workspace_membership_revoked: { user_id: string; email: string | null; display_name: string | null; role: string; workspace_id: string }[];
  users_suspended: { user_id: string; email: string | null; display_name: string | null }[];
  keys_revoked_count: number;
};
export type MappingPreview = {
  membership_basis: "scim" | "last_sign_in_derived";
  complete: boolean;
  digest: string | null;
  immediate: MappingPreviewImmediate;
  diff: { platform_role_gained: MappingPreviewEntry[]; platform_role_lost: MappingPreviewEntry[]; workspace_membership_gained: MappingPreviewEntry[]; workspace_membership_lost: MappingPreviewEntry[] };
  counts: { gained: number; lost: number; truncated: boolean };
  warnings: string[];
};
export type Provider = { id: string; name: string; provider: string; endpoint: string | null; region: string | null; enabled: boolean; auth_mode: "none" | "credential"; /** Bedrock identity mode; the reference itself is never returned. */ aws_auth?: "default" | "profile" | "role" | null; model_count?: number };
export type ModelProtocol = "chat_completions" | "responses" | "messages" | "embeddings" | "images" | "audio_transcriptions" | "audio_speech" | "rerank" | "systemone" | "realtime" | "videos" | "batches";
/** Server-computed aggregate counts (contract §2). Absent from older gateways: never assume zero. */
export type ModelReadiness = { routes: number; enabled_routes: number; priced_enabled_routes: number; catalogs: number; direct_workspaces: number; connections: { id: string; name: string }[]; /** Configuration check: smallest applicable tokens-per-minute default (null when none). */ type_tokens_per_minute?: number | null; /** Enabled routes whose latest input+output token ceilings exceed it. */ routes_over_token_limit?: number; /** Enabled routes on OpenRouter `:free` endpoints. */ openrouter_free_routes?: number; /** Enabled routes whose connection profile can serve the model (absent from older gateways). */ serving_routes?: number; /** Enabled routes whose connection profile serves none of the model's protocols. */ unsupported_routes?: number; /** Their connection profiles, e.g. ["vllm"]. */ unsupported_profiles?: string[] };
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
};
export type Deployment = { id: string; model_id: string; model_public_name?: string; provider_connection_id: string; provider_name?: string; upstream_model: string; enabled: boolean };
export type DeploymentConfigurationChange = {
  id: string; created_at: string; target: { kind: "deployment"; id: string };
  actor: { id: string | null; label: string; admin_token_id: string | null };
  action: string; schema_version: number | null; revision: string | null;
  values_available: boolean; before: { enabled: boolean } | null; after: { enabled: boolean } | null;
};
export type DeploymentHistoryPage = { data: DeploymentConfigurationChange[]; next_cursor: string | null };
/** Browser-only configuration observation, not an apply authorization or reservation. */
export type DeploymentPreview = {
  target: { kind: "deployment"; id: string };
  expected_revision: string;
  diff: { field: "enabled"; before: boolean; after: boolean }[];
  serving_routes_gained: string[];
  serving_routes_lost: string[];
  models_losing_last_serving_route: { id: string; public_name: string }[];
  affected_workspaces: { data: { id: string; name: string; kind: "team" | "project" }[]; total: number; limit: number; truncated: boolean };
  observed_at: string;
  dependency_versions: Record<string, string>;
  warnings: string[];
};
export type Catalog = { id: string; name: string; description: string | null; created_at?: string };
export type CatalogAvailability = { mode: "inherit" | "replace"; catalog_ids: string[]; effective_catalog_ids: string[] };
export type Grant = Pick<Model, "public_name" | "display_name"> & { model_id: string; sources?: ("catalog" | "direct")[]; source?: "catalog" | "direct"; catalog_ids?: string[]; selected: boolean; enabled: boolean; catalog_granted: boolean; direct_granted: boolean; available_from_catalog: boolean; supported_protocols: ModelProtocol[] };
export type CostCenter = { id: string; name: string; code: string; archived_at: string | null };
export type Execution = { id: string; public_model: string; provider: string; state: string; streamed: boolean; input_tokens: string | number | null; output_tokens: string | number | null; elapsed_ms: number | null; started_at: string; error_code: string | null };
export type Usage = { requests: string | number; input_tokens: string | number | null; output_tokens: string | number | null; unknown_usage_requests: string | number };
export type Audit = { id: string; actor_user_id: string | null; action: string; resource_type?: string; resource_id?: string | null; workspace_id: string | null; created_at: string; metadata?: Record<string, unknown>;
  /** Names resolved by the gateway under the caller's visibility; null when not visible or unknown. */
  actor_name?: string | null; target_name?: string | null;
  /** Platform audit only: the event's Team or Project and, for workspace targets, the target's kind. */
  workspace_name?: string | null; workspace_kind?: WorkspaceKind | null; target_kind?: WorkspaceKind | null };
export type Collection<T> = { data: T[]; has_more?: boolean };
export type OneTimeToken = { id: string; token: string };
export const platformPath = `${API}/platform`;
export const wsPath = (id: string) => `${API}/workspaces/${encodeURIComponent(id)}`;
export const platformWorkspacePath = (id: string) => `${platformPath}/workspaces/${encodeURIComponent(id)}`;

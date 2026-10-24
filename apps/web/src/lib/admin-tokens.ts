/*
 * Admin API tokens (#37 PR3): browser-only metadata for a Platform Admin's own long-lived
 * management credentials. Browser session, platform_write only (BrowserAdminOnly on the
 * backend): a caller creates for themself, any Admin may revoke any token, and there is no
 * renewal, enable or ownership transfer. Scopes are explicit per route and do not imply read
 * access (`platform:read` is separate from the four `*:write` scopes). These tokens can never
 * call inference or issue inference keys.
 */
import type { StatusPillProps } from "../components/templates/status-pill";

export const adminTokenScopes = [
  { value: "platform:read", label: "Read", description: "Read platform metadata: users, models, connections, usage and audit history." },
  { value: "catalog:write", label: "Catalog", description: "Manage connections, models, routes, catalogs, assignments and pricing." },
  { value: "access:write", label: "Access", description: "Manage users, roles, group mappings, platform workspaces and session revocation. Highly privileged." },
  { value: "governance:write", label: "Governance", description: "Manage workspace and type policy, cost centers, alerts and legal holds." },
  { value: "settings:write", label: "Settings", description: "Manage general, privacy, email and storage settings and their tests. Highly privileged." },
] as const;
export type AdminTokenScope = (typeof adminTokenScopes)[number]["value"];
export const adminTokenScopeValues: readonly AdminTokenScope[] = adminTokenScopes.map(s => s.value);
/** A scope's short display label, or the raw value for anything this dashboard doesn't recognize yet. */
export function scopeLabel(scope: string): string { return adminTokenScopes.find(s => s.value === scope)?.label ?? scope; }

export type AdminToken = {
  id: string; owner_user_id: string; owner_email: string; owner_display_name: string | null;
  name: string; scopes: AdminTokenScope[]; ip_allowlist: string[] | null;
  created_at: string; expires_at: string; revoked_at: string | null;
  last_used_at: string | null; last_used_address: string | null;
};
export type AdminTokenStatus = "active" | "expired" | "revoked";
/** Revocation is permanent and outranks expiry; otherwise status follows `expires_at` (exact instant counts as expired). */
export function adminTokenStatus(token: Pick<AdminToken, "revoked_at" | "expires_at">, now = Date.now()): AdminTokenStatus {
  if (token.revoked_at) return "revoked";
  if (Date.parse(token.expires_at) <= now) return "expired";
  return "active";
}
/** StatusPill props: expired reads "Expired" with the permanent (revoked) tone, like API keys. */
export function adminTokenPill(status: AdminTokenStatus): Pick<StatusPillProps, "status" | "label"> {
  return status === "expired" ? { status: "revoked", label: "Expired" } : { status };
}
/** Revoke is offered (and idempotent) for anything not already revoked; expired tokens can still be revoked for a clean audit trail. */
export const canRevokeAdminToken = (status: AdminTokenStatus) => status !== "revoked";

export const adminTokenExpiryDefault = 30;
/** Custom expiry in whole days (1 to 90); the server is authoritative. */
export function adminTokenExpiryError(raw: string): string | undefined {
  const value = raw.trim();
  if (!/^\d{1,3}$/.test(value)) return "Enter a whole number of days.";
  const n = Number(value);
  return n < 1 || n > 90 ? "Tokens must expire within 1 to 90 days." : undefined;
}
export function adminTokenNameError(raw: string): string | undefined {
  const value = raw.trim();
  if (!value) return "Enter a name.";
  if (new TextEncoder().encode(value).length > 120) return "Use at most 120 characters.";
  return undefined;
}
/** At least one scope is required; a default of platform:read only (read-only) is fine. */
export function adminTokenScopesError(selected: string[]): string | undefined {
  return selected.length === 0 ? "Select at least one scope." : undefined;
}

/**
 * Splits an optional CIDR allowlist textarea (one entry per line or comma-separated) into
 * trimmed, deduplicated entries in first-seen order, or `undefined` when left blank (omitted
 * means unrestricted). Deliberately not a network parser: this never validates CIDR syntax
 * itself. The server is authoritative and returns a clear rejection for malformed entries.
 */
export function parseCidrAllowlist(raw: string): string[] | undefined {
  const entries = [...new Set(raw.split(/[\n,]/).map(v => v.trim()).filter(Boolean))];
  return entries.length ? entries : undefined;
}
/** The one client-side bound on the allowlist (a count, not a network parser): 1 to 32 entries. */
export function adminTokenCidrError(raw: string): string | undefined {
  const entries = parseCidrAllowlist(raw);
  return entries && entries.length > 32 ? "List at most 32 addresses or ranges." : undefined;
}
/** Display-only summary of a token's immutable allowlist: it's set at creation and never edited. */
export function adminTokenAllowlistSummary(ipAllowlist: string[] | null): string {
  return ipAllowlist && ipAllowlist.length ? `${ipAllowlist.length} address${ipAllowlist.length === 1 ? "" : "es"}` : "Any";
}

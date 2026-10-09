/*
 * Display labels for people, records and audit pages. Display-only: Rust owns
 * authorization; nothing here grants or infers access.
 *
 * Provenance: label style adapted from Grounded web/src/pages/admin/people/common.tsx
 * and web/src/components/audit/labels.ts (read-only reference), rewritten for
 * OMG's platform roles, sibling Team/Project workspaces and audit action codes.
 */
import type { PlatformRole, PlatformUser, RoleGrant, Role, Workspace, WorkspaceKind } from "./api";
import type { Breakdown, Totals } from "./governance";

/** GET /platform/users rows (display-only fields; absent from older gateways). */
export type DirectoryUser = PlatformUser & { last_sign_in_at?: string | null; shared_workspace_count?: number };
/** One Team/Project grant group on GET /platform/users/{id}. Never personal. */
export type SharedMembership = { workspace_id: string; name: string; kind: Exclude<WorkspaceKind, "personal">; disabled_at?: string | null; role: Role; sources: RoleGrant["source"][]; grants?: { role: Role; source: RoleGrant["source"] }[] };
export type UserRecord = DirectoryUser & { first_sign_in_at?: string | null; shared_memberships?: SharedMembership[] };
/** GET /platform/workspaces rows: shared workspaces with display-only counts and the allocation by name. */
export type DirectoryWorkspace = Workspace & { member_count?: number; cost_center?: { id: string; name: string; code: string } | null };

const cap = (value: string) => value ? `${value[0].toUpperCase()}${value.slice(1)}` : value;
export const platformRoleLabels: Record<PlatformRole, string> = { admin: "Platform admin", auditor: "Platform auditor", user: "Platform user" };
export const workspaceRoleLabels: Record<Role, string> = { owner: "Owner", admin: "Admin", member: "Member" };
export const kindLabels: Record<WorkspaceKind, string> = { personal: "Personal", team: "Team", project: "Project" };
/** "Admin · group", "Owner · manual": a grant's role and where it came from. */
export const grantLabel = (grant: { role: string; source: string }) => `${cap(grant.role)} · ${grant.source}`;
export const activeGrants = <T extends { revoked_at?: string | null }>(grants?: T[] | null) => grants?.filter(g => !g.revoked_at) ?? [];

export type UserState = "active" | "suspended" | "cleaned";
export function userState(user: Pick<PlatformUser, "disabled_at" | "cleaned_at">): UserState { return user.cleaned_at ? "cleaned" : user.disabled_at ? "suspended" : "active"; }
const roleRank: PlatformRole[] = ["admin", "auditor", "user"];
/**
 * The one role shown for a person: the effective role, or for a suspended/cleaned
 * account the highest retained grant (inactive). Suspension itself is shown by Status.
 */
export function displayRole(user: Pick<PlatformUser, "platform_role" | "role_grants" | "disabled_at" | "cleaned_at">): { role: PlatformRole | null; inactive: boolean } {
  const granted = new Set<string>(activeGrants(user.role_grants).map(g => g.role));
  return { role: user.platform_role ?? roleRank.find(r => granted.has(r)) ?? null, inactive: !!user.disabled_at || !!user.cleaned_at };
}

/** Manual and bootstrap platform grants are removable by a Platform Admin; group grants follow SSO. */
export const removableGrant = (grant: { source: string; revoked_at?: string | null }) => !grant.revoked_at && (grant.source === "manual" || grant.source === "bootstrap");
export const groupGrantHint = "From SSO group mapping; changes at next sign-in";
export const removeGrantLabel = (grant: { role: string; source: string }) => `Remove ${cap(grant.role)} ${grant.source} grant`;
/**
 * Confirmation copy for DELETE /platform/users/{id}/roles/{role}, which revokes the
 * role's manual and bootstrap grants. The server enforces the last-admin and
 * last-owner safeguards; this only warns ahead of time.
 */
export function grantRemoval(user: Pick<PlatformUser, "email" | "role_grants" | "disabled_at">, grant: { role: string; source: string }) {
  const name = user.email ?? "this user", others = activeGrants(user.role_grants).filter(g => !(g.role === grant.role && removableGrant(g)));
  const parts = ["Group grants stay untouched."];
  if (!others.length) parts.push(user.disabled_at ? `This is ${name}'s last grant; reactivation will need a new grant.` : `This is ${name}'s last grant: losing every role suspends the account and revokes their sessions and user-owned keys. Revoked keys never come back.`);
  else parts.push("Losing every role revokes their sessions and their own keys.");
  if (grant.role === "admin" && !others.some(g => g.role === "admin")) parts.push("The last Platform Admin is protected and can't be removed.");
  return { title: `Remove ${cap(grant.role)} ${grant.source} grant from ${name}?`, description: parts.join(" "), submitLabel: "Remove grant" };
}

/** Audit actions recorded when a person signs in (hidden by "Hide sign-ins"). Mirrors the gateway's list. */
export const signInAuditActions = ["identity.groups_synchronized", "identity.rebound"];

/** The UTC report window for "last 30 days": 29 full days plus today (end exclusive). */
export function last30Days(now = new Date()) {
  const day = (offset: number) => new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + offset)).toISOString().slice(0, 10);
  return { start_date: day(-29), end_date: day(1) };
}
const sumTotals = (rows: Totals[]): Totals => {
  const add = (key: keyof Totals) => rows.reduce((sum, t) => sum + BigInt(t[key]), 0n).toString();
  return { known_cost_microusd: add("known_cost_microusd"), held_microusd: add("held_microusd"), attempts: add("attempts"), root_requests: add("root_requests"), unresolved_attempts: add("unresolved_attempts") };
};
/**
 * Splits a per-user workspace breakdown into named Team/Project rows and one
 * private personal total. A workspace is named only when it is in the shared
 * (Team/Project) directory; anything else is personal and contributes to the
 * aggregate only, never by name or identifier.
 */
export function splitUsage(rows: Breakdown[], shared: Map<string, { name: string; kind: WorkspaceKind }>) {
  const named: { id: string; name: string; kind: WorkspaceKind; totals: Totals }[] = [], personal: Totals[] = [];
  for (const row of rows) { const w = row.id ? shared.get(row.id) : undefined; if (row.id && w && w.kind !== "personal") named.push({ id: row.id, name: w.name, kind: w.kind, totals: row.totals }); else personal.push(row.totals); }
  return { shared: named, personal: personal.length ? sumTotals(personal) : null };
}

/** Audit actions emitted by the gateway; unknown codes fall back to the code itself. */
export const auditActionLabels: Record<string, string> = {
  "catalog.created": "Created catalog", "catalog.updated": "Changed catalog", "catalog.deleted": "Deleted catalog", "catalog.models_replaced": "Changed catalog models",
  "catalog.override_replaced": "Replaced workspace catalogs", "catalog.override_reset": "Reset workspace catalogs", "catalog.type_defaults_replaced": "Changed default catalogs",
  "configuration.updated": "Changed configuration",
  "cost_center.created": "Created cost center", "cost_center.updated": "Changed cost center", "cost_center.archived": "Archived cost center",
  "deployment.created": "Added route",
  "identity.groups_synchronized": "Synchronized SSO groups", "identity.rebound": "Re-linked sign-in identity",
  "installation.bootstrap_demo": "Seeded the demo", "installation.bootstrap_dev": "Set up the development install",
  "invitation.created": "Created invitation", "invitation.accepted": "Accepted invitation", "invitation.revoked": "Revoked invitation",
  "key.created": "Created API key", "key.revoked": "Revoked API key", "key.rotated": "Rotated API key", "key.enabled": "Enabled API key", "key.disabled": "Disabled API key",
  "mapping.created": "Added SSO group mapping", "mapping.updated": "Changed SSO group mapping", "mapping.deleted": "Deleted SSO group mapping",
  "model.created": "Added model", "model.updated": "Changed model", "model.granted": "Assigned model", "model.grant_revoked": "Revoked model assignment",
  "policy.installation_updated": "Changed installation limits", "policy.type_updated": "Changed default limits", "policy.override_updated": "Changed platform limits override",
  "policy.override_reset": "Reset platform limits override", "policy.local_updated": "Changed workspace limits", "policy.key_updated": "Changed key limits",
  "price.created": "Published price",
  "provider.created": "Added connection", "provider.updated": "Changed connection",
  "role.granted": "Granted platform role", "role.revoked": "Revoked platform role",
  "routing.deployment_updated": "Changed route settings", "routing.model_updated": "Changed model routing",
  "scim.user.created": "Provisioned user (SCIM)", "scim.user.updated": "Changed user (SCIM)", "scim.user.deactivated": "Deactivated user (SCIM)", "scim.user.reactivated": "Reactivated user (SCIM)",
  "scim.group.created": "Added group (SCIM)", "scim.group.updated": "Changed group (SCIM)", "scim.group.deleted": "Deleted group (SCIM)",
  "service_account.created": "Created service account", "service_account.updated": "Changed service account",
  "usage.reconciled": "Reconciled usage",
  "user.provisioned": "Provisioned user", "user.updated": "Changed user", "user.bootstrap_role": "Granted first admin role", "user.cleaned": "Cleaned up departed user",
  "workspace.created": "Created workspace", "workspace.updated": "Changed workspace", "workspace.manual_membership_set": "Set manual membership", "workspace.manual_membership_revoked": "Removed manual membership",
};
export const auditActionLabel = (code: string) => auditActionLabels[code] ?? code;
/**
 * The same grant codes record a workspace admin's catalog selection and a Platform Admin's direct assignment;
 * the event's `source` tells them apart. Unknown or missing sources keep the generic label.
 */
export function auditEventLabel(event: { action: string; metadata?: Record<string, unknown> }): string {
  if (event.metadata?.source === "catalog") {
    if (event.action === "model.granted") return "Added model";
    if (event.action === "model.grant_revoked") return "Removed model";
  }
  return auditActionLabel(event.action);
}
export const resourceTypeLabels: Record<string, string> = {
  user: "User", workspace: "Workspace", key: "API key", model: "Model", catalog: "Catalog", cost_center: "Cost center", group_mapping: "SSO group mapping",
  installation: "Installation", workspace_type: "Workspace type", provider: "Connection", deployment: "Route", price: "Price", service_account: "Service account",
  invitation: "Invitation", execution: "Request", scim_group: "SCIM group",
};
export const resourceTypeLabel = (type?: string) => type ? resourceTypeLabels[type] ?? cap(type.replaceAll("_", " ")) : "Unknown";

/** Cost-report accounting-health codes in sentence case. */
export const healthLabels: Record<string, string> = {
  pending_attempts: "Pending attempts", unknown_attempts: "Unknown-cost attempts", missing_reservation_attempts: "Attempts missing a reservation",
  unpriced_attempts: "Unpriced attempts", aged_hold_attempts: "Attempts with aged holds", unbounded_attempts: "Unbounded-cost attempts",
};
export const healthLabel = (key: string) => healthLabels[key] ?? cap(key.replaceAll("_", " "));

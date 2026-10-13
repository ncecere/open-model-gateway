/*
 * Audit rows carry the names the dashboard shows (actor, workspace, target),
 * resolved by the gateway under the caller's visibility (R06, #67), so audit
 * pages never load every user, member or workspace to label one page.
 */
import type { Audit, Session, WorkspaceKind } from "./api";
import type { DashboardSearch } from "./permissions";
import { resourceTypeLabel } from "./people";

export type AuditTarget = { name: string; search?: DashboardSearch; /** The name already states the resource type ("API key in Product"). */ typed?: boolean };
type Scope = { platform: true } | { platform: false; ws: string };

/** Where a named target links to, in this scope (unnamed targets aren't linked). */
function targetSearch(e: Audit, scope: Scope): DashboardSearch | undefined {
  const id = e.resource_id ?? undefined;
  if (!id) return undefined;
  if (scope.platform) switch (e.resource_type) {
    case "user": return { page: "user-detail", record: id };
    case "workspace": return e.target_kind === "team" || e.target_kind === "project" ? { page: "workspace-detail", record: id, kind: e.target_kind } : undefined;
    case "cost_center": return { page: "cost-centers" };
    case "group_mapping": return { page: "oidc" };
    case "model": return { page: "model-detail", record: id };
    case "catalog": return { page: "catalog-detail", record: id };
    case "provider": return { page: "provider-detail", record: id };
    default: return undefined;
  }
  switch (e.resource_type) {
    case "key": return { page: "key-detail", ws: scope.ws, record: id };
    case "model": return { page: "grants", ws: scope.ws };
    case "service_account": return { page: "workspace-settings", ws: scope.ws, tab: "service-accounts" };
    case "workspace": return { page: "workspace-settings", ws: scope.ws };
    default: return undefined;
  }
}

/** The event's Team or Project (platform audit), linked. */
export function auditWorkspace(e: Audit): AuditTarget | undefined {
  if (!e.workspace_id || !e.workspace_name || (e.workspace_kind !== "team" && e.workspace_kind !== "project")) return undefined;
  return { name: e.workspace_name, search: { page: "workspace-detail", record: e.workspace_id, kind: e.workspace_kind } };
}

/**
 * The target's name and link. A platform reader never sees a workspace's keys
 * or service accounts, so those are named by their Team or Project instead
 * ("API key in Product").
 */
export function auditTarget(e: Audit, scope: Scope): AuditTarget | undefined {
  if (e.target_name) return { name: e.target_name, search: targetSearch(e, scope) };
  const w = scope.platform ? auditWorkspace(e) : undefined;
  return w && e.resource_type && e.resource_type !== "workspace" ? { name: `${resourceTypeLabel(e.resource_type)} in ${w.name}`, search: w.search, typed: true } : undefined;
}

/** Who did it: you, a named person (linked for platform readers), or the scope's fallback. */
export function auditActor(e: Audit, scope: Scope, session?: Session): AuditTarget | undefined {
  if (!e.actor_user_id) return undefined;
  if (session && e.actor_user_id === session.user.id) return { name: scope.platform ? session.user.email : "You" };
  if (!e.actor_name) return undefined;
  return { name: e.actor_name, search: scope.platform ? { page: "user-detail", record: e.actor_user_id } : undefined };
}
export type { WorkspaceKind };

import type { Workspace } from "./api";
import { kindLabels, workspaceRoleLabels } from "./people";
/**
 * The switcher/Home subtitle: kind and the caller's role only ("Team · Owner"), never the
 * membership source (that belongs in Settings › Members). Personal workspaces are "Private".
 * The Workspace portal lists memberships only, so a shared workspace without a role is never
 * presented as a membership.
 */
export function authorityLabel(workspace: Workspace) {
  if (workspace.kind === "personal") return "Private";
  return workspace.role ? `${kindLabels[workspace.kind]} · ${workspaceRoleLabels[workspace.role]}` : kindLabels[workspace.kind];
}
export function ownKeyHelp(workspace: Workspace) {
  return workspace.kind === "personal" ? "Only the owner can issue keys in a private personal workspace." : "You aren't a member, so you can only create keys for service accounts.";
}
/** Optional capabilities newer gateways advertise; older ones omit them. */
type Extra = { rename?: boolean; manage_settings?: boolean; view_members?: boolean };
/**
 * Rename is never inferred for Personal (its name is fixed). Shared workspaces use the explicit
 * `rename`/`manage_settings` capability when the gateway serves one, else `manage_policy`.
 */
export function canRenameWorkspace(workspace: Workspace) {
  if (workspace.kind === "personal") return false;
  const extra = workspace.capabilities as Workspace["capabilities"] & Extra;
  return extra.rename ?? extra.manage_settings ?? workspace.capabilities.manage_policy;
}

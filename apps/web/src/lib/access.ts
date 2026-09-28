import type { Organization, Workspace } from "./api";

type Access = Pick<Organization | Workspace, "role" | "membership_role" | "authority_source">;
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);

export function authorityLabel(scope: Access): string {
  if (scope.authority_source === "personal") return "Personal owner · private";
  if (scope.authority_source === "platform" || scope.role === "operator") return "Platform administration · inherited";
  if (scope.authority_source === "organization") return "Organization administration · inherited";
  return `${title(scope.role)} · ${scope.authority_source === "direct" ? "direct membership" : "effective access"}`;
}
export function membershipLabel(scope: Access): string {
  if (scope.membership_role === undefined) return "Membership not reported";
  return scope.membership_role === null ? "No direct membership" : title(scope.membership_role);
}
export function ownKeyHelp(workspace: Workspace): string {
  return workspace.own_key_denial_reason === "organization_membership_required"
    ? "A human key requires active organization membership. Platform administration alone does not authorize one."
    : "A human key requires direct active membership in this shared workspace, not just inherited administration. Ask an authorized administrator to manage membership, or use an eligible service account.";
}

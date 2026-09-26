import type { Organization, Session, Workspace } from "./api";
import { navigation, organizationLanding, scopeSearch } from "./navigation";
import { canView, type DashboardSearch } from "./permissions";

export type JumpTarget = { id: string; label: string; group: string; hint?: string; keywords: string[]; search: DashboardSearch };

// Search only the caller's session/context; never fetch private activity, keys,
// or another user's workspaces. Rust still authorizes every destination.
export function jumpTargets(session: Session, organization?: Organization, workspace?: Workspace): JumpTarget[] {
  const pages = navigation.filter(item => canView(item.page, session, organization, workspace) && !(session.user.platform_admin && !organization && (item.page === "teams" || item.page === "projects")));
  const targets: JumpTarget[] = pages.map(item => ({
    id: `page:${item.page}`, label: item.page === "members" ? workspace?.kind === "project" ? "Project members" : "Team members" : item.label, group: "Pages", hint: item.group,
    keywords: [item.group, item.page], search: scopeSearch(item.page, organization?.id, workspace?.id),
  }));
  targets.push({ id: "page:profile", label: "Your profile", group: "Pages", keywords: ["account", "memberships"], search: scopeSearch("profile", organization?.id, workspace?.id) });
  targets.push({ id: "page:accept-invitation", label: "Accept invitation", group: "Pages", keywords: ["join", "organization"], search: scopeSearch("accept-invitation", organization?.id) });
  for (const org of session.organizations) targets.push({
    id: `org:${org.id}`, label: org.name, group: "Organizations", hint: "Organization",
    keywords: [org.slug], search: scopeSearch(canView("teams", session, org) ? organizationLanding(session, org) : "overview", org.id),
  });
  for (const ws of session.workspaces) {
    const org = session.organizations.find(item => item.id === ws.organization_id);
    if (!org) continue;
    targets.push({
      id: `workspace:${ws.id}`, label: ws.name, group: ws.kind === "personal" ? "Personal · private" : ws.kind === "project" ? "Projects" : "Teams", hint: org.name,
      keywords: [org.name, org.slug, ws.kind], search: scopeSearch("overview", ws.organization_id, ws.id),
    });
  }
  return targets;
}

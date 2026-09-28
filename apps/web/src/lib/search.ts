import type { Organization, Session, Workspace } from "./api";
import { navigation, scopeSearch } from "./navigation";
import { canView, type DashboardSearch } from "./permissions";

export type JumpTarget = { id: string; label: string; group: string; hint?: string; keywords: string[]; search: DashboardSearch };

// Only /me's current-user resources are searched. This never queries a user
// directory or uses operator access to enumerate someone else's personal scope.
export function jumpTargets(session: Session, organization?: Organization, workspace?: Workspace): JumpTarget[] {
  const org = session.organizations.find(item => item.id === organization?.id);
  const ws = session.workspaces.find(item => item.id === workspace?.id && item.organization_id === org?.id);
  const pages = navigation.filter(item => canView(item.page, session, org, ws));
  const targets: JumpTarget[] = pages.map(item => ({
    id: `page:${item.page}`, label: item.page === "platform-overview" ? "Platform overview" : item.label, group: "Pages", hint: item.group,
    keywords: [item.group, item.page], search: scopeSearch(item.page, org?.id, ws?.id),
  }));
  if (session.user.platform_admin) for (const [page, label] of [["routing", "Routing"], ["pricing", "Pricing"], ["model-access", "Model access"]] as const) targets.push({ id: `page:${page}`, label, group: "Pages", hint: "Platform", keywords: [page], search: scopeSearch(page) });
  targets.push({ id: "page:profile", label: "Your profile", group: "Pages", keywords: ["account", "memberships"], search: scopeSearch("profile") });
  targets.push({ id: "page:accept-invitation", label: "Accept invitation", group: "Pages", keywords: ["join", "organization"], search: scopeSearch("accept-invitation") });
  for (const org of session.organizations) targets.push({
    id: `org:${org.id}`, label: org.name, group: "Organizations", hint: "Organization",
    keywords: [org.slug], search: scopeSearch(session.user.platform_admin ? "organization-detail" : canView("organization-settings", session, org) ? "organization-settings" : "overview", org.id),
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

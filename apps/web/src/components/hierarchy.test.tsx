import { afterEach, describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import type { Organization, Session, Workspace } from "../lib/api";
import { ActionProvider, CollectionTable } from "./ui";
import { Organizations, PlatformTeams, PlatformUsers, Teams } from "../pages/hierarchy";
import { OrganizationMembers } from "../pages/organization";
import { adminGroups, adminLanding, contextOptions, navigation, pageScope, scopeSearch } from "../lib/navigation";
import { canAdminister, canView, dashboardSearch } from "../lib/permissions";

const org: Organization = { id: "org", name: "College of Business", slug: "business", role: "admin" };
const team: Workspace = { id: "team", organization_id: "org", name: "IT", kind: "team", role: "admin" };
const personal: Workspace = { ...team, id: "private", name: "My personal", kind: "personal", role: "owner" };
const session: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [personal, team] };
const operator: Session = { ...session, user: { ...session.user, platform_admin: true } };
const go = () => {};
const clients: QueryClient[] = [];
function render(node: ReactNode, entries: [string, unknown][] = []) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } }); clients.push(client);
  for (const [path, data] of entries) client.setQueryData(["api", path], data);
  return renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
afterEach(() => { clients.forEach(client => client.clear()); clients.length = 0; });
describe("hierarchical administration", () => {
  it("has explicit directory pages and separates platform, organization and team scopes", () => {
    expect(navigation.find(item => item.page === "organizations")?.group).toBe("Platform");
    expect(navigation.find(item => item.page === "users")?.group).toBe("Platform");
    expect(navigation.filter(item => item.group === "Platform").map(item => item.label)).toEqual(["Organizations", "Teams", "Projects", "Users"]);
    expect(adminGroups("teams", true)).not.toContain("Organization");
    expect(adminGroups("assigned-models", true)).not.toContain("Organization");
    expect(pageScope("platform-teams")).toBe("platform");
    expect(navigation.find(item => item.page === "teams")?.group).toBe("Organization");
    expect(navigation.find(item => item.page === "members")?.group).toBe("Shared workspace");
    expect(pageScope("members")).toBe("workspace");
  });
  it("keeps the context selector strictly to existing resources", () => {
    expect(contextOptions(operator, org)).toEqual([
      { label: "Platform", items: [{ label: "Platform", page: "organizations" }] },
      { label: "Organizations", items: [{ label: org.name, org: "org", ws: undefined }] },
      { label: "Personal · private", items: [{ label: personal.name, org: "org", ws: "private" }] },
      { label: "Teams", items: [{ label: "IT", org: "org", ws: "team" }] },
    ]);
    expect(contextOptions({ ...operator, organizations: [], workspaces: [] })).toEqual([{ label: "Platform", items: [{ label: "Platform", page: "organizations" }] }]);
  });
  it("does not include another organization’s teams in the selector", () => {
    const result = contextOptions({ ...session, workspaces: [...session.workspaces, { ...team, id: "foreign", organization_id: "other" }] }, org);
    expect(JSON.stringify(result)).not.toContain("foreign");
  });
  it("does not imply an organization context on platform-wide pages", () => {
    expect(adminGroups("users", true)).toEqual(["Platform", "Models", "Oversight"]);
    expect(adminGroups("organizations", true)).toEqual(["Platform", "Models", "Oversight"]);
    expect(adminGroups("teams")).toContain("Organization");
    expect(contextOptions(operator).map(group => group.label)).toEqual(["Platform", "Organizations"]);
  });
  it("clears lower-level context when navigating upward", () => {
    expect(scopeSearch("organizations", "org", "private")).toEqual({ page: "organizations", org: undefined, ws: undefined });
    expect(scopeSearch("users", "org", "private").org).toBeUndefined();
    expect(scopeSearch("teams", "org", "private")).toEqual({ page: "teams", org: "org", ws: undefined });
    expect(scopeSearch("members", "org", "team").ws).toBe("team");
    for (const page of ["organizations", "users", "teams"] as const) expect(dashboardSearch({ page, token: "secret" }).page).toBe(page);
  });
  it("allows an operator with no organizations to reach organization creation", () => {
    const empty = { ...operator, organizations: [], workspaces: [] };
    expect(canAdminister(empty)).toBe(true); expect(canView("organizations", empty)).toBe(true);
    expect(adminLanding(empty)).toBe("organizations");
    expect(render(<Organizations session={empty} go={go} />)).toContain("Create organization</button>");
  });
  it("offers organization creation only to operators", () => {
    expect(render(<Organizations session={session} go={go} />)).not.toContain("Create organization</button>");
    expect(canView("users", session, org, team)).toBe(false); expect(canView("users", operator)).toBe(true);
  });
  it("allows team administrators into their team directory, not platform users or organization management", () => {
    const memberOrg: Organization = { ...org, role: "member" };
    const manager: Session = { ...session, organizations: [memberOrg] };
    expect(canAdminister(manager)).toBe(true); expect(adminLanding(manager, memberOrg)).toBe("teams");
    expect(canView("teams", manager, memberOrg)).toBe(true); expect(canView("organization-members", manager, memberOrg)).toBe(false);
    const html = render(<Teams session={manager} organization={memberOrg} go={go} />, [["/api/v1/orgs/org/teams?limit=100&offset=0", { data: [team] }]]);
    expect(html).toContain("Members</button>"); expect(html).toContain("Rename</button>"); expect(html).not.toContain("Create team</button>");
  });
  it("personal ownership never grants access to administrative directories", () => {
    const memberOrg: Organization = { ...org, role: "member" };
    const member: Session = { ...session, organizations: [memberOrg], workspaces: [personal, { ...team, role: "member" }] };
    expect(canAdminister(member)).toBe(false); expect(canView("organizations", member)).toBe(false); expect(canView("teams", member, memberOrg)).toBe(false);
  });
  it("offers team creation to active org admins but not unjoined operators", () => {
    expect(render(<Teams session={session} organization={org} go={go} />)).toContain("Create team</button>");
    expect(render(<Teams session={operator} organization={{ ...org, role: "operator", membership_role: null }} go={go} />)).not.toContain("Create team</button>");
  });
  it("labels the global user directory read-only and sends membership management to organizations", () => {
    const html = render(<PlatformUsers go={go} />, [["/api/v1/platform/users?limit=100&offset=0", { data: [{ id: "me", email: "person@example.invalid", platform_admin: false, disabled_at: null, created_at: "2026-01-01T00:00:00Z" }] }]]);
    expect(html).toContain("person@example.invalid"); expect(html).toContain("read-only directory"); expect(html).toContain("Manage memberships by organization"); expect(html).not.toContain("Create user</button>");
  });
  it("offers a platform teams directory with explicit organization filtering", () => {
    const html = render(<PlatformTeams session={operator} go={go} />, [["/api/v1/platform/teams?limit=100&offset=0", { data: [{ ...team, organization_name: org.name }] }]]);
    expect(html).toContain("All organizations"); expect(html).toContain("College of Business"); expect(html).toContain("Personal workspaces are never included");
    expect(canView("platform-teams", operator)).toBe(true);
    expect(canView("platform-teams", session, org)).toBe(false);
    expect(scopeSearch("platform-teams", "org", "private")).toEqual({ page: "platform-teams", org: undefined, ws: undefined });
  });
  it("preserves organization filters when paginating a collection", () => {
    const html = render(<CollectionTable<{ id: string }> path="/api/v1/platform/teams?organization_id=org" label="Filtered teams" empty="Empty" rowKey={row => row.id} columns={[{ title: "ID", render: row => row.id }]} />, [["/api/v1/platform/teams?organization_id=org&limit=100&offset=0", { data: [{ id: "filtered-team" }] }]]);
    expect(html).toContain("filtered-team");
  });
  it("places invitation creation entry on the organization members page", () => {
    const html = render(<OrganizationMembers organization={org} onInvite={go} />);
    expect(html).toContain("Invite member</button>"); expect(html).toContain("College of Business");
  });
});

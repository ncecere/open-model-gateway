import { afterEach, describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import type { Organization, Session, Workspace } from "../lib/api";
import { authorityLabel, membershipLabel } from "../lib/access";
import { OrganizationDetail, WorkspaceSettings } from "../pages/resource-details";
import { PlatformOverview } from "../pages/start";
import { Keys } from "../pages/workspace";
import { ResourcePage } from "./resource-page";
import { ActionProvider, Heading } from "./ui";

const org: Organization = { id: "org", name: "Organization", slug: "org", role: "admin", membership_role: "admin", authority_source: "direct" };
const ws: Workspace = { id: "ws", name: "Shared team", organization_id: "org", kind: "team", role: "admin", membership_role: null, authority_source: "organization", own_key_denial_reason: "workspace_membership_required", capabilities: { issue_own_key: false, manage_members: true, manage_owners: false, manage_service_accounts: true, delegate_models: true, manage_policy: true, view_all_activity: true } };
const session: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [ws] };
const clients: QueryClient[] = [];
const noop = () => {};
function render(node: ReactNode, entries: [string, unknown][] = []) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } });
  clients.push(client);
  for (const [path, value] of entries) client.setQueryData(["api", path], value);
  return renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
afterEach(() => { clients.forEach(client => client.clear()); clients.length = 0; });

describe("resource composition", () => {
  it("renders only the active panel and nests headings correctly", () => {
    const html = render(<ResourcePage title="Resource" tab="second" onTabChange={noop} tabs={[
      { value: "first", label: "First", content: <Heading title="Inactive private section" /> },
      { value: "second", label: "Second", content: <Heading title="Active section" /> },
    ]} />);
    expect(html.match(/<h1\b/g)).toHaveLength(1);
    expect(html).toMatch(/<h2[^>]*>Active section/);
    expect(html).not.toContain("Inactive private section");
    expect(html).toContain('aria-label="Resource sections"');
  });
  it("falls back to an available tab without mounting a forbidden panel", () => {
    const html = render(<ResourcePage title="Resource" tab="forbidden" onTabChange={noop} tabs={[{ value: "overview", label: "Overview", content: "Safe overview" }]} />);
    expect(html).toContain("Safe overview");
    expect(html).not.toContain('value="forbidden"');
  });
  it("keeps organization administration contextual and excludes private resources", () => {
    const withPersonal = { ...session, workspaces: [...session.workspaces, { ...ws, id: "private", name: "Secret personal name", kind: "personal" as const }] };
    const html = render(<OrganizationDetail session={withPersonal} organization={org} go={noop} onTabChange={noop} />);
    expect(html).toContain("Organization settings");
    expect(html).toContain("Visible shared workspaces</dt><dd>1");
    expect(html).not.toContain("Secret personal name");
    expect(html).toContain("create a project");
    expect(html).toContain("/organizations/org/settings");
  });
  it("does not grant organization or platform access through workspace ownership", () => {
    expect(render(<OrganizationDetail session={session} organization={{ ...org, role: "member" }} go={noop} onTabChange={noop} />)).toContain("Access not available");
    expect(render(<OrganizationDetail session={session} organization={org} platform go={noop} onTabChange={noop} />)).toContain("Access not available");
    expect(render(<PlatformOverview session={session} />)).toContain("Access not available");
  });
  it("never offers shared membership or service accounts for a personal workspace", () => {
    const personal = { ...ws, kind: "personal" as const, role: "owner" as const, authority_source: "personal" as const, membership_role: "owner" as const, capabilities: { ...ws.capabilities!, manage_members: false, manage_service_accounts: false, manage_owners: false } };
    const html = render(<WorkspaceSettings session={session} organization={org} workspace={personal} onTabChange={noop} />);
    expect(html).not.toContain('>Members</');
    expect(html).not.toContain('>Service accounts</');
    expect(html).toContain("Only you can access this personal workspace");
    expect(html).toContain("Personal owner · private");
  });
  it("separates inherited authority from membership in workspace settings", () => {
    const html = render(<WorkspaceSettings session={session} organization={org} workspace={ws} onTabChange={noop} />);
    expect(html).toContain("No direct membership");
    expect(html).toContain("Organization administration · inherited");
    expect(html).not.toContain("Add existing member"); // inactive panel is unmounted
  });
  it("preserves members' limits view without exposing management panels", () => {
    const member = { ...ws, role: "member" as const, membership_role: "member" as const, authority_source: "direct" as const, capabilities: { ...ws.capabilities!, manage_members: false, manage_service_accounts: false, manage_policy: false, manage_owners: false, issue_own_key: true, view_all_activity: false, delegate_models: false } };
    const html = render(<WorkspaceSettings session={session} organization={{ ...org, role: "member" }} workspace={member} tab="limits" onTabChange={noop} />);
    expect(html).toContain("Loading policy");
    expect(html).not.toContain('>Members</');
    expect(html).not.toContain('>Service accounts</');
  });
  it("uses server issuance capabilities and never offers a human key to an inherited nonmember", () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } });
    clients.push(client);
    client.setQueryData(["api", "/api/v1/workspaces/ws/grants", "choices"], []);
    client.setQueryData(["api", "/api/v1/workspaces/ws/service-accounts", "choices"], []);
    const html = renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider><Keys session={session} organization={org} workspace={ws} /></ActionProvider></QueryClientProvider>);
    expect(html.match(/<button[^>]*>Create key<\/button>/)?.[0]).toContain("disabled");
    expect(html).toContain("direct active membership");
  });
  it("does not fabricate successful setup checks while loading", () => {
    const html = render(<PlatformOverview session={{ ...session, user: { ...session.user, platform_admin: true } }} />);
    expect(html).toContain("Checking configuration");
    expect(html).not.toContain(">Configured</");
    expect(html).toContain("never sends provider probes or paid inference");
  });
});

describe("authority labels", () => {
  it("does not present an effective owner as a direct owner", () => {
    expect(authorityLabel({ role: "owner", authority_source: "platform" })).toBe("Platform administration · inherited");
    expect(authorityLabel({ role: "owner", authority_source: "organization" })).toBe("Organization administration · inherited");
    expect(membershipLabel({ role: "owner", membership_role: null })).toBe("No direct membership");
    expect(membershipLabel({ role: "owner" })).toBe("Membership not reported");
  });
});

import { orgPath, platformPath, wsPath, type Organization, type Session } from "../lib/api";
import { authorityLabel, membershipLabel } from "../lib/access";
import { permissions } from "../lib/permissions";
import { ResourcePage, type ResourceTab } from "../components/resource-page";
import { ResourceLink } from "../components/navigation-link";
import { Badge, Heading, Panel } from "../components/ui";
import { Teams, type NavigateScope } from "./hierarchy";
import { AuditHistory, Invitations, OrganizationMembers } from "./organization";
import { AssignedModels, PlatformAssignment } from "./model-access";
import { Governance, PolicyPanel } from "./governance";
import { ServiceAccounts, WorkspaceMembers, type Scope } from "./workspace";

export function OrganizationDetail({ session, organization, tab, onTabChange, go, platform = false }: {
  session: Session; organization: Organization; tab?: string; onTabChange: (tab: string) => void; go: NavigateScope; platform?: boolean;
}) {
  const p = permissions(session, organization);
  if (!p.organizationAdmin || (platform && !session.user.platform_admin)) return <Heading title="Access not available" />;
  const page = platform ? "organization-detail" : "organization-settings";
  const shared = session.workspaces.filter(ws => ws.organization_id === organization.id && ws.kind !== "personal");
  const tabs: ResourceTab[] = [
    { value: "overview", label: "Overview", content: <>
      <Panel title="Organization"><dl className="details">
        <dt>Name</dt><dd>{organization.name}</dd><dt>Slug</dt><dd><code>{organization.slug}</code></dd>
        <dt>Your membership</dt><dd>{membershipLabel(organization)}</dd><dt>Administrative access</dt><dd>{authorityLabel(organization)}</dd>
        <dt>Visible shared workspaces</dt><dd>{shared.length}</dd>
      </dl><p className="help">Organizations are consuming tenants. Teams and projects are siblings. Private personal workspaces are not listed here.</p></Panel>
      <Panel title="Set up this organization"><ol className="setup-steps">
        <li><ResourceLink search={{ page, org: organization.id, tab: "members" }}>Invite people and review membership</ResourceLink><p>People need active organization membership before joining shared workspaces.</p></li>
        <li><ResourceLink search={{ page, org: organization.id, tab: "teams" }}>Create a team</ResourceLink> or <ResourceLink search={{ page, org: organization.id, tab: "projects" }}>create a project</ResourceLink><p>Choose a shared scope, then add its members.</p></li>
        <li><ResourceLink search={{ page, org: organization.id, tab: "models" }}>Review assigned models and delegate access</ResourceLink><p>Organization assignments do not automatically grant shared workspace access.</p></li>
        <li><ResourceLink search={{ page, org: organization.id, tab: "limits" }}>Review limits</ResourceLink><p>Child limits share the parent allowance. They never reserve slices of it.</p></li>
      </ol></Panel>
    </> },
    { value: "teams", label: "Teams", content: <Teams session={session} organization={organization} go={go} /> },
    { value: "projects", label: "Projects", content: <Teams session={session} organization={organization} go={go} kind="project" /> },
    { value: "members", label: "Members", content: <OrganizationMembers organization={organization} onInvite={() => onTabChange("invitations")} /> },
    { value: "invitations", label: "Invitations", content: <Invitations session={session} organization={organization} /> },
    { value: "models", label: "Models", content: <>{platform && <PlatformAssignment organization={organization} />}<AssignedModels organization={organization} /></> },
    { value: "limits", label: "Limits", content: <>
      {platform && <PolicyPanel title="Platform organization ceiling" path={`${platformPath}/orgs/${encodeURIComponent(organization.id)}/policy`} writable platform />}
      <PolicyPanel title="Organization limits" path={`${orgPath(organization.id)}/policy`} writable={p.managePolicy} ceilingLabel="Platform maximums (cannot be overridden)" organizationPolicy />
    </> },
    { value: "audit", label: "Audit", content: <AuditHistory organization={organization} /> },
  ];
  return <ResourcePage title={organization.name} description={<>{platform ? "Platform administration" : "Organization settings"} · <Badge>{authorityLabel(organization)}</Badge></>} tabs={tabs} tab={tab} onTabChange={onTabChange} />;
}

export function WorkspaceSettings({ session, organization, workspace, tab, onTabChange }: Scope & { tab?: string; onTabChange: (tab: string) => void }) {
  const p = permissions(session, organization, workspace);
  const tabs: ResourceTab[] = [
    { value: "overview", label: "Overview", content: <Panel title="Workspace access"><dl className="details">
      <dt>Organization</dt><dd>{organization.name}</dd><dt>Kind</dt><dd>{workspace.kind === "personal" ? "Personal · private" : workspace.kind === "project" ? "Project" : "Team"}</dd>
      <dt>Your membership</dt><dd>{membershipLabel(workspace)}</dd><dt>Effective authority</dt><dd>{authorityLabel(workspace)}</dd>
    </dl><p className="help">Administrative authority and direct membership are separate. Human keys require current membership; service-account keys have their own lifecycle.</p>
      {workspace.kind === "personal" && <p className="notice">Only you can access this personal workspace. Platform and organization administration do not grant access to another person's personal workspace.</p>}
      <ResourceLink className="button secondary" search={{ page: "keys", org: organization.id, ws: workspace.id }}>Manage API keys</ResourceLink>
    </Panel> },
    ...(p.manageTeam ? [{ value: "members", label: "Members", content: <WorkspaceMembers session={session} organization={organization} workspace={workspace} /> }] : []),
    ...(p.manageServiceAccounts ? [{ value: "service-accounts", label: "Service accounts", content: <ServiceAccounts session={session} organization={organization} workspace={workspace} /> }] : []),
    { value: "limits", label: "Limits", content: <Governance session={session} organization={organization} workspace={workspace} /> },
  ];
  return <ResourcePage title={`${workspace.name} settings`} description={`${organization.name} · ${workspace.kind === "personal" ? "Private personal workspace" : `${workspace.kind} workspace`}`} actions={<ResourceLink className="button secondary" search={{ page: "overview", org: organization.id, ws: workspace.id }}>Open workspace</ResourceLink>} tabs={tabs} tab={tab} onTabChange={onTabChange} />;
}

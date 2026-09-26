import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { API, api, orgPath, wsPath, type Organization, type Session, type Workspace } from "../lib/api";
import { canView, isAdmin, permissions, type Page } from "../lib/permissions";
import { nameField, slugError } from "../lib/forms";
import { Badge, Button, CollectionTable, DateTime, ErrorNotice, FormField, Heading, Id, NativeSelect, RowActions, useAction, useChoices } from "../components/ui";

export type NavigateScope = (org?: string, ws?: string, page?: Page) => void;
type DirectoryOrganization = Organization & { created_at: string };
type PlatformUser = { id: string; email: string; platform_admin: boolean; disabled_at: string | null; created_at: string };
type SharedKind = "team" | "project";

export function Organizations({ session, go }: { session: Session; go: NavigateScope }) {
  const ask = useAction();
  const client = useQueryClient();
  const create = () => ask({
    title: "Create organization",
    description: "An organization is a consumer tenant with teams, projects and memberships. You become its owner and receive a private personal workspace.",
    fields: [nameField, { name: "slug", label: "Organization slug", required: true, maxLength: 80, validate: slugError, placeholder: "college-of-business" }],
    submitLabel: "Create organization",
    run: async values => {
      const result = await api<{ id: string }>(`${API}/orgs`, { method: "POST", body: { name: values.name, slug: values.slug } });
      await client.invalidateQueries({ queryKey: ["session"] });
      go(result.id, undefined, "teams");
      return result;
    },
  });
  return <>
    <Heading title="Organizations" description="Organizations define the consumer tenant boundary. Teams and projects are sibling shared workspaces; infrastructure is owned by the platform." actions={session.user.platform_admin && <Button onClick={create}>Create organization</Button>} />
    <CollectionTable<DirectoryOrganization> path={`${API}/orgs`} label="Organizations" empty={session.user.platform_admin ? "Create the first organization for your platform." : "No accessible organizations are available."} rowKey={org => org.id} columns={[
      { title: "Organization", render: org => <><strong>{org.name}</strong><div className="muted">{org.slug}</div></> },
      { title: "Your access", render: org => <Badge>{org.role === "operator" ? "Platform operator" : org.role}</Badge> },
      { title: "Created", render: org => <DateTime value={org.created_at} /> },
      { title: "Manage", render: org => <RowActions>
        {canView("teams", session, org) ? <><Button size="sm" variant="secondary" onClick={() => go(org.id, undefined, "teams")}>Teams</Button><Button size="sm" variant="secondary" onClick={() => go(org.id, undefined, "projects")}>Projects</Button></> : <Button size="sm" variant="secondary" onClick={() => go(org.id)}>Open workspace</Button>}
        {isAdmin(org.role) && <><Button size="sm" variant="secondary" onClick={() => go(org.id, undefined, "assigned-models")}>Assigned models</Button><Button size="sm" variant="secondary" onClick={() => go(org.id, undefined, "organization-policy")}>Local limits</Button><Button size="sm" variant="secondary" onClick={() => go(org.id, undefined, "organization-members")}>Members</Button><Button size="sm" variant="ghost" onClick={() => ask({ title: "Rename organization", fields: [{ ...nameField, value: org.name }], run: values => api(orgPath(org.id), { method: "PATCH", body: { name: values.name } }) })}>Rename</Button></>}
      </RowActions> },
    ]} />
  </>;
}

export function Teams({ session, organization, go, kind = "team" }: { session: Session; organization: Organization; go: NavigateScope; kind?: SharedKind }) {
  const ask = useAction();
  const client = useQueryClient();
  const p = permissions(session, organization);
  const singular = kind === "project" ? "Project" : "Team";
  const plural = `${singular}s`;
  const create = () => ask({
    title: `Create ${kind}`,
    description: `Create a shared ${kind} in ${organization.name}. Teams and projects are siblings. Personal workspaces remain private.`,
    fields: [nameField], submitLabel: `Create ${kind}`,
    run: async values => {
      const result = await api<{ id: string }>(`${orgPath(organization.id)}/workspaces`, { method: "POST", body: { name: values.name, kind } });
      await client.invalidateQueries({ queryKey: ["session"] });
      go(organization.id, result.id, "members");
      return result;
    },
  });
  return <>
    <Heading title={plural} description={`${organization.name} · Shared workspaces within this organization. This directory never includes personal workspaces.`} actions={p.createWorkspace && <Button onClick={create}>Create {kind}</Button>} />
    {!p.createWorkspace && <p className="help">Organization administrators with active organization membership can create {plural.toLowerCase()}. Shared workspace administrators manage their existing workspaces.</p>}
    <CollectionTable<Workspace> path={`${orgPath(organization.id)}/${kind}s`} label={plural} empty={p.createWorkspace ? `Create a ${kind}, then add organization members to it.` : `No shared ${kind}s are accessible in this organization.`} rowKey={workspace => workspace.id} columns={[
      { title: singular, render: workspace => <><strong>{workspace.name}</strong><Id value={workspace.id} /></> },
      { title: "Organization", render: () => organization.name },
      { title: "Your access", render: workspace => <Badge>{workspace.role}</Badge> },
      { title: "Manage", render: workspace => <RowActions>
        <Button size="sm" variant="secondary" onClick={() => go(organization.id, workspace.id, "overview")}>Open workspace</Button>
        {isAdmin(workspace.role) && <><Button size="sm" variant="secondary" onClick={() => go(organization.id, workspace.id, "members")}>Members</Button><Button size="sm" variant="secondary" onClick={() => go(organization.id, workspace.id, "governance")}>Limits</Button><Button size="sm" variant="ghost" onClick={() => ask({ title: `Rename ${kind}`, description: `This ${kind} belongs to ${organization.name}. Renaming does not move its memberships or data.`, fields: [{ ...nameField, value: workspace.name }], run: values => api(wsPath(workspace.id), { method: "PATCH", body: { name: values.name } }) })}>Rename</Button></>}
      </RowActions> },
    ]} />
  </>;
}

type PlatformWorkspace = Omit<Workspace, "role"> & { organization_name: string; role: "operator" };
export function PlatformTeams({ session, go, kind = "team" }: { session: Session; go: NavigateScope; kind?: SharedKind }) {
  const [filter, setFilter] = useState("");
  const organizations = useChoices<DirectoryOrganization>(`${API}/orgs`);
  const eligible = organizations.data?.filter(org => permissions(session, org).createWorkspace) ?? [];
  const ask = useAction();
  const client = useQueryClient();
  const singular = kind === "project" ? "Project" : "Team";
  const plural = `${singular}s`;
  const create = () => ask({
    title: `Create ${kind}`, description: "Choose the parent organization. Only organizations where you hold an active membership are available.",
    fields: [{ name: "organization", label: "Organization", type: "select", required: true, value: eligible.some(org => org.id === filter) ? filter : "", options: eligible.map(org => ({ value: org.id, label: org.name })) }, nameField],
    submitLabel: `Create ${kind}`, run: async values => {
      const result = await api<{ id: string }>(`${orgPath(values.organization)}/workspaces`, { method: "POST", body: { name: values.name, kind } });
      await client.invalidateQueries({ queryKey: ["session"] });
      go(values.organization, result.id, "members");
      return result;
    },
  });
  const path = `${API}/platform/${kind}s${filter ? `?organization_id=${encodeURIComponent(filter)}` : ""}`;
  return <>
    <Heading title={plural} description={`Shared ${kind}s across the platform. Teams and projects each belong directly to an organization. Personal workspaces are never included.`} actions={eligible.length > 0 && <Button onClick={create}>Create {kind}</Button>} />
    {organizations.isError ? <ErrorNotice error={organizations.error} retry={() => void organizations.refetch()} /> : <FormField label="Organization" name={`${kind}-organization-filter`}><NativeSelect id={`${kind}-organization-filter`} value={filter} onChange={event => setFilter(event.target.value)} disabled={organizations.isPending}><option value="">All organizations</option>{organizations.data?.map(org => <option key={org.id} value={org.id}>{org.name}</option>)}</NativeSelect></FormField>}
    {organizations.isSuccess && !eligible.length && <p className="help">To create a {kind}, first create an organization or obtain an active membership in its parent organization.</p>}
    <CollectionTable<PlatformWorkspace> key={path} path={path} label={plural} empty={`No shared ${kind}s are available for this organization filter.`} rowKey={workspace => workspace.id} columns={[
      { title: singular, render: workspace => <><strong>{workspace.name}</strong><Id value={workspace.id} /></> },
      { title: "Organization", render: workspace => workspace.organization_name },
      { title: "Manage", render: workspace => <RowActions>
        <Button size="sm" variant="secondary" onClick={() => go(workspace.organization_id, workspace.id, "overview")}>Open workspace</Button>
        <Button size="sm" variant="secondary" onClick={() => go(workspace.organization_id, workspace.id, "members")}>Members</Button>
        <Button size="sm" variant="secondary" onClick={() => go(workspace.organization_id, workspace.id, "governance")}>Limits</Button>
        <Button size="sm" variant="ghost" onClick={() => ask({ title: `Rename ${kind}`, description: `This ${kind} belongs to ${workspace.organization_name}.`, fields: [{ ...nameField, value: workspace.name }], run: values => api(wsPath(workspace.id), { method: "PATCH", body: { name: values.name } }) })}>Rename</Button>
      </RowActions> },
    ]} />
  </>;
}

export function PlatformUsers({ go }: { go: NavigateScope }) {
  return <>
    <Heading title="Users" description="Platform identity directory. A user exists once and receives access through organization, team and project memberships." actions={<Button variant="secondary" onClick={() => go(undefined, undefined, "organizations")}>Manage memberships by organization</Button>} />
    <p className="notice">Identities are managed through your identity provider and trusted provisioning. Add people through organization invitations; manage their roles on Members pages. This read-only directory does not expose personal workspaces, keys, or activity.</p>
    <CollectionTable<PlatformUser> path={`${API}/platform/users`} label="Platform users" empty="No user identities are available." rowKey={user => user.id} columns={[
      { title: "User", render: user => <><strong>{user.email}</strong><Id value={user.id} /></> },
      { title: "Platform access", render: user => <Badge>{user.platform_admin ? "Platform operator" : "Standard user"}</Badge> },
      { title: "Status", render: user => <Badge tone={user.disabled_at ? "neutral" : "good"}>{user.disabled_at ? "Disabled" : "Active"}</Badge> },
      { title: "Created", render: user => <DateTime value={user.created_at} /> },
    ]} />
  </>;
}

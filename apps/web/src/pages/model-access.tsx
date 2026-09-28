import { useState } from "react";
import { API, api, orgPath, platformPath, wsPath, type Grant, type Member, type Model, type Organization, type Session, type Workspace } from "../lib/api";
import { ActionProvider, Button, CollectionTable, Empty, ErrorNotice, FormField, Heading, NativeSelect, Panel, RowActions, Status, useAction, useChoices } from "../components/ui";
import { PolicyPanel } from "./governance";
import { useDashboardNavigation } from "../components/navigation-link";

export function PlatformModelAccess({ session }: { session: Session }) {
  const [selected, setSelected] = useState("");
  const navigation = useDashboardNavigation();
  const organizations = useChoices<Organization>(`${API}/orgs`, session.user.platform_admin);
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  const organization = organizations.data?.find(org => org.id === selected);
  return <><Heading title="Model access" description="Assign platform models and a platform ceiling to an organization, then delegate within that entitlement. Assignments do not automatically grant shared workspaces." />
    {organizations.isError ? <ErrorNotice error={organizations.error} retry={() => void organizations.refetch()} /> : <FormField label="Organization"><NativeSelect value={selected} disabled={organizations.isPending} onChange={event => { if (event.target.value && navigation) navigation.navigate({ page: "organization-detail", org: event.target.value, tab: "models" }); else setSelected(event.target.value); }}><option value="">Choose an organization…</option>{organizations.data?.map(org => <option key={org.id} value={org.id}>{org.name}</option>)}</NativeSelect></FormField>}
    {organization ? <ActionProvider key={organization.id}><PlatformAssignment organization={organization} /><PolicyPanel title="Platform organization ceiling" path={`${platformPath}/orgs/${encodeURIComponent(organization.id)}/policy`} writable platform /><AssignedModels organization={organization} /></ActionProvider> : <Empty title="Select an organization">Global infrastructure needs no organization context. Choose a consumer organization here to manage its entitlements and ceiling.</Empty>}
  </>;
}
export function PlatformAssignment({ organization }: { organization: Organization }) {
  const ask = useAction();
  const models = useChoices<Model>(`${platformPath}/models`);
  const assigned = useChoices<Model>(`${orgPath(organization.id)}/models`);
  const available = models.data?.filter(model => !assigned.data?.some(item => item.id === model.id)) ?? [];
  const path = `${platformPath}/orgs/${encodeURIComponent(organization.id)}/models`;
  return <Panel title={`Platform assignment · ${organization.name}`}>
    {models.isError && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}{assigned.isError && <ErrorNotice error={assigned.error} retry={() => void assigned.refetch()} />}
    <Button disabled={!models.data || !assigned.data || !available.length} onClick={() => ask({ title: "Assign platform model", description: "This is the parent entitlement. Teams, projects and individual members still require delegation. A custom alias applies only in this organization.", submitLabel: "Assign model", fields: [{ name: "model_id", label: "Global model", type: "select", required: true, options: available.map(model => ({ value: model.id, label: `${model.display_name} (${model.public_name})` })) }, { name: "public_name", label: "Organization API model alias", maxLength: 200, help: "Leave blank to use the canonical platform alias.", validate: value => /^[A-Za-z0-9/_.:-]+$/.test(value) ? undefined : "Use letters, digits, slash, hyphen, underscore, dot, or colon." }], run: values => api(`${path}/${encodeURIComponent(values.model_id)}`, { method: "PUT", body: values.public_name ? { public_name: values.public_name } : {} }) })}>Assign model</Button>
    <CollectionTable<Model> path={`${orgPath(organization.id)}/models`} label="Organization entitlements" empty="Assign a platform model to this organization." rowKey={model => model.id} columns={[
      { title: "Model", render: model => <strong>{model.display_name}</strong> }, { title: "Organization alias", render: model => <code>{model.public_name}</code> },
      { title: "Actions", render: model => <Button size="sm" variant="danger" onClick={() => ask({ title: `Revoke ${model.public_name} from ${organization.name}?`, description: "Removes the organization entitlement and all child workspace and individual grants atomically. Reassigning later will not restore those grants.", danger: true, submitLabel: "Revoke assignment", run: () => api(`${path}/${encodeURIComponent(model.id)}`, { method: "DELETE" }) })}>Revoke assignment</Button> },
    ]} />
  </Panel>;
}

export function AssignedModels({ organization }: { organization: Organization }) {
  const ask = useAction();
  const path = `${orgPath(organization.id)}/models`;
  return <><Heading title="Assigned models" description={`${organization.name} · Only platform-assigned models can be delegated. Infrastructure, routing and pricing are managed by platform operators.`} />
    <CollectionTable<Model> path={path} label="Assigned catalog" empty="Ask a platform operator to assign models to this organization." rowKey={model => model.id} columns={[
      { title: "Model", render: model => <strong>{model.display_name}</strong> }, { title: "API model name", render: model => <code>{model.public_name}</code> }, { title: "Platform status", render: model => <Status enabled={model.enabled} /> },
      { title: "Personal default", render: model => model.personal_enabled ? "Available to all organization members for personal use" : "Not enabled by default" },
      { title: "Actions", render: model => <RowActions><Button size="sm" variant="secondary" onClick={() => ask({ title: `${model.personal_enabled ? "Revoke" : "Allow"} personal access?`, description: "Changes the organization-wide personal default for existing and future workspaces without exposing owners, keys or usage. Individual grants and shared team/project grants are unchanged.", danger: model.personal_enabled, submitLabel: model.personal_enabled ? "Revoke personal access" : "Allow personal access", run: () => api(`${path}/${encodeURIComponent(model.id)}/personal-access`, { method: "PUT", body: { enabled: !model.personal_enabled } }) })}>{model.personal_enabled ? "Revoke personal access" : "Allow personal access"}</Button></RowActions> },
    ]} />
    <Delegation organization={organization} />
  </>;
}
export function recipientGrantPath(organization: string, kind: "team" | "project" | "user", recipient: string) {
  return kind === "user" ? `${orgPath(organization)}/users/${encodeURIComponent(recipient)}/grants` : `${wsPath(recipient)}/grants`;
}
function Delegation({ organization }: { organization: Organization }) {
  const [localKind, setLocalKind] = useState<"team" | "project" | "user">("team");
  const [localSelected, setLocalSelected] = useState("");
  const navigation = useDashboardNavigation();
  const kind = navigation?.search.recipientKind ?? localKind;
  const selected = navigation?.search.recipient ?? localSelected;
  const selectKind = (next: typeof kind) => { if (navigation) navigation.navigate({ ...navigation.search, recipientKind: next, recipient: undefined }); else { setLocalKind(next); setLocalSelected(""); } };
  const setSelected = (recipient: string) => navigation ? navigation.navigate({ ...navigation.search, recipient: recipient || undefined }) : setLocalSelected(recipient);
  const path = `${orgPath(organization.id)}/${kind === "user" ? "members" : `${kind}s`}`;
  const targets = useChoices<Workspace | Member>(path);
  const choices = targets.data?.filter(item => !("disabled_at" in item) || !item.disabled_at).map(item => "user_id" in item ? { id: item.user_id, label: item.email } : { id: item.id, label: item.name }) ?? [];
  const target = choices.find(item => item.id === selected);
  const grantsPath = recipientGrantPath(organization.id, kind, selected);
  return <Panel title="Delegate assigned models">
    <p className="help">Teams and projects receive shared workspace grants. Individual grants authorize only that member’s own personal use within this organization, never their team or project keys. Private workspaces are not listed.</p>
    <FormField label="Recipient kind"><NativeSelect value={kind} onChange={event => selectKind(event.target.value as typeof kind)}><option value="team">Team</option><option value="project">Project</option><option value="user">Individual member · personal use</option></NativeSelect></FormField>
    {targets.isError ? <ErrorNotice error={targets.error} retry={() => void targets.refetch()} /> : <FormField label={kind === "user" ? "Organization member" : kind === "project" ? "Project" : "Team"}><NativeSelect value={selected} disabled={targets.isPending} onChange={event => setSelected(event.target.value)}><option value="">Choose an existing recipient…</option>{choices.map(item => <option key={item.id} value={item.id}>{item.label}</option>)}</NativeSelect></FormField>}
    {selected && targets.isSuccess && !target && <p className="notice">This recipient is no longer available in the selected organization.</p>}
    {target && <ActionProvider key={`${kind}:${selected}`}><DelegatedGrants organization={organization} path={grantsPath} label={target.label} personal={kind === "user"} /></ActionProvider>}
  </Panel>;
}
export function DelegatedGrants({ organization, path, label, personal }: { organization: Organization; path: string; label: string; personal: boolean }) {
  const ask = useAction();
  const models = useChoices<Model>(`${orgPath(organization.id)}/models`);
  const grants = useChoices<Grant>(path);
  const choices = models.data?.filter(model => !grants.data?.some(grant => grant.model_id === model.id)) ?? [];
  return <>
    <h3>{label}{personal ? " · individual personal use" : " · shared workspace"}</h3>
    {models.isError && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}{grants.isError && <ErrorNotice error={grants.error} retry={() => void grants.refetch()} />}
    <Button disabled={!models.data || !grants.data || !choices.length} onClick={() => ask({ title: "Delegate assigned model", description: "Only models in this organization’s assigned catalog are eligible. This grant cannot enlarge the parent entitlement.", submitLabel: "Grant access", fields: [{ name: "model_id", label: "Assigned model", type: "select", required: true, options: choices.map(model => ({ value: model.id, label: `${model.display_name} (${model.public_name})` })) }], run: values => api(path, { method: "POST", body: { model_id: values.model_id } }) })}>Delegate model</Button>
    <CollectionTable<Grant> path={path} label="Delegated grants" empty="No models delegated to this recipient." rowKey={grant => grant.model_id} columns={[
      { title: "Model", render: grant => grant.display_name }, { title: "API model name", render: grant => <code>{grant.public_name}</code> },
      { title: "Actions", render: grant => <Button size="sm" variant="danger" onClick={() => ask({ title: `Remove ${grant.public_name}?`, description: personal ? "Removes this individual grant. Organization-wide personal access may still authorize the model." : "This workspace’s keys will no longer be authorized to use this model.", danger: true, submitLabel: "Remove grant", run: () => api(`${path}/${encodeURIComponent(grant.model_id)}`, { method: "DELETE" }) })}>Remove grant</Button> },
    ]} />
  </>;
}

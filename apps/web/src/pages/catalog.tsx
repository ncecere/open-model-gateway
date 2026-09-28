import { useEffect, useRef, useState } from "react";
import { ResourceLink } from "../components/navigation-link";
import { api, platformPath, type Session, type Organization, type Model, type Provider, type Deployment } from "../lib/api";
import { enabledField, nameField, type Field } from "../lib/forms";
import { permissions } from "../lib/permissions";
import { ActionProvider, CollectionTable, ErrorNotice, Heading, Id, RowActions, Status, Panel, useAction, useApi, useChoices, type Action } from "../components/ui";

type OrgScope = { session: Session; organization?: Organization };
function toggle(ask: (action: Action) => void, path: string, name: string, enabled: boolean) {
  ask({ title: `${enabled ? "Disable" : "Enable"} ${name}?`, description: enabled ? "Disabling this resource makes it unavailable for new inference requests that depend on it." : "Enabling permits eligible inference requests to use this resource. Provider calls may incur charges.", danger: enabled, submitLabel: enabled ? "Disable" : "Enable", run: () => api(path, { method: "PATCH", body: { enabled: !enabled } }) });
}
export const referenceField: Field = { name: "credential_variable", label: "Credential environment variable name", required: true, maxLength: 128, placeholder: "OPENAI_API_KEY", help: "Reference only — never paste a secret value. The gateway must already have this variable configured and allowlisted. Stored as env:NAME; references are never returned.", validate: (v) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(v) ? undefined : "Enter an environment variable name only, not a secret, URL, or env: prefix." };
const workloadField: Field = { name: "workload_identity", label: "Credential reference", type: "select", required: true, value: "aws:default", options: [{ value: "aws:default", label: "AWS default workload identity (aws:default)" }], help: "Uses the gateway’s configured AWS workload identity. No static secret is entered here." };
export function Models({ session }: OrgScope) {
  const ask = useAction();
  const [created, setCreated] = useState<string>();
  const path = `${platformPath}/models`;
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  return <><Heading title="Models" description="Platform-owned model catalog. Configure global deployments here, then assign models to organizations from Model access." actions={<button className="button" onClick={() => ask({ title: "Create model alias", fields: [{ name: "public_name", label: "API model name", required: true, maxLength: 200, placeholder: "company/smart", validate: (v) => /^[A-Za-z0-9/_.:-]+$/.test(v) ? undefined : "Use letters, digits, slash, hyphen, underscore, dot, or colon." }, { name: "display_name", label: "Display name", required: true, maxLength: 120 }, enabledField], submitLabel: "Create alias", successNotice: "Model alias created. Add a deployment, then configure routing and organization access.", after: result => setCreated(createdId(result)), run: (v) => api(path, { method: "POST", body: { public_name: v.public_name, display_name: v.display_name, enabled: v.enabled === "true" } }) })}>Create alias</button>} />{created !== undefined && <CreationOutcome kind="model" id={created} />}<CollectionTable<Model> searchable statusFilter path={path} label="Model aliases" empty="Create an alias to decouple client model names from upstream deployments." rowKey={(m) => m.id} columns={[
    { title: "Name", render: (m) => <><ResourceLink search={{ page: "model-detail", record: m.id }}><strong>{m.display_name}</strong></ResourceLink><Id value={m.id} /></> },
    { title: "API model name", render: (m) => <code>{m.public_name}</code> },
    { title: "Status", render: (m) => <Status enabled={m.enabled} /> },
    { title: "Actions", render: (m) => <button className="button secondary small" onClick={() => toggle(ask, `${path}/${m.id}`, m.public_name, m.enabled)}>{m.enabled ? "Disable" : "Enable"}</button> },
  ]} /></>;
}
export function Providers({ session, organization }: OrgScope) {
  const ask = useAction();
  const [created, setCreated] = useState<string>();
  const path = `${platformPath}/providers`;
  const allowed = permissions(session, organization).manageProviders;
  if (!allowed) return <Heading title="Access not available" />;
  return <><Heading title="Provider connections" description="Upstream provider configuration. Credential values and references are never returned by the gateway." actions={allowed && <button className="button" onClick={() => ask({ title: "Create provider connection", description: "Configure credentials on the gateway first. Only allowlisted secret references and provider-approved endpoints are accepted.", fields: [nameField, { name: "provider", label: "Provider", type: "select", required: true, value: "openai", options: [{ value: "openai", label: "OpenAI" }, { value: "anthropic", label: "Anthropic" }, { value: "bedrock", label: "Amazon Bedrock" }] }, { ...referenceField, visibleWhen: (v) => v.provider !== "bedrock" }, { ...workloadField, visibleWhen: (v) => v.provider === "bedrock" }, { name: "endpoint", label: "Endpoint", visibleWhen: (v) => v.provider !== "bedrock", help: "Leave blank for the provider’s public API endpoint. Custom endpoints are not supported.", maxLength: 2048, validate: (v, values) => { const expected = values.provider === "anthropic" ? "https://api.anthropic.com/v1" : "https://api.openai.com/v1"; return v === expected || v === `${expected}/` ? undefined : `Only ${expected} is supported.`; } }, { name: "region", label: "AWS region", required: true, visibleWhen: (v) => v.provider === "bedrock", maxLength: 32, placeholder: "us-east-1", validate: (v) => /^[a-z0-9-]+$/.test(v) ? undefined : "Use a valid lowercase AWS region name." },  { ...enabledField, value: "false" }], submitLabel: "Create connection", successNotice: "Provider connection created. Review its status and add a deployment. Credentials remain hidden.", after: result => setCreated(createdId(result)), run: (v) => api(path, { method: "POST", body: { name: v.name, provider: v.provider, credential_ref: v.provider === "bedrock" ? "aws:default" : `env:${v.credential_variable}`, enabled: v.enabled === "true", ...(v.endpoint && v.provider !== "bedrock" ? { endpoint: v.endpoint } : {}), ...(v.provider === "bedrock" ? { region: v.region } : {}) } }) })}>Create connection</button>} />{created !== undefined && <CreationOutcome kind="provider" id={created} />}{!allowed && <p className="notice">Read-only access. Only platform operators can create, enable, disable, or rotate provider credentials.</p>}<CollectionTable<Provider> searchable statusFilter path={path} label="Provider connections" empty={allowed ? "Create a connection using a credential reference configured by your operator." : "Ask a platform operator to configure a provider connection."} rowKey={(p) => p.id} columns={[
    { title: "Name", render: (p) => <><ResourceLink search={{ page: "provider-detail", record: p.id }}><strong>{p.name}</strong></ResourceLink><Id value={p.id} /></> },
    { title: "Provider", render: (p) => <code>{p.provider}</code> },
    { title: "Endpoint / region", render: (p) => <><div className="break-word">{p.endpoint ?? "Provider default"}</div><span className="muted">{p.region ?? "No region"}</span></> },
    { title: "Credentials", render: (p) => <span className="muted">{p.provider === "bedrock" ? "AWS workload identity; rotate through AWS" : "Secret reference (hidden)"}</span> },
    { title: "Status", render: (p) => <Status enabled={p.enabled} /> },
    ...(allowed ? [{ title: "Actions", render: (p: Provider) => <RowActions><button className="button secondary small" onClick={() => toggle(ask, `${path}/${p.id}`, p.name, p.enabled)}>{p.enabled ? "Disable" : "Enable"}</button>{p.provider !== "bedrock" && <button className="button secondary small" onClick={() => ask({ title: `Rotate reference for ${p.name}`, description: "New requests will resolve the new credential reference. The existing reference is not exposed. Connection status will be preserved.", danger: true, fields: [p.provider === "bedrock" ? workloadField : referenceField], submitLabel: "Rotate reference", run: (v) => api(`${path}/${p.id}`, { method: "PATCH", body: { enabled: p.enabled, credential_ref: p.provider === "bedrock" ? "aws:default" : `env:${v.credential_variable}` } }) })}>Rotate reference</button>}</RowActions> }] : []),
  ]} /></>;
}
type DeploymentScope = OrgScope & { modelId?: string; providerId?: string; embedded?: boolean };
export function Deployments({ session, modelId, providerId, embedded = false }: DeploymentScope) {
  const allowed = session.user.platform_admin;
  const model = useApi<Model>(`${platformPath}/models/${encodeURIComponent(modelId ?? "")}`, allowed && !!modelId);
  const provider = useApi<Provider>(`${platformPath}/providers/${encodeURIComponent(providerId ?? "")}`, allowed && !!providerId);
  if (!allowed) return <Heading title="Access not available" />;
  if (modelId && model.isError) return <ErrorNotice error={model.error} retry={() => void model.refetch()} />;
  if (providerId && provider.isError) return <ErrorNotice error={provider.error} retry={() => void provider.refetch()} />;
  if ((modelId && model.isPending) || (providerId && provider.isPending)) return <p role="status">Loading deployment context…</p>;
  if ((modelId && model.data?.id !== modelId) || (providerId && provider.data?.id !== providerId)) return <ErrorNotice error={new Error("The gateway returned a different parent. Deployment controls are unavailable.")} />;
  return <ActionProvider key={`${modelId ?? ""}:${providerId ?? ""}`}><DeploymentCollection model={modelId ? model.data : undefined} provider={providerId ? provider.data : undefined} embedded={embedded} /></ActionProvider>;
}
function DeploymentCollection({ model, provider, embedded }: { model?: Model; provider?: Provider; embedded: boolean }) {
  const createTrigger = useRef<HTMLButtonElement>(null);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<string>();
  const filters = new URLSearchParams();
  if (model) filters.set("model_id", model.id);
  if (provider) filters.set("provider_connection_id", provider.id);
  const path = `${platformPath}/deployments${filters.size ? `?${filters}` : ""}`;
  const create = <button ref={createTrigger} className="button" onClick={() => setCreating(true)} disabled={creating}>Create deployment</button>;
  return <>{embedded ? <div className="table-toolbar"><h2>Deployments</h2>{create}</div> : <Heading title="Deployments" description="Connect a public model alias to a provider’s upstream model. Disabled dependencies prevent inference even when a deployment is enabled." actions={create} />}
    {(model || provider) && <p className="notice">Fixed creation context (read-only): {model && <>Model <strong>{model.public_name}</strong> (<Id value={model.id} />). </>}{provider && <>Provider <strong>{provider.name}</strong> (<Id value={provider.id} />).</>} A new deployment stays attached to this context.</p>}
    {created !== undefined && <CreationOutcome kind="deployment" id={created} />}
    {creating && <PrepareDeployment model={model} provider={provider} returnFocus={createTrigger.current ?? undefined} onClose={() => setCreating(false)} onCreated={result => setCreated(createdId(result))} />}
    <CollectionTable<Deployment> key={path} searchable statusFilter path={path} label="Deployments" empty="Add a deployment after configuring a model alias and provider connection." rowKey={d => d.id} columns={[
      { title: "Upstream model", render: d => <><ResourceLink search={{ page: "deployment-detail", record: d.id }}><code>{d.upstream_model}</code></ResourceLink><Id value={d.id} /></> },
      { title: "Model alias", render: d => <ResourceLink search={{ page: "model-detail", record: d.model_id }}>{model && model.id === d.model_id ? model.public_name : <Id value={d.model_id} />}</ResourceLink> },
      { title: "Provider connection", render: d => <ResourceLink search={{ page: "provider-detail", record: d.provider_connection_id }}>{provider && provider.id === d.provider_connection_id ? provider.name : <Id value={d.provider_connection_id} />}</ResourceLink> },
      { title: "Status", render: d => <Status enabled={d.enabled} /> },
      { title: "Actions", render: d => <CatalogStatusButton kind="deployments" record={d} name={d.upstream_model} /> },
    ]} />
  </>;
}
// Catalog choices are fetched only after an explicit creation request, never to
// locate a detail record. Fixed parents are omitted from editable form fields.
function PrepareDeployment({ model, provider, onClose, onCreated, returnFocus }: { model?: Model; provider?: Provider; onClose: () => void; onCreated: (result: unknown) => void; returnFocus?: HTMLElement }) {
  const ask = useAction();
  const models = useChoices<Model>(`${platformPath}/models`, !model);
  const providers = useChoices<Provider>(`${platformPath}/providers`, !provider);
  const availableModels = model ? [model] : models.data;
  const availableProviders = provider ? [provider] : providers.data;
  const failed = (!model && models.isError) || (!provider && providers.isError);
  const ready = !failed && !!availableModels?.length && !!availableProviders?.length;
  useEffect(() => {
    if (!ready) return;
    ask({ ...deploymentCreateAction({ model, provider, models: availableModels!, providers: availableProviders!, after: onCreated }), returnFocus });
    onClose();
  }, [ready, ask, model, provider, availableModels, availableProviders, onCreated, onClose, returnFocus]);
  return <Panel title="Prepare deployment">
    {!model && models.isError && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}
    {!provider && providers.isError && <ErrorNotice error={providers.error} retry={() => void providers.refetch()} />}
    {!failed && (!availableModels || !availableProviders) && <p role="status">Loading creation options…</p>}
    {availableModels?.length === 0 && <p>Create a <ResourceLink search={{ page: "models" }}>model alias</ResourceLink> first.</p>}
    {availableProviders?.length === 0 && <p>Create a <ResourceLink search={{ page: "providers" }}>provider connection</ResourceLink> first.</p>}
    <button className="button secondary" onClick={() => { onClose(); queueMicrotask(() => returnFocus?.focus()); }}>Cancel</button>
  </Panel>;
}
export function deploymentCreateAction({ model, provider, models, providers, after }: { model?: Model; provider?: Provider; models: Model[]; providers: Provider[]; after?: (result: unknown) => void }): Action {
  return {
    title: "Create deployment",
    description: [model ? `Model (read-only): ${model.public_name} (${model.id}).` : "", provider ? `Provider (read-only): ${provider.name} (${provider.id}).` : "", "New deployments are disabled by default. Configure routing and pricing before enabling; disabled parents still prevent inference."].filter(Boolean).join(" "),
    fields: [
      ...(!model ? [{ name: "model_id", label: "Model alias", type: "select", required: true, options: models.map(m => ({ value: m.id, label: `${m.public_name}${m.enabled ? "" : " — disabled"}` })) } satisfies Field] : []),
      ...(!provider ? [{ name: "provider_connection_id", label: "Provider connection", type: "select", required: true, options: providers.map(p => ({ value: p.id, label: `${p.name} (${p.provider})${p.enabled ? "" : " — disabled"}` })) } satisfies Field] : []),
      { name: "upstream_model", label: "Upstream model identifier", required: true, maxLength: 300, help: "Use the exact model ID supported by the provider account." },
      { ...enabledField, value: "false" },
    ],
    submitLabel: "Create deployment",
    successNotice: "Deployment created. Review routing and publish a price version before enabling inference.",
    after,
    run: values => api(`${platformPath}/deployments`, { method: "POST", body: { model_id: model?.id ?? values.model_id, provider_connection_id: provider?.id ?? values.provider_connection_id, upstream_model: values.upstream_model, enabled: values.enabled === "true" } }),
  };
}
export function CatalogStatusButton({ kind, record, name }: { kind: "models" | "providers" | "deployments"; record: { id: string; enabled: boolean }; name: string }) {
  const ask = useAction();
  return <button className="button secondary small" onClick={() => toggle(ask, `${platformPath}/${kind}/${encodeURIComponent(record.id)}`, name, record.enabled)}>{record.enabled ? "Disable" : "Enable"}</button>;
}
function createdId(result: unknown): string {
  // Retain only the identifier for navigation; never retain a whole provider response.
  return result && typeof result === "object" && "id" in result && typeof result.id === "string" ? result.id : "";
}
function CreationOutcome({ kind, id }: { kind: "model" | "provider" | "deployment"; id: string }) {
  const page = kind === "model" ? "model-detail" : kind === "provider" ? "provider-detail" : "deployment-detail";
  return <div className="notice" role="status"><p>{kind === "model" ? "Model alias created. Add a deployment and configure organization access." : kind === "provider" ? "Provider connection created. Credential values and references remain hidden. Add a deployment and review status before use." : "Deployment created. Review routing, publish pricing, and check parent status before enabling inference."}</p>{id ? <div className="row-actions"><ResourceLink search={{ page, record: id }}>Open {kind}</ResourceLink>{kind === "deployment" ? <><ResourceLink search={{ page, record: id, tab: "routing" }}>Configure routing</ResourceLink><ResourceLink search={{ page, record: id, tab: "pricing" }}>Publish pricing</ResourceLink></> : <ResourceLink search={{ page, record: id, tab: "deployments" }}>Add deployment</ResourceLink>}</div> : <p>No identifier was returned. Check the refreshed list before creating another resource.</p>}</div>;
}

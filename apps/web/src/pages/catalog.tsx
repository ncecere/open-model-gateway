import { api, platformPath, type Session, type Organization, type Model, type Provider, type Deployment } from "../lib/api";
import { enabledField, nameField, type Field } from "../lib/forms";
import { permissions } from "../lib/permissions";
import { CollectionTable, ErrorNotice, Heading, Id, RowActions, Status, useAction, useChoices, type Action } from "../components/ui";

type OrgScope = { session: Session; organization?: Organization };
function toggle(ask: (action: Action) => void, path: string, name: string, enabled: boolean) {
  ask({ title: `${enabled ? "Disable" : "Enable"} ${name}?`, description: enabled ? "Disabling this resource makes it unavailable for new inference requests that depend on it." : "Enabling permits eligible inference requests to use this resource. Provider calls may incur charges.", danger: enabled, submitLabel: enabled ? "Disable" : "Enable", run: () => api(path, { method: "PATCH", body: { enabled: !enabled } }) });
}
export const referenceField: Field = { name: "credential_variable", label: "Credential environment variable name", required: true, maxLength: 128, placeholder: "OPENAI_API_KEY", help: "Reference only — never paste a secret value. The gateway must already have this variable configured and allowlisted. Stored as env:NAME; references are never returned.", validate: (v) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(v) ? undefined : "Enter an environment variable name only, not a secret, URL, or env: prefix." };
const workloadField: Field = { name: "workload_identity", label: "Credential reference", type: "select", required: true, value: "aws:default", options: [{ value: "aws:default", label: "AWS default workload identity (aws:default)" }], help: "Uses the gateway’s configured AWS workload identity. No static secret is entered here." };
export function Models({ session }: OrgScope) {
  const ask = useAction();
  const path = `${platformPath}/models`;
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  return <><Heading title="Models" description="Platform-owned model catalog. Configure global deployments here, then assign models to organizations from Model access." actions={<button className="button" onClick={() => ask({ title: "Create model alias", fields: [{ name: "public_name", label: "API model name", required: true, maxLength: 200, placeholder: "company/smart", validate: (v) => /^[A-Za-z0-9/_.:-]+$/.test(v) ? undefined : "Use letters, digits, slash, hyphen, underscore, dot, or colon." }, { name: "display_name", label: "Display name", required: true, maxLength: 120 }, enabledField], submitLabel: "Create alias", run: (v) => api(path, { method: "POST", body: { public_name: v.public_name, display_name: v.display_name, enabled: v.enabled === "true" } }) })}>Create alias</button>} /><CollectionTable<Model> path={path} label="Model aliases" empty="Create an alias to decouple client model names from upstream deployments." rowKey={(m) => m.id} columns={[
    { title: "Name", render: (m) => <><strong>{m.display_name}</strong><Id value={m.id} /></> },
    { title: "API model name", render: (m) => <code>{m.public_name}</code> },
    { title: "Status", render: (m) => <Status enabled={m.enabled} /> },
    { title: "Actions", render: (m) => <button className="button secondary small" onClick={() => toggle(ask, `${path}/${m.id}`, m.public_name, m.enabled)}>{m.enabled ? "Disable" : "Enable"}</button> },
  ]} /></>;
}
export function Providers({ session, organization }: OrgScope) {
  const ask = useAction();
  const path = `${platformPath}/providers`;
  const allowed = permissions(session, organization).manageProviders;
  if (!allowed) return <Heading title="Access not available" />;
  return <><Heading title="Provider connections" description="Upstream provider configuration. Credential values and references are never returned by the gateway." actions={allowed && <button className="button" onClick={() => ask({ title: "Create provider connection", description: "Configure credentials on the gateway first. Only allowlisted secret references and provider-approved endpoints are accepted.", fields: [nameField, { name: "provider", label: "Provider", type: "select", required: true, value: "openai", options: [{ value: "openai", label: "OpenAI" }, { value: "anthropic", label: "Anthropic" }, { value: "bedrock", label: "Amazon Bedrock" }] }, { ...referenceField, visibleWhen: (v) => v.provider !== "bedrock" }, { ...workloadField, visibleWhen: (v) => v.provider === "bedrock" }, { name: "endpoint", label: "Endpoint", visibleWhen: (v) => v.provider !== "bedrock", help: "Leave blank for the provider’s public API endpoint. Custom endpoints are not supported.", maxLength: 2048, validate: (v, values) => { const expected = values.provider === "anthropic" ? "https://api.anthropic.com/v1" : "https://api.openai.com/v1"; return v === expected || v === `${expected}/` ? undefined : `Only ${expected} is supported.`; } }, { name: "region", label: "AWS region", required: true, visibleWhen: (v) => v.provider === "bedrock", maxLength: 32, placeholder: "us-east-1", validate: (v) => /^[a-z0-9-]+$/.test(v) ? undefined : "Use a valid lowercase AWS region name." },  { ...enabledField, value: "false" }], submitLabel: "Create connection", run: (v) => api(path, { method: "POST", body: { name: v.name, provider: v.provider, credential_ref: v.provider === "bedrock" ? "aws:default" : `env:${v.credential_variable}`, enabled: v.enabled === "true", ...(v.endpoint && v.provider !== "bedrock" ? { endpoint: v.endpoint } : {}), ...(v.provider === "bedrock" ? { region: v.region } : {}) } }) })}>Create connection</button>} />{!allowed && <p className="notice">Read-only access. Only platform operators can create, enable, disable, or rotate provider credentials.</p>}<CollectionTable<Provider> path={path} label="Provider connections" empty={allowed ? "Create a connection using a credential reference configured by your operator." : "Ask a platform operator to configure a provider connection."} rowKey={(p) => p.id} columns={[
    { title: "Name", render: (p) => <><strong>{p.name}</strong><Id value={p.id} /></> },
    { title: "Provider", render: (p) => <code>{p.provider}</code> },
    { title: "Endpoint / region", render: (p) => <><div className="break-word">{p.endpoint ?? "Provider default"}</div><span className="muted">{p.region ?? "No region"}</span></> },
    { title: "Credentials", render: (p) => <span className="muted">{p.provider === "bedrock" ? "AWS workload identity; rotate through AWS" : "Secret reference (hidden)"}</span> },
    { title: "Status", render: (p) => <Status enabled={p.enabled} /> },
    ...(allowed ? [{ title: "Actions", render: (p: Provider) => <RowActions><button className="button secondary small" onClick={() => toggle(ask, `${path}/${p.id}`, p.name, p.enabled)}>{p.enabled ? "Disable" : "Enable"}</button>{p.provider !== "bedrock" && <button className="button secondary small" onClick={() => ask({ title: `Rotate reference for ${p.name}`, description: "New requests will resolve the new credential reference. The existing reference is not exposed. Connection status will be preserved.", danger: true, fields: [p.provider === "bedrock" ? workloadField : referenceField], submitLabel: "Rotate reference", run: (v) => api(`${path}/${p.id}`, { method: "PATCH", body: { enabled: p.enabled, credential_ref: p.provider === "bedrock" ? "aws:default" : `env:${v.credential_variable}` } }) })}>Rotate reference</button>}</RowActions> }] : []),
  ]} /></>;
}
export function Deployments({ session }: OrgScope) {
  const ask = useAction();
  const base = platformPath;
  const path = `${base}/deployments`;
  const models = useChoices<Model>(`${base}/models`, session.user.platform_admin);
  const providers = useChoices<Provider>(`${base}/providers`, session.user.platform_admin);
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  const ready = !!models.data?.length && !!providers.data?.length;
  return <><Heading title="Deployments" description="Connect a public model alias to a provider’s upstream model. Disabled dependencies prevent inference even when a deployment is enabled." actions={<button className="button" disabled={!ready} onClick={() => ask({ title: "Create deployment", fields: [{ name: "model_id", label: "Model alias", type: "select", required: true, options: models.data?.map((m) => ({ value: m.id, label: `${m.public_name}${m.enabled ? "" : " — disabled"}` })) ?? [] }, { name: "provider_connection_id", label: "Provider connection", type: "select", required: true, options: providers.data?.map((p) => ({ value: p.id, label: `${p.name} (${p.provider})${p.enabled ? "" : " — disabled"}` })) ?? [] }, { name: "upstream_model", label: "Upstream model identifier", required: true, maxLength: 300, help: "Use the exact model ID supported by the provider account." }, { ...enabledField, value: "false" }], submitLabel: "Create deployment", run: (v) => api(path, { method: "POST", body: { model_id: v.model_id, provider_connection_id: v.provider_connection_id, upstream_model: v.upstream_model, enabled: v.enabled === "true" } }) })}>Create deployment</button>} />{models.isError && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}{providers.isError && <ErrorNotice error={providers.error} retry={() => void providers.refetch()} />}{models.data && providers.data && !ready && <p className="notice">Create at least one model alias and provider connection before creating a deployment.</p>}<CollectionTable<Deployment> path={path} label="Deployments" empty="Add a deployment after configuring a model alias and provider connection." rowKey={(d) => d.id} columns={[
    { title: "Model alias", render: (d) => models.data?.find((m) => m.id === d.model_id)?.public_name ?? <Id value={d.model_id} /> },
    { title: "Provider connection", render: (d) => providers.data?.find((p) => p.id === d.provider_connection_id)?.name ?? <Id value={d.provider_connection_id} /> },
    { title: "Upstream model", render: (d) => <code>{d.upstream_model}</code> },
    { title: "Status", render: (d) => <Status enabled={d.enabled} /> },
    { title: "Actions", render: (d) => <button className="button secondary small" onClick={() => toggle(ask, `${path}/${d.id}`, d.upstream_model, d.enabled)}>{d.enabled ? "Disable" : "Enable"}</button> },
  ]} /></>;
}

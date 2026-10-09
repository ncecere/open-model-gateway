import { useEffect, useRef, useState } from "react";
import { Plus } from "lucide-react";
import { ActionMenu } from "../components/templates/action-menu";
import { Inline } from "../components/ui/layout/layout";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { IconCell, LabIcon, ProviderIcon, WithIcon } from "../components/provider-icon";
import { api, platformPath, type Session, type Model, type Provider, type Deployment, type ModelProtocol, type ServerPolicy } from "../lib/api";
import { enabledField, nameField, checkboxValues, parseCheckboxValues, type Field, type Values } from "../lib/forms";
import { awsAccessBody, awsAccessFields, bedrockUpstreamError, regionFields, regionValue } from "../lib/bedrock";
import { modelReadiness, protocolOptions, protocolLabel, protocolSetError, readinessLabels, readinessText } from "../lib/model-setup";
import { ActionProvider, Button, CollectionTable, ErrorNotice, FormField, Heading, Id, NativeSelect, Status, Stack, Panel, useAction, useApi, useChoices, type Action } from "../components/ui";
import s from "./shared.module.css";
export { protocolOptions };
export function modelFields(model?: Model): Field[] { return [{ name: "public_name", label: "API model name", required: true, value: model?.public_name ?? "", maxLength: 200, validate: v => /^[A-Za-z0-9/_.:-]+$/.test(v) ? undefined : "Use letters, digits, slash, hyphen, underscore, dot or colon." }, { name: "display_name", label: "Display name", value: model?.display_name ?? "", required: true, maxLength: 120 }, { name: "description", label: "Description", type: "textarea", value: model?.description ?? "", maxLength: 2000 }, { name: "supported_protocols", label: "Supported client protocols", type: "checkboxes", required: true, maxSelections: 3, value: JSON.stringify(model?.supported_protocols ?? ["chat_completions"]), options: protocolOptions, validate: protocolSetError, help: "Select the model's certified protocols. Serving also requires the route's connection profile to support the protocol; embeddings are input-only workloads." }, { ...enabledField, value: String(model?.enabled ?? false) }]; }
export function modelBody(values: Record<string, string>) { return { public_name: values.public_name, display_name: values.display_name, description: values.description || null, supported_protocols: checkboxValues(modelFields().find(f => f.name === "supported_protocols")!, values.supported_protocols) as ModelProtocol[], enabled: values.enabled === "true" }; }
/** Edit leaves status out: Enable/Disable is its own header action, and a PATCH without `enabled` preserves it. */
export const modelEditFields = (model: Model) => modelFields(model).filter(f => f.name !== "enabled");
export function modelEditBody(values: Record<string, string>) { const { enabled: _enabled, ...body } = modelBody({ ...values, enabled: "false" }); return body; }
/** Ready / Needs setup / Needs attention / Not serving (readinessText), from server readiness counts only (contract §2). */
export function ReadinessBadge({ model, policy }: { model: Model; policy?: ServerPolicy }) {
  const r = modelReadiness(model, policy);
  return <BitopBadge tone={r.state === "ready" ? "success" : r.state === "needs_setup" || r.state === "needs_attention" || r.state === "not_serving" ? "warning" : "neutral"} dot>{readinessText[r.state]}</BitopBadge>;
}
export const readinessNote = (model: Model, policy?: ServerPolicy) => modelReadiness(model, policy).warnings.map(w => readinessLabels[w]).join(" · ");
/** The server's provider policy for readiness checks; unknown (undefined) until loaded. */
export const useServerPolicy = (session: Session) => useApi<ServerPolicy>(`${platformPath}/server-policy`, session.capabilities.platform_read).data;
/** `profiles` (when loaded) adds each connection's profile logo; the name stays the link text. */
export function ConnectionNames({ model, profiles }: { model: Model; profiles?: Provider[] }) {
  const list = model.readiness?.connections;
  if (!list) return <span className={s.muted}>Unknown</span>;
  if (!list.length) return <span className={s.muted}>No routes</span>;
  return <span className={s.badges}>{list.map((c, i) => { const profile = profiles?.find(p => p.id === c.id)?.provider, link = <ResourceLink search={{ page: "provider-detail", record: c.id }}>{c.name}</ResourceLink>; return <span key={c.id}>{profile ? <WithIcon icon={<ProviderIcon profile={profile} size="sm" />}>{link}</WithIcon> : link}{i < list.length - 1 ? "," : ""}</span>; })}</span>;
}
/** Admin › Models: the catalog page (type tabs, filters, list/table) lives in model-catalog.tsx. */
export { Models } from "./model-catalog";
export const referenceField: Field = { name: "credential_variable", label: "Credential environment variable name", required: true, maxLength: 128, placeholder: "UPSTREAM_API_KEY", help: "The variable's name, never the secret.", validate: v => /^[A-Za-z_][A-Za-z0-9_]*$/.test(v) ? undefined : "Enter an environment variable name only, not a secret or env: prefix." };
export const localProfiles = ["openai_compatible", "vllm", "sglang", "ollama"];
export const providerOptions = [{ value: "openai", label: "OpenAI" }, { value: "anthropic", label: "Anthropic" }, { value: "openrouter", label: "OpenRouter" }, { value: "bedrock", label: "Amazon Bedrock" }, { value: "openai_compatible", label: "OpenAI-compatible" }, { value: "vllm", label: "vLLM" }, { value: "sglang", label: "SGLang" }, { value: "ollama", label: "Ollama" }];
export const providerLabel = (provider: string) => providerOptions.find(o => o.value === provider)?.label ?? provider;
const isBedrock = (v: Record<string, string>) => v.provider === "bedrock";
/** Only approved local profiles may run without authentication; cloud profiles never offer (or send) it. */
const noAuthAllowed = (v: Record<string, string>) => localProfiles.includes(v.provider);
const usesNoAuth = (v: Record<string, string>) => noAuthAllowed(v) && v.auth_mode === "none";
export function providerFields(provider?: Provider): Field[] {
  return [{ ...nameField, value: provider?.name }, { name: "provider", label: "Provider profile", type: "select", display: "icon-select", required: true, value: provider?.provider ?? "openai", options: providerOptions.map(o => ({ ...o, icon: <ProviderIcon profile={o.value} /> })), helpFor: v => profileHelp[v.provider] ?? (localProfiles.includes(v.provider) ? profileHelp.local : undefined) }, { name: "auth_mode", label: "Authentication", type: "select", required: true, value: "environment", options: [{ value: "environment", label: "Secret in an environment variable" }, { value: "none", label: "No authentication" }], visibleWhen: noAuthAllowed }, { ...referenceField, visibleWhen: v => v.provider !== "bedrock" && v.provider !== "openrouter" && !usesNoAuth(v) }, { ...referenceField, placeholder: "OPENROUTER_API_KEY", visibleWhen: v => v.provider === "openrouter" }, { name: "endpoint", label: "Endpoint", requiredWhen: v => localProfiles.includes(v.provider), value: provider?.endpoint ?? "", maxLength: 2048, visibleWhen: v => !(v.provider in fixedEndpoints) && v.provider !== "bedrock", placeholder: "http://127.0.0.1:8000/v1", help: "Must match an endpoint the server approves.", validate: (v, values) => { if (!v && localProfiles.includes(values.provider)) return "An explicitly approved local endpoint is required."; if (!v) return; try { const u = new URL(v); if (!['http:', 'https:'].includes(u.protocol) || u.username || u.password || u.search || u.hash) return "Use an HTTP(S) endpoint without credentials, query or fragment."; if (!localProfiles.includes(values.provider) && u.protocol !== "https:") return "Cloud endpoints require HTTPS."; } catch { return "Enter a valid approved endpoint URL."; } } }, ...regionFields(isBedrock, provider?.region), ...awsAccessFields(isBedrock, provider), provider ? { ...enabledField, value: String(provider.enabled) } : { name: "enabled", label: "Enable now", type: "switch", value: "true", helpFor: v => v.enabled === "true" ? "Its models can serve once enabled." : "Nothing is sent until you enable it." }];
}
/** What each profile connects to, shown under the profile choice (review #46). */
const profileHelp: Record<string, string> = {
  openai: "Uses api.openai.com.",
  anthropic: "Uses api.anthropic.com.",
  openrouter: "Uses openrouter.ai.",
  bedrock: "Uses Bedrock Runtime in the AWS region you choose.",
  local: "Uses an endpoint the server already approves.",
};
/**
 * Cloud profiles whose endpoint the server fixes: it accepts only null or this exact base URL, never a custom one.
 * The form hides Endpoint for them, keeping a stored fixed base unchanged and sending null otherwise.
 */
export const fixedEndpoints: Record<string, string> = { openai: "https://api.openai.com/v1", anthropic: "https://api.anthropic.com/v1", openrouter: "https://openrouter.ai/api/v1" };
function endpointBody(v: Record<string, string>) {
  const fixed = fixedEndpoints[v.provider];
  if (v.provider === "bedrock") return null;
  if (fixed) return v.endpoint === fixed || v.endpoint === `${fixed}/` ? v.endpoint : null;
  return v.endpoint || null;
}
export function providerBody(v: Record<string, string>) {
  if (isBedrock(v)) return { name: v.name, provider: v.provider, ...awsAccessBody(v), region: regionValue(v), enabled: v.enabled === "true" };
  return { name: v.name, provider: v.provider, credential_ref: usesNoAuth(v) ? "none" : `env:${v.credential_variable}`, endpoint: endpointBody(v), region: null, enabled: v.enabled === "true" };
}
export const connectionCreateAction = (): Action => ({ title: "Add connection", description: "Credentials stay on the server.", fields: providerFields(), submitLabel: "Add connection", run: (v, signal) => api(`${platformPath}/providers`, { method: "POST", body: providerBody(v), signal }) });
export function Providers({ session }: { session: Session }) {
  const ask = useAction(); if (!session.capabilities.platform_read) return <Heading title="Access not available" />;
  return <Stack gap={6} className={s.page}><Heading title="Connections" description="Where models send requests. Credentials stay on the server." actions={session.capabilities.platform_write && <Button onClick={() => ask(connectionCreateAction())}><Plus aria-hidden />Add connection</Button>} /><CollectionTable<Provider> searchable statusFilter path={`${platformPath}/providers`} label="Connections" empty="Add an approved endpoint with a server-side credential reference, then add models from it." rowKey={p => p.id} columns={[{ title: "Connection", render: p => <WithIcon icon={<ProviderIcon profile={p.provider} />}><ResourceLink search={{ page: "provider-detail", record: p.id }}>{p.name}</ResourceLink></WithIcon> }, { title: "Profile", render: p => providerLabel(p.provider) }, { title: "Endpoint / region", narrow: true, render: p => <>{p.endpoint ?? "Provider default"}<span className={s.secondary}>{p.region}</span></> }, { title: "Models", numeric: true, render: p => p.model_count === undefined ? <span className={s.muted}>Unknown</span> : <ResourceLink search={{ page: "models", connections: p.id }}>{String(p.model_count)}</ResourceLink> }, { title: "Status", render: p => <Status enabled={p.enabled} /> }]} /></Stack>;
}
export function deploymentCreateAction({ model, provider, models, providers, after }: { model?: Model; provider?: Provider; models: Model[]; providers: Provider[]; after?: (result: unknown) => void }): Action { const connectionFor = (v: Values) => provider ?? providers.find(p => p.id === v.provider_connection_id); return { title: "Add route", description: [model ? `Model: ${model.display_name} (${model.public_name}).` : "", provider ? `Connection: ${provider.name}.` : "", "Disabled by default. Review protocol support, routing and pricing before enabling."].filter(Boolean).join(" "), fields: [...(!model ? [{ name: "model_id", label: "Model", type: "select", required: true, options: models.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}` })) } satisfies Field] : []), ...(!provider ? [{ name: "provider_connection_id", label: "Connection", type: "select", required: true, options: providers.map(p => ({ value: p.id, label: `${p.name} · ${providerLabel(p.provider)}${p.enabled ? "" : " · disabled"}` })) } satisfies Field] : []), { name: "upstream_model", label: "Upstream model ID", required: true, maxLength: 512, helpFor: v => connectionFor(v)?.provider === "bedrock" ? "Model ID, inference profile ID (us.…, global.…) or ARN in the connection's region." : undefined, validate: (value, v) => { const c = connectionFor(v); return c?.provider === "bedrock" ? bedrockUpstreamError(value, c.region) : undefined; } }, { ...enabledField, value: "false" }], submitLabel: "Add route", after, run: (v, signal) => api(`${platformPath}/deployments`, { method: "POST", body: { model_id: model?.id ?? v.model_id, provider_connection_id: provider?.id ?? v.provider_connection_id, upstream_model: v.upstream_model, enabled: v.enabled === "true" }, signal }) }; }
export function Deployments({ session, modelId, providerId, embedded = false }: { session: Session; modelId?: string; providerId?: string; embedded?: boolean }) {
  const allowed = session.capabilities.platform_read;
  const model = useApi<Model>(`${platformPath}/models/${encodeURIComponent(modelId ?? "")}`, allowed && !!modelId), provider = useApi<Provider>(`${platformPath}/providers/${encodeURIComponent(providerId ?? "")}`, allowed && !!providerId);
  if (!allowed) return <Heading title="Access not available" />;
  if (modelId && model.isError) return <ErrorNotice error={model.error} retry={() => void model.refetch()} />;
  if (providerId && provider.isError) return <ErrorNotice error={provider.error} retry={() => void provider.refetch()} />;
  if (modelId && model.isPending || providerId && provider.isPending) return <p role="status">Loading route context…</p>;
  if (modelId && model.data?.id !== modelId || providerId && provider.data?.id !== providerId) return <ErrorNotice error={new Error("The gateway returned a different parent. Controls are unavailable.")} />;
  return <ActionProvider key={`${modelId}:${providerId}`}><DeploymentCollection model={modelId ? model.data : undefined} provider={providerId ? provider.data : undefined} writable={session.capabilities.platform_write} embedded={embedded} /></ActionProvider>;
}
function DeploymentCollection({ model, provider, writable, embedded }: { model?: Model; provider?: Provider; writable: boolean; embedded: boolean }) {
  const [creating, setCreating] = useState(false), trigger = useRef<HTMLButtonElement>(null);
  const filters = new URLSearchParams(); if (model) filters.set("model_id", model.id); if (provider) filters.set("provider_connection_id", provider.id);
  const create = writable && <Button ref={trigger} onClick={() => setCreating(true)} disabled={creating}><Plus aria-hidden />Add route</Button>;
  return <Stack gap={6} className={s.page}>{embedded ? <Heading title="Routes" actions={create} /> : <Heading title="All routes" description="Every model route on every connection. Routes are usually managed from their model's page. A disabled model or connection still prevents inference." actions={create} />}{creating && <PrepareDeployment model={model} provider={provider} returnFocus={trigger.current ?? undefined} onClose={() => setCreating(false)} />}<CollectionTable<Deployment> searchable statusFilter path={`${platformPath}/deployments${filters.size ? `?${filters}` : ""}`} label="Routes" empty="Add a model from a connection to create its first route." rowKey={d => d.id} columns={[{ title: "Upstream model", render: d => <IconCell icon={<LabIcon model={[d.upstream_model, d.model_public_name]} />}><ResourceLink search={{ page: "deployment-detail", record: d.id }}>{d.upstream_model}</ResourceLink><Id value={d.id} /></IconCell> }, { title: "Model", render: d => <ResourceLink search={{ page: "model-detail", record: d.model_id }}>{model?.public_name ?? d.model_public_name ?? d.model_id}</ResourceLink> }, { title: "Connection", render: d => { const link = <ResourceLink search={{ page: "provider-detail", record: d.provider_connection_id }}>{provider?.name ?? d.provider_name ?? d.provider_connection_id}</ResourceLink>; return provider ? <WithIcon icon={<ProviderIcon profile={provider.provider} size="sm" />}>{link}</WithIcon> : link; } }, { title: "Status", render: d => <Status enabled={d.enabled} /> }]} /></Stack>;
}
function PrepareDeployment({ model, provider, returnFocus, onClose }: { model?: Model; provider?: Provider; returnFocus?: HTMLElement; onClose: () => void }) {
  const ask = useAction(), models = useChoices<Model>(`${platformPath}/models`, !model), providers = useChoices<Provider>(`${platformPath}/providers`, !provider);
  const ms = model ? [model] : models.data, ps = provider ? [provider] : providers.data, ready = !!ms?.length && !!ps?.length;
  useEffect(() => { if (ready) { ask({ ...deploymentCreateAction({ model, provider, models: ms!, providers: ps! }), returnFocus }); onClose(); } }, [ready]);
  return <Panel title="Prepare route">{models.isError && !model && <ErrorNotice error={models.error} retry={() => void models.refetch()} />}{providers.isError && !provider && <ErrorNotice error={providers.error} retry={() => void providers.refetch()} />}{!ms || !ps ? <p role="status">Loading creation options…</p> : <p>{!ms.length ? "Add a model first. " : ""}{!ps.length ? "Add a connection first." : ""}</p>}<Button variant="secondary" onClick={onClose}>Cancel</Button></Panel>;
}
export function CatalogStatusButton({ kind, record, name }: { kind: "models" | "providers" | "deployments"; record: { id: string; enabled: boolean }; name: string }) { const ask = useAction(); return <Inline gap={2}>{catalogStatusActions(ask, kind, record, name)}</Inline>; }
/** Enable/Disable and the "⋯" Retire menu as plain header actions, so a phone header folds them into its one "⋯" menu (HeaderActions). */
export function catalogStatusActions(ask: ReturnType<typeof useAction>, kind: "models" | "providers" | "deployments", record: { id: string; enabled: boolean }, name: string) { return <><Button variant="secondary" onClick={() => ask({ title: `${record.enabled ? "Disable" : "Enable"} ${name}?`, description: "This changes eligibility for new inference requests. It is not a readiness test.", danger: record.enabled, submitLabel: record.enabled ? "Disable" : "Enable", run: (_, signal) => api(`${platformPath}/${kind}/${encodeURIComponent(record.id)}`, { method: "PATCH", body: { enabled: !record.enabled }, signal }) })}>{record.enabled ? "Disable" : "Enable"}</Button><ActionMenu label={`Actions for ${name}`} actions={[{ label: "Retire resource…", danger: true, hidden: !record.enabled, onSelect: () => ask({ title: `Retire ${name}?`, description: "Disables live inference eligibility. Records, immutable prices and historical executions are retained, not physically deleted.", danger: true, submitLabel: "Retire resource", run: (_, signal) => api(`${platformPath}/${kind}/${encodeURIComponent(record.id)}`, { method: "DELETE", signal }) }) }]} /></>; }
// Exposed for protocol/form tests without making an upstream call.
export const selectedProtocols = (value: string) => parseCheckboxValues(value);

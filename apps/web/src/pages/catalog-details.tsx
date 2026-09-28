import { useEffect, type ReactNode } from "react";
import { ResourceLink } from "../components/navigation-link";
import { ResourcePage } from "../components/resource-page";
import { ActionProvider, ErrorNotice, Heading, Id, Panel, Status, useAction, useApi } from "../components/ui";
import { api, platformPath, type Deployment, type Model, type Provider, type Session } from "../lib/api";
import { CatalogStatusButton, Deployments, referenceField } from "./catalog";
import { DeploymentRoutingPanel, ModelRoutingPanel, PriceVersions } from "./governance";

type DetailProps = { session: Session; id: string; tab?: string; onTabChange: (tab: string) => void };
// Parent lookup is direct, not a paginated catalog scan. Even cached data must
// match the requested parent and a failed refresh cannot expose writable panels.
function CatalogRecord<T extends { id: string }>({ session, id, kind, children }: { session: Session; id: string; kind: "models" | "providers" | "deployments"; children: (record: T, path: string) => ReactNode }) {
  const path = `${platformPath}/${kind}/${encodeURIComponent(id)}`;
  const query = useApi<T>(path, session.user.platform_admin && !!id);
  useEffect(() => {
    if (query.isPending || (document.activeElement && document.activeElement !== document.body)) return;
    const heading = document.querySelector<HTMLElement>("#main h1");
    if (heading) { heading.tabIndex = -1; heading.focus({ preventScroll: true }); }
  }, [id, query.isPending, query.isError]);
  if (!session.user.platform_admin) return <Heading title="Access not available" description="Only platform operators can manage catalog infrastructure." />;
  if (!id) return <Heading title="No resource selected" description="Open a resource from the catalog." />;
  if (query.isPending) return <><Heading title="Loading resource…" /><p role="status">Loading {kind === "models" ? "model" : kind === "providers" ? "provider connection" : "deployment"}…</p></>;
  if (query.isError) return <><Heading title="Resource unavailable" /><ErrorNotice error={query.error} retry={() => void query.refetch()} /></>;
  if (!query.data || query.data.id !== id) return <ErrorNotice error={new Error("The gateway returned a different resource. Detail controls are unavailable.")} />;
  return <ActionProvider key={path}>{children(query.data, path)}</ActionProvider>;
}

export function ModelDetail({ session, id, tab, onTabChange }: DetailProps) {
  return <CatalogRecord<Model> session={session} id={id} kind="models">{(model, path) => <ResourcePage title={model.display_name} description={<><ResourceLink search={{ page: "models" }}>Models</ResourceLink> / <code>{model.public_name}</code></>} actions={<CatalogStatusButton kind="models" record={model} name={model.public_name} />} tab={tab} onTabChange={onTabChange} tabs={[
    { value: "overview", label: "Overview", content: <Panel title="Model alias"><dl className="details"><dt>API model name</dt><dd><code>{model.public_name}</code></dd><dt>Display name</dt><dd>{model.display_name}</dd><dt>Identifier</dt><dd><Id value={model.id} /></dd><dt>Status</dt><dd><Status enabled={model.enabled} /></dd></dl><p>Clients request this public alias. An enabled deployment and provider connection are also required; organization and workspace grants still apply.</p><div className="row-actions"><ResourceLink search={{ page: "model-detail", record: model.id, tab: "deployments" }}>Manage deployments</ResourceLink><ResourceLink search={{ page: "model-detail", record: model.id, tab: "routing" }}>Configure model routing</ResourceLink><ResourceLink search={{ page: "organizations" }}>Manage organization access</ResourceLink></div></Panel> },
    { value: "deployments", label: "Deployments", content: <Deployments session={session} modelId={model.id} embedded /> },
    { value: "routing", label: "Routing", content: <><p className="notice">Every fallback requires matching explicit residency. Ambiguous failover may duplicate provider charges; failover never occurs after a stream is returned.</p><ModelRoutingPanel key={path} path={`${path}/routing`} /></> },
  ]} />}</CatalogRecord>;
}

export function ProviderDetail({ session, id, tab, onTabChange }: DetailProps) {
  return <CatalogRecord<Provider> session={session} id={id} kind="providers">{provider => <ResourcePage title={provider.name} description={<><ResourceLink search={{ page: "providers" }}>Provider connections</ResourceLink> / {provider.provider}</>} actions={<CatalogStatusButton kind="providers" record={provider} name={provider.name} />} tab={tab} onTabChange={onTabChange} tabs={[
    { value: "overview", label: "Overview", content: <Panel title="Provider connection"><dl className="details"><dt>Identifier</dt><dd><Id value={provider.id} /></dd><dt>Provider</dt><dd>{provider.provider}</dd><dt>Endpoint</dt><dd className="break-word">{provider.endpoint ?? "Provider default"}</dd><dt>Region</dt><dd>{provider.region ?? "No region"}</dd><dt>Status</dt><dd><Status enabled={provider.enabled} /></dd><dt>Credentials</dt><dd>{provider.provider === "bedrock" ? "AWS workload identity; rotate through AWS" : "Secret reference (hidden)"}</dd></dl><p>Credential values and references are never returned. The gateway must have the configured credentials available; this page does not test paid inference or claim provider health.</p><div className="row-actions"><ResourceLink search={{ page: "provider-detail", record: provider.id, tab: "deployments" }}>Manage deployments</ResourceLink>{provider.provider !== "bedrock" && <RotateProviderReference provider={provider} />}</div></Panel> },
    { value: "deployments", label: "Deployments", content: <Deployments session={session} providerId={provider.id} embedded /> },
  ]} />}</CatalogRecord>;
}
function RotateProviderReference({ provider }: { provider: Provider }) {
  const ask = useAction();
  return <button className="button secondary" onClick={() => ask({ title: `Rotate reference for ${provider.name}`, description: "New requests will resolve the new credential reference. The existing reference is not exposed. Connection status will be preserved.", danger: true, fields: [referenceField], submitLabel: "Rotate reference", successNotice: "Credential reference updated. The reference remains hidden; connection status is unchanged.", run: values => api(`${platformPath}/providers/${encodeURIComponent(provider.id)}`, { method: "PATCH", body: { enabled: provider.enabled, credential_ref: `env:${values.credential_variable}` } }) })}>Rotate reference</button>;
}

export function DeploymentDetail({ session, id, tab, onTabChange }: DetailProps) {
  return <CatalogRecord<Deployment> session={session} id={id} kind="deployments">{(deployment, path) => <ResourcePage title={deployment.upstream_model} description={<><ResourceLink search={{ page: "deployments" }}>Deployments</ResourceLink> / <Id value={deployment.id} /></>} actions={<CatalogStatusButton kind="deployments" record={deployment} name={deployment.upstream_model} />} tab={tab} onTabChange={onTabChange} tabs={[
    { value: "overview", label: "Overview", content: <Panel title="Deployment"><dl className="details"><dt>Identifier</dt><dd><Id value={deployment.id} /></dd><dt>Upstream model</dt><dd><code>{deployment.upstream_model}</code></dd><dt>Model alias</dt><dd><ResourceLink search={{ page: "model-detail", record: deployment.model_id }}><Id value={deployment.model_id} /></ResourceLink></dd><dt>Provider connection</dt><dd><ResourceLink search={{ page: "provider-detail", record: deployment.provider_connection_id }}><Id value={deployment.provider_connection_id} /></ResourceLink></dd><dt>Status</dt><dd><Status enabled={deployment.enabled} /></dd></dl><p>Disabled model or provider dependencies prevent inference even when this deployment is enabled. Review routing and pricing before enabling; no paid test request is sent by this page.</p><div className="row-actions"><ResourceLink search={{ page: "deployment-detail", record: deployment.id, tab: "routing" }}>Configure routing</ResourceLink><ResourceLink search={{ page: "deployment-detail", record: deployment.id, tab: "pricing" }}>Manage price versions</ResourceLink></div></Panel> },
    { value: "routing", label: "Routing", content: <><p className="notice">Residency labels are operator assertions, not verified provider geography. Passive observations are not an uptime guarantee.</p><DeploymentRoutingPanel key={path} path={`${path}/routing`} operator /></> },
    { value: "pricing", label: "Pricing", content: <><p className="notice">Enter rates in US dollars per million tokens. Versions are immutable estimates, not invoices. Input limits must be accurate hard upstream ceilings; admission reserves the full ceiling.</p><PriceVersions key={path} path={`${path}/prices`} writable /></> },
  ]} />}</CatalogRecord>;
}

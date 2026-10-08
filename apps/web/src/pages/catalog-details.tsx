import { type ReactNode } from "react";
import { Boxes, Plus, Settings } from "lucide-react";
import { api, platformPath, type Model, type Provider, type Session } from "../lib/api";
import { ResourceLink } from "../components/navigation-link";
import { ActionProvider, Button, ErrorNotice, Heading, Status, Alert, useAction, useApi, useChoices } from "../components/ui";
import { RecordPage } from "../components/templates/record-page";
import { CatalogStatusButton, ReadinessBadge, providerLabel, referenceField } from "./catalog";
import { ProviderBadge, ProviderIcon, WithIcon } from "../components/provider-icon";
import { awsAccessBody, awsAccessFields, awsAuthLabel } from "../lib/bedrock";
import { ModelPage, ModelReadinessChecklist, RoutePage, type RouteDetail } from "./model-page";
import s from "./shared.module.css";
import t from "../components/templates/templates.module.css";
const enc = encodeURIComponent;
type DetailProps = { session: Session; id: string; tab?: string; onTabChange: (tab: string) => void };
export function CatalogRecord<T extends { id: string }>({ session, id, kind, children }: { session: Session; id: string; kind: "models" | "providers" | "deployments"; children: (record: T, path: string) => ReactNode }) {
  const path = `${platformPath}/${kind}/${enc(id)}`, q = useApi<T>(path, session.capabilities.platform_read && !!id);
  if (!session.capabilities.platform_read) return <Heading title="Access not available" />;
  if (q.isPending) return <p role="status">Loading resource…</p>; if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The gateway returned a different resource. Detail controls are unavailable.")} />;
  return <ActionProvider key={path}>{children(q.data, path)}</ActionProvider>;
}
const code = (value: string) => <code className={s.mono}>{value}</code>;

// ---------------------------------------------------------------------------
// Model page: one long page with a sticky section nav (model-page.tsx).
// Old ?tab= links (routes, pricing, routing, availability) open at that section.
// ---------------------------------------------------------------------------
export function ModelDetail({ session, id, tab }: DetailProps) { return <CatalogRecord<Model> session={session} id={id} kind="models">{(model, path) => <ModelPage session={session} model={model} path={path} tab={tab} />}</CatalogRecord>; }
export { ModelReadinessChecklist };

// ---------------------------------------------------------------------------
// Connection record: Details, Models on this connection, Authentication.
// ---------------------------------------------------------------------------
export function ProviderDetail({ session, id, tab, onTabChange }: DetailProps) { return <CatalogRecord<Provider> session={session} id={id} kind="providers">{p => <ConnectionRecord session={session} provider={p} tab={tab} onTabChange={onTabChange} />}</CatalogRecord>; }
function ConnectionRecord({ session, provider: p, tab, onTabChange }: { session: Session; provider: Provider; tab?: string; onTabChange: (tab: string) => void }) {
  const writable = session.capabilities.platform_write, models = useChoices<Model>(`${platformPath}/models?provider_connection_id=${enc(p.id)}`);
  const add = <Button render={<ResourceLink search={{ page: "model-new", connection: p.id }} />}><Plus aria-hidden />Add model</Button>;
  return <RecordPage title={p.name} meta={<><ProviderBadge profile={p.provider} label={providerLabel(p.provider)} /><Status enabled={p.enabled} /></>} description={`${providerLabel(p.provider)} connection. Configuration only: this page makes no upstream call or readiness claim.`} back={{ label: "Connections", search: { page: "providers" } }} tab={tab} onTabChange={onTabChange}
    actions={writable && <><CatalogStatusButton kind="providers" record={p} name={p.name} />{add}</>}
    facts={[{ label: "Profile", value: <WithIcon icon={<ProviderIcon profile={p.provider} />}>{providerLabel(p.provider)}</WithIcon> }, { label: "Endpoint", value: p.endpoint ? code(p.endpoint) : p.provider === "bedrock" ? "Regional endpoint" : "Provider default" }, { label: "Region", value: p.region ?? undefined }, { label: "Authentication", value: p.auth_mode === "none" ? "Explicit no authentication · approved local endpoint" : p.provider === "bedrock" ? awsAuthLabel(p.aws_auth) : "Environment credential reference · hidden" }, { label: "Status", value: <Status enabled={p.enabled} /> }, { label: "Models", value: p.model_count === undefined ? "Unknown" : String(p.model_count) }, { label: "Identifier", value: code(p.id) }]}
    sections={[
      { id: "models", title: "Models on this connection", tabLabel: "Models", icon: <Boxes aria-hidden />, count: models.data?.length ?? p.model_count, content: models.isError ? <ErrorNotice error={models.error} retry={() => void models.refetch()} /> : !models.data ? <p role="status">Loading models…</p> : models.data.length ? <ul className={t.linkList}>{models.data.map(m => <li key={m.id}><ResourceLink search={{ page: "model-detail", record: m.id }}>{m.display_name}</ResourceLink> <span className={s.muted}>({m.public_name})</span> <ReadinessBadge model={m} /></li>)}</ul> : <p className={s.muted}>No models yet.{writable ? " Add one to start routing requests through this connection." : ""}</p> },
      { id: "settings", title: p.provider === "bedrock" ? "AWS access" : "Authentication reference", tabLabel: "Settings", icon: <Settings aria-hidden />, hidden: !writable, content: p.provider === "bedrock" ? <ChangeAwsAccess provider={p} /> : <RotateProviderReference provider={p} /> },
    ]}>
    <Alert tone="info">Endpoint approval, pinned network destinations and credential allowlists are enforced by the server.</Alert>
  </RecordPage>;
}
/** Replaces a Bedrock connection's AWS identity and VPC endpoint; status is preserved. */
function ChangeAwsAccess({ provider }: { provider: Provider }) { const ask = useAction(); return <div className={t.inlineActions}><p className={s.note}>Stored references are never shown. Restate the identity to change it.</p><Button variant="secondary" onClick={() => ask({ title: `Change AWS access for ${provider.name}`, fields: awsAccessFields(undefined, provider), submitLabel: "Save AWS access", run: (v, signal) => api(`${platformPath}/providers/${enc(provider.id)}`, { method: "PATCH", body: { enabled: provider.enabled, ...awsAccessBody(v) }, signal }) })}>Change AWS access</Button></div>; }
function RotateProviderReference({ provider }: { provider: Provider }) { const ask = useAction(); return <div className={t.inlineActions}><p className={s.note}>The existing reference is never returned. New requests resolve the replacement; the connection's status is preserved.</p><Button variant="secondary" onClick={() => ask({ title: `Rotate reference for ${provider.name}`, fields: [referenceField], submitLabel: "Rotate reference", run: (v, signal) => api(`${platformPath}/providers/${enc(provider.id)}`, { method: "PATCH", body: { enabled: provider.enabled, credential_ref: `env:${v.credential_variable}` }, signal }) })}>Rotate credential reference</Button></div>; }

// ---------------------------------------------------------------------------
// Route page (/admin/deployments/{id}): Back to the model, Previous/Next across its routes.
// ---------------------------------------------------------------------------
export function DeploymentDetail({ session, id, tab }: DetailProps) { return <CatalogRecord<RouteDetail> session={session} id={id} kind="deployments">{(d, path) => <RoutePage session={session} route={d} path={path} tab={tab} />}</CatalogRecord>; }

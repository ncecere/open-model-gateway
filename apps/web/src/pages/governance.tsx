import { useState } from "react";
import { api, wsPath, platformPath, platformWorkspacePath, type Session, type Workspace, type Key, type Deployment, type Model, type Provider } from "../lib/api";
import { nameField, type Field } from "../lib/forms";
import { permissions } from "../lib/permissions";
import { integerField, passiveHealth, residencyError, modelRoutingBody, deploymentRoutingBody, type PolicyResponse, type ModelRouting, type DeploymentRouting, type Price } from "../lib/governance";
import { formatCount } from "../lib/reports";
import { CopyId } from "../components/templates/copy-id";
import { Badge, Button, CollectionTable, DateTime, Empty, ErrorNotice, FormField, Heading, NativeSelect, Panel, StatCard, Stack, Inline, Alert, Table, useAction, useApi, useChoices, useCollection } from "../components/ui";
import { PriceEditorDialog, PriceLinesView } from "../components/price-editor";
import { priceDisplayLines, priceTokenCeilings, workloadOf } from "../lib/pricing";
import { Tabs, TabsList, Tab, TabsPanel } from "../components/ui/tabs/tabs";
import { useDashboardNavigation, ResourceLink } from "../components/navigation-link";
import { SettingsForm } from "../components/templates/settings-form";
import type { Scope } from "./workspace";
import { PlatformLimits } from "./settings/limits";
import { ScopeLimits } from "../components/scope-limits";
import { EffectiveAccess } from "../components/effective-access";
import s from "./shared.module.css";
import t from "../components/resource-page.module.css";
const id = encodeURIComponent;
export function Governance({ session, workspace }: Scope) {
  // Settings › Limits (Team/Project admins): tighten-only workspace caps. Effective access is the separate Access tab.
  // Key caps live on each key's page (old ?scope=keys links redirect there; lib/locations.ts).
  const writable = permissions(session, workspace).managePolicy;
  // Tabs hold cards, never a second page title (review rule 1): ScopeLimits is the "Workspace limits" card.
  return <Stack gap={6}><ScopeLimits mode="local" path={`${wsPath(workspace.id)}/policy`} writable={writable} kind={workspace.kind} readOnlyReason={workspace.kind === "personal" ? "Personal limits are set by a Platform Admin." : "Only workspace admins change these limits."} /><p className={s.note}>Cap a single key from <ResourceLink search={{ page: "keys", ws: workspace.id }}>API keys</ResourceLink>.</p></Stack>;
}
/** Admin › Limits: one pill tab per scope (installation ceiling, type defaults), `?tab=` (pages/settings/limits.tsx; Admin › Settings › Defaults & limits). */
export function PlatformPolicies({ session, tab, onTabChange }: { session: Session; tab?: string; onTabChange?: (tab: string) => void }) { return <PlatformLimits session={session} tab={tab} onTabChange={onTabChange} />; }
/** Admin › Team/Project › Limits: live type defaults or this workspace's replacement override (components/scope-limits.tsx). */
export function WorkspacePlatformPolicy({ session, workspace }: Scope) { return <Stack gap={6}><ScopeLimits mode="replacement" path={`${platformWorkspacePath(workspace.id)}/policy`} writable={session.capabilities.platform_write} kind={workspace.kind} /><EffectiveAccess workspace={workspace} /></Stack>; }
/** Usage & costs moved to pages/usage/* (Overview | Explore | Records + chart page); re-exported for existing imports. */
export { Costs, PlatformCosts } from "./usage/usage-costs";
export { CacheAccounting, MeterAccounting, meterUsageText, meterSummary, tokenUsageText } from "./usage/accounting";
/** Token ceilings for display; hidden for meters a v3 price marks not applicable. */
export function ceilingText(p: Pick<Price, "pricing_version" | "price_lines" | "input_token_limit" | "output_token_limit">): string {
  const c = priceTokenCeilings(p), parts = [c.input ? `${formatCount(p.input_token_limit)} input` : "", c.output ? `${formatCount(p.output_token_limit)} output` : ""].filter(Boolean);
  return parts.length ? `${parts.join(" / ")} tokens` : "No token meters";
}
export function ModelRoutingPanel({ path, writable = true }: { path: string; writable?: boolean }) { return <Panel title="Model routing"><ModelRoutingForm path={path} writable={writable} /></Panel>; }
/** The model routing policy form without its card, for the model record page. */
/** A model's routing policy as fields (read-only description list on the model page, its Edit dialog, the legacy form). */
export function modelRoutingFields(policy: ModelRouting): Field[] { return [{ name: "strategy", label: "Strategy", type: "select", required: true, value: policy.strategy, options: [{ value: "priority", label: "Priority" }, { value: "weighted", label: "Weighted within priority tiers" }] }, { ...integerField("max_attempts", "Maximum attempts", 1, 3), value: String(policy.max_attempts) }, { name: "allow_ambiguous_failover", label: "Ambiguous failover", type: "select", required: true, value: String(policy.allow_ambiguous_failover), options: [{ value: "false", label: "Disabled · recommended" }, { value: "true", label: "Allow · duplicate charges possible" }] }, { name: "required_residency", label: "Required region", value: policy.required_residency ?? "", maxLength: 64, validate: residencyError }]; }
export const modelRoutingHelp = "By default each request tries one route. Fallback only happens before any response is sent, and only to routes in the required region.";
export function ModelRoutingForm({ path, writable = true }: { path: string; writable?: boolean }) { const q = useApi<{ policy: ModelRouting }>(path); return q.isPending ? <p role="status">Loading model routing…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <SettingsForm writable={writable} fields={modelRoutingFields(q.data.policy)} description={modelRoutingHelp} onSave={(v, signal) => api(path, { method: "PUT", body: modelRoutingBody(v), signal })} />; }
/** Priority, weight and circuit settings of one route, for an ActionDialog. */
export function deploymentRoutingFields(routing: DeploymentRouting["routing"]): Field[] { return [{ ...integerField("priority", "Priority", -2147483648), value: String(routing.priority), help: "Lower numbers are tried first." }, { ...integerField("weight", "Weight", 1, 1000), value: String(routing.weight), help: "Share of traffic within a priority tier when the model uses weighted routing." }, { name: "residency", label: "Region you declare (not verified)", value: routing.residency ?? "", validate: residencyError, help: "A region you declare; the gateway doesn't verify it." }, { ...integerField("failure_threshold", "Failure threshold"), value: String(routing.failure_threshold) }, { ...integerField("cooldown_seconds", "Cooldown seconds", 1, 3600), value: String(routing.cooldown_seconds) }]; }
export function DeploymentRoutingPanel({ path, operator }: { path: string; operator: boolean }) { const q = useApi<DeploymentRouting>(path); return q.isPending ? <p role="status">Loading deployment routing…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <Stack gap={6}><Panel title="Passive observations"><dl className={s.details}><dt>Health</dt><dd>{passiveHealth(q.data.health)}</dd><dt>Last observed</dt><dd>{q.data.health.last_observed_at ?? "Unknown"}</dd><dt>Cooldown until</dt><dd>{q.data.health.open_until ?? "No recorded cooldown"}</dd></dl><p>The region is one you declare; the gateway doesn't verify it. Health observations do not establish current upstream readiness.</p></Panel><Panel title="Deployment routing"><SettingsForm writable={operator} fields={[{ ...integerField("priority", "Priority", -2147483648), value: String(q.data.routing.priority) }, { ...integerField("weight", "Weight", 1, 1000), value: String(q.data.routing.weight) }, { name: "residency", label: "Region you declare (not verified)", value: q.data.routing.residency ?? "", validate: residencyError }, { ...integerField("failure_threshold", "Failure threshold"), value: String(q.data.routing.failure_threshold) }, { ...integerField("cooldown_seconds", "Cooldown seconds", 1, 3600), value: String(q.data.routing.cooldown_seconds) }]} onSave={(v, signal) => api(path, { method: "PUT", body: deploymentRoutingBody(v, q.data.routing, operator), signal })} /></Panel></Stack>; }
/** "Publish price version" plus the OpenRouter import, once the route's model (workload) and connection (profile) are known. */
function PublishPriceVersion({ path, deployment }: { path: string; deployment?: Deployment }) {
  const deploymentId = deployment?.id ?? /\/deployments\/([^/]+)\/prices$/.exec(path)?.[1] ?? "";
  const route = useApi<Deployment>(`${platformPath}/deployments/${deploymentId}`, !deployment && !!deploymentId), d = deployment ?? route.data;
  const model = useApi<Model>(`${platformPath}/models/${id(d?.model_id ?? "")}`, !!d), connection = useApi<Provider>(`${platformPath}/providers/${id(d?.provider_connection_id ?? "")}`, !!d);
  const current = useCollection<Price>(`${path}?limit=1&offset=0`), [editing, setEditing] = useState<{ importOnOpen: boolean } | null>(null);
  const ready = !!d && !!model.data && !current.isPending, profile = connection.data?.provider;
  return <Inline gap={2}><Button disabled={!ready} onClick={() => setEditing({ importOnOpen: false })}>Publish price version</Button>{profile === "openrouter" && <Button variant="secondary" disabled={!ready} onClick={() => setEditing({ importOnOpen: true })}>Import current OpenRouter price</Button>}{model.isError && <span className={s.note}>The route's model could not be loaded, so its workload is unknown.</span>}
    {editing && ready && <PriceEditorDialog deployment={d!} workload={workloadOf(model.data!.supported_protocols)} profile={profile} current={current.data?.data[0]} importOnOpen={editing.importOnOpen} onClose={() => setEditing(null)} />}</Inline>;
}
/** One "Price" cell: every version's display lines; not-applicable meters collapse into one muted line. */
function PriceColumn({ price }: { price: Price }) {
  const lines = priceDisplayLines(price), na = lines.filter(l => l.notApplicable);
  return <ul aria-label="Price lines" className={s.plainList}>{lines.filter(l => !l.notApplicable).map((line, i) => <li key={i}>{line.text}</li>)}{na.length > 0 && <li className={s.muted}>Not applicable: {na.map(l => l.label).join(", ")}</li>}</ul>;
}
export function PriceVersions({ path, writable, deployment }: { path: string; writable: boolean; deployment?: Deployment }) {
  return <Stack gap={6}>{writable && <PublishPriceVersion path={path} deployment={deployment} />}<CollectionTable<Price> path={path} label="Price versions" empty="Unpriced. Missing rates and usage are unknown, never free." rowKey={p => p.id} columns={[{ title: "Version", render: p => <><DateTime value={p.created_at} /><span className={s.secondary}>Pricing v{p.pricing_version} · <CopyId value={p.id} label="price version ID" /></span></> }, { title: "Price", render: (p: Price) => <PriceColumn price={p} /> }, { title: "Hard ceilings", narrow: true, render: p => ceilingText(p) }]} /><Alert tone="info">New prices apply to new requests; past usage keeps its price. Prices are US dollars per unit, exact to the micro-dollar. Cache-write tiers are included in the cache-write total, not added to it.</Alert></Stack>;
}
export { Pricing } from "./pricing-overview";
export function Routing({ session }: { session: Session }) { const models = useChoices<Model>(`${platformPath}/models`, session.capabilities.platform_read), [selected, setSelected] = useState(""); return <Stack gap={6} className={s.page}><Heading title="Routing" description="Explicit bounded failover. Each model page now shows its routing policy and routes together." />{models.isError ? <ErrorNotice error={models.error} /> : <FormField label="Model"><NativeSelect value={selected} onChange={e => setSelected(e.target.value)}><option value="">Choose…</option>{models.data?.map(m => <option key={m.id} value={m.id}>{m.public_name}</option>)}</NativeSelect></FormField>}{selected && <ModelRoutingPanel key={selected} path={`${platformPath}/models/${id(selected)}/routing`} writable={session.capabilities.platform_write} />}{selected && <ResourceLink search={{ page: "model-detail", record: selected }}>Open model page</ResourceLink>}</Stack>; }

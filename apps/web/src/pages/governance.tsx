import { useEffect, useRef, useState } from "react";
import { api, orgPath, wsPath, platformPath, type Organization, type Session, type Workspace, type Key, type Model, type Deployment } from "../lib/api";
import { permissions } from "../lib/permissions";
import type { Field } from "../lib/forms";
import { canReconcile, formatMicroUsd, integerField, passiveHealth, residencyError, policyBody, policyFields, effectivePolicy, modelRoutingBody, deploymentRoutingBody, priceFields, priceBody, type Policy, type ModelRouting, type DeploymentRouting, type Price, type Cost, type CostSummary } from "../lib/governance";
import { saveCsv, usageCsv } from "../lib/usage-export";
import { Badge, Button, CollectionTable, DateTime, Empty, ErrorNotice, FormField, Heading, Id, NativeSelect, Panel, StatCard, useAction, useApi, useChoices } from "../components/ui";
import type { Scope } from "./workspace";
import { Tabs, TabsList, Tab, TabsPanel } from "../components/ui/tabs/tabs";

type OrgScope = { session: Session; organization: Organization };
const id = encodeURIComponent;
const count = (value: number | null) => value == null ? "Unknown" : value.toLocaleString();
export function PolicyPanel({ title, path, writable, ceilingLabel = "Inherited platform / organization limits", platform = false, organizationPolicy = false }: { title: string; path: string; writable: boolean; ceilingLabel?: string; platform?: boolean; organizationPolicy?: boolean }) {
  const query = useApi<{ policy: Policy; ceiling?: Policy }>(path);
  const ask = useAction();
  return <Panel title={title}>{query.isPending ? <p role="status">Loading policy…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : <>
    {platform ? <><h3>Platform-assigned ceiling</h3><PolicyValues policy={query.data.policy} platform /></> : <Tabs key={path} defaultValue="effective" className="gateway-policy-views">
      <TabsList variant="pills" className="gateway-pill-list" aria-label={`${title} details`}>
        <Tab value="effective">Effective</Tab><Tab value="local">{organizationPolicy ? "Additional" : "Local limits"}</Tab><Tab value="inherited">{organizationPolicy ? "Platform" : "Inherited"}</Tab>
      </TabsList>
      <TabsPanel value="effective"><h3>{organizationPolicy ? "Effective organization caps" : "Effective limits"}</h3>{query.data.ceiling ? <PolicyValues policy={effectivePolicy(query.data.policy, query.data.ceiling)} effective /> : <p className="help">Effective limits unavailable because inherited limits could not be loaded. Parent restrictions still apply.</p>}<p className="help">The lower of each local and parent limit applies. Parent allowances are shared, not reserved for this scope.</p></TabsPanel>
      <TabsPanel value="local"><h3>{organizationPolicy ? "Additional organization limits" : "Limits at this scope"}</h3><PolicyValues policy={query.data.policy} /></TabsPanel>
      <TabsPanel value="inherited"><h3>{ceilingLabel} · read-only</h3>{query.data.ceiling ? <PolicyValues policy={query.data.ceiling} inherited /> : <p className="help">Inherited limits unavailable. Parent restrictions still apply.</p>}</TabsPanel>
    </Tabs>}
    {organizationPolicy && <p className="help">Optional stricter caps set by your organization, across all teams, projects and personal use combined. Clearing these fields never removes a platform maximum.</p>}
    {writable ? <Button variant="secondary" onClick={() => ask({ title: organizationPolicy ? "Edit additional organization limits" : `Edit ${title.toLowerCase()}`, description: organizationPolicy ? "You are changing additional organization limits, not platform maximums. Enter a lower cap, or leave a field blank to use the platform maximum. Platform maximums cannot be overridden. Existing tighter workspace and key limits still apply." : platform ? "Set the platform ceiling for this organization. Existing tighter child limits still apply. Blank means no platform cap for that field." : "Blank means shared parent allowance, not unlimited access. Scope limits may only tighten inherited ceilings. These are caps, not reserved allocations; child limits do not need to sum to the parent. Minute windows and calendar months use UTC.", fields: policyFields(query.data.policy, query.data.ceiling), run: values => api(path, { method: "PUT", body: policyBody(values) }) })}>{organizationPolicy ? "Edit additional organization limits" : `Edit ${title.toLowerCase()}`}</Button> : <p className="help">Read-only · administrators manage limits for their scope.</p>}
  </>}</Panel>;
}
function PolicyValues({ policy, inherited = false, platform = false, effective = false }: { policy: Policy; inherited?: boolean; platform?: boolean; effective?: boolean }) {
  const empty = effective ? "No configured cap" : platform ? "No platform cap" : inherited ? "No configured parent cap" : "Shared parent allowance";
  return <dl className="details"><dt>Attempts / minute</dt><dd>{policy.requests_per_minute ?? empty}</dd><dt>Tokens / minute</dt><dd>{policy.tokens_per_minute ?? empty}</dd><dt>Concurrency</dt><dd>{policy.concurrent_requests ?? empty}</dd><dt>Monthly budget</dt><dd>{policy.monthly_budget_microusd === null ? empty : formatMicroUsd(policy.monthly_budget_microusd)}</dd></dl>;
}
export function Governance({ session, organization, workspace }: OrgScope & { workspace?: Workspace }) {
  const p = permissions(session, organization, workspace);
  return <><Heading title={!workspace && p.organizationAdmin ? "Organization limits" : "Governance"} description={!workspace && p.organizationAdmin ? "Platform maximums are mandatory. Your organization may add stricter caps, but cannot override those maximums." : "Organization, workspace, and key limits compose; a child policy never overrides a parent limit."} />
    {p.managePolicy && !p.organizationAdmin && <p className="help">Delegated administrators can only lower existing scope caps. Ask an organization administrator to raise or remove an explicit limit, even when a tighter parent currently applies.</p>}
    {workspace ? <Tabs key={`${organization.id}:${workspace.id}:${p.organizationAdmin}`} defaultValue="workspace" className="gateway-policy-scopes">
      <TabsList variant="pills" className="gateway-pill-list" aria-label="Limit scope">
        <Tab value="workspace">Workspace</Tab>{p.organizationAdmin && <Tab value="organization">Organization</Tab>}<Tab value="keys">API keys</Tab>
      </TabsList>
      <TabsPanel value="workspace"><PolicyPanel title="Workspace policy" path={`${wsPath(workspace.id)}/policy`} writable={p.managePolicy} /></TabsPanel>
      {p.organizationAdmin && <TabsPanel value="organization"><PolicyPanel title="Organization limits" path={`${orgPath(organization.id)}/policy`} ceilingLabel="Platform maximums (cannot be overridden)" writable organizationPolicy /></TabsPanel>}
      <TabsPanel value="keys"><KeyPolicies workspace={workspace} writable={p.managePolicy} /></TabsPanel>
    </Tabs> : p.organizationAdmin ? <PolicyPanel title="Organization limits" path={`${orgPath(organization.id)}/policy`} ceilingLabel="Platform maximums (cannot be overridden)" writable organizationPolicy /> : null}
    <details className="notice governance-notes"><summary>How limits and budget estimates work</summary><p>Token and budget enforcement require explicit output limits and configured pricing bounds. The full upstream input ceiling is reserved, not a guessed token estimate. Unknown charges retain holds; existing unpriced usage can block a new budget until the UTC month rolls over and cannot be retroactively priced.</p><p>Admission counts upstream attempts, including fallbacks. Limits use fixed UTC minute windows, UTC calendar months, and database concurrency leases. Budget figures are configured-rate estimates in US dollars, not invoices.</p></details>
  </>;
}
function KeyPolicies({ workspace, writable }: { workspace: Workspace; writable: boolean }) {
  const path = `${wsPath(workspace.id)}/keys`;
  const keys = useChoices<Key>(path);
  const [selected, setSelected] = useState("");
  const key = keys.data?.find(key => key.id === selected);
  return <><Panel title="Key policy selection">{keys.isPending ? <p role="status">Loading visible keys…</p> : keys.isError ? <ErrorNotice error={keys.error} retry={() => void keys.refetch()} /> : keys.data.length ? <FormField label="API key" description="Only visible keys are listed. Rotations share policy and consumption with their predecessors. Separate new keys have separate scopes; use workspace/organization limits for overall ceilings."><NativeSelect value={key?.id ?? ""} onChange={event => setSelected(event.target.value)}><option value="">Choose a key…</option>{keys.data.map(key => <option key={key.id} value={key.id}>{key.name}{key.revoked_at ? " · revoked" : ""}</option>)}</NativeSelect></FormField> : <Empty title="No visible keys">Create a key in the API keys screen to inspect its limits.</Empty>}</Panel>{key && <PolicyPanel key={key.id} title="Key policy" path={`${path}/${id(key.id)}/policy`} writable={writable} />}</>;
}
export function Costs({ session, organization, workspace }: Scope) {
  const p = permissions(session, organization, workspace);
  const path = wsPath(workspace.id);
  const query = useApi<CostSummary>(`${path}/cost-summary`);
  const ask = useAction();
  const exportAbort = useRef<AbortController | null>(null);
  useEffect(() => () => exportAbort.current?.abort(), []);
  const download = () => ask({ title: "Download a bounded CSV page", successNotice: "CSV page downloaded.", description: "Exports visible execution, known usage, and cost fields only. No prompts or secrets. Download at most 1,000 rows per page; advance the offset manually for subsequent pages. This is not a complete invoice or snapshot.", submitLabel: "Download CSV", fields: [{ ...integerField("limit", "Rows", 1, 1000), value: "1000" }, { ...integerField("offset", "Offset", 0, 100000), value: "0" }], run: async values => {
    exportAbort.current?.abort();
    const controller = new AbortController(); exportAbort.current = controller;
    const offset = Number(values.offset);
    const blob = await usageCsv(workspace.id, Number(values.limit), offset, controller.signal);
    controller.signal.throwIfAborted(); saveCsv(blob, offset);
  } });
  const reconcile = (cost: Cost) => ask({ title: "Reconcile unknown cost", description: "Platform operator action: supply authoritative token counts and an evidence reference, never a prompt or invoice upload. Reconciliation uses the original pinned price and atomically settles the ledger and audit record. It cannot establish vendor invoice accuracy.", submitLabel: "Reconcile usage", fields: [integerField("input_tokens", "Authoritative input tokens", 0), integerField("output_tokens", "Authoritative output tokens", 0), { name: "evidence", label: "Authoritative evidence reference", required: true, maxLength: 200, help: "A provider usage-report or support reference; no secrets or prompt contents." }], run: values => api(`${path}/costs/${id(cost.id)}/reconcile`, { method: "POST", body: { input_tokens: Number(values.input_tokens), output_tokens: Number(values.output_tokens), evidence: values.evidence } }) });
  return <><Heading title="Costs" description={`Current UTC month · ${p.workspaceAdmin ? "Workspace-wide activity" : "Only activity from your own human keys"} · Estimates, not vendor invoices`} actions={<Button variant="secondary" onClick={download}>Download CSV page</Button>} />
    {query.isPending ? <p role="status">Loading cost summary…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : <div className="stats">
      <StatCard label="Known estimated cost · USD" value={formatMicroUsd(query.data.known_cost_microusd)} hint="Reported usage at pinned configured rates" />
      <StatCard label="Held reservations · USD" value={formatMicroUsd(query.data.held_microusd)} hint="Not a settled charge or additional invoice total" />
      <StatCard label="Unknown-cost attempts" value={count(query.data.unknown_cost_requests)} hint="Missing cost is never zero" />
      <StatCard label="Upstream attempts" value={count(query.data.requests)} hint="Includes fallback attempts · current UTC month" />
    </div>}
    <p className="help">Known estimates exclude unresolved charges. Held reservations are shown separately, not added to known costs. Unpriced records may have no hold and still block a budget. Platform operators can reconcile only visible, unresolved terminal records with a pinned price; personal workspaces remain private.</p>
    <CollectionTable<Cost> path={`${path}/costs`} label="Cost records" empty="No visible cost records. Costs appear only after real executions." rowKey={cost => cost.id} columns={[
      { title: "Started", render: cost => <><DateTime value={cost.started_at} /><Id value={cost.id} /></> },
      { title: "Model / provider", render: cost => <><code>{cost.public_model}</code><div className="muted">{cost.provider}</div></> },
      { title: "State / cost status", render: cost => <><Badge>{cost.state}</Badge><div>{cost.cost_status}</div></> },
      { title: "Known cost · USD", render: cost => formatMicroUsd(cost.cost_microusd) },
      { title: "Held · USD", render: cost => cost.reserved_microusd === null ? "No recorded hold" : formatMicroUsd(cost.reserved_microusd) },
      { title: "Input / output", render: cost => `${count(cost.input_tokens)} / ${count(cost.output_tokens)}` },
      { title: "Pinned price", render: cost => cost.price_id ? <Id value={cost.price_id} /> : "Unpriced" },
      ...(p.reconcileCosts ? [{ title: "Actions", render: (cost: Cost) => canReconcile(cost) ? <Button size="sm" variant="secondary" onClick={() => reconcile(cost)}>Reconcile</Button> : <span className="muted">{cost.cost_microusd !== null ? "Settled" : !cost.price_id ? "No pinned price" : "Not terminal"}</span> }] : []),
    ]} />
  </>;
}
function CatalogSelection<T extends { id: string }>({ title, path, value, onChange, label }: { title: string; path: string; value: string; onChange: (value: string) => void; label: (item: T) => string }) {
  const query = useChoices<T>(path);
  return <Panel>{query.isPending ? <p role="status">Loading {title.toLowerCase()}…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : query.data.length ? <FormField label={title}><NativeSelect value={value} onChange={event => onChange(event.target.value)}><option value="">Choose…</option>{query.data.map(item => <option key={item.id} value={item.id}>{label(item)}</option>)}</NativeSelect></FormField> : <Empty title={`No ${title.toLowerCase()} available`}>Configure the catalog first. No example data is substituted.</Empty>}</Panel>;
}
export function Routing({ session }: { session: Session; organization?: Organization }) {
  const [model, setModel] = useState("");
  const [deployment, setDeployment] = useState("");
  const path = platformPath;
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  return <><Heading title="Routing" description="Bounded, explicit failover with passive failure observations. No external uptime claim." />
    <p className="notice">Fallbacks require identical explicit residency labels. A model’s required residency also restricts the primary. Labels are operator assertions, not verified provider geography. No failover after a stream is returned, even before its first event.</p>
    <CatalogSelection<Model> title="Model alias" path={`${path}/models`} value={model} onChange={setModel} label={model => `${model.display_name} · ${model.public_name}`} />
    {model && <ModelRoutingPanel key={model} path={`${path}/models/${id(model)}/routing`} />}
    <CatalogSelection<Deployment> title="Deployment" path={`${path}/deployments`} value={deployment} onChange={setDeployment} label={deployment => `${deployment.upstream_model} · ${deployment.id}`} />
    {deployment && <DeploymentRoutingPanel key={deployment} path={`${path}/deployments/${id(deployment)}/routing`} operator={session.user.platform_admin} />}
  </>;
}
export function ModelRoutingPanel({ path }: { path: string }) {
  const query = useApi<{ policy: ModelRouting }>(path);
  const ask = useAction();
  const edit = (policy: ModelRouting) => ask({ title: "Edit model routing", description: "Default one upstream attempt; maximum three. Ambiguous failover can duplicate provider charges. Every fallback requires matching explicit residency, regardless of the required-residency setting.", fields: [
    { name: "strategy", label: "Strategy", type: "select", required: true, value: policy.strategy, options: [{ value: "priority", label: "Priority" }, { value: "weighted", label: "Weighted within priority tiers" }] },
    { ...integerField("max_attempts", "Maximum attempts", 1, 3), value: String(policy.max_attempts) },
    { name: "allow_ambiguous_failover", label: "Ambiguous failover", type: "select", required: true, value: String(policy.allow_ambiguous_failover), options: [{ value: "false", label: "Disabled (recommended)" }, { value: "true", label: "Allow · may duplicate charges" }] },
    { ...integerField("failure_threshold", "Failure threshold"), value: String(policy.failure_threshold) },
    { ...integerField("cooldown_seconds", "Cooldown seconds", 1, 3600), value: String(policy.cooldown_seconds) },
    { name: "required_residency", label: "Required residency", value: policy.required_residency ?? "", maxLength: 64, validate: value => value === "unspecified" ? "Choose an explicit residency or leave blank." : residencyError(value), help: "Restricts all attempts including primary. Blank removes this model constraint; it does not relax fallback residency matching." },
  ], run: values => api(path, { method: "PUT", body: modelRoutingBody(values) }) });
  return <Panel title="Model policy">{query.isPending ? <p role="status">Loading model policy…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : <><dl className="details"><dt>Strategy</dt><dd>{query.data.policy.strategy}</dd><dt>Maximum attempts</dt><dd>{query.data.policy.max_attempts}</dd><dt>Ambiguous failover</dt><dd>{query.data.policy.allow_ambiguous_failover ? "Opted in · duplicate charges possible" : "Disabled"}</dd><dt>Failure threshold</dt><dd>{query.data.policy.failure_threshold}</dd><dt>Cooldown</dt><dd>{query.data.policy.cooldown_seconds} seconds</dd><dt>Required residency</dt><dd>{query.data.policy.required_residency ?? "No model constraint"}</dd></dl><Button variant="secondary" onClick={() => edit(query.data.policy)}>Edit model routing</Button></>}</Panel>;
}
export function DeploymentRoutingPanel({ path, operator }: { path: string; operator: boolean }) {
  const query = useApi<DeploymentRouting>(path);
  const ask = useAction();
  const edit = (routing: DeploymentRouting["routing"]) => ask({ title: "Edit deployment routing", description: operator ? "Priorities sort ascending; weights apply within each priority tier. Residency is your operator assertion, not a geography verification." : `Residency is operator-controlled and will be preserved (${routing.residency || "not configured"}). Priorities sort ascending; weights apply within each tier.`, fields: [
    { ...integerField("priority", "Priority", -2147483648, 2147483647), value: String(routing.priority) },
    { ...integerField("weight", "Weight", 1, 1000), value: String(routing.weight) },
    ...(operator ? [{ name: "residency", label: "Asserted residency label", required: true, value: routing.residency, maxLength: 64, validate: residencyError, help: "Identical, explicit labels are required for failover." } satisfies Field] : []),
  ], run: values => api(path, { method: "PUT", body: deploymentRoutingBody(values, routing, operator) }) });
  return <Panel title="Deployment routing and passive health">{query.isPending ? <p role="status">Loading deployment routing…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : <><dl className="details"><dt>Priority / weight</dt><dd>{query.data.routing.priority} / {query.data.routing.weight}</dd><dt>Residency assertion</dt><dd>{query.data.routing.residency || "Not configured"}{!operator && " · operator-managed (read-only)"}</dd><dt>Passive health</dt><dd>{passiveHealth(query.data.health)}</dd><dt>Last observed</dt><dd>{query.data.health.last_observed_at ? <DateTime value={query.data.health.last_observed_at} /> : "Unknown"}</dd><dt>Open until</dt><dd>{query.data.health.open_until ? <DateTime value={query.data.health.open_until} /> : "No recorded cooldown"}</dd></dl><Button variant="secondary" onClick={() => edit(query.data.routing)}>Edit deployment routing</Button></>}</Panel>;
}
export function Pricing({ session }: { session: Session; organization?: Organization }) {
  const [deployment, setDeployment] = useState("");
  const path = platformPath;
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  return <><Heading title="Pricing" description="Immutable deployment price versions. Existing executions retain their original admission version. Estimates, not vendor invoices." />
    <p className="notice">Enter rates in US dollars per million tokens. Input token limits must be accurate hard upstream model ceilings: the full ceiling is reserved at admission, never an estimated prompt size. Only platform operators can publish a new version.</p>
    <CatalogSelection<Deployment> title="Deployment" path={`${path}/deployments`} value={deployment} onChange={setDeployment} label={deployment => `${deployment.upstream_model} · ${deployment.id}`} />
    {deployment && <PriceVersions key={deployment} path={`${path}/deployments/${id(deployment)}/prices`} writable={session.user.platform_admin} />}
  </>;
}
export function PriceVersions({ path, writable }: { path: string; writable: boolean }) {
  const ask = useAction();
  const publish = () => ask({ title: "Publish immutable price version", description: "The new version applies to subsequent admissions only. Verify rates and hard upstream token ceilings. This is a configured estimate, not an invoice. Versions cannot be edited or deleted.", submitLabel: "Publish version", fields: priceFields(), run: values => api(path, { method: "POST", body: priceBody(values) }) });
  return <>{writable ? <Button onClick={publish}>Publish price version</Button> : <p className="help">Read-only access · platform operators publish prices.</p>}<CollectionTable<Price> path={path} label="Price versions" empty="This deployment is unpriced. Missing cost is unknown, never free. Versions are listed newest first." rowKey={price => price.id} columns={[
    { title: "Created", render: price => <DateTime value={price.created_at} /> }, { title: "Version", render: price => <Id value={price.id} /> },
    { title: "Input · USD / million", render: price => formatMicroUsd(price.input_microusd_per_million) }, { title: "Output · USD / million", render: price => formatMicroUsd(price.output_microusd_per_million) },
    { title: "Input ceiling", render: price => count(price.input_token_limit) }, { title: "Output ceiling", render: price => count(price.output_token_limit) },
  ]} /><p className="help">Versions are immutable, ordered newest first. Publishing does not retroactively change execution costs.</p></>;
}

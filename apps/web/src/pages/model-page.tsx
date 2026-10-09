/*
 * Model page and route page (Admin; Auditor read-only). One long page with a
 * sticky SectionNav, OpenRouter-style, inside the Grounded shell: header
 * StatTiles, then Overview, Routes, Pricing, Routing policy, Availability,
 * Protocols and Usage. Every edit happens in place (dialogs from the existing
 * actions); there are no drawers. A route opens its own routed page
 * (/admin/routes/{id}; breadcrumb Admin › Models › {model} › {connection}
 * route, H1 "{connection} route" with the upstream ID as secondary text) with
 * Back to the model and Previous/Next across the model's routes in the same
 * order as the Routes table. Enable/disable is the header action only.
 *
 * Data: GET /platform/deployments?model_id (routes), GET
 * /platform/deployments/{id} (provider, data policy, latest price, features;
 * ux-api-contract §10) and /routing per route. Prices are exact integer
 * micro-USD per unit; unknown stays unknown (amber), never $0.
 */
import { useEffect, useState, type ReactNode } from "react";
import { useQueries } from "@tanstack/react-query";
import { Activity, CircleDollarSign, FileCode2, Layers, LayoutDashboard, Library, Pencil, Plus, Route as RouteIcon, ShieldQuestion, Split, History } from "lucide-react";
import { api, platformPath, type Catalog, type Deployment, type Model, type ModelProtocol, type Provider, type ServerPolicy, type Session } from "../lib/api";
import { deploymentRoutingBody, formatMicroUsd, modelRoutingBody, passiveHealth, type DeploymentRouting, type ModelRouting, type Price, type WorkloadKind } from "../lib/governance";
import { HEADLINE_METERS, METER_SPECS, basePrice, cheapestPrice, countNoun, formatAudio, priceItems, priceTokenCeilings, unitFor, workloadLabels, workloadOf, type BasePrice } from "../lib/pricing";
import { formatCount } from "../lib/reports";
import { modelReadiness, modelSectionFor, protocolEndpoints, protocolLabel, protocolOptions, protocolProfiles, readinessText, retiredWorkloads, routeDataPolicy, routeSectionFor, routeTiers, workloadModalities, type RouteTiers } from "../lib/model-setup";
import { parseCheckboxValues, type Field } from "../lib/forms";
import { ResourceLink } from "../components/navigation-link";
import { LabBadge, LabIcon, ProviderIcon, TitleWithIcon, WithIcon } from "../components/provider-icon";
import { Alert, Badge, Button, DateTime, ErrorNotice, useAction, useApi, useApiScope, useChoices, useCollection } from "../components/ui";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { DescriptionList, type DescriptionEntry } from "../components/ui/description-list/description-list";
import { Stack } from "../components/ui/layout/layout";
import { Disclosure } from "../components/ui/disclosure/disclosure";
import { PageHeader } from "../components/templates/page-header";
import { Table, Td, Th, Tr, type TableColumn } from "../components/ui/table/table";
import { useResourceName, useResourceParent } from "../components/layout/breadcrumbs";
import { ActionMenu } from "../components/templates/action-menu";
import { BackLink } from "../components/templates/form-page";
import { Checklist, type ChecklistStep } from "../components/templates/checklist";
import { ReadOnlyFields, fieldDisplay } from "../components/templates/settings-form";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { SectionNav, SectionNavLayout, type SectionNavItem } from "../components/templates/section-nav";
import { PriceLines } from "../components/templates/price-line";
import { DataPolicyBadge } from "../components/templates/data-policy-badge";
import { TableDividerRow } from "../components/templates/table-rows";
import { PrevNext } from "../components/templates/prev-next";
import { CopyId } from "../components/templates/copy-id";
import { SectionHeadings } from "../components/ui";
import { PriceEditorDialog } from "../components/price-editor";
import { RouteBatchScheduling, editBatchScheduling, useBatchScheduling } from "../components/batch-scheduling";
import { catalogStatusActions, ConnectionNames, ReadinessBadge, deploymentCreateAction, modelEditBody, modelEditFields, useServerPolicy } from "./catalog";
import { PriceVersions, deploymentRoutingFields, modelRoutingFields, modelRoutingHelp } from "./governance";
import { WhoGetsIt } from "./model-access";
import s from "./shared.module.css";
import m from "./models.module.css";

const enc = encodeURIComponent;
const code = (value: string) => <code className={s.mono}>{value}</code>;

// ---------------------------------------------------------------------------
// Route data shared by both pages
// ---------------------------------------------------------------------------
/** GET /platform/deployments/{id} (contract §10); every addition is optional for older gateways. */
export type RouteDetail = Deployment & { provider?: string; connection_enabled?: boolean; region?: string | null; residency?: string | null; protocols?: ModelProtocol[]; workload?: WorkloadKind; data_policy?: { data_collection: string; basis: string }; price?: (Omit<Price, "deployment_id"> & { deployment_id?: string }) | null; features?: string[] };
export type RouteRow = { id: string; route: Deployment; detail?: RouteDetail; routing?: DeploymentRouting; enabled: boolean; connectionEnabled?: boolean; priority?: number; weight?: number };
export const routePath = (id: string) => `${platformPath}/deployments/${enc(id)}`;
const routingPath = (id: string) => `${routePath(id)}/routing`;
/** The model's routes with details and routing, in the Routes table order (default, fallback, not serving). */
export function useModelRoutes(modelId: string) {
  const scope = useApiScope(), list = useChoices<Deployment>(`${platformPath}/deployments?model_id=${enc(modelId)}`), policy = useApi<{ policy: ModelRouting }>(`${platformPath}/models/${enc(modelId)}/routing`);
  const ids = list.data?.map(d => d.id) ?? [];
  const details = useQueries({ queries: ids.map(id => ({ queryKey: ["api", scope, routePath(id)], retry: false, queryFn: ({ signal }: { signal: AbortSignal }) => api<RouteDetail>(routePath(id), { signal }) })) });
  const routings = useQueries({ queries: ids.map(id => ({ queryKey: ["api", scope, routingPath(id)], retry: false, queryFn: ({ signal }: { signal: AbortSignal }) => api<DeploymentRouting>(routingPath(id), { signal }) })) });
  const rows: RouteRow[] = (list.data ?? []).map((route, i) => { const detail = details[i]?.data?.id === route.id ? details[i].data : undefined, routing = routings[i]?.data; return { id: route.id, route, detail, routing, enabled: route.enabled, connectionEnabled: detail?.connection_enabled, priority: routing?.routing.priority, weight: routing?.routing.weight }; });
  const tiers = routeTiers(rows, policy.data?.policy);
  return { list, policy, rows, tiers, ordered: [...tiers.primary, ...tiers.fallback, ...tiers.unknown, ...tiers.inactive] };
}
type RouteTier = "primary" | "fallback" | "inactive";
const tierOf = (tiers: RouteTiers<RouteRow>, id: string): RouteTier | undefined => tiers.primary.some(r => r.id === id) ? "primary" : tiers.fallback.some(r => r.id === id) ? "fallback" : tiers.inactive.some(r => r.id === id) ? "inactive" : undefined;
const tierBadge: Record<RouteTier, ReactNode> = { primary: <BitopBadge size="sm" tone="success">Default route</BitopBadge>, fallback: <BitopBadge size="sm" tone="neutral">Fallback only</BitopBadge>, inactive: <BitopBadge size="sm" tone="neutral">Not serving</BitopBadge> };
const asPrice = (route: RouteRow | RouteDetail | undefined): Price | null | undefined => { const d = route && "route" in route ? route.detail : route; return d?.price ? { deployment_id: d.id, ...d.price } as Price : d ? null : undefined; };
/** "$0.10 / M input tokens" text for tiles; unknown, not applicable and loading are explicit. */
function priceText(price: BasePrice): string | null { return price.amount !== null ? `${formatMicroUsd(price.amount)} / ${price.unit}` : price.notApplicable ? "Not applicable" : null; }
/**
 * Header tile text for one or two meters: the amounts as the value ("$0.10 / $0.50", short enough for a tile) and the
 * units as the hint ("per M input tokens / M output tokens"). Unknown stays "Unknown", never $0.
 */
export function tilePrices(prices: BasePrice[]): { value: string | null; units: string } {
  const amounts = prices.map(p => p.amount !== null ? formatMicroUsd(p.amount) : "notApplicable" in p && p.notApplicable ? "N/A" : "Unknown");
  return { value: prices.some(p => p.amount !== null) ? amounts.join(" / ") : null, units: `per ${prices.map(p => p.unit).join(" / ")}` };
}
function ceilingText(prices: (Price | null | undefined)[]): string | null {
  const known = prices.filter((p): p is Price => !!p);
  if (!known.length) return null;
  const input = known.filter(p => priceTokenCeilings(p).input), output = known.filter(p => priceTokenCeilings(p).output);
  if (!input.length && !output.length) return "Not token-limited";
  const max = (list: Price[], key: "input_token_limit" | "output_token_limit") => list.reduce((n, p) => Math.max(n, p[key]), 0);
  return [input.length ? `${formatCount(max(input, "input_token_limit"))} in` : "", output.length ? `${formatCount(max(output, "output_token_limit"))} out` : ""].filter(Boolean).join(" · ");
}
const coolingDown = (routing?: DeploymentRouting) => !!routing?.health.open_until && Date.parse(routing.health.open_until) > Date.now();
/** One status badge plus short notes underneath (connection off, cooling down), so the column stays narrow and nothing is clipped. */
function RouteStatus({ row }: { row: RouteRow }) {
  const notes = [row.detail?.connection_enabled === false ? "Connection off" : "", coolingDown(row.routing) ? "Cooling down" : ""].filter(Boolean);
  return <span className={m.statusCell}><Badge tone={row.enabled ? "good" : "neutral"}>{row.enabled ? "Enabled" : "Disabled"}</Badge>{notes.map(n => <span key={n} className={m.warnNote}>{n}</span>)}</span>;
}
/** Short labels for the Routes table column; the full label and its basis are in the tooltip. */
const shortPolicy: Record<"keeps" | "no_keep" | "unknown", string> = { no_keep: "Not collected", keeps: "May collect", unknown: "Unknown" };
function PolicyBadge({ detail }: { detail?: RouteDetail }) { const p = routeDataPolicy(detail?.data_policy); return <span title={`${p.label} · ${p.detail}`}><DataPolicyBadge policy={p.policy} label={shortPolicy[p.policy]} /><span className="sr-only"> ({p.label})</span></span>; }

// ---------------------------------------------------------------------------
// Model page
// ---------------------------------------------------------------------------
type ModelPageProps = { session: Session; model: Model; path: string; tab?: string };
function useSectionFromTab(id: string | undefined) {
  useEffect(() => { if (!id) return; const el = document.getElementById(id); if (!el) return; el.scrollIntoView?.({ block: "start" }); if (!el.hasAttribute("tabindex")) el.setAttribute("tabindex", "-1"); el.focus({ preventScroll: true }); }, [id]);
}
const jumpTo = (id: string) => { const el = document.getElementById(id); el?.scrollIntoView?.({ behavior: "smooth", block: "start" }); };
export function ModelPage({ session, model, path, tab }: ModelPageProps) {
  const writable = session.capabilities.platform_write, ask = useAction(), policy = useServerPolicy(session);
  const set = useModelRoutes(model.id), connections = useChoices<Provider>(`${platformPath}/providers`);
  // GET /platform/models/{id} carries created_at (wave 2); older gateways leave it unknown.
  const workload = workloadOf(model.supported_protocols);
  useResourceName(model.display_name);
  useSectionFromTab(modelSectionFor(tab));
  const edit = () => ask({ title: `Edit ${model.display_name}`, description: "Changing the API model name changes what clients must send.", fields: modelEditFields(model), submitLabel: "Save model", run: (v, signal) => api(path, { method: "PATCH", body: modelEditBody(v), signal }) });
  const addRoute = () => { if (connections.data) ask(deploymentCreateAction({ model, models: [model], providers: connections.data })); };
  // The lab logo is left of the title like the list rows (ui-principles 11); the lab badge keeps just its name.
  const labIds = [model.public_name, model.display_name, ...set.rows.map(row => row.route.upstream_model)];
  const r = model.readiness, serving = [...set.tiers.primary, ...set.tiers.fallback, ...set.tiers.unknown], servingPrices = serving.map(asPrice);
  // Sections are read-only description lists; each editable one has its own Edit (a dialog), so Auditors see plain values (review #22, rule 12).
  const membership = useApi<{ catalog_ids: string[] }>(`${path}/catalogs`), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`);
  const sectionEdit = (label: string, onClick: () => void, disabled = false) => writable ? <Button size="sm" variant="secondary" disabled={disabled} onClick={onClick} aria-label={label}><Pencil aria-hidden />Edit</Button> : undefined;
  const editRouting = () => { const current = set.policy.data?.policy; if (current) ask({ title: "Edit routing policy", description: modelRoutingHelp, fields: modelRoutingFields(current), submitLabel: "Save routing policy", successNotice: "Routing policy saved.", run: (v, signal) => api(`${path}/routing`, { method: "PUT", body: modelRoutingBody(v), signal }) }); };
  const editAvailability = () => { if (membership.data && catalogs.data) ask({ title: "Edit availability", description: catalogRemovalHelp, fields: [catalogField(membership.data.catalog_ids, catalogs.data)], submitLabel: "Save availability", successNotice: "Availability saved.", run: (v, signal) => api(`${path}/catalogs`, { method: "PUT", body: { catalog_ids: parseCheckboxValues(v.catalog_ids) }, signal }) }); };
  const sections: (SectionNavItem & { title: string; description?: ReactNode; actions?: ReactNode; content: ReactNode })[] = [
    { id: "overview", label: "Overview", icon: <LayoutDashboard aria-hidden />, title: "Overview", actions: sectionEdit("Edit model details", edit), content: <ModelOverview model={model} policy={policy} writable={writable} profiles={connections.data} /> },
    { id: "routes", label: "Routes", icon: <RouteIcon aria-hidden />, title: "Routes", description: "Tried in order; fallback follows the routing policy.", actions: writable && <Button size="sm" variant="secondary" disabled={!connections.data} onClick={addRoute}><Plus aria-hidden />Add route</Button>, content: <RoutesTable set={set} model={model} workload={workload} writable={writable} /> },
    { id: "pricing", label: "Pricing", icon: <CircleDollarSign aria-hidden />, title: "Pricing", description: "Estimates, not invoices. A missing rate is unknown, not free.", content: <RoutePricing set={set} model={model} workload={workload} writable={writable} /> },
    { id: "routing", label: "Routing policy", icon: <Split aria-hidden />, title: "Routing policy", description: modelRoutingHelp, actions: sectionEdit("Edit routing policy", editRouting, !set.policy.data), content: <><RoutingPolicySummary query={set.policy} />{set.tiers.tieNote && <Alert tone="warning">{set.tiers.tieNote}</Alert>}</> },
    { id: "availability", label: "Availability", icon: <Library aria-hidden />, title: "Availability", description: "Through catalogs, or by direct assignment.", actions: sectionEdit("Edit availability", editAvailability, !membership.data || !catalogs.data || !catalogs.data.length), content: <ModelAvailability model={model} membership={membership} catalogs={catalogs} /> },
    { id: "protocols", label: "Protocols", icon: <FileCode2 aria-hidden />, title: "Protocols", description: `${model.supported_protocols.map(protocolLabel).join(" · ")}. Anything else is refused.`, content: <Disclosure title="Endpoint details" keepMounted><ProtocolsPanel protocols={model.supported_protocols} routes={set.rows} /></Disclosure> },
    { id: "usage", label: "Usage", icon: <Activity aria-hidden />, title: "Usage", content: <UsageLink model={model} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <PageHeader title={<TitleWithIcon icon={<LabIcon model={labIds} size="xl" />}>{model.display_name}</TitleWithIcon>} meta={<><LabBadge model={labIds} icon={false} /><ReadinessBadge model={model} policy={policy} /></>} description={<>{code(model.public_name)}{model.description ? <> · {model.description}</> : null}</>} breadcrumbs={<BackLink label="Models" search={{ page: "models" }} />}
      actions={writable ? catalogStatusActions(ask, "models", model, model.display_name) : undefined} />
    <StatTileGrid label="Model summary" columns={4}>
      <StatTile label="Type" value={workloadModalities[workload]} hint={model.supported_protocols.map(protocolLabel).join(" · ")} />
      <ModelPriceTile workload={workload} prices={servingPrices} loading={set.list.isPending} />
      <StatTile label="Context ceilings" value={set.list.isPending ? "…" : ceilingText(servingPrices)} hint="Largest per-request token ceilings of serving routes" />
      <StatTile label="Routes" value={r ? `${r.enabled_routes} of ${r.routes} enabled` : null} hint={set.list.data ? `${set.tiers.primary.length} default · ${set.tiers.fallback.length} fallback only` : undefined} />
    </StatTileGrid>
    <SectionNavLayout nav={<SectionNav items={sections.map(({ id, label, icon }) => ({ id, label, icon }))} />}>
      <div className={m.sections}>{sections.map(section => <Card key={section.id} id={section.id} title={section.title} titleAs="h2" description={section.description} actions={section.actions}><SectionHeadings>{section.content}</SectionHeadings></Card>)}</div>
    </SectionNavLayout>
  </Stack>;
}
function ModelPriceTile({ workload, prices, loading }: { workload: WorkloadKind; prices: (Price | null | undefined)[]; loading: boolean }) {
  const meters = HEADLINE_METERS[workload].slice(0, 2), parts = meters.map(meter => cheapestPrice(prices, meter));
  const label = workload === "generation" || workload === "systemone" ? "Input / output price" : meters.length > 1 ? "Unit prices" : workload === "embeddings" ? "Input price" : "Unit price";
  if (loading) return <StatTile label={label} value="…" />;
  const tile = tilePrices(parts.map(p => p.price)), title = parts.map(p => priceText(p.price) ?? "Unknown").join(" · ");
  const basis = !prices.length ? "No serving route" : parts.some(p => p.someUnknown) ? "Cheapest serving route · some routes unpriced" : parts.some(p => p.varies) ? "Cheapest serving route · routes differ" : "Serving routes";
  return <StatTile label={label} value={tile.value === null ? null : <span title={title}>{tile.value}</span>} hint={tile.value === null ? basis : <>{tile.units}<br />{basis}</>} />;
}
function ModelOverview({ model, policy, writable, profiles }: { model: Model; policy?: ServerPolicy; writable: boolean; profiles?: Provider[] }) {
  // API name and description are the subtitle, status the header badge, protocols the Type tile: listed once each.
  const facts: DescriptionEntry[] = [{ label: "Connections", value: <ConnectionNames model={model} profiles={profiles} /> }, { label: "ID", value: <CopyId value={model.id} label="model ID" /> }, ...(model.created_at ? [{ label: "Created", value: <DateTime value={model.created_at} /> }] : [])];
  return <Stack gap={5}><DescriptionList items={facts} dividers /><ModelReadinessChecklist model={model} writable={writable} policy={policy} onJump={jumpTo} /></Stack>;
}
/** Server counts only; fix actions jump to the section that resolves the step and are omitted for read-only viewers. */
export function ModelReadinessChecklist({ model, writable, onJump, policy }: { model: Model; writable: boolean; onJump?: (section: string) => void; policy?: ServerPolicy }) {
  const ask = useAction(), r = model.readiness;
  const retired = retiredWorkloads[workloadOf(model.supported_protocols)];
  if (retired) return <Alert tone="warning" title={readinessText.retired}>{retired} Requests are refused with unsupported_capability; no job is created.</Alert>;
  if (!r) return <Alert tone="info">This gateway did not report readiness for the model, so it is shown as unknown rather than ready.</Alert>;
  const readiness = modelReadiness(model, policy), jump = (id: string, label: string) => !writable ? undefined : onJump ? { label, onClick: () => onJump(id) } : { label, render: <a href={`#${id}`} /> };
  const steps: ChecklistStep[] = [
    { id: "enabled", title: "Model is enabled", description: model.enabled ? "Eligible for new requests." : "Disabled models never serve requests.", done: model.enabled, action: writable ? { label: "Enable", onClick: () => ask({ title: `Enable ${model.display_name}?`, description: "This changes eligibility for new inference requests. It is not a readiness test.", submitLabel: "Enable", run: (_, signal) => api(`${platformPath}/models/${enc(model.id)}`, { method: "PATCH", body: { enabled: true }, signal }) }) } : undefined },
    { id: "route", title: "Has an enabled route", description: `${r.routes} route${r.routes === 1 ? "" : "s"}, ${r.enabled_routes} enabled. A route also needs its connection enabled.`, done: r.enabled_routes > 0, action: jump("routes", "Review routes") },
    { id: "pricing", title: "Enabled routes are priced", description: r.enabled_routes ? `${r.priced_enabled_routes} of ${r.enabled_routes} enabled route${r.enabled_routes === 1 ? "" : "s"} priced. Unpriced usage is recorded as unknown cost.` : "Recommended once a route is enabled.", done: r.enabled_routes > 0 && r.priced_enabled_routes >= r.enabled_routes, action: jump("pricing", "Set prices") },
    { id: "offered", title: "Offered to workspaces", description: `In ${r.catalogs} catalog${r.catalogs === 1 ? "" : "s"}; directly assigned to ${r.direct_workspaces} workspace${r.direct_workspaces === 1 ? "" : "s"}.`, done: r.catalogs > 0 || r.direct_workspaces > 0, action: jump("availability", "Choose catalogs") },
  ];
  // Configuration checks (best effort): shown only when they find something.
  if (readiness.warnings.includes("token_ceiling")) steps.push({ id: "ceilings", title: "Token ceilings fit tokens-per-minute defaults", description: `${r.routes_over_token_limit} enabled route${r.routes_over_token_limit === 1 ? "'s" : "s'"} input + output token ceilings exceed the ${r.type_tokens_per_minute != null ? `${r.type_tokens_per_minute.toLocaleString("en-US")} ` : ""}tokens-per-minute limit of a workspace type that can use this model. Each request reserves both ceilings, so those workspaces are refused (token_reservation_exceeds_limit). Lower the price's token ceilings or raise the limit. This is a configuration check; overrides and local limits are not included.`, done: false, action: jump("pricing", "Review ceilings") });
  if (readiness.warnings.includes("free_blocked")) steps.push({ id: "free", title: "Free OpenRouter endpoints are usable", description: `${r.openrouter_free_routes} enabled route${r.openrouter_free_routes === 1 ? " uses" : "s use"} an OpenRouter :free model. Free endpoints may train on prompts, and this server denies data collection, so OpenRouter rejects those requests. Use a paid model ID, or have the operator allow data collection.`, done: false, action: jump("routes", "Review routes") });
  const list = <Checklist embedded title="Readiness" steps={steps} complete={readiness.state === "ready" ? "Ready: enabled, routed, priced and offered." : undefined} />;
  // Nothing to do when ready: the steps stay one click away (progressive disclosure).
  return readiness.state === "ready" ? <Disclosure title="Readiness" summary="Ready: enabled, routed, priced and offered." keepMounted>{list}</Disclosure> : list;
}

export type RouteSet = ReturnType<typeof useModelRoutes>;
export function RoutesTable({ set, model, workload, writable }: { set: RouteSet; model: Model; workload: WorkloadKind; writable: boolean }) {
  if (set.list.isError) return <ErrorNotice error={set.list.error} retry={() => void set.list.refetch()} />;
  if (!set.list.data) return <p role="status">Loading routes…</p>;
  if (!set.rows.length) return <p className={s.muted}>No routes yet.{writable ? " Add a route on a connection so requests can be served." : ""}</p>;
  const { primary, fallback, unknown, inactive } = set.tiers, { columns, meters } = routeColumns(workload), span = columns.length;
  const row = (r: RouteRow) => { const connection = `${r.route.provider_name ?? r.detail?.provider_name ?? "Unknown connection"}${r.detail?.region ? ` · ${r.detail.region}` : ""}`; return <Tr key={r.id}>
    <Th scope="row"><span className={m.routeName}><span className={m.routeLine}><ProviderIcon profile={r.detail?.provider} size="sm" /><span className={m.truncate} title={r.route.upstream_model}><ResourceLink search={{ page: "deployment-detail", record: r.id }}>{r.route.upstream_model}</ResourceLink></span></span><span className={m.truncateNote} title={connection}>{connection}</span></span></Th>
    {meters.map(meter => <Td key={meter} numeric><MeterCell row={r} meter={meter} /></Td>)}
    <Td>{r.routing ? <><span className={s.primary}>{r.routing.routing.priority}</span><span className={s.secondary}>Weight {r.routing.routing.weight}</span></> : <span className={s.muted}>Unknown</span>}</Td>
    <Td><PolicyBadge detail={r.detail} /></Td>
    <Td><RouteStatus row={r} /></Td>
    <Td stickyEnd><RouteActions row={r} model={model} workload={workload} writable={writable} /></Td>
  </Tr>; };
  return <Table caption={`Routes for ${model.display_name}`} columns={columns} framed className={m.routesTable}>
    {primary.map(row)}
    {fallback.length > 0 && <TableDividerRow colSpan={span} label="Fallback only · not used by default" reason={set.tiers.fallbackReason} />}
    {fallback.map(row)}
    {unknown.length > 0 && <TableDividerRow colSpan={span} label="Routing order unknown" reason={set.policy.isError ? "The routing policy couldn't be loaded, so which route serves by default is unknown." : "Loading routing settings…"} />}
    {unknown.map(row)}
    {inactive.length > 0 && <TableDividerRow colSpan={span} label="Not serving" reason="Disabled routes, and routes on disabled connections, receive no requests." />}
    {inactive.map(row)}
  </Table>;
}
type RouteMeter = Parameters<typeof basePrice>[1];
/**
 * The Routes table's columns: the route, one price column per headline meter, priority, data policy, status and
 * the row actions (pinned to the end). Fixed widths except the route, which takes the rest and truncates; every row
 * (route, divider, not serving) spans exactly these columns.
 */
export function routeColumns(workload: WorkloadKind): { columns: TableColumn[]; meters: RouteMeter[] } {
  const meters = HEADLINE_METERS[workload].slice(0, 2);
  return { meters, columns: ["Route", ...meters.map(meter => ({ label: METER_SPECS[meter].sku, numeric: true, width: "7rem" })), { label: "Priority", width: "5.5rem" }, { label: "Data policy", width: "9rem" }, { label: "Status", width: "8rem" }, { label: "Actions", hideLabel: true, width: "3.5rem", stickyEnd: true }] };
}
/** The base rate on one line and its compact unit under it ("$0.10" / "/M tokens"); tiers are noted, the full text is in the tooltip. */
function MeterCell({ row, meter }: { row: RouteRow; meter: RouteMeter }) {
  if (!row.detail) return <span className={s.muted}>…</span>;
  const price = asPrice(row), base = basePrice(price, meter), items = price ? priceItems(price, row.detail.workload ?? workloadOf(row.detail.protocols ?? [])).find(i => i.meter === meter && !i.notApplicable) : undefined;
  if (!price) return <Badge>Unpriced</Badge>;
  if (base.amount === null && "notApplicable" in base && base.notApplicable) return <span className={s.muted}>Not applicable</span>;
  const unit = ("batch" in base && base.batch ? unitFor(meter, base.batch)?.unitLabel : undefined) ?? `/${base.unit}`, tiered = (items?.tiers?.length ?? 0) > 1;
  if (base.amount === null) return <span className={m.unknown} title={`Unknown ${METER_SPECS[meter].noun} price · not free`}>Unknown</span>;
  return <span className={m.priceCell} title={`${formatMicroUsd(base.amount)} per ${base.unit}${tiered ? " (base tier; larger prompts are priced differently)" : ""}`}><span className={s.primary}>{formatMicroUsd(base.amount)}</span><span className={s.secondary}>{unit}</span>{tiered && <span className={s.secondary}>Tiered</span>}</span>;
}
function useCurrentPrice(route: Deployment, enabled = true) { return useCollection<Price>(`${routePath(route.id)}/prices?limit=1&offset=0`, enabled); }
type Editing = { importOnOpen: boolean } | null;
function RouteEditor({ route, workload, profile, editing, onClose }: { route: Deployment; workload: WorkloadKind; profile?: string; editing: Editing; onClose: () => void }) {
  const q = useCurrentPrice(route, !!editing);
  if (!editing || q.isPending) return null;
  return <PriceEditorDialog deployment={route} workload={workload} profile={profile} current={q.data?.data[0]} importOnOpen={editing.importOnOpen} onClose={onClose} />;
}
function RouteActions({ row, workload, writable }: { row: RouteRow; model: Model; workload: WorkloadKind; writable: boolean }) {
  const { route } = row, ask = useAction(), [editing, setEditing] = useState<Editing>(null), profile = row.detail?.provider;
  // Read-only viewers get the same ⋯ menu (review #48), with only what they can do.
  if (!writable) return <ActionMenu label={`Actions for route ${route.upstream_model}`} actions={[{ label: "Route page", render: <ResourceLink search={{ page: "deployment-detail", record: route.id }} /> }, { label: "Price history", render: <ResourceLink search={{ page: "deployment-detail", record: route.id, tab: "pricing" }} /> }]} />;
  return <><ActionMenu label={`Actions for route ${route.upstream_model}`} actions={[
    { label: "Edit routing…", disabled: !row.routing, disabledReason: "Routing could not be loaded", onSelect: () => row.routing && ask({ title: `Routing · ${route.upstream_model}`, description: "No implicit retries: fallback attempts follow the model's routing policy only.", fields: deploymentRoutingFields(row.routing.routing), submitLabel: "Save routing", run: (v, signal) => api(routingPath(route.id), { method: "PUT", body: deploymentRoutingBody(v, row.routing!.routing, true), signal }) }) },
    { label: "Publish price…", onSelect: () => setEditing({ importOnOpen: false }) },
    { label: "Import current OpenRouter price…", hidden: profile !== "openrouter", onSelect: () => setEditing({ importOnOpen: true }) },
    { label: "Route page", render: <ResourceLink search={{ page: "deployment-detail", record: route.id }} /> },
    { label: route.enabled ? "Disable route…" : "Enable route…", danger: route.enabled, onSelect: () => ask({ title: `${route.enabled ? "Disable" : "Enable"} route ${route.upstream_model}?`, description: "Changes eligibility for new requests. A disabled model or connection still prevents serving.", danger: route.enabled, submitLabel: route.enabled ? "Disable route" : "Enable route", run: (_, signal) => api(routePath(route.id), { method: "PATCH", body: { enabled: !route.enabled }, signal }) }) },
  ]} /><RouteEditor route={route} workload={workload} profile={profile} editing={editing} onClose={() => setEditing(null)} /></>;
}
/** Per-route price lines with tiers, ceilings and the publish/import actions. */
function RoutePricing({ set, model, workload, writable }: { set: RouteSet; model: Model; workload: WorkloadKind; writable: boolean }) {
  if (set.list.isError) return <ErrorNotice error={set.list.error} retry={() => void set.list.refetch()} />;
  if (!set.list.data) return <p role="status">Loading routes…</p>;
  if (!set.rows.length) return <p className={s.muted}>Add a route to price it.</p>;
  return <ul className={m.priceRoutes} aria-label={`Prices for ${model.display_name}`}>{set.ordered.map(row => <RoutePriceEntry key={row.id} row={row} tier={tierOf(set.tiers, row.id)} workload={workload} writable={writable} />)}</ul>;
}
export function RoutePriceLines({ price, workload }: { price: Price | null | undefined; workload: WorkloadKind }) {
  if (price === undefined) return <p role="status" className={m.note}>Loading price…</p>;
  if (!price) return <p className={m.note}><Badge>Unpriced</Badge> Usage on this route is recorded with unknown cost.</p>;
  const items = priceItems(price, workload), na = price.price_lines?.filter(l => "not_applicable" in l) ?? [];
  const ceilings = ceilingText([price]), maxima = Object.entries(price.max_units ?? {}).filter(([, v]) => v != null).map(([meter, v]) => meter.endsWith("_audio_seconds_ms") ? `${formatAudio(v!)} ${METER_SPECS[meter as keyof typeof METER_SPECS].noun}` : countNoun(meter as keyof typeof METER_SPECS, v!));
  return <Stack gap={2}>
    <PriceLines items={items.filter(i => !i.notApplicable).map(i => ({ label: i.label, price: i.tiers ? { unit: i.unit, tiers: i.tiers } : { amount: i.amount, unit: i.unit } }))} />
    <p className={m.note}>{na.length > 0 && <>Not applicable: {na.map(l => protocolMeterName(l.meter)).join(", ")}. </>}Ceilings: {ceilings ?? "unknown"}{maxima.length ? `; per request: ${maxima.join(", ")}` : ""}. Pricing v{price.pricing_version}{price.created_at ? <>, published <DateTime value={price.created_at} /></> : null}.</p>
  </Stack>;
}
const protocolMeterName = (meter: string) => meter.replace(/_/g, " ").replace(/ ms$/, "");
function RoutePriceEntry({ row, tier, workload, writable }: { row: RouteRow; tier?: RouteTier; workload: WorkloadKind; writable: boolean }) {
  const [editing, setEditing] = useState<Editing>(null), price = asPrice(row), profile = row.detail?.provider;
  return <li className={m.priceRoute}>
    <div><p className={s.primary}><WithIcon icon={<LabIcon model={row.route.upstream_model} size="sm" />}><ResourceLink search={{ page: "deployment-detail", record: row.id }}>{row.route.upstream_model}</ResourceLink></WithIcon> {tier && tierBadge[tier]}</p><span className={s.secondary}>{row.route.provider_name ?? "Unknown connection"}</span>
      <RoutePriceLines price={row.detail ? price : undefined} workload={row.detail?.workload ?? workload} /></div>
    <div className={m.inlineActions}>{writable && <Button size="sm" variant="secondary" onClick={() => setEditing({ importOnOpen: false })}>{price ? "Publish new price" : "Set price"}</Button>}{writable && profile === "openrouter" && <Button size="sm" variant="secondary" onClick={() => setEditing({ importOnOpen: true })}>Import current OpenRouter price</Button>}<ResourceLink search={{ page: "deployment-detail", record: row.id, tab: "pricing" }}>Price history</ResourceLink></div>
    {writable && <RouteEditor route={row.route} workload={workload} profile={profile} editing={editing} onClose={() => setEditing(null)} />}
  </li>;
}
const catalogRemovalHelp = "If you remove the model from its last catalog, workspaces lose it and keys that list it stop using it. Adding it back doesn't restore those choices.";
const catalogField = (ids: string[], catalogs: Catalog[]): Field => ({ name: "catalog_ids", label: "Catalogs offering this model", type: "checkboxes", value: JSON.stringify(ids), maxSelections: 200, options: catalogs.map(c => ({ value: c.id, label: c.name })) });
function RoutingPolicySummary({ query }: { query: RouteSet["policy"] }) {
  if (query.isPending) return <p role="status">Loading routing policy…</p>;
  if (query.isError) return <ErrorNotice error={query.error} retry={() => void query.refetch()} />;
  return <ReadOnlyFields fields={modelRoutingFields(query.data.policy)} />;
}
function ModelAvailability({ model, membership, catalogs }: { model: Model; membership: ReturnType<typeof useApi<{ catalog_ids: string[] }>>; catalogs: ReturnType<typeof useChoices<Catalog>> }) {
  const direct = model.readiness?.direct_workspaces;
  const directNote = <p className={s.note}>Direct assignments: {direct === undefined ? "unknown" : `${direct} workspace${direct === 1 ? "" : "s"}`}. Assign directly from a Team or Project page.</p>;
  if (membership.isError || catalogs.isError) return <><ErrorNotice error={membership.error ?? catalogs.error} retry={() => { void membership.refetch(); void catalogs.refetch(); }} />{directNote}</>;
  if (!membership.data || !catalogs.data) return <p role="status">Loading availability…</p>;
  if (!catalogs.data.length) return <><p className={s.muted}>No catalogs yet. <ResourceLink search={{ page: "catalogs" }}>Create a catalog</ResourceLink> to offer models to workspaces.</p>{directNote}</>;
  const offering = catalogs.data.filter(c => membership.data.catalog_ids.includes(c.id));
  // Each catalog links to its page, with who gets it (type defaults and workspaces' own catalog choices).
  return <><DescriptionList dividers items={[{ label: "Catalogs offering this model", value: offering.length ? <ul className={s.plainList}>{offering.map(c => <li key={c.id}><ResourceLink search={{ page: "catalog-detail", record: c.id }}>{c.name}</ResourceLink> <WhoGetsIt catalog={c} /></li>)}</ul> : "None" }]} /><p className={s.note}>{catalogRemovalHelp} <ResourceLink search={{ page: "catalogs" }}>All catalogs</ResourceLink></p>{directNote}</>;
}
/** Client endpoints the model serves (docs/protocol-matrix.md), and which routes' adapters can carry each. */
export function ProtocolsPanel({ protocols, routes }: { protocols: ModelProtocol[]; routes: RouteRow[] }) {
  const workload = workloadOf(protocols), siblings = protocolOptions.filter(o => workloadOf([o.value]) === workload && !protocols.includes(o.value));
  return <div className={m.protocols}>
    {protocols.map(p => { const e = protocolEndpoints[p], serving = routes.filter(r => r.enabled && r.detail?.connection_enabled !== false), able = serving.filter(r => r.detail?.provider && protocolProfiles[p].includes(r.detail.provider)), unknown = serving.filter(r => !r.detail?.provider);
      return <section key={p} className={m.endpoint} aria-label={protocolLabel(p)}>
        <h3 className={m.endpointLine}><BitopBadge size="sm" variant="outline">{e.method}</BitopBadge><code className={m.code}>{e.path}</code><span className={s.muted}>{protocolLabel(p)}</span></h3>
        <DescriptionList items={[{ label: "Headers", value: <ul className={m.chips}>{e.headers.map(h => <li key={h.name}><code className={m.code}>{h.name}: {h.value}</code></li>)}</ul> }, { label: "Body", value: e.contentType }, { label: "Key parameters", value: <ul className={s.plainList}>{e.params.map(x => <li key={x.name}><code className={m.code}>{x.name}</code>{x.required ? " (required)" : ""} · {x.note}</li>)}</ul> }, { label: "Not supported", value: e.unsupported }, { label: "Adapters", value: e.adapters }, { label: "Serving routes", value: !routes.length ? "No routes yet" : able.length ? `${able.length} of ${serving.length} serving route${serving.length === 1 ? "" : "s"} can carry it${unknown.length ? ` · ${unknown.length} unknown` : ""}` : unknown.length ? "Unknown until route details load" : <span className={m.unknown}>No serving route's connection supports this protocol; requests fail with unsupported_capability.</span> }]} dividers />
      </section>; })}
    <p className={m.note}>{siblings.length ? <>Not enabled for this model: {siblings.map(o => o.label).join(", ")}. </> : null}Other workloads need their own model. Calls to an endpoint the model doesn't serve are refused (400 unsupported_capability, or 501 for text and embeddings).</p>
  </div>;
}
function UsageLink({ model }: { model: Model }) {
  return <Stack gap={3}><p className={m.note}>Counted by API model name ({code(model.public_name)}).</p><div className={m.inlineActions}><Button variant="secondary" render={<ResourceLink search={{ page: "platform-costs", tab: "records", model: model.public_name }} />}>Usage records for this model</Button><Button variant="secondary" render={<ResourceLink search={{ page: "platform-costs", tab: "explore", group: "model" }} />}>Compare models in Explore</Button></div></Stack>;
}

// ---------------------------------------------------------------------------
// Route page: /admin/routes/{id} (old /admin/deployments/{id} links are rewritten)
// ---------------------------------------------------------------------------
export function RoutePage({ session, route: d, path, tab }: { session: Session; route: RouteDetail; path: string; tab?: string }) {
  const writable = session.capabilities.platform_write, ask = useAction();
  const model = useApi<Model>(`${platformPath}/models/${enc(d.model_id)}`), set = useModelRoutes(d.model_id), routing = useApi<DeploymentRouting>(routingPath(d.id)), batch = useBatchScheduling(d.id);
  const workload = d.workload ?? (model.data ? workloadOf(model.data.supported_protocols) : workloadOf(d.protocols ?? []));
  const price = asPrice(d), modelName = model.data?.display_name ?? d.model_public_name ?? "Model", tier = tierOf(set.tiers, d.id);
  const connectionName = d.provider_name ?? "Connection", title = `${connectionName} route`;
  const index = set.ordered.findIndex(r => r.id === d.id), prev = index > 0 ? set.ordered[index - 1] : undefined, next = index >= 0 ? set.ordered[index + 1] : undefined;
  const target = (r?: RouteRow) => r && { label: `${r.route.upstream_model} on ${r.route.provider_name ?? "its connection"}`, render: <ResourceLink search={{ page: "deployment-detail", record: r.id }} /> };
  const policy = routeDataPolicy(d.data_policy), meters = HEADLINE_METERS[workload].slice(0, 2);
  // Breadcrumb: Admin › Models › {model display name} › {connection} route (review #20).
  useResourceName(title);
  useResourceParent({ label: modelName, to: { page: "model-detail", record: d.model_id } });
  useSectionFromTab(routeSectionFor(tab));
  const sections: (SectionNavItem & { title: string; description?: ReactNode; actions?: ReactNode; content: ReactNode })[] = [
    { id: "pricing", label: "Price lines", icon: <CircleDollarSign aria-hidden />, title: "Price lines", description: "Latest published price. Estimates, not invoices.", content: <><RoutePriceLines price={price} workload={workload} />{writable && <PublishControls route={d} workload={workload} />}</> },
    { id: "data-policy", label: "Data policy", icon: <ShieldQuestion aria-hidden />, title: "Data policy", content: <RouteDataPolicy route={d} /> },
    { id: "protocols", label: "Protocols & features", icon: <FileCode2 aria-hidden />, title: "Protocols & features", content: <RouteFeatures route={d} workload={workload} /> },
    { id: "routing", label: "Routing", icon: <Split aria-hidden />, title: "Routing settings", description: "Fallback follows the model's routing policy.", actions: writable ? <Button size="sm" variant="secondary" disabled={!routing.data} aria-label="Edit routing settings" onClick={() => routing.data && ask({ title: `Routing · ${title}`, description: "No implicit retries: fallback attempts follow the model's routing policy only.", fields: deploymentRoutingFields(routing.data.routing), submitLabel: "Save routing", successNotice: "Routing saved.", run: (v, signal) => api(routingPath(d.id), { method: "PUT", body: deploymentRoutingBody(v, routing.data!.routing, true), signal }) })}><Pencil aria-hidden />Edit</Button> : undefined, content: <RouteRouting query={routing} /> },
    { id: "batch-scheduling", label: "Batch scheduling", icon: <Layers aria-hidden />, title: "Batch scheduling", description: "When gateway-run batch lines may start here.", actions: writable ? <Button size="sm" variant="secondary" disabled={!batch.data} aria-label="Edit batch scheduling" onClick={() => batch.data && editBatchScheduling(ask, d.id, batch.data, title)}><Pencil aria-hidden />Edit</Button> : undefined, content: <RouteBatchScheduling query={batch} /> },
    { id: "price-history", label: "Price history", icon: <History aria-hidden />, title: "Price history", content: <PriceVersions path={`${path}/prices`} writable={false} deployment={d} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <PageHeader title={<TitleWithIcon icon={<ProviderIcon profile={d.provider} size="xl" />}>{title}</TitleWithIcon>} meta={<><Badge tone={d.enabled ? "good" : "neutral"}>{d.enabled ? "Enabled" : "Disabled"}</Badge>{tier && tierBadge[tier]}</>}
      description={<>{code(d.upstream_model)} · Route of <ResourceLink search={{ page: "model-detail", record: d.model_id }}>{modelName}</ResourceLink> on <ResourceLink search={{ page: "provider-detail", record: d.provider_connection_id }}>{d.provider_name ?? "its connection"}</ResourceLink></>}
      breadcrumbs={<BackLink label={modelName} search={{ page: "model-detail", record: d.model_id }} />}
      actions={<>{set.list.data && <PrevNext noun="route" position={index >= 0 ? { index, total: set.ordered.length } : undefined} prev={target(prev)} next={target(next)} shortcuts />}{writable && catalogStatusActions(ask, "deployments", d, d.upstream_model)}</>} />
    <StatTileGrid label="Route summary" columns={5}>
      <StatTile label="Connection" value={d.provider_name ?? null} hint={d.connection_enabled === false ? "Connection disabled · route can't serve" : d.region ? `Region ${d.region}` : d.connection_enabled ? "Connection enabled" : undefined} />
      <RoutePriceTile price={price} meters={meters} />
      <StatTile label="Context ceilings" value={price ? ceilingText([price]) : null} />
      <StatTile label="Priority / weight" value={routing.data ? `${routing.data.routing.priority} / ${routing.data.routing.weight}` : routing.isPending ? "…" : null} hint={routing.data ? passiveHealth(routing.data.health) : undefined} />
      <StatTile label="Data policy" value={<DataPolicyBadge policy={policy.policy} label={policy.label} size="md" />} hint={policy.detail} />
    </StatTileGrid>
    <SectionNavLayout nav={<SectionNav items={sections.map(({ id, label, icon }) => ({ id, label, icon }))} />}>
      <div className={m.sections}>{sections.map(section => <Card key={section.id} id={section.id} title={section.title} titleAs="h2" description={section.description} actions={section.actions}><SectionHeadings>{section.content}</SectionHeadings></Card>)}</div>
    </SectionNavLayout>
  </Stack>;
}
function RoutePriceTile({ price, meters }: { price: Price | null | undefined; meters: RouteMeter[] }) {
  const label = meters.length > 1 ? "Prices" : "Price";
  if (!price) return <StatTile label={label} value={null} hint="Unpriced: cost recorded as unknown" />;
  const bases = meters.map(meter => basePrice(price, meter)), tile = tilePrices(bases);
  return <StatTile label={label} value={tile.value === null ? null : <span title={bases.map(b => priceText(b) ?? "Unknown").join(" · ")}>{tile.value}</span>} hint={<>{tile.units}<br />Pricing v{price.pricing_version}</>} />;
}
function PublishControls({ route, workload }: { route: RouteDetail; workload: WorkloadKind }) {
  const [editing, setEditing] = useState<Editing>(null);
  return <div className={m.inlineActions}><Button size="sm" onClick={() => setEditing({ importOnOpen: false })}>Publish price version</Button>{route.provider === "openrouter" && <Button size="sm" variant="secondary" onClick={() => setEditing({ importOnOpen: true })}>Import current OpenRouter price</Button>}<RouteEditor route={route} workload={workload} profile={route.provider} editing={editing} onClose={() => setEditing(null)} /></div>;
}
function RouteDataPolicy({ route }: { route: RouteDetail }) {
  const p = routeDataPolicy(route.data_policy);
  return <Stack gap={3}>
    <div><DataPolicyBadge policy={p.policy} label={p.label} size="md" detail={p.detail} /></div>
    {p.policy === "unknown" ? <Alert tone="warning" title="Retention and training are unknown">The gateway has no data policy for this provider, so it can't say whether prompts are kept or used for training. Check the provider's terms before routing sensitive work here.</Alert>
      : <p className={m.note}>{route.provider === "openrouter" ? `Every request to OpenRouter sets provider.data_collection to "${route.data_policy?.data_collection}" from the server setting.${route.data_policy?.data_collection === "deny" ? " Free (:free) endpoints may train on prompts and are refused while it is denied." : ""}` : "From the server's configuration for this provider."}</p>}
  </Stack>;
}
const featureLabels: Record<string, string> = { cache_pricing: "Cache pricing", prompt_size_tiers: "Prompt-size price tiers", openrouter_free_variant: "OpenRouter free variant" };
function RouteFeatures({ route, workload }: { route: RouteDetail; workload: WorkloadKind }) {
  const protocols = route.protocols ?? [];
  const facts: DescriptionEntry[] = [
    { label: "Workload", value: workloadLabels[workload] },
    { label: "Client protocols", value: protocols.length ? <ul className={m.chips}>{protocols.map(p => { const able = route.provider ? protocolProfiles[p].includes(route.provider) : undefined; return <li key={p}><BitopBadge size="sm" tone={able === false ? "warning" : "neutral"}>{protocolLabel(p)}{able === false ? " · not supported by this connection" : ""}</BitopBadge></li>; })}</ul> : <span className={m.unknown}>Unknown</span> },
    { label: "Features", value: route.features ? route.features.length ? <ul className={m.chips}>{route.features.map(f => <li key={f}><BitopBadge size="sm" variant="outline">{featureLabels[f] ?? f}</BitopBadge></li>)}</ul> : "None configured" : <span className={m.unknown}>Unknown</span> },
    // The declared region is under Routing settings and the upstream ID is the subtitle: each shown once.
    ...(route.region ? [{ label: "Connection region", value: route.region }] : []),
    { label: "ID", value: <CopyId value={route.id} label="route ID" /> },
  ];
  return <DescriptionList items={facts} dividers />;
}
function RouteRouting({ query }: { query: ReturnType<typeof useApi<DeploymentRouting>> }) {
  if (query.isPending) return <p role="status">Loading routing…</p>;
  if (query.isError) return <ErrorNotice error={query.error} retry={() => void query.refetch()} />;
  const { routing, health } = query.data;
  return <DescriptionList dividers items={[...deploymentRoutingFields(routing).map(f => ({ label: f.label, value: fieldDisplay(f, f.value) })), { label: "Health check (from real traffic)", value: passiveHealth(health) }, { label: "Last observed", value: health.last_observed_at ? <DateTime value={health.last_observed_at} /> : "Unknown" }, { label: "Cooldown until", value: health.open_until ? <DateTime value={health.open_until} /> : "No recorded cooldown" }]} />;
}

/*
 * Models catalog (Admin) and the read-only workspace variant. OpenRouter-style
 * catalog inside the Grounded/Bitop shell: type tabs with counts, then one
 * URL-backed FilterToolbar row (search, facets with counts, price range, Sort
 * and List/Table at its end; in place behind "Filters (n)" on a phone, never
 * a sheet), and List (compact two-line rows) or Table views; the result count
 * sits after the type tabs.
 *
 * Admin rows: GET /platform/models (ux-api-contract §10). q, sort, data policy,
 * the input-price ceiling and "hide deprecated" are server filters; one query
 * per selected data policy is merged by id. Connection, status, readiness and
 * the price floor are applied here, so tab and facet counts are computed from
 * the same rows the server would count.
 *
 * Workspace rows: GET /workspaces/{ws}/catalog — only models eligible for the
 * workspace (selected, assigned, or available from its catalogs), with the
 * server's eligibility reason. Select/Remove are the existing self-service
 * grant calls; nothing here bypasses catalog eligibility.
 */
import { useState, type ReactNode } from "react";
import { useQueries } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { api, platformPath, wsPath, type Collection, type Grant, type Provider, type ServerPolicy, type Session, type Workspace } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { permissions } from "../lib/permissions";
import { catalogSorts, catalogTypeTabs, eligibilityLabels, readinessLabels, readinessText, filterCatalog, modelReadiness, modelWorkload, protocolLabel, retiredWorkloads, sortCatalog, typeCounts, type CatalogFilters, type CatalogModel, type CatalogSort, type Eligibility, type Readiness, type WorkspaceCatalogModel } from "../lib/model-setup";
import { HEADLINE_METERS, compareDecimal, formatDecimalMicroUsd, unitText, usdPerMillionFilter, usdToMicroUsd, workloadLabels } from "../lib/pricing";
import type { WorkloadKind } from "../lib/governance";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { IconCell, LabIcon, ProviderIcon } from "../components/provider-icon";
import { Button, DateTime, ErrorNotice, Heading, Stack, Status, useAction, useApiScope, useChoices } from "../components/ui";
import { copyIdAction } from "../components/people";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { TooltipText } from "../components/ui/tooltip/tooltip";
import type { DataTableColumn } from "../components/ui/data-table/data-table";
import { FilterToolbar, SortSelect, ViewToggle, type RangeFacet, type ToolbarFacet } from "../components/templates/filter-toolbar";
import type { FacetCounts, FilterValues } from "../components/ui/filter-bar/filter-bar";
import { HintBadge } from "../components/templates/hint-badge";
import { TypeTabs } from "../components/templates/type-tabs";
import { ColumnChooser, ViewDataTable, chooserColumns, tableViewFromSearch, tableViewToSearch, type ChooserColumn, type TableView } from "../components/templates/table-view";
import { ConnectionNames, ReadinessBadge, useServerPolicy } from "./catalog";
import { notServing } from "../lib/home";
import { ModelHeadlinePrice, useRoutePrices, type RoutePrice } from "./pricing-overview";
import { addModelsAction } from "./workspace";
import { ActionMenu } from "../components/templates/action-menu";
import { CompareBar, CompareSelect } from "./model-compare";
import { MAX_COMPARE, toggleCompare } from "../lib/model-compare";
import s from "./shared.module.css";
import m from "./models.module.css";

const enc = encodeURIComponent;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------
/** Every page of several collection paths, keyed like `useChoices` so caches are shared. */
export function useChoicesMany<T>(paths: string[], enabled = true) {
  const scope = useApiScope();
  return useQueries({ queries: paths.map(path => ({ queryKey: ["api", scope, path, "choices"], enabled, retry: false, queryFn: async ({ signal }: { signal: AbortSignal }) => {
    const rows: T[] = [];
    for (let offset = 0; offset < 20000; offset += 200) {
      const page = await api<Collection<T>>(`${path}${path.includes("?") ? "&" : "?"}limit=200&offset=${offset}`, { signal }); rows.push(...page.data);
      if (page.has_more === false || page.data.length < 200) return rows;
    }
    throw new Error("Too many models to load. Narrow the search first.");
  } })) });
}
/** Server-side part of the admin catalog query: one path per selected data policy (merged by id). */
export function catalogQueryPaths(search: DashboardSearch): string[] {
  const base = new URLSearchParams();
  if (search.q) base.set("q", search.q);
  base.set("sort", search.sort ?? "name");
  const max = usdPerMillionFilter(search.max_price);
  if (max) base.set("max_input_price", max);
  if (search.deprecated === "hide") base.set("include_deprecated", "false");
  const policies = search.policy?.split(",") ?? [];
  return (policies.length ? policies : [undefined]).map(policy => { const q = new URLSearchParams(base); if (policy) q.set("data_policy", policy); return `${platformPath}/models?${q}`; });
}
function mergeById<T extends { id: string }>(lists: (T[] | undefined)[]): T[] {
  const seen = new Map<string, T>();
  for (const list of lists) for (const row of list ?? []) if (!seen.has(row.id)) seen.set(row.id, row);
  return [...seen.values()];
}
/** URL state for this page: the router when mounted in the dashboard, local state in isolation (tests). */
function useCatalogSearch(page: DashboardSearch["page"]) {
  const nav = useDashboardNavigation(), [local, setLocal] = useState<DashboardSearch>({ page });
  const search = nav?.search ?? local;
  const go = (patch: Partial<DashboardSearch>) => { const next = { ...search, ...patch, offset: undefined }; if (nav) nav.navigate(next); else setLocal(next); };
  return [search, go] as const;
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------
const tokenInput = new Set<WorkloadKind>(["generation", "embeddings", "systemone"]);
const perUnit = (workload: WorkloadKind) => { const meter = HEADLINE_METERS[workload][0]; return `Priced per ${unitText(meter, meter === "input_audio_seconds_ms" ? 60000 : meter === "input_characters" ? 1000000 : 1)}`; };
/** Unknown stays unknown (amber, never $0): a short "Unpriced" with the full meaning as its tooltip. */
function UnknownPrice({ workload }: { workload?: WorkloadKind }) { return <TooltipText content={workload && !tokenInput.has(workload) ? "No price published · cost is recorded as unknown, not free" : "Input price unknown · not free"} className={m.unknown}>Unpriced</TooltipText>; }
/** Enabled routes with a price; undefined when not reported (older gateways). */
const pricedCount = (v: number | string | undefined) => v === undefined || v === null || !/^\d+$/.test(String(v)) ? undefined : Number(v);
/**
 * "From $0.10 / M input tokens"; unknown stays unknown (amber) for token-priced workloads. A unit-priced workload
 * (images, audio…) reads "Priced per image" only when a route has a price (`pricedRoutes` > 0); with none it is
 * "Unpriced", never a price summary. Table cells.
 */
export function InputPriceSummary({ value, workload, from = false, pricedRoutes }: { value: string | null | undefined; workload: WorkloadKind; from?: boolean; pricedRoutes?: number }) {
  const text = formatDecimalMicroUsd(value);
  if (text) return <span>{from ? "From " : ""}{text}<span className={s.muted}> / M input tokens</span></span>;
  if (tokenInput.has(workload)) return <span className={m.unknown}>Input price unknown · not free</span>;
  if (pricedRoutes === 0) return <UnknownPrice workload={workload} />;
  return <span className={s.muted}>{perUnit(workload)}</span>;
}
/** One line for list rows: "$0.10 in · $0.40 out / 1M tokens", "From $0.02 / 1M tokens" or "Unpriced". */
function PriceSummary({ input, output, workload, from = false, noRoutes = false, pricedRoutes }: { input: string | null | undefined; output?: string | null; workload: WorkloadKind; from?: boolean; /** No enabled route: nothing can be priced yet, so not "Unpriced" (which the Pricing filter means as an enabled route without a price). */ noRoutes?: boolean; /** Enabled routes with a price (undefined: not reported). 0 makes a unit-priced workload "Unpriced". */ pricedRoutes?: number }) {
  const i = formatDecimalMicroUsd(input), o = workload === "embeddings" ? null : formatDecimalMicroUsd(output);
  if (!i && noRoutes) return <TooltipText content="No enabled route, so no price yet" className={s.muted}>—</TooltipText>;
  // "/M" keeps the row's price on one line at 1440 (the full unit is the tooltip).
  if (i) return <span className={m.price} title={`${from ? "From " : ""}${i}${o ? ` in · ${o} out` : workload === "embeddings" ? "" : " in"} per 1M tokens`}>{from ? "From " : ""}{i}{o ? <> in · {o} out</> : workload === "embeddings" ? null : " in"}<span className={s.muted}> /M</span></span>;
  if (tokenInput.has(workload) || pricedRoutes === 0) return <UnknownPrice workload={workload} />;
  return <span className={s.muted}>{perUnit(workload)}</span>;
}
/** "14 models" / "3 of 14 models" after the type tabs; never shown while loading or after a load error (no fabricated zero). */
function resultCount(known: boolean, total: number, shown: number) {
  if (!known || !shown) return undefined;
  return <span role="status">{shown === total ? `${total} model${total === 1 ? "" : "s"}` : `${shown} of ${total} models`}</span>;
}
/** Sort, List/Table and (Table view) Columns: the end of the catalog's toolbar row, without labels of their own. */
function CatalogActions({ search, go, sorts = catalogSorts, columns, columnIds }: { search: DashboardSearch; go: (patch: Partial<DashboardSearch>) => void; sorts?: readonly { value: CatalogSort; label: string }[]; columns?: ChooserColumn[]; columnIds: string[] }) {
  const table = search.layout === "table", view = tableViewFromSearch({ cols: search.cols, density: search.density }, columnIds);
  const setView = (next: TableView) => go(tableViewToSearch(next) as Partial<DashboardSearch>);
  return <>
    <SortSelect value={search.sort ?? "name"} options={sorts} onChange={v => go({ sort: v === "name" ? undefined : v })} />
    <ViewToggle value={table ? "table" : "list"} onChange={v => go({ layout: v === "table" ? "table" : undefined })} />
    {table && columns && <ColumnChooser columns={columns} hidden={view.hidden} onHiddenChange={hidden => setView({ ...view, hidden })} density={{ value: view.density, onChange: density => setView({ ...view, density }) }} />}
  </>;
}
const searchBox = (search: DashboardSearch, go: (patch: Partial<DashboardSearch>) => void) => ({ label: "Search models", placeholder: "Name or API name", value: search.q ?? "", onChange: (q: string) => go({ q: q || undefined }) });
const priceFacet: RangeFacet = { id: "price", label: "Input price ($/M)", type: "range", prefix: "$", inputMode: "decimal", placeholders: ["Min", "Max"], description: "Cheapest enabled route, per million input tokens. While a bound is set, models with an unknown price are left out.", validate: v => usdToMicroUsd(v).ok ? undefined : "Enter US dollars, e.g. 0.15" };
const priceNote = (search: DashboardSearch) => search.min_price || search.max_price ? "Models with an unknown price are hidden while a price bound is set." : undefined;
const csv = (v?: string) => v ? v.split(",") : [];
const joined = (v: FilterValues[string]) => Array.isArray(v) && v.length ? v.join(",") : undefined;
const first = (v: FilterValues[string]) => Array.isArray(v) ? v[0] : undefined;
/** Facet values from the URL and back. Status is the single `enabled` key. */
function facetValues(search: DashboardSearch): FilterValues {
  return { connections: search.connections ? csv(search.connections) : search.connection ? [search.connection] : [], price: search.min_price || search.max_price ? [search.min_price ?? "", search.max_price ?? ""] : [], policy: csv(search.policy), readiness: csv(search.readiness), enabled: search.enabled ? [search.enabled] : [], deprecated: search.deprecated ? ["hide"] : [], eligibility: csv(search.eligibility), pricing: search.pricing ? ["unpriced"] : [] };
}
function facetSearch(v: FilterValues): Partial<DashboardSearch> {
  const enabled = first(v.enabled), price = Array.isArray(v.price) ? v.price : [];
  return { connections: joined(v.connections), connection: undefined, min_price: price[0]?.trim() || undefined, max_price: price[1]?.trim() || undefined, policy: joined(v.policy), readiness: joined(v.readiness), enabled: enabled === "true" || enabled === "false" ? enabled : undefined, deprecated: first(v.deprecated) === "hide" ? "hide" : undefined, eligibility: joined(v.eligibility), pricing: first(v.pricing) === "unpriced" ? "unpriced" : undefined };
}
/** Type tabs: counts only when known; a type with no models at all is hidden (except All and the selected one). */
export function typeTabItems(counts: Record<string, number> | null, selected: string | undefined, present?: Record<string, number>) {
  return catalogTypeTabs.filter(t => !counts || !present || t.value === "all" || t.value === (selected ?? "all") || (present[t.value] ?? 0) > 0).map(t => ({ value: t.value, label: t.label, count: counts ? counts[t.value] ?? 0 : null }));
}
const countBy = <T,>(rows: T[], test: (row: T) => boolean) => rows.reduce((n, r) => n + (test(r) ? 1 : 0), 0);
/** Wraps a row's name with its compare checkbox (Model compare). */
type Select = (id: string, name: string, children: ReactNode) => ReactNode;
/** Picked models for Compare (2 to 4), local to the page. */
function useCompareSelection() {
  const [picked, setPicked] = useState<string[]>([]);
  const select: Select = (id, name, children) => <CompareSelect name={name} checked={picked.includes(id)} full={picked.length >= MAX_COMPARE} onChange={on => setPicked(current => toggleCompare(current, id, on))}>{children}</CompareSelect>;
  return { picked, select, clear: () => setPicked([]) };
}
const readinessOptions: { value: Readiness["state"]; label: string }[] = (["ready", "needs_setup", "needs_attention", "not_serving", "retired", "unknown"] as const).map(value => ({ value, label: readinessText[value] }));
/** The model's icon, display name and (when different) its mono API name: the first cell of every row. */
function ModelName({ display, api, search }: { display: string; api: string; search: DashboardSearch }) {
  return <IconCell icon={<LabIcon model={[api, display]} />}><ResourceLink className={m.rowLink} search={search}>{display || api}</ResourceLink>{display && display !== api && <code className={m.rowId}>{api}</code>}</IconCell>;
}

// ---------------------------------------------------------------------------
// Admin › Models
// ---------------------------------------------------------------------------
type AdminRow = CatalogModel & { workload: WorkloadKind };
/** Enabled routes without a price (their usage is recorded with unknown cost); undefined when readiness is unknown. */
export const unpricedRoutes = (m: Pick<CatalogModel, "readiness">) => m.readiness ? Math.max(0, m.readiness.enabled_routes - m.readiness.priced_enabled_routes) : undefined;
export const isUnpricedModel = (m: Pick<CatalogModel, "readiness">) => (unpricedRoutes(m) ?? 0) > 0;
/** "Unpriced" next to a model with an enabled route that has no price. */
export function UnpricedBadge({ model }: { model: Pick<CatalogModel, "readiness"> }) { const n = unpricedRoutes(model); return n ? <BitopBadge size="sm" tone="warning" dot title={`${n} enabled route${n === 1 ? " has" : "s have"} no price; cost is recorded as unknown`}>Unpriced</BitopBadge> : null; }
/** Least-used facets: behind "More filters" so the row fits one line at 1440px. */
export const adminMoreFacets = ["pricing", "policy", "readiness", "enabled", "deprecated"];
/** One status per row: Disabled, else Ready / Needs setup / Needs attention / Not serving; the reasons are its tooltip. */
export function adminStatus(model: CatalogModel, policy?: ServerPolicy): { label: string; tone: "success" | "warning" | "neutral"; hint?: string } {
  if (!model.enabled) return { label: "Disabled", tone: "neutral", hint: "Turned off. Requests are refused." };
  const r = modelReadiness(model, policy), reasons = r.warnings.map(w => readinessLabels[w]);
  if (r.state === "not_serving") return { label: readinessText.not_serving, tone: "warning", hint: "No enabled route to a provider, so requests fail." };
  if (r.state === "retired") return { label: readinessText.retired, tone: "warning", hint: retiredWorkloads[modelWorkload(model)] };
  return { label: readinessText[r.state], tone: r.state === "ready" ? "success" : r.state === "unknown" ? "neutral" : "warning", hint: reasons.join(" · ") || undefined };
}
export function Models({ session }: { session: Session }) {
  const [search, go] = useCatalogSearch("models"), allowed = session.capabilities.platform_read, compare = useCompareSelection();
  const connections = useChoices<Provider>(`${platformPath}/providers`, allowed), policy = useServerPolicy(session);
  const queries = useChoicesMany<CatalogModel>(catalogQueryPaths(search), allowed);
  // Route prices only for the Table view's Price column (one request per route, like the former Pricing page).
  const table = search.layout === "table", routePrices = useRoutePrices(allowed && table);
  if (!allowed) return <Heading title="Access not available" />;
  const failed = queries.find(q => q.isError), loading = queries.some(q => q.isPending);
  const rows: AdminRow[] = mergeById(queries.map(q => q.data)).map(r => ({ ...r, workload: modelWorkload(r) }));
  const filters: CatalogFilters = { connections: search.connections ? csv(search.connections) : search.connection ? [search.connection] : undefined, enabled: search.enabled, readiness: csv(search.readiness) as Readiness["state"][], minPrice: usdPerMillionFilter(search.min_price), maxPrice: usdPerMillionFilter(search.max_price) };
  const sort = search.sort ?? "name", type = search.type;
  // "Unpriced only" (the former Admin › Pricing page) is applied here, from each model's route readiness.
  const unpricedOnly = search.pricing === "unpriced", priced = (list: AdminRow[]) => unpricedOnly ? list.filter(isUnpricedModel) : list;
  const filtered = sortCatalog(priced(filterCatalog(rows, filters, policy)), sort), counts = typeCounts(filtered);
  const ofType = (list: AdminRow[]) => type ? list.filter(r => r.workload === type) : list;
  const shown = ofType(filtered);
  const without = (key: keyof CatalogFilters) => ofType(priced(filterCatalog(rows, { ...filters, [key]: undefined }, policy)));
  const byPricing = ofType(filterCatalog(rows, filters, policy));
  const byConnection = without("connections"), byReadiness = without("readiness"), byStatus = without("enabled");
  // Compact facets: multi-select popovers (with counts) for the longer lists, segmented toggles for the short ones.
  const facets: ToolbarFacet[] = [
    { id: "connections", label: "Connection", type: "select", multiple: true, placeholder: "Any connection", options: (connections.data ?? []).map(c => ({ value: c.id, label: c.name, icon: <ProviderIcon profile={c.provider} size="sm" /> })) },
    priceFacet,
    { id: "pricing", label: "Pricing", type: "toggle", allLabel: "All", options: [{ value: "unpriced", label: "Unpriced only" }] },
    { id: "policy", label: "Data collection", type: "toggle", multiple: true, allLabel: "All", options: [{ value: "deny", label: "Denied" }, { value: "allow", label: "Allowed" }, { value: "unknown", label: "Unknown" }] },
    { id: "readiness", label: "Readiness", type: "select", multiple: true, placeholder: "Any readiness", options: readinessOptions },
    { id: "enabled", label: "Status", type: "toggle", allLabel: "All", options: [{ value: "true", label: "Enabled" }, { value: "false", label: "Disabled" }] },
    // No separate deprecation state: retiring a model disables it, so this hides every disabled model (server filter).
    { id: "deprecated", label: "Retired", type: "toggle", allLabel: "Show", options: [{ value: "hide", label: "Hide" }] },
  ];
  const facetCounts: FacetCounts | undefined = loading || failed ? undefined : {
    connections: Object.fromEntries((connections.data ?? []).map(c => [c.id, countBy(byConnection, r => !!r.readiness?.connections.some(x => x.id === c.id))])),
    readiness: Object.fromEntries(readinessOptions.map(o => [o.value, countBy(byReadiness, r => modelReadiness(r, policy).state === o.value)])),
    enabled: { true: countBy(byStatus, r => r.enabled), false: countBy(byStatus, r => !r.enabled) },
    pricing: { unpriced: countBy(byPricing, isUnpricedModel) },
  };
  // Unknown while loading or after a load error: no counts, never a fabricated 0.
  const known = !loading && !failed, tabs = typeTabItems(known ? counts : null, type, typeCounts(rows));
  const columns = adminColumns({ policy, profiles: connections.data, prices: routePrices.routes.isError ? undefined : routePrices.byModel, pricesLoading: routePrices.routes.isPending, select: compare.select });
  return <Stack gap={6} className={s.page}>
    <Heading title="Models" description="Models your users can call, and the routes they use." actions={session.capabilities.platform_write && <Button render={<ResourceLink search={{ page: "model-new", connection: filters.connections?.length === 1 ? filters.connections[0] : undefined }} />}><Plus aria-hidden />Add model</Button>} />
    <TypeTabs label="Model type" items={tabs} value={type ?? "all"} onChange={v => go({ type: v === "all" ? undefined : v as DashboardSearch["type"] })} end={resultCount(known, filtered.length, shown.length)} />
    <FilterToolbar search={searchBox(search, go)} facets={facets} more={adminMoreFacets} counts={facetCounts} values={facetValues(search)} onChange={v => go(facetSearch(v))} note={priceNote(search) ?? (search.pricing ? "Unpriced: usage is recorded with unknown cost. Publish a price on the model's route." : undefined)}
      end={<CatalogActions search={search} go={go} columns={chooserColumns(columns)} columnIds={adminColumnIds} />} />
    <CompareBar count={compare.picked.length} target={{ page: "platform-model-compare", ids: compare.picked.join(",") }} onClear={compare.clear} />
    <div className={m.results}>
      {connections.isError && !failed && <ErrorNotice error={connections.error} retry={() => void connections.refetch()} />}
      {failed ? <ErrorNotice error={failed.error} retry={() => queries.forEach(q => void q.refetch())} />
        : loading ? <p role="status">Loading models…</p>
        : !shown.length ? <EmptyState size="compact" title={rows.length ? "No models match" : "No models yet"} description={rows.length ? "Change the type or clear filters." : "Add a model from a connection."} action={rows.length ? <Button variant="secondary" onClick={() => go({ ...facetSearch({}), q: undefined, type: undefined })}>Clear filters</Button> : undefined} />
        : table ? <ViewDataTable<AdminRow> caption="Models" columns={columns} data={shown} getRowId={r => r.id} hideViewControls view={tableViewFromSearch({ cols: search.cols, density: search.density }, adminColumnIds)} onViewChange={v => go(tableViewToSearch(v) as Partial<DashboardSearch>)} />
        : <ul className={m.rows} aria-label="Models">{shown.map(row => <AdminListRow key={row.id} model={row} policy={policy} profiles={connections.data} select={compare.select} />)}</ul>}
    </div>
  </Stack>;
}
/** Two-line row (Grounded's models list): name + API name | type | status | price | connection | ⋯. Details live on the model page. */
function AdminListRow({ model, policy, profiles, select }: { model: AdminRow; policy?: ServerPolicy; profiles?: Provider[]; select?: Select }) {
  const r = model.readiness, status = adminStatus(model, policy), conns = r?.connections ?? [], lead = conns[0];
  const profile = lead && profiles?.find(p => p.id === lead.id)?.provider, unpriced = unpricedRoutes(model);
  const detail = { page: "model-detail", record: model.id } as const;
  return <li className={`${m.row} ${m.adminRow}`}>
    <span className={m.rowName}>{select ? select(model.id, model.display_name || model.public_name, <ModelName display={model.display_name} api={model.public_name} search={detail} />) : <ModelName display={model.display_name} api={model.public_name} search={detail} />}</span>
    <span className={m.rowType}><BitopBadge size="sm" variant="outline">{workloadLabels[model.workload]}</BitopBadge></span>
    <span className={m.rowStatus}><HintBadge tone={status.tone} hint={status.hint}>{status.label}</HintBadge></span>
    <span className={m.rowPrice}><PriceSummary input={model.min_input_microusd_per_million} workload={model.workload} from={(r?.enabled_routes ?? 0) > 1} noRoutes={r?.enabled_routes === 0} pricedRoutes={r?.priced_enabled_routes} />{!!unpriced && (r?.priced_enabled_routes ?? 0) > 0 && <> <TooltipText content={`${unpriced} enabled route${unpriced === 1 ? " has" : "s have"} no price; its cost is recorded as unknown`} className={m.unknown}>· Unpriced</TooltipText></>}</span>
    <span className={m.rowSource}>{!r ? <span className={m.unknown}>Unknown</span> : !lead ? <span className={s.muted}>No routes</span> : <IconCell icon={profile ? <ProviderIcon profile={profile} size="sm" /> : null}><span className={m.sourceWrap} title={conns.map(c => c.name).join(", ")}><ResourceLink search={{ page: "provider-detail", record: lead.id }}>{lead.name}</ResourceLink>{conns.length > 1 && <span className={s.muted}> +{conns.length - 1}</span>}</span></IconCell>}</span>
    <span className={m.rowActions}><ActionMenu label={`Actions for ${model.display_name}`} actions={[{ label: "View details", render: <ResourceLink search={detail} /> }, copyIdAction(model.public_name, "Copy API name")]} /></span>
  </li>;
}
export const adminColumnIds = ["type", "price", "pricing", "connections", "routes", "readiness", "status", "created"];
/** Admin table columns. Price: headline prices of the cheapest priced enabled route (PriceLine; prices are published on route pages). */
function adminColumns({ policy, profiles, prices, pricesLoading, select }: { policy?: ServerPolicy; profiles?: Provider[]; prices?: Map<string, RoutePrice[]>; pricesLoading?: boolean; select?: Select }): DataTableColumn<AdminRow>[] {
  const name = (r: AdminRow) => <IconCell icon={<LabIcon model={[r.public_name, r.display_name]} />}><ResourceLink search={{ page: "model-detail", record: r.id }}>{r.display_name}</ResourceLink><span className={s.secondary}>{r.public_name}</span></IconCell>;
  return [
    { id: "model", header: "Model", rowHeader: true, hideable: false, sortable: true, accessor: r => r.display_name, cell: r => select ? select(r.id, r.display_name || r.public_name, name(r)) : name(r) },
    { id: "type", header: "Type", sortable: true, accessor: r => workloadLabels[r.workload] },
    { id: "price", header: "Price", label: "Price", sortable: true, accessor: r => r.min_input_microusd_per_million ?? "", sortFn: (a, b) => compareDecimal(a.min_input_microusd_per_million, b.min_input_microusd_per_million), cell: r => pricesLoading ? <span className={s.muted}>Loading…</span> : prices ? <ModelHeadlinePrice routes={prices.get(r.id)} workload={r.workload} /> : <InputPriceSummary value={r.min_input_microusd_per_million} workload={r.workload} pricedRoutes={r.readiness?.enabled_routes ? r.readiness.priced_enabled_routes : undefined} /> },
    { id: "pricing", header: "Priced routes", label: "Priced routes", accessor: r => unpricedRoutes(r) ?? -1, sortable: true, cell: r => r.readiness ? <><span>{r.readiness.priced_enabled_routes} of {r.readiness.enabled_routes} enabled</span>{isUnpricedModel(r) && <span className={s.badges}><UnpricedBadge model={r} /></span>}</> : <span className={m.unknown}>Unknown</span> },
    { id: "connections", header: "Connections", cell: r => <ConnectionNames model={r} profiles={profiles} /> },
    { id: "routes", header: "Routes", numeric: true, accessor: r => r.readiness?.enabled_routes ?? -1, sortable: true, cell: r => r.readiness ? `${r.readiness.enabled_routes} / ${r.readiness.routes}` : <span className={m.unknown}>Unknown</span> },
    { id: "readiness", header: "Readiness", accessor: r => modelReadiness(r, policy).state, cell: r => <ReadinessBadge model={r} policy={policy} /> },
    { id: "status", header: "Status", accessor: r => r.enabled ? "Enabled" : "Disabled", cell: r => <Status enabled={r.enabled} /> },
    { id: "created", header: "Added", sortable: true, accessor: r => r.created_at ?? "", cell: r => <DateTime value={r.created_at} /> },
  ];
}

// ---------------------------------------------------------------------------
// Workspace › Models (read-only catalog with self-service selection)
// ---------------------------------------------------------------------------
type WorkspaceRow = WorkspaceCatalogModel & { selection?: Grant };
/** The read-only workspace model page (`/workspaces/{ws}/models/{id}`). */
export const workspaceModelSearch = (ws: string, id: string): DashboardSearch => ({ page: "workspace-model", ws, record: id });
const eligibilityTone: Record<Eligibility, "success" | "info" | "neutral"> = { selected: "success", direct: "info", available_from_catalog: "neutral" };
export function EligibilityBadge({ eligibility, hint }: { eligibility: Eligibility; /** The server's reason, as a tooltip. */ hint?: string }) { return <HintBadge size="sm" tone={eligibilityTone[eligibility] ?? "neutral"} hint={hint}>{eligibilityLabels[eligibility] ?? "Unknown eligibility"}</HintBadge>; }
/** Same rule as the access reason "no_enabled_route": a model with no enabled route (route and connection both on) can't serve. */
export const notServingNote = "No route to a provider is turned on, so requests fail. A Platform Admin can enable one.";
export function NotServingBadge({ hint }: { hint?: string }) { return <HintBadge size="sm" tone="warning" hint={hint}>{readinessText.not_serving}</HintBadge>; }
/** The Add dialog's one line: Personal has no members; shared workspaces' keys get it unless limited to other models. */
export const addModelText = (workspace: Pick<Workspace, "kind">) => workspace.kind === "personal" ? "Your keys can call it, unless a key is limited to other models." : "Members' keys can call it, unless a key is limited to other models.";
export function WorkspaceModels({ session, workspace }: { session: Session; workspace: Workspace }) {
  const [search, go] = useCatalogSearch("grants"), ask = useAction(), canSelect = permissions(session, workspace).manageGrants, compare = useCompareSelection();
  const catalog = useChoices<WorkspaceCatalogModel>(`${wsPath(workspace.id)}/catalog`), grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`);
  const grantPath = `${wsPath(workspace.id)}/models`;
  const rows: WorkspaceRow[] = (catalog.data ?? []).map(r => ({ ...r, selection: grants.data?.find(g => g.model_id === r.model_id) }));
  const minPrice = usdPerMillionFilter(search.min_price), maxPrice = usdPerMillionFilter(search.max_price), eligibility = csv(search.eligibility);
  const priced = (r: WorkspaceRow) => (minPrice === undefined || r.min_input_microusd_per_million != null && compareDecimal(r.min_input_microusd_per_million, minPrice) >= 0) && (maxPrice === undefined || r.min_input_microusd_per_million != null && compareDecimal(r.min_input_microusd_per_million, maxPrice) <= 0);
  const base = sortCatalog(rows.filter(priced), search.sort ?? "name");
  const filtered = base.filter(r => !eligibility.length || eligibility.includes(r.eligibility)), counts = typeCounts(filtered);
  const ofType = (list: WorkspaceRow[]) => search.type ? list.filter(r => r.workload === search.type) : list;
  const shown = ofType(filtered), loading = catalog.isPending, known = !loading && !catalog.isError;
  const available = rows.filter(r => r.eligibility === "available_from_catalog");
  const select = (r: WorkspaceRow) => ask({ title: `Add ${r.display_name}?`, description: addModelText(workspace), submitLabel: "Add model", successNotice: `Added ${r.display_name}.`, run: (_, signal) => api(grantPath, { method: "POST", body: { model_id: r.model_id }, signal }) });
  const remove = (r: WorkspaceRow) => ask({ title: `Remove ${r.display_name}?`, description: r.selection?.direct_granted ? "A Platform Admin also assigned it, so it stays available." : "Keys can't call it any more. Adding it back doesn't restore it on keys limited to selected models.", danger: true, submitLabel: "Remove model", successNotice: `Removed ${r.display_name}.`, run: (_, signal) => api(`${grantPath}/${enc(r.model_id)}`, { method: "DELETE", signal }) });
  // "Add" is the row's one visible action; Remove lives in the row's ⋯ menu (a dozen identical buttons were noise).
  const actions = (r: WorkspaceRow) => !canSelect ? null : r.eligibility === "available_from_catalog" ? <Button size="sm" variant="secondary" onClick={() => select(r)}>Add</Button> : r.selection?.catalog_granted ? <ActionMenu label={`Actions for ${r.display_name}`} actions={[{ label: "Remove from workspace…", danger: true, onSelect: () => remove(r) }]} /> : null;
  const eligibilities = ["selected", "direct", "available_from_catalog"] as const;
  const facets: ToolbarFacet[] = [
    { id: "eligibility", label: "Show", type: "toggle", multiple: true, allLabel: "All", options: eligibilities.map(e => ({ value: e, label: eligibilityLabels[e] })) },
    priceFacet,
  ];
  const facetCounts: FacetCounts | undefined = !known ? undefined : { eligibility: Object.fromEntries(eligibilities.map(e => [e, countBy(ofType(base), r => r.eligibility === e)])) };
  const columns = workspaceColumns(workspace.id, compare.select);
  return <Stack gap={6} className={s.page}>
    <Heading title="Models" description={workspace.kind === "personal" ? "Models you can call from your personal workspace." : "Models this workspace can call. Keys can be limited further."} actions={canSelect && <Button variant="secondary" disabled={!available.length} onClick={() => ask(addModelsAction(grantPath, available.map(r => ({ model_id: r.model_id, display_name: r.display_name, public_name: r.public_name }) as Grant)))}><Plus aria-hidden />Add models</Button>} />
    <TypeTabs label="Model type" items={typeTabItems(known ? counts : null, search.type, typeCounts(rows))} value={search.type ?? "all"} onChange={v => go({ type: v === "all" ? undefined : v as DashboardSearch["type"] })} end={resultCount(known, filtered.length, shown.length)} />
    <FilterToolbar search={searchBox(search, go)} facets={facets} counts={facetCounts} values={facetValues(search)} onChange={v => { const next = facetSearch(v); go({ eligibility: next.eligibility, min_price: next.min_price, max_price: next.max_price }); }} note={priceNote(search)}
      end={<CatalogActions search={search} go={go} sorts={catalogSorts} columns={chooserColumns(columns)} columnIds={workspaceColumnIds} />} />
    <CompareBar count={compare.picked.length} target={{ page: "model-compare", ws: workspace.id, ids: compare.picked.join(",") }} onClear={compare.clear} />
    <div className={m.results}>
      {grants.isError && !catalog.isError && <ErrorNotice error={grants.error} retry={() => void grants.refetch()} />}
      {catalog.isError ? <ErrorNotice error={catalog.error} retry={() => void catalog.refetch()} />
        : loading ? <p role="status">Loading models…</p>
        : !shown.length ? <EmptyState size="compact" title={rows.length ? "No models match" : "No models available"} description={rows.length ? "Change the type or clear filters." : "No catalog offers models here yet. Ask a Platform Admin."} />
        : search.layout === "table" ? <ViewDataTable<WorkspaceRow> caption="Models" columns={columns} data={shown} getRowId={r => r.model_id} rowActions={r => actions(r)} hideViewControls view={tableViewFromSearch({ cols: search.cols, density: search.density }, workspaceColumnIds)} onViewChange={v => go(tableViewToSearch(v) as Partial<DashboardSearch>)} />
        : <ul className={m.rows} aria-label="Models">{shown.map(row => <WorkspaceListRow key={row.model_id} model={row} ws={workspace.id} actions={actions(row)} select={compare.select} />)}</ul>}
    </div>
  </Stack>;
}
/** Two-line row: name + API name | type | Ready / Not serving | price | Added / Assigned / Available to add | action. */
function WorkspaceListRow({ model, ws, actions, select }: { model: WorkspaceRow; ws: string; actions: ReactNode; select?: Select }) {
  const off = notServing(model), name = <ModelName display={model.display_name} api={model.public_name} search={workspaceModelSearch(ws, model.model_id)} />;
  return <li className={m.row}>
    <span className={m.rowName}>{select ? select(model.model_id, model.display_name || model.public_name, name) : name}</span>
    <span className={m.rowType}><BitopBadge size="sm" variant="outline">{workloadLabels[model.workload] ?? model.workload}</BitopBadge></span>
    <span className={m.rowStatus}>{off ? <NotServingBadge hint={notServingNote} /> : <HintBadge tone="success">{readinessText.ready}</HintBadge>}</span>
    <span className={m.rowPrice}><PriceSummary input={model.min_input_microusd_per_million} output={model.min_output_microusd_per_million} workload={model.workload} from={Number(model.routes) > 1} noRoutes={off} pricedRoutes={pricedCount(model.priced_routes)} /></span>
    <span className={m.rowSource}><EligibilityBadge eligibility={model.eligibility} hint={model.reason} /></span>
    <span className={m.rowActions}>{actions}</span>
  </li>;
}
function WorkspacePrices({ model }: { model: WorkspaceRow }) {
  const output = formatDecimalMicroUsd(model.min_output_microusd_per_million);
  // Not serving and no price: nothing can be priced yet, so "—" (as Admin's list), not "unknown · not free".
  if (notServing(model) && !formatDecimalMicroUsd(model.min_input_microusd_per_million)) return <TooltipText content="No enabled route, so no price yet" className={s.muted}>—</TooltipText>;
  return <><InputPriceSummary value={model.min_input_microusd_per_million} workload={model.workload} from={Number(model.routes) > 1} pricedRoutes={pricedCount(model.priced_routes)} />{output && model.workload !== "embeddings" && <span> · {output}<span className={s.muted}> / M output tokens</span></span>}</>;
}
const workspaceColumnIds = ["type", "price", "protocols", "eligibility", "created"];
function workspaceColumns(ws: string, select?: Select): DataTableColumn<WorkspaceRow>[] {
  const name = (r: WorkspaceRow) => <IconCell icon={<LabIcon model={[r.public_name, r.display_name]} />}><ResourceLink search={workspaceModelSearch(ws, r.model_id)}>{r.display_name}</ResourceLink><span className={s.secondary}>{r.public_name}</span></IconCell>;
  return [
    { id: "model", header: "Model", rowHeader: true, hideable: false, sortable: true, accessor: r => r.display_name, cell: r => select ? select(r.model_id, r.display_name || r.public_name, name(r)) : name(r) },
    { id: "type", header: "Type", sortable: true, accessor: r => workloadLabels[r.workload] ?? r.workload },
    { id: "price", header: "Price", label: "Price", sortable: true, accessor: r => r.min_input_microusd_per_million ?? "", sortFn: (a, b) => compareDecimal(a.min_input_microusd_per_million, b.min_input_microusd_per_million), cell: r => <WorkspacePrices model={r} /> },
    { id: "protocols", header: "Protocols", accessor: r => r.protocols.map(protocolLabel).join(" · ") },
    { id: "eligibility", header: "Status", accessor: r => eligibilityLabels[r.eligibility], cell: r => <span className={s.badges}><EligibilityBadge eligibility={r.eligibility} hint={r.reason} />{notServing(r) && <NotServingBadge hint={notServingNote} />}</span> },
    // created_at is when the model was created on the gateway (not when this workspace added it).
    { id: "created", header: "Created", sortable: true, accessor: r => r.created_at ?? "", cell: r => <DateTime value={r.created_at} /> },
  ];
}

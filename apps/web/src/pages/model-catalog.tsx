/*
 * Models catalog (Admin) and the read-only workspace variant. OpenRouter-style
 * catalog inside the Grounded/Bitop shell: type tabs with counts, then one
 * URL-backed FilterToolbar row (search, facets with counts, price range, Sort
 * and List/Table at its end; in place behind "Filters (n)" on a phone, never
 * a sheet), and List (cards) or Table (column chooser + density) views.
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
import { catalogSorts, catalogTypeTabs, eligibilityLabels, readinessText, filterCatalog, modelReadiness, modelWorkload, protocolLabel, sortCatalog, typeCounts, workloadModalities, type CatalogFilters, type CatalogModel, type CatalogSort, type Eligibility, type Readiness, type WorkspaceCatalogModel } from "../lib/model-setup";
import { HEADLINE_METERS, compareDecimal, formatDecimalMicroUsd, unitText, usdPerMillionFilter, usdToMicroUsd, workloadLabels } from "../lib/pricing";
import type { WorkloadKind } from "../lib/governance";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { IconCell, LabIcon, ProviderIcon } from "../components/provider-icon";
import { Button, DateTime, ErrorNotice, Heading, NativeSelect, Stack, Status, useAction, useApiScope, useChoices } from "../components/ui";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { ToggleGroup, ToggleGroupItem } from "../components/ui/toggle-group/toggle-group";
import type { DataTableColumn } from "../components/ui/data-table/data-table";
import { FilterToolbar, ToolbarField, type RangeFacet, type ToolbarFacet } from "../components/templates/filter-toolbar";
import type { FacetCounts, FilterValues } from "../components/ui/filter-bar/filter-bar";
import { TypeTabs } from "../components/templates/type-tabs";
import { ViewDataTable, tableViewFromSearch, tableViewToSearch } from "../components/templates/table-view";
import { ConnectionNames, ReadinessBadge, readinessNote, useServerPolicy } from "./catalog";
import { notServing } from "../lib/home";
import { ModelHeadlinePrice, useRoutePrices, type RoutePrice } from "./pricing-overview";
import { addModelsAction } from "./workspace";
import { ActionMenu } from "../components/templates/action-menu";
import { LayoutList, Table2 } from "lucide-react";
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
/** "From $0.10 / M input tokens"; unknown stays unknown (amber) for token-priced workloads. */
export function InputPriceSummary({ value, workload, from = false }: { value: string | null | undefined; workload: WorkloadKind; from?: boolean }) {
  const text = formatDecimalMicroUsd(value);
  if (text) return <span>{from ? "From " : ""}{text}<span className={s.muted}> / M input tokens</span></span>;
  if (tokenInput.has(workload)) return <span className={m.unknown}>Input price unknown · not free</span>;
  return <span className={s.muted}>Priced per {unitText(HEADLINE_METERS[workload][0], HEADLINE_METERS[workload][0] === "input_audio_seconds_ms" ? 60000 : HEADLINE_METERS[workload][0] === "input_characters" ? 1000000 : 1)}</span>;
}
/** "7 models" / "3 of 7 models" above the results; never shown while loading or after a load error (no fabricated zero). */
function ResultCount({ total, shown }: { total: number; shown: number }) {
  return <p className={m.summary} role="status">{shown === total ? `${total} model${total === 1 ? "" : "s"}` : `${shown} of ${total} models`}</p>;
}
/** Sort and List/Table: the end of the catalog's FilterToolbar row. */
function CatalogActions({ search, go, sorts = catalogSorts }: { search: DashboardSearch; go: (patch: Partial<DashboardSearch>) => void; sorts?: readonly { value: CatalogSort; label: string }[] }) {
  return <>
    <ToolbarField label="Sort"><NativeSelect size="sm" className={m.sort} value={search.sort ?? "name"} onChange={event => go({ sort: event.target.value === "name" ? undefined : event.target.value as CatalogSort })}>{sorts.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect></ToolbarField>
    <ToolbarField label="View" group>
      <ToggleGroup aria-label="Layout" joined variant="outline" size="sm" value={[search.layout === "table" ? "table" : "list"]} onValueChange={next => { const v = next[0]; if (v) go({ layout: v === "table" ? "table" : undefined }); }}>
        <ToggleGroupItem value="list"><LayoutList aria-hidden /> List</ToggleGroupItem>
        <ToggleGroupItem value="table"><Table2 aria-hidden /> Table</ToggleGroupItem>
      </ToggleGroup>
    </ToolbarField>
  </>;
}
const searchBox = (search: DashboardSearch, go: (patch: Partial<DashboardSearch>) => void) => ({ label: "Search models", placeholder: "Name or API name", value: search.q ?? "", onChange: (q: string) => go({ q: q || undefined }) });
const priceFacet: RangeFacet = { id: "price", label: "Input price ($/M tokens)", type: "range", prefix: "$", inputMode: "decimal", placeholders: ["Min", "Max"], description: "Cheapest enabled route, per million input tokens. While a bound is set, models with an unknown price are left out.", validate: v => usdToMicroUsd(v).ok ? undefined : "Enter US dollars, e.g. 0.15" };
const priceNote = (search: DashboardSearch) => search.min_price || search.max_price ? "Models with an unknown input price are hidden while a price bound is set." : undefined;
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
const readinessOptions: { value: Readiness["state"]; label: string }[] = (["ready", "needs_setup", "needs_attention", "not_serving", "unknown"] as const).map(value => ({ value, label: readinessText[value] }));

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
export const adminMoreFacets = ["policy", "readiness", "enabled", "deprecated"];
export function Models({ session }: { session: Session }) {
  const [search, go] = useCatalogSearch("models"), allowed = session.capabilities.platform_read;
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
  return <Stack gap={6} className={s.page}>
    <Heading title="Models" description="Models your users can call. Each model sends requests through one or more routes; workspaces get models from catalogs." actions={session.capabilities.platform_write && <Button render={<ResourceLink search={{ page: "model-new", connection: filters.connections?.length === 1 ? filters.connections[0] : undefined }} />}><Plus aria-hidden />Add model</Button>} />
    <TypeTabs label="Model type" items={tabs} value={type ?? "all"} onChange={v => go({ type: v === "all" ? undefined : v as DashboardSearch["type"] })} />
    <FilterToolbar search={searchBox(search, go)} facets={facets} more={adminMoreFacets} counts={facetCounts} values={facetValues(search)} onChange={v => go(facetSearch(v))} note={priceNote(search) ?? (search.pricing ? "Unpriced: an enabled route has no price, so its usage is recorded with unknown cost. Open the model to publish a price on its route." : undefined)}
      end={<CatalogActions search={search} go={go} />} />
    <div className={m.results}>
      {connections.isError && !failed && <ErrorNotice error={connections.error} retry={() => void connections.refetch()} />}
      {known && shown.length > 0 && <ResultCount total={filtered.length} shown={shown.length} />}
      {failed ? <ErrorNotice error={failed.error} retry={() => queries.forEach(q => void q.refetch())} />
        : loading ? <p role="status">Loading models…</p>
        : !shown.length ? <EmptyState size="compact" title={rows.length ? "No models match" : "No models yet"} description={rows.length ? "Change the type or clear filters." : "Add a model from a connection, then offer it through a catalog."} action={rows.length ? <Button variant="secondary" onClick={() => go({ ...facetSearch({}), q: undefined, type: undefined })}>Clear filters</Button> : undefined} />
        : table ? <AdminTable rows={shown} search={search} go={go} policy={policy} profiles={connections.data} prices={routePrices.routes.isError ? undefined : routePrices.byModel} pricesLoading={routePrices.routes.isPending} />
        : <ul className={m.cards} aria-label="Models">{shown.map(row => <AdminCard key={row.id} model={row} policy={policy} profiles={connections.data} />)}</ul>}
    </div>
  </Stack>;
}
function AdminCard({ model, policy, profiles }: { model: AdminRow; policy?: ServerPolicy; profiles?: Provider[] }) {
  const r = model.readiness;
  return <li className={m.card}>
    <span className={m.cardIcon}><LabIcon model={[model.public_name, model.display_name]} size="xl" /></span>
    <div className={m.cardBody}>
      <h2 className={m.cardTitle}><ResourceLink search={{ page: "model-detail", record: model.id }}>{model.display_name}</ResourceLink><BitopBadge size="sm" variant="outline">{workloadLabels[model.workload]}</BitopBadge><ReadinessBadge model={model} policy={policy} /><UnpricedBadge model={model} />{!model.enabled && <Status enabled={false} />}</h2>
      <code className={s.mono}>{model.public_name}</code>
      {model.description && <p className={m.description}>{model.description}</p>}
      {readinessNote(model, policy) && <p className={m.note}>{readinessNote(model, policy)}</p>}
      <ul className={m.meta}>
        <li>{workloadModalities[model.workload]}</li>
        <li><InputPriceSummary value={model.min_input_microusd_per_million} workload={model.workload} from={(r?.enabled_routes ?? 0) > 1} /></li>
        <li>{r ? `${r.enabled_routes} of ${r.routes} route${r.routes === 1 ? "" : "s"} enabled` : <span className={m.unknown}>Routes unknown</span>}</li>
        {r && r.connections.length > 0 && <li><ConnectionNames model={model} profiles={profiles} /></li>}
        {model.created_at && <li>Added <DateTime value={model.created_at} /></li>}
      </ul>
    </div>
  </li>;
}
export const adminColumnIds = ["type", "price", "pricing", "connections", "routes", "readiness", "status", "created"];
/** Admin table. Price: headline prices of the cheapest priced enabled route (PriceLine; prices are published on route pages). */
function AdminTable({ rows, search, go, policy, profiles, prices, pricesLoading }: { rows: AdminRow[]; search: DashboardSearch; go: (patch: Partial<DashboardSearch>) => void; policy?: ServerPolicy; profiles?: Provider[]; prices?: Map<string, RoutePrice[]>; pricesLoading?: boolean }) {
  const columns: DataTableColumn<AdminRow>[] = [
    { id: "model", header: "Model", rowHeader: true, hideable: false, sortable: true, accessor: r => r.display_name, cell: r => <IconCell icon={<LabIcon model={[r.public_name, r.display_name]} />}><ResourceLink search={{ page: "model-detail", record: r.id }}>{r.display_name}</ResourceLink><span className={s.secondary}>{r.public_name}</span></IconCell> },
    { id: "type", header: "Type", sortable: true, accessor: r => workloadLabels[r.workload] },
    { id: "price", header: "Price", label: "Price", sortable: true, accessor: r => r.min_input_microusd_per_million ?? "", sortFn: (a, b) => compareDecimal(a.min_input_microusd_per_million, b.min_input_microusd_per_million), cell: r => pricesLoading ? <span className={s.muted}>Loading…</span> : prices ? <ModelHeadlinePrice routes={prices.get(r.id)} workload={r.workload} /> : <InputPriceSummary value={r.min_input_microusd_per_million} workload={r.workload} /> },
    { id: "pricing", header: "Priced routes", label: "Priced routes", accessor: r => unpricedRoutes(r) ?? -1, sortable: true, cell: r => r.readiness ? <><span>{r.readiness.priced_enabled_routes} of {r.readiness.enabled_routes} enabled</span>{isUnpricedModel(r) && <span className={s.badges}><UnpricedBadge model={r} /></span>}</> : <span className={m.unknown}>Unknown</span> },
    { id: "connections", header: "Connections", cell: r => <ConnectionNames model={r} profiles={profiles} /> },
    { id: "routes", header: "Routes", numeric: true, accessor: r => r.readiness?.enabled_routes ?? -1, sortable: true, cell: r => r.readiness ? `${r.readiness.enabled_routes} / ${r.readiness.routes}` : <span className={m.unknown}>Unknown</span> },
    { id: "readiness", header: "Readiness", accessor: r => modelReadiness(r, policy).state, cell: r => <ReadinessBadge model={r} policy={policy} /> },
    { id: "status", header: "Status", accessor: r => r.enabled ? "Enabled" : "Disabled", cell: r => <Status enabled={r.enabled} /> },
    { id: "created", header: "Added", sortable: true, accessor: r => r.created_at ?? "", cell: r => <DateTime value={r.created_at} /> },
  ];
  return <ViewDataTable<AdminRow> caption="Models" columns={columns} data={rows} getRowId={r => r.id} view={tableViewFromSearch({ cols: search.cols, density: search.density }, adminColumnIds)} onViewChange={v => go(tableViewToSearch(v) as Partial<DashboardSearch>)} />;
}

// ---------------------------------------------------------------------------
// Workspace › Models (read-only catalog with self-service selection)
// ---------------------------------------------------------------------------
type WorkspaceRow = WorkspaceCatalogModel & { selection?: Grant };
/** The read-only workspace model page (`/workspaces/{ws}/models/{id}`). */
export const workspaceModelSearch = (ws: string, id: string): DashboardSearch => ({ page: "workspace-model", ws, record: id });
const eligibilityTone: Record<Eligibility, "success" | "info" | "neutral"> = { selected: "success", direct: "info", available_from_catalog: "neutral" };
export function EligibilityBadge({ eligibility }: { eligibility: Eligibility }) { return <BitopBadge size="sm" tone={eligibilityTone[eligibility] ?? "neutral"} dot>{eligibilityLabels[eligibility] ?? "Unknown eligibility"}</BitopBadge>; }
/** A model with no enabled route (route and connection both on) can't serve: requests fail. Same rule as the access reason "no_enabled_route". */
export function NotServingBadge() { return <BitopBadge size="sm" tone="warning" dot>{readinessText.not_serving}</BitopBadge>; }
const notServingNote = "No route to a provider is turned on for this model, so requests fail. A Platform Admin can enable one.";
export function WorkspaceModels({ session, workspace }: { session: Session; workspace: Workspace }) {
  const [search, go] = useCatalogSearch("grants"), ask = useAction(), canSelect = permissions(session, workspace).manageGrants;
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
  const select = (r: WorkspaceRow) => ask({ title: `Add ${r.display_name}?`, description: "Members can call it with keys that aren't restricted to other models. Catalog availability can change; removal never restores retired key allowlist entries.", submitLabel: "Add model", run: (_, signal) => api(grantPath, { method: "POST", body: { model_id: r.model_id }, signal }) });
  const remove = (r: WorkspaceRow) => ask({ title: `Remove ${r.display_name}?`, description: r.selection?.direct_granted ? "The Platform Admin's direct assignment still authorizes this model." : "Keys lose this model's authorization; retired allowlist entries do not come back if it is added again.", danger: true, submitLabel: "Remove model", run: (_, signal) => api(`${grantPath}/${enc(r.model_id)}`, { method: "DELETE", signal }) });
  // "Add" is the row's one visible action; Remove lives in the row's ⋯ menu (a dozen identical buttons were noise).
  const actions = (r: WorkspaceRow) => !canSelect ? null : r.eligibility === "available_from_catalog" ? <Button size="sm" variant="secondary" onClick={() => select(r)}>Add</Button> : r.selection?.catalog_granted ? <ActionMenu label={`Actions for ${r.display_name}`} actions={[{ label: "Remove from workspace…", danger: true, onSelect: () => remove(r) }]} /> : null;
  const eligibilities = ["selected", "direct", "available_from_catalog"] as const;
  const facets: ToolbarFacet[] = [
    { id: "eligibility", label: "Eligibility", type: "toggle", multiple: true, allLabel: "All", options: eligibilities.map(e => ({ value: e, label: eligibilityLabels[e] })) },
    priceFacet,
  ];
  const facetCounts: FacetCounts | undefined = !known ? undefined : { eligibility: Object.fromEntries(eligibilities.map(e => [e, countBy(ofType(base), r => r.eligibility === e)])) };
  const personal = workspace.kind === "personal";
  return <Stack gap={6} className={s.page}>
    <Heading title="Models" description={personal ? "Models you can use in your personal workspace: the ones you added from your available catalogs and any assigned to you." : "Models this workspace can use, and the ones its catalogs offer. Keys can be restricted further."} actions={canSelect && <Button variant="secondary" disabled={!available.length} onClick={() => ask(addModelsAction(grantPath, available.map(r => ({ model_id: r.model_id, display_name: r.display_name, public_name: r.public_name }) as Grant)))}><Plus aria-hidden />Add models</Button>} />
    <TypeTabs label="Model type" items={typeTabItems(known ? counts : null, search.type, typeCounts(rows))} value={search.type ?? "all"} onChange={v => go({ type: v === "all" ? undefined : v as DashboardSearch["type"] })} />
    <FilterToolbar search={searchBox(search, go)} facets={facets} counts={facetCounts} values={facetValues(search)} onChange={v => { const next = facetSearch(v); go({ eligibility: next.eligibility, min_price: next.min_price, max_price: next.max_price }); }} note={priceNote(search)}
      end={<CatalogActions search={search} go={go} sorts={catalogSorts} />} />
    <div className={m.results}>
      {grants.isError && !catalog.isError && <ErrorNotice error={grants.error} retry={() => void grants.refetch()} />}
      {known && shown.length > 0 && <ResultCount total={filtered.length} shown={shown.length} />}
      {catalog.isError ? <ErrorNotice error={catalog.error} retry={() => void catalog.refetch()} />
        : loading ? <p role="status">Loading models…</p>
        : !shown.length ? <EmptyState size="compact" title={rows.length ? "No models match" : "No models available"} description={rows.length ? "Change the type or clear filters." : "No catalog offers models to this workspace yet, and none are assigned. Ask a Platform Admin."} />
        : search.layout === "table" ? <WorkspaceTable rows={shown} ws={workspace.id} search={search} go={go} actions={actions} />
        : <ul className={m.cards} aria-label="Models">{shown.map(row => <WorkspaceCard key={row.model_id} model={row} ws={workspace.id} actions={actions(row)} />)}</ul>}
    </div>
  </Stack>;
}
function WorkspacePrices({ model }: { model: WorkspaceRow }) {
  const output = formatDecimalMicroUsd(model.min_output_microusd_per_million);
  return <><InputPriceSummary value={model.min_input_microusd_per_million} workload={model.workload} from={Number(model.routes) > 1} />{output && model.workload !== "embeddings" && <span> · {output}<span className={s.muted}> / M output tokens</span></span>}</>;
}
function WorkspaceCard({ model, ws, actions }: { model: WorkspaceRow; ws: string; actions: ReactNode }) {
  return <li className={m.card}>
    <span className={m.cardIcon}><LabIcon model={[model.public_name, model.display_name]} size="xl" /></span>
    <div className={m.cardBody}>
      <h2 className={m.cardTitle}><ResourceLink search={workspaceModelSearch(ws, model.model_id)}>{model.display_name}</ResourceLink><BitopBadge size="sm" variant="outline">{workloadLabels[model.workload] ?? model.workload}</BitopBadge><EligibilityBadge eligibility={model.eligibility} />{notServing(model) && <NotServingBadge />}</h2>
      <code className={s.mono}>{model.public_name}</code>
      {model.description && <p className={m.description}>{model.description}</p>}
      <ul className={m.meta}>
        <li>{workloadModalities[model.workload] ?? model.protocols.map(protocolLabel).join(" · ")}</li>
        <li><WorkspacePrices model={model} /></li>
        <li>{model.reason}</li>
        {model.created_at && model.eligibility !== "available_from_catalog" && <li>Added <DateTime value={model.created_at} /></li>}
      </ul>
      {notServing(model) && <p className={m.note}>{notServingNote}</p>}
    </div>
    {actions && <div className={m.cardActions}>{actions}</div>}
  </li>;
}
const workspaceColumnIds = ["type", "price", "protocols", "eligibility", "created"];
function WorkspaceTable({ rows, ws, search, go, actions }: { rows: WorkspaceRow[]; ws: string; search: DashboardSearch; go: (patch: Partial<DashboardSearch>) => void; actions: (r: WorkspaceRow) => ReactNode }) {
  const columns: DataTableColumn<WorkspaceRow>[] = [
    { id: "model", header: "Model", rowHeader: true, hideable: false, sortable: true, accessor: r => r.display_name, cell: r => <IconCell icon={<LabIcon model={[r.public_name, r.display_name]} />}><ResourceLink search={workspaceModelSearch(ws, r.model_id)}>{r.display_name}</ResourceLink><span className={s.secondary}>{r.public_name}</span></IconCell> },
    { id: "type", header: "Type", sortable: true, accessor: r => workloadLabels[r.workload] ?? r.workload },
    { id: "price", header: "Price", label: "Price", sortable: true, accessor: r => r.min_input_microusd_per_million ?? "", sortFn: (a, b) => compareDecimal(a.min_input_microusd_per_million, b.min_input_microusd_per_million), cell: r => <WorkspacePrices model={r} /> },
    { id: "protocols", header: "Protocols", accessor: r => r.protocols.map(protocolLabel).join(" · ") },
    { id: "eligibility", header: "Eligibility", accessor: r => eligibilityLabels[r.eligibility], cell: r => <><span className={s.badges}><EligibilityBadge eligibility={r.eligibility} />{notServing(r) && <NotServingBadge />}</span><span className={s.secondary}>{notServing(r) ? notServingNote : r.reason}</span></> },
    { id: "created", header: "Added", sortable: true, accessor: r => r.created_at ?? "", cell: r => r.eligibility === "available_from_catalog" ? <span className={s.muted}>Not added</span> : <DateTime value={r.created_at} /> },
  ];
  return <ViewDataTable<WorkspaceRow> caption="Models" columns={columns} data={rows} getRowId={r => r.model_id} rowActions={r => actions(r)} view={tableViewFromSearch({ cols: search.cols, density: search.density }, workspaceColumnIds)} onViewChange={v => go(tableViewToSearch(v) as Partial<DashboardSearch>)} />;
}

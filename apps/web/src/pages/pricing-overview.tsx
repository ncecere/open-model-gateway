/*
 * Admin › Pricing: the current price of every route in one table (finding #5),
 * instead of a single "Choose…" select. One row per route: model, route and
 * connection, its headline prices (PriceLine, exact micro-USD), pricing
 * version and publish date. Unpriced enabled routes are flagged (their usage
 * is recorded with unknown cost) and every row links to its route page,
 * where prices are published. Prices are estimates, not provider invoices.
 */
import { useQueries } from "@tanstack/react-query";
import { api, platformPath, type Collection, type Deployment, type Model, type Session } from "../lib/api";
import type { Price } from "../lib/governance";
import { modelWorkload, type CatalogModel } from "../lib/model-setup";
import { priceItems, workloadLabels } from "../lib/pricing";
import type { DashboardSearch } from "../lib/permissions";
import { DateTime, ErrorNotice, Heading, Stack, Status, useApiScope, useChoices } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { FilterToolbar, type ToolbarFacet } from "../components/templates/filter-toolbar";
import { InfoBanner } from "../components/templates/notices";
import { PriceLine } from "../components/templates/price-line";
import { Badge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { DataTable, type DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import s from "./shared.module.css";

type Row = { route: Deployment; model?: Model; price: Price | null | undefined; failed: boolean };
/** Unpriced = an enabled route with no price at all (unknown cost). Loading or failed rows are never called unpriced. */
export const isUnpriced = (r: Row) => r.price === null && r.route.enabled;

export function Pricing({ session }: { session: Session }) {
  const allowed = session.capabilities.platform_read, scope = useApiScope(), nav = useDashboardNavigation();
  const routes = useChoices<Deployment>(`${platformPath}/deployments`, allowed), models = useChoices<Model>(`${platformPath}/models`, allowed);
  const prices = useQueries({ queries: (routes.data ?? []).map(d => {
    const path = `${platformPath}/deployments/${encodeURIComponent(d.id)}/prices?limit=1&offset=0`;
    return { queryKey: ["api", scope, path], retry: false, queryFn: ({ signal }: { signal: AbortSignal }) => api<Collection<Price>>(path, { signal }) };
  }) });
  const search = nav?.search ?? { page: "pricing" as const }, q = (search.q ?? "").toLowerCase(), only = search.status === "unknown" ? "unpriced" : undefined;
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const byId = new Map((models.data ?? []).map(m => [m.id, m]));
  const rows: Row[] = (routes.data ?? []).map((route, i) => ({ route, model: byId.get(route.model_id), price: prices[i]?.data ? prices[i]!.data!.data[0] ?? null : undefined, failed: !!prices[i]?.isError }));
  const shown = rows.filter(r => (!q || [r.route.upstream_model, r.route.provider_name, r.model?.display_name, r.model?.public_name, r.route.model_public_name].some(x => x?.toLowerCase().includes(q))) && (!only || isUnpriced(r)))
    .sort((a, b) => (a.model?.display_name ?? a.route.model_public_name ?? "").localeCompare(b.model?.display_name ?? b.route.model_public_name ?? "", undefined, { sensitivity: "base" }) || a.route.upstream_model.localeCompare(b.route.upstream_model));
  const unpriced = rows.filter(isUnpriced).length, pricesLoading = prices.some(p => p.isPending);
  const facets: ToolbarFacet[] = [{ id: "status", label: "Show", type: "toggle", allLabel: "All routes", options: [{ value: "unknown", label: "Unpriced only" }] }];
  const columns: DataTableColumn<Row>[] = [
    { id: "model", header: "Model", rowHeader: true, hideable: false, cell: r => r.model ? <><ResourceLink search={{ page: "model-detail", record: r.model.id }}>{r.model.display_name}</ResourceLink><span className={s.secondary}>{r.model.public_name}</span></> : <span>{r.route.model_public_name ?? "Unknown model"}</span> },
    { id: "route", header: "Route", cell: r => <><ResourceLink search={{ page: "deployment-detail", record: r.route.id }}>{r.route.upstream_model}</ResourceLink><span className={s.secondary}>{r.route.provider_name ?? "Unknown connection"}</span></> },
    { id: "prices", header: "Current price", cell: r => <RoutePrices row={r} /> },
    { id: "version", header: "Published", cell: r => r.price ? <>v{r.price.pricing_version}<span className={s.secondary}><DateTime value={r.price.created_at} /></span></> : <span className={s.muted}>—</span> },
    { id: "status", header: "Status", cell: r => <span className={s.badges}><Status enabled={r.route.enabled} />{isUnpriced(r) && <Badge size="sm" tone="warning" dot>Unpriced</Badge>}</span> },
  ];
  if (!allowed) return <Heading title="Access not available" />;
  return <Stack gap={6} className={s.page}>
    <Heading title="Pricing" description="Current price for every route. Prices are estimates for budgets, not provider invoices. Publishing a new price never changes past usage." />
    {unpriced > 0 && !pricesLoading && <InfoBanner tone="warning" title={`${unpriced} enabled route${unpriced === 1 ? " has" : "s have"} no price`}>Requests on {unpriced === 1 ? "it" : "them"} are recorded with unknown cost, which holds budget until resolved. Open a route to publish a price.</InfoBanner>}
    <Card title="Prices by route" titleAs="h2" description="Open a route to see its price history or publish a new price.">
      <Stack gap={4}>
        <FilterToolbar search={{ label: "Search prices", placeholder: "Model, route or connection", value: search.q ?? "", onChange: v => go({ q: v || undefined }) }} facets={facets} values={only ? { status: ["unknown"] } : {}} onChange={v => go({ status: Array.isArray(v.status) && v.status.length ? "unknown" : undefined })} />
        {routes.isError || models.isError ? <ErrorNotice error={routes.error ?? models.error} retry={() => { void routes.refetch(); void models.refetch(); }} />
          : routes.isPending ? <p role="status">Loading routes…</p>
          : !rows.length ? <EmptyState size="compact" title="No routes yet" description="Add a model and its route from a connection, then price the route." action={<ResourceLink search={{ page: "models" }}>Go to Models</ResourceLink>} />
          : !shown.length ? <EmptyState size="compact" title="No routes match these filters" action={<ResourceLink search={{ page: "pricing" }}>Clear filters</ResourceLink>} />
          : <DataTable<Row> caption="Current price by route" columns={columns} data={shown} getRowId={r => r.route.id} />}
      </Stack>
    </Card>
  </Stack>;
}

/** The headline price lines of a route (input, output, or the workload's own meters), exact amounts. */
function RoutePrices({ row }: { row: Row }) {
  if (row.failed) return <span className={s.muted}>Couldn't load</span>;
  if (row.price === undefined) return <span className={s.muted}>Loading…</span>;
  if (row.price === null) return <span className={s.muted}>No price · cost recorded as unknown</span>;
  const workload = row.model ? modelWorkload(row.model as unknown as CatalogModel) : "generation";
  const items = priceItems(row.price, workload).filter(i => !i.notApplicable).slice(0, 2);
  return <span className={s.priceStack}>{items.map(i => <span key={i.key}><span className={s.muted}>{i.label} </span><PriceLine amount={i.amount} unit={i.unit} tiers={i.tiers?.map(t => ({ label: t.label, amount: t.amount, unit: t.unit }))} /></span>)}<span className={s.secondary}>{workloadLabels[workload]}</span></span>;
}

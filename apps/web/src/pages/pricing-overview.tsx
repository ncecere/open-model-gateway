/*
 * Route prices for Admin › Models (Table view). The former Admin › Pricing
 * page is now the Models table: its "Price" column shows the headline prices
 * of each model's cheapest priced enabled route (PriceLine, exact micro-USD)
 * and "Priced routes" flags enabled routes without a price (their usage is
 * recorded with unknown cost), with an "Unpriced only" filter. `/admin/pricing`
 * redirects there (lib/locations.ts). Prices are still published on route
 * pages; they are estimates for budgets, not provider invoices.
 */
import { useEffect } from "react";
import { useQueries } from "@tanstack/react-query";
import { api, platformPath, type Collection, type Deployment, type Session } from "../lib/api";
import type { Price, WorkloadKind } from "../lib/governance";
import { HEADLINE_METERS, basePrice, priceItems } from "../lib/pricing";
import { useApiScope, useChoices } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { PriceLine } from "../components/templates/price-line";
import type { DashboardSearch } from "../lib/permissions";
import s from "./shared.module.css";

/** Where /admin/pricing now lives: the Models table, unpriced models only. */
export const pricingSearch: DashboardSearch = { page: "models", layout: "table", pricing: "unpriced" };

/** One route with its latest price: undefined while loading, null when it has none (unknown cost, never free). */
export type RoutePrice = { route: Deployment; price: Price | null | undefined; failed: boolean };
/** Every route and its latest price, grouped by model. Only fetched when `enabled` (the Table view). */
export function useRoutePrices(enabled: boolean) {
  const scope = useApiScope(), routes = useChoices<Deployment>(`${platformPath}/deployments`, enabled);
  const prices = useQueries({ queries: (enabled ? routes.data ?? [] : []).map(d => {
    const path = `${platformPath}/deployments/${encodeURIComponent(d.id)}/prices?limit=1&offset=0`;
    return { queryKey: ["api", scope, path], retry: false, queryFn: ({ signal }: { signal: AbortSignal }) => api<Collection<Price>>(path, { signal }) };
  }) });
  const byModel = new Map<string, RoutePrice[]>();
  (enabled ? routes.data ?? [] : []).forEach((route, i) => {
    const q = prices[i], row: RoutePrice = { route, price: q?.data ? q.data.data[0] ?? null : undefined, failed: !!q?.isError };
    byModel.set(route.model_id, [...byModel.get(route.model_id) ?? [], row]);
  });
  return { routes, byModel, loading: routes.isPending || prices.some(p => p.isPending) };
}
/** The amount of the workload's first headline meter, for picking the cheapest route (unknown sorts last). */
function headlineAmount(price: Price, workload: WorkloadKind): bigint | null {
  const b = basePrice(price, HEADLINE_METERS[workload][0]!);
  return b.amount !== null && /^\d+$/.test(b.amount) ? BigInt(b.amount) : null;
}
/** Headline prices (input/output, or the workload's own unit) of a model's cheapest priced enabled route. */
export function ModelHeadlinePrice({ routes, workload }: { routes: RoutePrice[] | undefined; workload: WorkloadKind }) {
  if (!routes) return <span className={s.muted}>No routes</span>;
  const enabled = routes.filter(r => r.route.enabled);
  if (!enabled.length) return <span className={s.muted}>No enabled route</span>;
  if (enabled.some(r => r.price === undefined && !r.failed)) return <span className={s.muted}>Loading…</span>;
  const priced = enabled.filter((r): r is RoutePrice & { price: Price } => !!r.price);
  if (!priced.length) return <span className={s.muted}>{enabled.some(r => r.failed) ? "Couldn't load" : "No price · cost recorded as unknown"}</span>;
  const cheapest = priced.slice().sort((a, b) => { const x = headlineAmount(a.price, workload), y = headlineAmount(b.price, workload); return x === null ? 1 : y === null ? -1 : x < y ? -1 : x > y ? 1 : 0; })[0]!;
  const items = priceItems(cheapest.price, workload).filter(i => !i.notApplicable).slice(0, 2);
  return <span className={s.priceStack}>
    {items.map(i => <span key={i.key}><span className={s.muted}>{i.label} </span><PriceLine amount={i.amount} unit={i.unit} tiers={i.tiers?.map(t => ({ label: t.label, amount: t.amount, unit: t.unit }))} /></span>)}
    {priced.length > 1 && <span className={s.secondary}>Cheapest of {priced.length} priced routes · <ResourceLink search={{ page: "deployment-detail", record: cheapest.route.id }}>{cheapest.route.upstream_model}</ResourceLink></span>}
  </span>;
}
/** Legacy Admin › Pricing: opens the Models table with "Unpriced only" (deep links are rewritten before this renders). */
export function Pricing(_: { session: Session }) {
  const nav = useDashboardNavigation();
  useEffect(() => { nav?.navigate(pricingSearch); }, []);
  return <p role="status">Prices are now in <ResourceLink search={pricingSearch}>Models › Table</ResourceLink>.</p>;
}

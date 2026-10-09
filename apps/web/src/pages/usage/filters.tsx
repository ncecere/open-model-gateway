/*
 * Usage & costs filters: one URL-backed FilterToolbar row under the pill tabs
 * (no drawer or sheet; on a phone it collapses in place behind "Filters (n)").
 * Period (and Admin's workspace) lead the row; everything else sits behind one
 * "More filters (n)" popover with its chips under the row. Model, key, member (workspace-wide
 * viewers only; never in Personal), status (several at once), cost center
 * (platform scope; a page fact in a workspace) and service account; Records adds the
 * accounting "Cost" state. Filters only narrow what the caller may already
 * see; the gateway authorizes every number.
 *
 * Option sources never widen visibility: a workspace's own model, key, member
 * and service-account lists (each already scoped by the server), and at
 * platform scope this period's aggregates (top keys, members and cost
 * centers), never a user or key directory. Personal workspace keys stay
 * collapsed at platform scope.
 */
import { wsPath, platformPath, type Grant, type Member, type Model, type ServiceAccount, type Workspace } from "../../lib/api";
import type { KeyRow } from "../../lib/keys";
import type { DashboardSearch } from "../../lib/permissions";
import { exploreQuery, recordStatuses, usageFilters, usageStatuses, type Dimension, type ExploreResponse, type RecordStatus, type UsageContext, type UsagePeriod } from "../../lib/usage";
import { useApi, useChoices } from "../../components/ui";
import type { ReactNode } from "react";
import type { Facet, FacetOption, FilterValues } from "../../components/ui/filter-bar/filter-bar";
import { FilterToolbar, type ToolbarChip } from "../../components/templates/filter-toolbar";
import type { UsageNav } from "./shared";

export type FilterOptions = { models: FacetOption[]; keys: FacetOption[]; members: FacetOption[]; costCenters: FacetOption[]; serviceAccounts: FacetOption[] };
type Top = "key" | "member" | "cost_center";

/** This period's top 25 groups of one dimension (platform options and cost centers), without the other filters. */
function useTop(base: string, period: UsagePeriod | undefined, groupBy: Top, enabled: boolean, workspaceFilter?: string) {
  const on = enabled && !!period;
  return useApi<ExploreResponse>(on ? `${base}/usage/explore?${exploreQuery(period!, { metric: "spend", groupBy: groupBy as Dimension, thenBy: "none", top: 25 }, workspaceFilter)}` : "", on);
}
const label = (name: string, publicName: string) => name === publicName ? publicName : `${name} · ${publicName}`;

/** Filter options for this scope; see the file comment for where each list comes from. */
export function useFilterOptions(workspace: Workspace | undefined, ctx: UsageContext, period: UsagePeriod | undefined, workspaceFilter?: string): FilterOptions {
  const ws = workspace ? wsPath(workspace.id) : "", base = workspace ? ws : platformPath, platform = !workspace;
  // Disabled lists get no path at all, so nothing is even addressed for callers who may not see it.
  const useList = <T,>(path: string, on: boolean) => useChoices<T>(on ? path : "", on);
  const grants = useList<Grant>(`${ws}/models`, !!workspace), models = useList<Model>(`${platformPath}/models`, platform);
  const keys = useList<KeyRow>(`${ws}/keys`, !!workspace);
  const members = useList<Member>(`${ws}/members`, !!workspace && ctx.members);
  const accounts = useList<ServiceAccount>(`${ws}/service-accounts`, !!workspace && ctx.members && workspace.capabilities.manage_service_accounts);
  const topKeys = useTop(base, period, "key", platform, workspaceFilter), topMembers = useTop(base, period, "member", platform, workspaceFilter);
  const centers = useTop(base, period, "cost_center", ctx.members, workspaceFilter);
  const named = (res: ExploreResponse | undefined) => res?.rows.flatMap(r => r.group.id && r.group.name ? [{ value: r.group.id, label: r.group.name }] : []) ?? [];
  return {
    models: workspace ? [...new Map((grants.data ?? []).map(g => [g.model_id, { value: g.model_id, label: label(g.display_name, g.public_name) }])).values()] : (models.data ?? []).map(m => ({ value: m.id, label: label(m.display_name, m.public_name) })),
    keys: workspace ? (keys.data ?? []).map(k => ({ value: k.id, label: k.name, group: k.service_account_id ? "Service accounts" : "People" })) : named(topKeys.data),
    members: workspace ? (members.data ?? []).filter(m => m.email).map(m => ({ value: m.user_id, label: m.email })) : named(topMembers.data),
    costCenters: centers.data?.rows.map(r => ({ value: r.group.id ?? "unallocated", label: r.group.name ?? "Unallocated" })) ?? [],
    serviceAccounts: (accounts.data ?? []).map(a => ({ value: a.id, label: a.disabled_at ? `${a.name} (disabled)` : a.name })),
  };
}

/** The API name of the filtered model (records match on it), or undefined while it loads or when it's unknown. */
export function useModelName(workspace: Workspace | undefined, modelId: string | undefined): { name?: string; pending: boolean } {
  const grants = useChoices<Grant>(workspace && modelId ? `${wsPath(workspace.id)}/models` : "", !!workspace && !!modelId);
  const models = useChoices<Model>(!workspace && modelId ? `${platformPath}/models` : "", !workspace && !!modelId);
  if (!modelId) return { pending: false };
  const q = workspace ? grants : models;
  const name = workspace ? grants.data?.find(g => g.model_id === modelId)?.public_name : models.data?.find(m => m.id === modelId)?.public_name;
  return { name, pending: q.isPending };
}

const first = (v: FilterValues[string]) => Array.isArray(v) ? v[0] : undefined;
/** Keeps a URL value that isn't among this period's options visible (so it can be seen and cleared). */
const keep = (list: FacetOption[], current: string | undefined, fallback: string) => current && !list.some(o => o.value === current) ? [...list, { value: current, label: fallback }] : list;

/** Facets behind "More filters" (Records' Cost state stays inline: it's that tab's own question). */
export const moreFacetIds = ["model", "key", "member", "status", "cost_center", "service_account"];

export function UsageFilterBar({ options, ctx, nav, records = false, start, end, extraChips, extraActive }: { options: FilterOptions; ctx: UsageContext; nav: UsageNav; records?: boolean; /** Leading controls: Period, then Admin's workspace scope. */ start?: ReactNode; /** Page actions at the right end of the row (e.g. By workspace's Columns). */ end?: ReactNode; extraChips?: ToolbarChip[]; extraActive?: number }) {
  const s = nav.search, f = usageFilters(s, ctx);
  const facets: Facet[] = [
    { id: "model", label: "Model", type: "select", placeholder: "All models", options: keep(options.models, f.model_id, "Selected model") },
    { id: "key", label: "API key", type: "select", placeholder: "All keys", options: keep(options.keys, f.key_id, "Selected key") },
    ...(ctx.members ? [{ id: "member", label: "Member", type: "select" as const, placeholder: "Everyone", options: keep(options.members, f.member, "Selected member") }] : []),
    { id: "status", label: "Status", type: "toggle", multiple: true, allLabel: "All", options: usageStatuses },
    // Platform scope only: inside a workspace the cost center is a page fact (one per workspace), shown in the header.
    ...(ctx.platform || f.cost_center_id ? [{ id: "cost_center", label: "Cost center", type: "select" as const, placeholder: "All cost centers", options: keep(options.costCenters, f.cost_center_id, f.cost_center_id === "unallocated" ? "Unallocated" : "Selected cost center") }] : []),
    ...(options.serviceAccounts.length || f.service_account_id ? [{ id: "service_account", label: "Service account", type: "select" as const, placeholder: "Any key", options: keep(options.serviceAccounts, f.service_account_id, "Selected service account") }] : []),
    ...(records ? [{ id: "cost", label: "Cost", type: "toggle" as const, allLabel: "All", options: recordStatuses.map(r => ({ value: r.value, label: r.label })) }] : []),
  ];
  const value: FilterValues = { model: f.model_id ? [f.model_id] : undefined, key: f.key_id ? [f.key_id] : undefined, member: f.member ? [f.member] : undefined, status: f.status.length ? f.status : undefined, cost_center: f.cost_center_id ? [f.cost_center_id] : undefined, service_account: f.service_account_id ? [f.service_account_id] : undefined, cost: records && s.cost_status ? [s.cost_status] : undefined };
  const change = (next: FilterValues) => {
    const status = Array.isArray(next.status) ? next.status.join(",") : "";
    const patch: Partial<DashboardSearch> = { model_id: first(next.model), key_id: first(next.key), actor_user_id: ctx.members ? first(next.member) : undefined, status: status || undefined, cost_center_id: first(next.cost_center), service_account_id: first(next.service_account) };
    if (records) patch.cost_status = first(next.cost) as RecordStatus | undefined;
    nav.navigate(patch);
  };
  return <FilterToolbar facets={facets} values={value} onChange={change} start={start} end={end} extraChips={extraChips} extraActive={extraActive} more={moreFacetIds} />;
}

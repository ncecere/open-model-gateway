/*
 * Usage & costs (both portals): header (title + one subtitle line with the
 * period) → pill tabs Overview | Explore | Cost records (Admin: By workspace) →
 * ONE FilterToolbar row (Period, Admin's Workspace, "More filters") → the tab.
 * A routed chart page per tile (`?tab=chart&metric=…`). Sister-app layout:
 * Grounded/Bitop header, cards and pill tabs; no drawers or modals for filters.
 *
 * Privacy: the gateway authorizes every number. The page additionally never
 * offers member breakdowns to ordinary members or in Personal, and Admin sees
 * personal workspaces as totals only (no keys or request records are fetched).
 */
import type { Session, Workspace } from "../../lib/api";
import { platformPath } from "../../lib/api";
import { chartMetrics, exploreQuery, periodLabel, usageContext, usagePeriod, type ChartMetric, type ExploreResponse, type UsagePeriod } from "../../lib/usage";
import { ToolbarField } from "../../components/templates/filter-toolbar";
import { ErrorNotice, Heading, NativeSelect, Stack, useApi } from "../../components/ui";
import { Tabs, TabsList, Tab, TabsPanel } from "../../components/ui/tabs/tabs";
import { PeriodControl, scopeLine, useUsageSearch, type UsageNav } from "./shared";
import { UsageFilterBar, useFilterOptions } from "./filters";
import { UsageOverviewTab } from "./overview";
import { UsageExploreTab } from "./explore";
import { PlatformRecords, WorkspaceRecords } from "./records";
import { BackToUsage, UsageChart } from "./chart";
import s from "../shared.module.css";
import u from "./usage.module.css";

type Tab = "overview" | "explore" | "records";
const tabOf = (tab?: string): Tab | "chart" => tab === "explore" || tab === "records" || tab === "chart" ? tab : "overview";

export function Costs({ session, workspace }: { session: Session; workspace: Workspace }) { return <UsageCosts session={session} workspace={workspace} />; }
export function PlatformCosts({ session }: { session: Session }) { return <UsageCosts session={session} />; }

export function UsageCosts({ session, workspace }: { session: Session; workspace?: Workspace }) {
  const nav = useUsageSearch(workspace), ctx = usageContext(workspace), tab = tabOf(nav.search.tab);
  let period: UsagePeriod | undefined, invalid: Error | undefined;
  try { period = usagePeriod(nav.search); } catch (error) { invalid = error as Error; }
  const workspaceFilter = workspace ? undefined : nav.search.workspace_id;
  // One inline filter bar for every tab and the chart page (URL-backed; Records adds the accounting state).
  const options = useFilterOptions(workspace, ctx, period, workspaceFilter);
  // Workspace scope: the cost center is a fact about the page (the platform assigns it), not a filter.
  const centers = options.costCenters.filter(c => c.value !== "unallocated"), centerFact = workspace && centers.length ? centers.length === 1 ? `Cost center: ${String(centers[0]!.label)}` : `${centers.length} cost centers` : undefined;
  const header = (title: string) => <Heading title={title} description={<span className={u.scope}><span>{scopeLine(workspace)}</span>{period && <span>· {periodLabel(period)}</span>}{centerFact && <span>· {centerFact}</span>}</span>} />;
  const periodControl = <PeriodControl key={`${period?.start_date}-${period?.end_date}-${period?.preset}`} period={period} navigate={nav.navigate} />;
  // Admin: narrow every tab to one workspace (a leading toolbar control with its own chip).
  const scoped = !workspace && session.capabilities.platform_read && !!period, spaces = useWorkspaceScope(period, scoped);
  const scope = scoped && <WorkspaceScope spaces={spaces} nav={nav} />;
  const scopeChips = scoped && workspaceFilter ? [{ key: "workspace", label: "Workspace", text: spaces.find(w => w.id === workspaceFilter)?.label ?? "Selected workspace", onRemove: () => nav.navigate({ workspace_id: undefined }) }] : [];
  // One FilterToolbar row; it collapses in place behind "Filters (n)" on a phone (no sheet). An invalid custom period
  // still gets the Period control so it can be fixed.
  const filters = <UsageFilterBar options={options} ctx={ctx} nav={nav} records={tab === "records"} start={<>{periodControl}{scope || null}</>} extraChips={scopeChips} extraActive={period && period.preset === "month" ? 0 : 1} />;
  if (tab === "chart") {
    const metric: ChartMetric = chartMetrics.includes(nav.search.metric as ChartMetric) ? nav.search.metric as ChartMetric : "spend";
    return <Stack gap={6} className={s.page}><BackToUsage nav={nav} />{header(chartTitle(metric))}{filters}
      {invalid || !period ? <ErrorNotice error={invalid} /> : <UsageChart workspace={workspace} metric={metric} period={period} nav={nav} workspaceFilter={workspaceFilter} />}</Stack>;
  }
  return <Stack gap={6} className={s.page}>
    {header("Usage & costs")}
    <Tabs value={tab} onValueChange={v => nav.navigate({ tab: v === "overview" ? undefined : String(v), metric: undefined })}>
      <Stack gap={6}>
      <TabsList variant="pills" overflow="scroll" aria-label="Usage & costs views"><Tab value="overview">Overview</Tab><Tab value="explore">Explore</Tab><Tab value="records">{workspace ? "Cost records" : "By workspace"}</Tab></TabsList>
      {filters}
      {invalid || !period ? <ErrorNotice error={invalid} /> : <>
      <TabsPanel value="overview"><UsageOverviewTab workspace={workspace} ctx={ctx} period={period} nav={nav} workspaceFilter={workspaceFilter} /></TabsPanel>
      <TabsPanel value="explore"><UsageExploreTab workspace={workspace} ctx={ctx} period={period} nav={nav} workspaceFilter={workspaceFilter} /></TabsPanel>
      <TabsPanel value="records">{workspace ? <WorkspaceRecords session={session} workspace={workspace} ctx={ctx} period={period} nav={nav} /> : <PlatformRecords session={session} ctx={ctx} period={period} nav={nav} workspaceFilter={workspaceFilter} />}</TabsPanel>
      </>}
      </Stack>
    </Tabs>
  </Stack>;
}
const chartTitle = (metric: ChartMetric) => ({ spend: "Spend", requests: "Requests", tokens: "Tokens", cache_hit_rate: "Cache hit rate", blended: "Cost per 1M tokens" })[metric];

/**
 * Admin › Usage & costs: narrow every tab to one workspace. Options come from this period's aggregate workspace totals
 * (top 25 by spend), never from a directory; personal workspaces stay totals-only.
 */
type ScopeOption = { id: string; label: string };
function useWorkspaceScope(period: UsagePeriod | undefined, enabled: boolean): ScopeOption[] {
  const q = useApi<ExploreResponse>(enabled && period ? `${platformPath}/usage/explore?${exploreQuery(period, { metric: "spend", groupBy: "workspace", thenBy: "none", top: 25 })}` : "", enabled && !!period);
  const rows = q.data?.rows.filter(r => r.group.id) ?? [], names = rows.map(r => r.group.name ?? "Workspace");
  const label = (name: string, id: string) => names.filter(n => n === name).length > 1 ? `${name} · ${id.slice(0, 8)}` : name;
  return rows.map(r => ({ id: r.group.id!, label: label(r.group.name ?? "Workspace", r.group.id!) }));
}
function WorkspaceScope({ spaces, nav }: { spaces: ScopeOption[]; nav: UsageNav }) {
  const current = nav.search.workspace_id;
  return <ToolbarField label="Workspace" hint="Applies to every tab. Personal workspaces show totals only.">
    <NativeSelect size="sm" className={u.scopeSelect} value={current ?? ""} onChange={e => nav.navigate({ workspace_id: e.target.value || undefined })}>
      <option value="">All workspaces</option>
      {spaces.map(w => <option key={w.id} value={w.id}>{w.label}</option>)}
      {current && !spaces.some(w => w.id === current) && <option value={current}>Selected workspace</option>}
    </NativeSelect>
  </ToolbarField>;
}

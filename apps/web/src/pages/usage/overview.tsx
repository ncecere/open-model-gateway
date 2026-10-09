import type { CSSProperties } from "react";
import { cssPercent, formatShare, shareBasisPoints } from "../../components/templates/kit-format";
/*
 * Usage & costs › Overview, calm Grounded-style layout: three tiles (Spend,
 * Requests, Tokens; Δ only when the previous period has data), one compact
 * Budgets row when budgets exist, the daily chart (or a one-line note), top-5
 * lists as compact tables, the compact Accounting card (workspace admins and
 * platform readers only) and a muted footer ("Estimates from configured
 * prices, not invoices · updated …").
 * Cache hit rate and cost per 1M tokens live in Explore and Accounting.
 * Each tile opens its routed chart page.
 */
import { useId, type ReactElement, type ReactNode } from "react";
import { DetailTime } from "../../components/templates/when";
import { wsPath, platformPath, type InstallationBudget, type Workspace } from "../../lib/api";
import { formatMicroUsd } from "../../lib/governance";
import type { DashboardSearch } from "../../lib/permissions";
import { countLabel, formatCount } from "../../lib/reports";
import { activeDays, chartLabeler, chartNumber, filterCount, formatMetric, longDate, metricInfo, resetsAt, shortDate, tileDelta, usageFilters, usageQuery, visibleBudgets, type BudgetSpan, type Tile, type TopRow, type UsageBudgetWindow, type UsageContext, type UsageOverview, type UsagePeriod } from "../../lib/usage";
import { Button, ErrorNotice, Stack, useApi } from "../../components/ui";
import { ArrowRight } from "lucide-react";
import { ResourceLink } from "../../components/navigation-link";
import { StatTile, StatTileGrid } from "../../components/templates/stat-tile";
import { UsageBar } from "../../components/templates/usage-bar";
import { Card } from "../../components/ui/card/card";
import { BarChart } from "../../components/ui/bar-chart/bar-chart";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { Table, Td, Th, Tr } from "../../components/ui/table/table";
import { ToggleGroup, ToggleGroupItem } from "../../components/ui/toggle-group/toggle-group";
import { AccountingSection } from "./accounting";
import { isAdmin } from "../../lib/permissions";
import type { UsageNav } from "./shared";
import u from "./usage.module.css";

const zero = (v: string | null | undefined) => v != null && /^0+(?:\.0+)?$/.test(v);
const positive = (v: string | null | undefined) => v != null && /^\d+$/.test(v) && BigInt(v) > 0n;
/** The overview chart's measures (rates are in Explore). */
export type OverviewMetric = "spend" | "requests" | "tokens";
export const overviewMetrics: OverviewMetric[] = ["spend", "requests", "tokens"];
const series = (metric: OverviewMetric, t: Tile) => t.daily.map(d => d.value == null ? null : chartNumber(metric, d.value));
/** Exact labels for a tile's drawn daily values. */
const labels = (metric: OverviewMetric, t: Tile) => chartLabeler(metric, t.daily.map(d => d.value));
/** How many top rows the overview lists; the rest are in Explore. */
export const TOP_ROWS = 5;

/** Who sees Accounting by default: workspace admins (and every Admin-portal viewer, who are platform readers). */
export const showsAccounting = (workspace?: Workspace) => !workspace || isAdmin(workspace.role) || workspace.capabilities.view_all_activity;
export function UsageOverviewTab({ workspace, ctx, period, nav, workspaceFilter, accounting = showsAccounting(workspace) }: { workspace?: Workspace; ctx: UsageContext; period: UsagePeriod; nav: UsageNav; workspaceFilter?: string; /** Show the Accounting card (roles that act on it); plain members see only tiles and charts. */ accounting?: boolean }) {
  const base = workspace ? wsPath(workspace.id) : platformPath, filters = usageFilters(nav.search, ctx), filtered = filterCount(filters) > 0;
  const q = useApi<UsageOverview>(`${base}/usage/overview?${usageQuery(period, workspaceFilter, filters)}`);
  if (q.isPending) return <p role="status">Loading usage…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const o = q.data, t = o.tiles, idle = zero(t.requests.value) && zero(t.spend.value);
  const chart = (metric: OverviewMetric) => <ResourceLink search={{ ...nav.search, tab: "chart", metric, offset: undefined }} />;
  const retries = positive(t.requests.attempts) && positive(t.requests.value) && BigInt(t.requests.attempts!) > BigInt(t.requests.value!) ? BigInt(t.requests.attempts!) - BigInt(t.requests.value!) : 0n;
  const unknownTokens = positive(t.tokens.unknown_token_attempts);
  const metric: OverviewMetric = overviewMetrics.includes(nav.search.metric as OverviewMetric) ? nav.search.metric as OverviewMetric : "spend";
  const chartDays = Math.max(...overviewMetrics.map(m => activeDays(t[m].daily)));
  const accountingPath = `${base}/cost-report?${usageQuery(period, workspaceFilter)}&compare=none`;
  return <Stack gap={6}>
    <Stack gap={3}>
      {/* One day of data: the tiles' sparklines say it all (no sentence explaining the missing chart). */}
      <StatTileGrid label="Usage summary" columns={3}>
        <SpendTile overview={o} workspace={workspace} nav={nav} period={period} render={chart("spend")} />
        <StatTile label="Requests" value={t.requests.value == null ? null : formatCount(t.requests.value)} hint={retries > 0n ? `including ${formatCount(retries.toString())} retr${retries === 1n ? "y" : "ies"}` : undefined} series={series("requests", t.requests)} formatSeriesValue={labels("requests", t.requests)} delta={tileDelta(t.requests.value, t.requests.previous, "neutral", period.days)} render={chart("requests")} />
        <StatTile label="Tokens" value={t.tokens.value == null ? null : `${unknownTokens ? "At least " : ""}${formatCount(t.tokens.value)}`} hint={unknownTokens ? `${countLabel(t.tokens.unknown_token_attempts, "request")} didn't report tokens` : t.tokens.input_tokens != null && t.tokens.output_tokens != null ? `${formatCount(t.tokens.input_tokens)} in · ${formatCount(t.tokens.output_tokens)} out` : undefined} series={series("tokens", t.tokens)} formatSeriesValue={labels("tokens", t.tokens)} delta={tileDelta(t.tokens.value, t.tokens.previous, "neutral", period.days)} render={chart("tokens")} />
      </StatTileGrid>
    </Stack>
    {workspace ? <WorkspaceBudgetRow workspace={workspace} /> : <InstallationBudgetRow budgets={o.installation_budgets} />}
    {idle ? <Card><EmptyState title={filtered ? "No requests match these filters" : "No requests in this period"} titleAs="h2" description={filtered ? "Change or clear the filters to see more." : workspace ? `Requests made with ${workspace.kind === "personal" || !workspace.capabilities.view_all_activity ? "your keys" : `${workspace.name}'s keys`} appear here.` : "Requests made in any workspace appear here."} /></Card> : <>
      {chartDays >= 2 && <DailyChart overview={o} metric={metric} onMetric={m => nav.navigate({ metric: m === "spend" ? undefined : m })} />}
      <div className={u.grid}>
        {/* A name opens the key, model or person where this portal has a page for it; filtering is in "More filters". */}
        <TopList title="Top API keys" nameHeader="API key" rows={o.top.keys} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: "key", metric: undefined, then: undefined }} empty="No key spent anything yet." open={r => workspace && r.id ? { page: "key-detail", ws: workspace.id, record: r.id } : undefined} />
        <TopList title="Top models" nameHeader="Model" rows={o.top.models} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: undefined, metric: undefined, then: undefined }} empty="No model spent anything yet." open={r => !r.model_id ? undefined : workspace ? { page: "workspace-model", ws: workspace.id, record: r.model_id } : { page: "model-detail", record: r.model_id }} />
        {ctx.members && o.top.members !== null && <TopList title="Top members" nameHeader="Member" rows={o.top.members} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: "member", metric: undefined, then: undefined }} empty="No member spent anything yet." open={r => !workspace && r.id ? { page: "user-detail", record: r.id } : undefined} />}
      </div>
    </>}
    {accounting && <AccountingSection path={accountingPath} rates={o} unresolved={{ ...nav.search, tab: "records", metric: undefined, offset: undefined }} statusFilter={!!workspace} />}
    <p className={u.footer}>Estimates from configured prices, not invoices{o.observed_at && <> · updated <DetailTime value={o.observed_at} fallback={o.observed_at} /></>}</p>
  </Stack>;
}

/**
 * Spend, with what isn't final yet as its secondary line: "+$x on hold" (one-sentence explanation as tooltip and for
 * screen readers) or "N requests with unknown cost", and a "View unresolved" link to the records. No banner.
 */
function SpendTile({ overview: o, workspace, nav, period, render }: { overview: UsageOverview; workspace?: Workspace; nav: UsageNav; period: UsagePeriod; render: ReactElement }) {
  const t = o.tiles.spend, held = positive(t.held_microusd), unknown = positive(t.unresolved_attempts);
  const requests = countLabel(t.unresolved_attempts, "request");
  const why = `${formatMicroUsd(t.held_microusd)} is on hold for ${requests} whose final cost isn't known yet; it counts toward budgets until resolved, and the final cost may be higher.`;
  const hint = held ? <span title={why}>+{formatMicroUsd(t.held_microusd)} on hold<span className="sr-only">. {why}</span></span>
    : unknown ? <span title="Their final cost isn't known yet, so Spend is a lower bound.">{requests} with unknown cost</span> : undefined;
  // The unresolved link shares the hint line, so all three tiles keep the same layout and their charts line up.
  const target: DashboardSearch = { ...nav.search, tab: "records", metric: undefined, offset: undefined, cost_status: workspace ? held ? "on_hold" : "cost_unknown" : undefined };
  const line = hint ? <>{hint} · <ResourceLink search={target}>View unresolved</ResourceLink></> : undefined;
  return <StatTile label="Spend" value={t.value == null ? null : formatMicroUsd(t.value)} hint={line}
    series={series("spend", t)} formatSeriesValue={labels("spend", t)} delta={tileDelta(t.value, t.previous, "bad", period.days)} render={render} />;
}

function DailyChart({ overview, metric, onMetric }: { overview: UsageOverview; metric: OverviewMetric; onMetric: (m: OverviewMetric) => void }) {
  const tile = overview.tiles[metric], info = metricInfo[metric], days = activeDays(tile.daily);
  const data = tile.daily.map(d => ({ label: shortDate(d.date), values: { value: chartNumber(metric, d.value) } })), format = labels(metric, tile);
  const summary = `${info.label} per UTC day, ${shortDate(overview.period.start_date)} to ${longDate(tile.daily.at(-1)?.date ?? overview.period.start_date)}. Total ${formatMetric(metric, tile.value)}.`;
  const options: [OverviewMetric, string][] = [["spend", "Spend"], ["requests", "Requests"], ["tokens", "Tokens"]];
  return <Card title={`Daily ${info.label.toLowerCase()}`} actions={<ToggleGroup aria-label="Chart measure" joined variant="outline" size="sm" value={[metric]} onValueChange={next => { const v = next[0] as OverviewMetric | undefined; if (v && v !== metric) onMetric(v); }}>{options.map(([v, l]) => <ToggleGroupItem key={v} value={v}>{l}</ToggleGroupItem>)}</ToggleGroup>}>
    {days < 2 ? <p className={u.note}>{days === 0 ? `No ${info.label.toLowerCase()} recorded on any day yet.` : `Only one day in this period has ${info.label.toLowerCase()}: ${formatMetric(metric, tile.value)} in total.`}</p>
      : <div className={u.chartScroll}><BarChart data={data} series={[{ key: "value", label: info.label }]} summary={summary} legend={false} formatValue={format} dataTable={{ caption: `${info.label} per day`, labelHeader: "UTC day" }} /></div>}
  </Card>;
}

/** A top-5 list as one compact table: name (opens the entity where there is a page) with requests, share and a share bar beneath | spend. Two columns fit a one-third-width card. */
function TopList({ title, nameHeader, rows, total, more, empty, open }: { title: string; nameHeader: string; rows: TopRow[]; total: string | null; more: DashboardSearch; empty: string; open?: (row: TopRow) => DashboardSearch | undefined }) {
  const shown = rows.slice(0, TOP_ROWS);
  // A short ghost link in the header, like Overview's "All requests →" (a long underlined link wrapped under the title in a one-third card).
  return <Card title={title} titleAs="h2" actions={<Button variant="ghost" size="sm" render={<ResourceLink search={more} aria-label="View all in Explore" />}>View all <ArrowRight aria-hidden /></Button>} flush>
    {shown.length === 0 ? <p className={u.cardNote}>{empty}</p> : <Table caption={title} stack className={u.topTable} columns={[nameHeader, { label: "Spend", numeric: true, width: "7.5rem" }]}>
      {shown.map((r, i) => { const name = r.name ?? (r.id === null ? "Service accounts" : "Unknown"), page = open?.(r); return <Tr key={`${r.id ?? "none"}-${i}`}>
        <Th scope="row"><span className={u.topName} title={r.name ?? undefined}>{page ? <ResourceLink search={page}>{name}</ResourceLink> : name}</span><span className={u.topMeta}>{formatCount(r.requests)} {r.requests === "1" ? "request" : "requests"} · {formatShare(r.spend_microusd, total)} of spend</span><span className={u.topTrack} aria-hidden><span className={u.topBar} style={{ "--percent": cssPercent(shareBasisPoints(r.spend_microusd, total)) } as CSSProperties} /></span></Th>
        <Td numeric>{formatMicroUsd(r.spend_microusd)}</Td>
      </Tr>; })}
    </Table>}
  </Card>;
}

const layerNames: Record<UsageBudgetWindow["layer"], string> = { platform: "Platform", local: "Workspace", key: "Key" };
const spanNames: Record<BudgetSpan, string> = { day: "daily", week: "weekly", month: "monthly", lifetime: "lifetime" };
/** The single compact "Budgets" row under the tiles; nothing at all when there are no budgets to show. */
function BudgetRow({ description, children }: { description: string; children: ReactNode }) {
  const id = useId();
  return <section className={u.budgetRow} aria-labelledby={id}>
    <h2 id={id} className={u.rowLabel} title={description}>Budgets<span className="sr-only">. {description}</span></h2>
    <div className={u.budgets}>{children}</div>
  </section>;
}
/** Workspace-wide budget windows with visible usage (members see none). */
function WorkspaceBudgetRow({ workspace }: { workspace: Workspace }) {
  const q = useApi<{ budgets?: UsageBudgetWindow[] }>(`${wsPath(workspace.id)}/policy`);
  if (q.isPending || q.isError) return null;
  const windows = visibleBudgets(q.data.budgets);
  if (!windows.length) return null;
  return <BudgetRow description="Spent plus on hold in each budget's current UTC window. Every budget is enforced.">
    {windows.map(w => <UsageBar key={`${w.layer}-${w.span}`} size="sm" label={`${layerNames[w.layer]} ${spanNames[w.span]} budget`} showLabel used={w.used_microusd} limit={w.amount} period={w.span}
      description={`${w.unresolved_usage ? "At least this much: some costs aren't known yet. " : ""}${resetsAt(w.window_end)}`} />)}
  </BudgetRow>;
}
/** Installation budgets (platform scope; not narrowed by filters). None configured: nothing (Admin › Limits sets them). */
function InstallationBudgetRow({ budgets }: { budgets: InstallationBudget[] | null | undefined }) {
  if (!budgets?.length) return null;
  const order: BudgetSpan[] = ["day", "week", "month", "lifetime"], sorted = [...budgets].sort((a, b) => order.indexOf(a.period) - order.indexOf(b.period));
  return <BudgetRow description="Installation budgets are shared by every workspace and not narrowed by filters. Used is spent plus on hold in each budget's current UTC window; every budget is enforced.">
    {sorted.map(b => <UsageBar key={b.period} size="sm" label={`Installation ${spanNames[b.period]} budget${b.exhausted ? " (used up)" : ""}`} showLabel used={b.used_microusd} limit={b.amount_microusd} period={b.period}
      description={`${b.unresolved_usage ? "At least this much: some costs aren't known yet. " : ""}Spent ${formatMicroUsd(b.settled_microusd)} · on hold ${formatMicroUsd(b.held_microusd)} · ${resetsAt(b.window_end, true)}`} />)}
  </BudgetRow>;
}

/**
 * Installation budgets card (Admin › Overview): each period's amount, used (settled plus on hold, as admission counts
 * it), and both parts. A lower bound when some costs aren't known yet.
 */
export function InstallationBudgets({ budgets }: { budgets: InstallationBudget[] | null | undefined }) {
  // null/absent: not reported (workspace scope or an older gateway). An empty list is a real answer: none is set.
  if (!budgets) return null;
  if (!budgets.length) return <Card title="Installation budgets"><p className={u.note}>No installation-wide budget. Workspace budgets and type defaults still apply; Admin › Limits can add one shared by every workspace.</p></Card>;
  const order: BudgetSpan[] = ["day", "week", "month", "lifetime"], sorted = [...budgets].sort((a, b) => order.indexOf(a.period) - order.indexOf(b.period));
  return <Card title="Installation budgets" description="Shared by every workspace. Used is spent plus on hold in each budget's current UTC window; every budget is enforced. Not narrowed by filters.">
    <div className={u.budgets}>{sorted.map(b => <UsageBar key={b.period} label={`Installation ${spanNames[b.period]} budget${b.exhausted ? " (used up)" : ""}`} showLabel used={b.used_microusd} limit={b.amount_microusd} period={b.period}
      description={`${b.unresolved_usage ? "At least this much: some costs aren't known yet. " : ""}Spent ${formatMicroUsd(b.settled_microusd)} · on hold ${formatMicroUsd(b.held_microusd)} · ${resetsAt(b.window_end, true)}`} />)}</div>
  </Card>;
}

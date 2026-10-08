/*
 * Usage & costs › Overview: one answer first (tiles with sparkline and Δ vs the
 * previous equal period), then budgets, the daily chart, top lists and the
 * collapsed accounting details. Each tile opens its routed chart page.
 */
import { DetailTime } from "../../components/templates/when";
import { wsPath, platformPath, type InstallationBudget, type Workspace } from "../../lib/api";
import { formatMicroUsd } from "../../lib/governance";
import type { DashboardSearch } from "../../lib/permissions";
import { countLabel, formatCount } from "../../lib/reports";
import { activeDays, chartLabeler, chartNumber, comparisonLabel, decimalChange, filterCount, formatMetric, formatRateMicroUsd, formatRatioPercent, longDate, metricInfo, resetsAt, shortDate, usageFilters, usageQuery, visibleBudgets, type BudgetSpan, type ChartMetric, type Tile, type TopRow, type UsageBudgetWindow, type UsageContext, type UsageOverview, type UsagePeriod } from "../../lib/usage";
import { ErrorNotice, Stack, useApi } from "../../components/ui";
import { ResourceLink } from "../../components/navigation-link";
import { StatTile, StatTileGrid } from "../../components/templates/stat-tile";
import { PercentBarCell } from "../../components/templates/percent-bar-cell";
import { UsageBar } from "../../components/templates/usage-bar";
import { InfoBanner } from "../../components/templates/notices";
import { Card } from "../../components/ui/card/card";
import { BarChart } from "../../components/ui/bar-chart/bar-chart";
import { LineChart } from "../../components/ui/line-chart/line-chart";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { ToggleGroup, ToggleGroupItem } from "../../components/ui/toggle-group/toggle-group";
import { Time } from "../../components/ui/time/time";
import { IconButton } from "../../components/ui/button/button";
import { Filter } from "lucide-react";
import { AccountingDetails } from "./accounting";
import type { UsageNav } from "./shared";
import u from "./usage.module.css";

const zero = (v: string | null | undefined) => v != null && /^0+(?:\.0+)?$/.test(v);
const positive = (v: string | null | undefined) => v != null && /^\d+$/.test(v) && BigInt(v) > 0n;
const series = (metric: ChartMetric, t: Tile) => t.daily.map(d => d.value == null ? null : chartNumber(metric, d.value));
/** Exact labels for a tile's drawn daily values. */
const labels = (metric: ChartMetric, t: Tile) => chartLabeler(metric, t.daily.map(d => d.value));
const tileKeys: Record<ChartMetric, keyof UsageOverview["tiles"]> = { spend: "spend", requests: "requests", tokens: "tokens", cache_hit_rate: "cache_hit_rate", blended: "blended_microusd_per_million" };

export function UsageOverviewTab({ workspace, ctx, period, nav, workspaceFilter }: { workspace?: Workspace; ctx: UsageContext; period: UsagePeriod; nav: UsageNav; workspaceFilter?: string }) {
  const base = workspace ? wsPath(workspace.id) : platformPath, filters = usageFilters(nav.search, ctx), filtered = filterCount(filters) > 0;
  const q = useApi<UsageOverview>(`${base}/usage/overview?${usageQuery(period, workspaceFilter, filters)}`);
  if (q.isPending) return <p role="status">Loading usage…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const o = q.data, t = o.tiles, vs = comparisonLabel(period.days), idle = zero(t.requests.value) && zero(t.spend.value);
  const chart = (metric: ChartMetric) => <ResourceLink search={{ ...nav.search, tab: "chart", metric, offset: undefined }} />;
  const retries = positive(t.requests.attempts) && positive(t.requests.value) && BigInt(t.requests.attempts!) > BigInt(t.requests.value!) ? BigInt(t.requests.attempts!) - BigInt(t.requests.value!) : 0n;
  const unknownTokens = positive(t.tokens.unknown_token_attempts);
  const rate = formatRatioPercent(t.cache_hit_rate.value), blended = t.blended_microusd_per_million.value, blendedText = formatRateMicroUsd(blended);
  const unresolved = positive(t.spend.unresolved_attempts) || positive(t.spend.held_microusd);
  const metric: ChartMetric = metricInfo[nav.search.metric as ChartMetric] ? nav.search.metric as ChartMetric : "spend";
  return <Stack gap={6}>
    {o.observed_at && <p className={u.note}>Updated <DetailTime value={o.observed_at} fallback={o.observed_at} /> · estimates from configured prices, not provider invoices.</p>}
    <StatTileGrid label="Usage summary" columns={5}>
      <StatTile label="Spend" value={t.spend.value == null ? null : formatMicroUsd(t.spend.value)} hint={positive(t.spend.held_microusd) ? `${formatMicroUsd(t.spend.held_microusd)} on hold` : "Final costs"} series={series("spend", t.spend)} formatSeriesValue={labels("spend", t.spend)} delta={{ current: t.spend.value, previous: t.spend.previous, increaseIs: "bad", label: vs }} render={chart("spend")} />
      <StatTile label="Requests" value={t.requests.value == null ? null : formatCount(t.requests.value)} hint={retries > 0n ? `including ${formatCount(retries.toString())} retr${retries === 1n ? "y" : "ies"}` : undefined} series={series("requests", t.requests)} formatSeriesValue={labels("requests", t.requests)} delta={{ current: t.requests.value, previous: t.requests.previous, increaseIs: "neutral", label: vs }} render={chart("requests")} />
      <StatTile label="Tokens" value={t.tokens.value == null ? null : `${unknownTokens ? "At least " : ""}${formatCount(t.tokens.value)}`} hint={unknownTokens ? `${countLabel(t.tokens.unknown_token_attempts, "request")} didn't report tokens` : t.tokens.input_tokens != null && t.tokens.output_tokens != null ? `${formatCount(t.tokens.input_tokens)} in · ${formatCount(t.tokens.output_tokens)} out` : undefined} series={series("tokens", t.tokens)} formatSeriesValue={labels("tokens", t.tokens)} delta={{ current: t.tokens.value, previous: t.tokens.previous, increaseIs: "neutral", label: vs }} render={chart("tokens")} />
      <StatTile label="Cache hit rate" value={rate ?? (idle ? "—" : null)} hint="Share of input tokens read from cache" series={series("cache_hit_rate", t.cache_hit_rate)} formatSeriesValue={labels("cache_hit_rate", t.cache_hit_rate)} delta={{ change: decimalChange(t.cache_hit_rate.value, t.cache_hit_rate.previous, t.cache_hit_rate.change_ratio), increaseIs: "good", label: vs }} render={chart("cache_hit_rate")} />
      <StatTile label="Cost per 1M tokens" value={blended == null ? (idle ? "—" : null) : <span title={`${blendedText.exact} per 1M tokens${blendedText.rounded ? " (exact)" : ""}`}>{blendedText.text}</span>} hint="Final costs over their tokens" series={series("blended", t.blended_microusd_per_million)} formatSeriesValue={labels("blended", t.blended_microusd_per_million)} delta={{ change: decimalChange(blended, t.blended_microusd_per_million.previous, t.blended_microusd_per_million.change_ratio), increaseIs: "bad", label: vs }} render={chart("blended")} />
    </StatTileGrid>
    {unresolved && <InfoBanner title="Some costs aren't final yet" actions={workspace ? <><ResourceLink search={{ ...nav.search, tab: "records", cost_status: "on_hold", offset: undefined }}>Show requests on hold</ResourceLink><ResourceLink search={{ ...nav.search, tab: "records", cost_status: "cost_unknown", offset: undefined }}>Show requests with unknown cost</ResourceLink></> : undefined}>
      {`${formatMicroUsd(t.spend.held_microusd)} is on hold for ${formatCount(t.spend.unresolved_attempts)} request${t.spend.unresolved_attempts === "1" ? "" : "s"} whose final cost isn't known yet; it counts toward budgets until resolved and the final cost may be higher.`}
    </InfoBanner>}
    {workspace ? <Budgets workspace={workspace} /> : <InstallationBudgets budgets={o.installation_budgets} />}
    {idle ? <Card><EmptyState title={filtered ? "No requests match these filters" : "No requests in this period"} titleAs="h2" description={filtered ? "Change or clear the filters to see more." : workspace ? `Requests made with ${workspace.kind === "personal" || !workspace.capabilities.view_all_activity ? "your keys" : `${workspace.name}'s keys`} appear here.` : "Requests made in any workspace appear here."} /></Card> : <>
      <DailyChart overview={o} metric={metric} onMetric={m => nav.navigate({ metric: m === "spend" ? undefined : m })} />
      <div className={u.grid}>
        {/* A name opens the key, model or person (where this portal has a page for it); the funnel button filters the page (review #39). */}
        <TopList title="Top API keys" rows={o.top.keys} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: "key", metric: undefined, then: undefined }} empty="No key spent anything yet." drill={r => r.id ? { ...nav.search, key_id: r.id, offset: undefined } : undefined} open={r => workspace && r.id ? { page: "key-detail", ws: workspace.id, record: r.id } : undefined} />
        <TopList title="Top models" rows={o.top.models} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: undefined, metric: undefined, then: undefined }} empty="No model spent anything yet." rate drill={r => r.model_id ? { ...nav.search, model_id: r.model_id, offset: undefined } : undefined} open={r => !r.model_id ? undefined : workspace ? { page: "workspace-model", ws: workspace.id, record: r.model_id } : { page: "model-detail", record: r.model_id }} />
        {ctx.members && o.top.members !== null && <TopList title="Top members" rows={o.top.members} total={t.spend.value} more={{ ...nav.search, tab: "explore", group: "member", metric: undefined, then: undefined }} empty="No member spent anything yet." drill={r => r.id ? { ...nav.search, actor_user_id: r.id, offset: undefined } : undefined} open={r => !workspace && r.id ? { page: "user-detail", record: r.id } : undefined} />}
      </div>
    </>}
    <AccountingDetails path={`${base}/cost-report?${usageQuery(period, workspaceFilter)}&compare=none`} />
  </Stack>;
}

function DailyChart({ overview, metric, onMetric }: { overview: UsageOverview; metric: ChartMetric; onMetric: (m: ChartMetric) => void }) {
  const tile = overview.tiles[tileKeys[metric]], info = metricInfo[metric], days = activeDays(tile.daily);
  const data = tile.daily.map(d => ({ label: shortDate(d.date), values: { value: chartNumber(metric, d.value) } })), format = labels(metric, tile);
  const summary = `${info.label} per UTC day, ${shortDate(overview.period.start_date)} to ${longDate(tile.daily.at(-1)?.date ?? overview.period.start_date)}. Total ${formatMetric(metric, tile.value)}.`;
  const options: [ChartMetric, string][] = [["spend", "Spend"], ["requests", "Requests"], ["tokens", "Tokens"], ["cache_hit_rate", "Cache hit"], ["blended", "$/1M"]];
  return <Card title={`Daily ${info.label.toLowerCase()}`} description={info.additive ? `Exact values per day are under "Show data".` : "Days without data are left blank."} actions={<ToggleGroup aria-label="Chart measure" joined variant="outline" size="sm" overflow="scroll" value={[metric]} onValueChange={next => { const v = next[0] as ChartMetric | undefined; if (v && v !== metric) onMetric(v); }}>{options.map(([v, l]) => <ToggleGroupItem key={v} value={v}>{l}</ToggleGroupItem>)}</ToggleGroup>}>
    {days < 2 ? <p className={u.note}>{days === 0 ? `No ${info.label.toLowerCase()} recorded on any day yet.` : `Only one day in this period has ${info.label.toLowerCase()}, so there is no chart yet: ${formatMetric(metric, tile.value)} in total.`}</p>
      : <div className={u.chartScroll}>{info.additive ? <BarChart data={data} series={[{ key: "value", label: info.label }]} summary={summary} legend={false} formatValue={format} dataTable={{ caption: `${info.label} per day`, labelHeader: "UTC day" }} /> : <LineChart data={data} series={[{ key: "value", label: info.label }]} summary={summary} legend={false} formatValue={format} dataTable={{ caption: `${info.label} per day`, labelHeader: "UTC day" }} />}</div>}
  </Card>;
}

/**
 * A top-10 list. A row's name opens the entity (`open`); a separate funnel button filters every tab to it (`drill`);
 * `rate` adds the row's blended cost per 1M tokens (Unknown when no settled attempt reported tokens, never $0).
 */
function TopList({ title, rows, total, more, empty, drill, open, rate = false }: { title: string; rows: TopRow[]; total: string | null; more: UsageNav["search"]; empty: string; drill?: (row: TopRow) => DashboardSearch | undefined; open?: (row: TopRow) => DashboardSearch | undefined; rate?: boolean }) {
  return <Card title={title} titleAs="h2" actions={<ResourceLink search={more}>Explore</ResourceLink>}>
    {rows.length === 0 ? <p className={u.note}>{empty}</p> : <ol className={u.topList} aria-label={title}>{rows.map((r, i) => { const name = r.name ?? (r.id === null ? "Service accounts" : "Unknown"), target = drill?.(r), page = open?.(r); return <li key={`${r.id ?? "none"}-${i}`} className={u.topItem}>
      <span className={u.topHead}><span className={u.topName} title={r.name ?? undefined}>{page ? <ResourceLink search={page}>{name}</ResourceLink> : name}</span>{target && <IconButton size="sm" icon={<Filter aria-hidden />} label={`Filter by ${name}`} render={<ResourceLink search={target} />} />}</span>
      <span className={u.topAmount}>{formatMicroUsd(r.spend_microusd)}</span>
      <span className={u.topShare}><PercentBarCell part={r.spend_microusd} total={total} /> <span className={u.note}>{countLabel(r.requests, "request")}{rate && r.blended_microusd_per_million !== undefined ? r.blended_microusd_per_million === null && r.tokens === "0" ? <> · Not token-priced</> : <> · <span title={`${formatRateMicroUsd(r.blended_microusd_per_million).exact} per 1M tokens`}>{formatRateMicroUsd(r.blended_microusd_per_million).text}</span> per 1M tokens</> : ""}</span></span>
    </li>; })}</ol>}
  </Card>;
}

const layerNames: Record<UsageBudgetWindow["layer"], string> = { platform: "Platform", local: "Workspace", key: "Key" };
const spanNames: Record<BudgetSpan, string> = { day: "daily", week: "weekly", month: "monthly", lifetime: "lifetime" };
/** Budget cards: workspace-wide windows with visible usage (members see none). */
function Budgets({ workspace }: { workspace: Workspace }) {
  const q = useApi<{ budgets?: UsageBudgetWindow[] }>(`${wsPath(workspace.id)}/policy`);
  if (q.isPending || q.isError) return null;
  const windows = visibleBudgets(q.data.budgets), canSee = workspace.kind === "personal" || workspace.capabilities.view_all_activity;
  if (!windows.length) return canSee ? <p className={u.note}>No budget is set for {workspace.kind === "personal" ? "your personal workspace" : workspace.name}.</p> : null;
  return <Card title="Budgets" description="Spent plus on hold in each budget's current UTC window. Every budget is enforced.">
    <div className={u.budgets}>{windows.map(w => <UsageBar key={`${w.layer}-${w.span}`} label={`${layerNames[w.layer]} ${spanNames[w.span]} budget`} showLabel used={w.used_microusd} limit={w.amount} period={w.span}
      description={`${w.unresolved_usage ? "At least this much: some costs aren't known yet. " : ""}${resetsAt(w.window_end)}`} />)}</div>
  </Card>;
}

/**
 * Installation budgets (platform scope): each period's amount, used (settled plus on hold, as admission counts it),
 * and both parts. Ignores the filters. A lower bound when some costs aren't known yet.
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

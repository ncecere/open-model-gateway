/*
 * Usage & costs › Explore: metric × group by × then by × top N (URL-backed) →
 * a daily chart of the top groups and a table with %-of-total bars. The CSV is
 * the current table only, behind an explicit button that states its scope.
 */
import { wsPath, platformPath, type Workspace } from "../../lib/api";
import { formatMicroUsd } from "../../lib/governance";
import { formatCount } from "../../lib/reports";
import { chartLabeler, chartNumber, dimensionOptions, filterCount, seriesValue, usageFilters, downloadText, exploreCsv, exploreMetrics, exploreQuery, exploreTopN, formatMetric, groupName, longDate, metricInfo, periodLabel, pivotFromUsageSearch, pivotToUsageSearch, shortDate, type Dimension, type ExploreMetric, type ExploreResponse, type ExploreRow, type UsageContext, type UsagePeriod } from "../../lib/usage";
import { Button, ErrorNotice, Stack, useAction, useApi } from "../../components/ui";
import { PivotControls, PIVOT_NONE } from "../../components/templates/pivot-controls";
import { PercentBarCell } from "../../components/templates/percent-bar-cell";
import { ViewDataTable } from "../../components/templates/table-view";
import { Card } from "../../components/ui/card/card";
import { BarChart } from "../../components/ui/bar-chart/bar-chart";
import { LineChart } from "../../components/ui/line-chart/line-chart";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import type { DataTableColumn } from "../../components/ui/data-table/data-table";
import type { UsageNav } from "./shared";
import u from "./usage.module.css";

type TableRow = ExploreRow & { key: string; other?: boolean };
const MAX_SERIES = 7;

export function UsageExploreTab({ workspace, ctx, period, nav, workspaceFilter }: { workspace?: Workspace; ctx: UsageContext; period: UsagePeriod; nav: UsageNav; workspaceFilter?: string }) {
  const ask = useAction(), base = workspace ? wsPath(workspace.id) : platformPath, dims = dimensionOptions(ctx), pivot = pivotFromUsageSearch(nav.search, ctx);
  const filters = usageFilters(nav.search, ctx), filtered = filterCount(filters) > 0;
  const q = useApi<ExploreResponse>(`${base}/usage/explore?${exploreQuery(period, pivot, workspaceFilter, filters)}`);
  const dimLabel = (d: Dimension | null) => dims.find(o => o.value === d)?.label ?? "Group";
  const controls = <PivotControls value={{ metric: pivot.metric, groupBy: pivot.groupBy, thenBy: pivot.thenBy === "none" ? PIVOT_NONE : pivot.thenBy, topN: String(pivot.top) }} topN={exploreTopN}
    metrics={exploreMetrics.map(m => ({ value: m, label: metricInfo[m].label }))} dimensions={dims}
    onChange={v => nav.navigate(pivotToUsageSearch({ metric: v.metric as ExploreMetric, groupBy: v.groupBy as Dimension, thenBy: v.thenBy === PIVOT_NONE ? "none" : v.thenBy as Dimension, top: Number(v.topN) }))} />;
  const exportTable = (res: ExploreResponse, rows: number) => ask({ title: "Export this table as CSV?", submitLabel: "Download CSV", successNotice: "CSV downloaded.",
    description: `Downloads the ${rows} row${rows === 1 ? "" : "s"} shown: ${metricInfo[res.metric].label.toLowerCase()} by ${dimLabel(res.group_by).toLowerCase()}${res.then_by ? ` then ${dimLabel(res.then_by).toLowerCase()}` : ""}, ${periodLabel(period)}${res.other ? ", plus one \"Other\" row" : ""}. Totals only: no request records, prompts or responses. Amounts are exact estimates from configured prices.`,
    run: async () => { downloadText(exploreCsv(res, period), `usage-${res.metric}-by-${res.group_by}${res.then_by ? `-${res.then_by}` : ""}-${period.start_date}-${period.last_date}.csv`); } });
  return <Stack gap={6}>
    <Card title="Build a breakdown" description="Pick a measure and how to break it down. Top groups are ranked by the measure.">{controls}</Card>
    {q.isPending ? <p role="status">Loading breakdown…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <ExploreResult res={q.data} period={period} dimLabel={dimLabel} onExport={exportTable} filtered={filtered} />}
  </Stack>;
}

function ExploreResult({ res, period, dimLabel, onExport, filtered }: { res: ExploreResponse; period: UsagePeriod; dimLabel: (d: Dimension | null) => string; onExport: (res: ExploreResponse, rows: number) => void; filtered: boolean }) {
  const info = metricInfo[res.metric], money = res.metric === "spend";
  if (!res.rows.length) return <Card><EmptyState title="Nothing to break down" titleAs="h2" description={`No ${info.label.toLowerCase()} in ${periodLabel(period)}${filtered ? " with these filters" : ""}.`} /></Card>;
  const rows: TableRow[] = [...res.rows.map((r, i) => ({ ...r, key: String(i) })), ...(res.other ? [{ key: "other", other: true, group: { id: null, name: "Other" }, then: null, value: res.other.value, share: res.other.share, held_microusd: null, unresolved_attempts: null }] : [])];
  const columns: DataTableColumn<TableRow>[] = [
    { id: "group", header: dimLabel(res.group_by), rowHeader: true, cell: r => r.other ? `Other ${dimLabel(res.group_by).toLowerCase()}s` : groupName(r.group, res.group_by) },
    ...(res.then_by ? [{ id: "then", header: dimLabel(res.then_by), cell: (r: TableRow) => r.other ? "—" : groupName(r.then, res.then_by) }] : []),
    { id: "value", header: `${info.label} (${info.unit})`, numeric: true, cell: r => formatMetric(res.metric, r.value) },
    ...(info.additive ? [{ id: "share", header: "Share of total", cell: (r: TableRow) => <PercentBarCell part={r.value} total={res.total.value} /> }] : []),
    ...(money ? [{ id: "held", header: "On hold (USD)", numeric: true, defaultHiddenNarrow: true, cell: (r: TableRow) => r.other ? "—" : formatMicroUsd(r.held_microusd) }, { id: "unknown", header: "Cost unknown (attempts)", numeric: true, defaultHiddenNarrow: true, cell: (r: TableRow) => r.other ? "—" : formatCount(r.unresolved_attempts) }] : []),
  ];
  return <Card title={`${info.label} by ${dimLabel(res.group_by).toLowerCase()}${res.then_by ? ` and ${dimLabel(res.then_by).toLowerCase()}` : ""}`} description={<>Total {formatMetric(res.metric, res.total.value)}{money && res.total.held_microusd && res.total.held_microusd !== "0" ? ` · ${formatMicroUsd(res.total.held_microusd)} on hold` : ""} · {periodLabel(period)}</>}
    actions={<Button size="sm" variant="secondary" onClick={() => onExport(res, rows.length)}>Export table (CSV)</Button>}>
    <Stack gap={5}>
      <ExploreChart res={res} />
      <ViewDataTable<TableRow> caption={`${info.label} breakdown`} columns={columns} data={rows} getRowId={r => r.key} hideDensity />
      {(res.truncated || res.then_by) && <p className={u.note}>{res.truncated ? `Showing the top ${new Set(res.rows.map(r => `${r.group.id}|${r.group.name}`)).size} groups; the rest are combined in "Other". ` : ""}{res.then_by ? `Each group lists its top 5 ${dimLabel(res.then_by).toLowerCase()}s.` : ""}</p>}
    </Stack>
  </Card>;
}

/** Day grouping: one bar per active day. Otherwise the daily series of the top groups, stacked (rates: lines). */
function ExploreChart({ res }: { res: ExploreResponse }) {
  const info = metricInfo[res.metric];
  if (res.group_by === "day") {
    if (res.then_by) return null; // Rows are per day and sub-group; no per-day total to chart.
    const data = res.rows.map(r => ({ label: shortDate(r.group.name ?? ""), values: { value: chartNumber(res.metric, r.value) } }));
    if (data.length < 2) return null;
    const summary = `${info.label} on ${data.length} days with activity.`, format = chartLabeler(res.metric, res.rows.map(r => r.value));
    return <div className={u.chartScroll}>{info.additive ? <BarChart data={data} series={[{ key: "value", label: info.label }]} summary={summary} legend={false} formatValue={format} /> : <LineChart data={data} series={[{ key: "value", label: info.label }]} summary={summary} legend={false} formatValue={format} />}</div>;
  }
  if (res.series.length < 2) return null;
  const groups = res.rows.filter((r, i) => res.rows.findIndex(x => x.group.id === r.group.id && x.group.name === r.group.name) === i).slice(0, MAX_SERIES);
  const series = groups.map((g, i) => ({ key: `g${i}`, label: groupName(g.group, res.group_by) }));
  // Numbers only draw the marks; titles and labels map back to the exact decimal strings.
  const data = res.series.map(d => ({ label: shortDate(d.date), values: Object.fromEntries(groups.map((g, i) => [`g${i}`, chartNumber(res.metric, seriesValue(d, g.group.id))])) }));
  const format = chartLabeler(res.metric, res.series.flatMap(d => d.values.map(v => v.value)));
  const summary = `Daily ${info.label.toLowerCase()} of the top ${groups.length} groups from ${shortDate(res.series[0]!.date)} to ${longDate(res.series.at(-1)!.date)}.`;
  const distinct = new Set(res.rows.map(r => `${r.group.id}|${r.group.name}`)).size;
  return <Stack gap={2}><div className={u.chartScroll}>{info.additive ? <BarChart data={data} series={series} layout="stack" summary={summary} formatValue={format} /> : <LineChart data={data} series={series} summary={summary} formatValue={format} />}</div>
    {distinct > groups.length && <p className={u.note}>The chart shows the top {groups.length} {groups.length === 1 ? "group" : "groups"}; the table shows all {distinct}.</p>}</Stack>;
}

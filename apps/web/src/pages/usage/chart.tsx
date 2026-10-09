/*
 * Usage & costs › {measure}: the routed page a tile opens (`?tab=chart&metric=…`,
 * "Back to Usage & costs"; no drawer). A stacked daily chart of the top models
 * and a per-model Min / Max / Avg per day and Total table with units.
 */
import { wsPath, platformPath, type Workspace } from "../../lib/api";
import { activeDays, chartLabeler, chartNumber, exploreQuery, formatMetric, groupStats, longDate, metricInfo, periodLabel, seriesValue, shortDate, usageContext, usageFilters, usageQuery, type ChartMetric, type ExploreMetric, type ExploreResponse, type UsageFilters, type UsageOverview, type UsagePeriod } from "../../lib/usage";
import { ErrorNotice, Stack, useApi } from "../../components/ui";
import { useCrumbTail } from "../../components/layout/breadcrumbs";
import { Card } from "../../components/ui/card/card";
import { BarChart } from "../../components/ui/bar-chart/bar-chart";
import { LineChart } from "../../components/ui/line-chart/line-chart";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { Table, Td, Th, Tr } from "../../components/ui/table/table";
import type { UsageNav } from "./shared";
import u from "./usage.module.css";

const MAX_SERIES = 7;

export function UsageChart({ workspace, metric, period, nav, workspaceFilter }: { workspace?: Workspace; metric: ChartMetric; period: UsagePeriod; nav: UsageNav; workspaceFilter?: string }) {
  useCrumbTail(metricInfo[metric].label);
  const filters = usageFilters(nav.search, usageContext(workspace));
  return metric === "blended" ? <BlendedChart workspace={workspace} period={period} workspaceFilter={workspaceFilter} filters={filters} /> : <ModelChart workspace={workspace} metric={metric} period={period} workspaceFilter={workspaceFilter} filters={filters} />;
}

function ModelChart({ workspace, metric, period, workspaceFilter, filters }: { workspace?: Workspace; metric: ExploreMetric; period: UsagePeriod; workspaceFilter?: string; filters: UsageFilters }) {
  const base = workspace ? wsPath(workspace.id) : platformPath, info = metricInfo[metric];
  const q = useApi<ExploreResponse>(`${base}/usage/explore?${exploreQuery(period, { metric, groupBy: "model", thenBy: "none", top: 10 }, workspaceFilter, filters)}`);
  if (q.isPending) return <p role="status">Loading chart…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const res = q.data, stats = groupStats(res, metric, period.days);
  if (!stats.length) return <Card><EmptyState title={`No ${info.label.toLowerCase()} in this period`} titleAs="h2" description={periodLabel(period)} /></Card>;
  const charted = res.rows.slice(0, MAX_SERIES), series = charted.map((r, i) => ({ key: `g${i}`, label: stats[i]!.name }));
  const data = res.series.map(d => ({ label: shortDate(d.date), values: Object.fromEntries(charted.map((r, i) => [`g${i}`, chartNumber(metric, seriesValue(d, r.group.id))])) }));
  const format = chartLabeler(metric, res.series.flatMap(d => d.values.map(v => v.value)));
  const summary = `Daily ${info.label.toLowerCase()} by model, ${shortDate(period.start_date)} to ${longDate(period.last_date)}. Total ${formatMetric(metric, res.total.value)}.`;
  const unit = info.additive ? `${info.unit} per day` : info.unit;
  return <Stack gap={6}>
    <Card title={`Daily ${info.label.toLowerCase()} by model`} description={`Total ${formatMetric(metric, res.total.value)} · ${periodLabel(period)}${res.rows.length > MAX_SERIES ? ` · chart shows the top ${MAX_SERIES} models` : ""}`}>
      {data.length < 2 ? <p className={u.note}>The period is a single day, so there is no chart.</p> : <div className={u.chartScroll}>{info.additive ? <BarChart size="lg" layout="stack" data={data} series={series} summary={summary} formatValue={format} dataTable={{ caption: `${info.label} per day and model`, labelHeader: "UTC day" }} /> : <LineChart size="lg" data={data} series={series} summary={summary} formatValue={format} dataTable={{ caption: `${info.label} per day and model`, labelHeader: "UTC day" }} />}</div>}
    </Card>
    <Card title="By model" description={info.additive ? `Per UTC day over ${period.days} days; idle days count as zero; ≈ is rounded down.` : "Over days with data; Overall is the whole period."} flush>
      <Table caption={`${info.label} by model`} stack columns={["Model", { label: `Min (${unit})`, numeric: true }, { label: `Max (${unit})`, numeric: true }, { label: `Avg (${unit})`, numeric: true }, { label: `${info.additive ? "Total" : "Overall"} (${info.unit})`, numeric: true }]}>
        {stats.map(r => <Tr key={r.key}><Th scope="row">{r.name}</Th><Td numeric>{r.min}</Td><Td numeric>{r.max}</Td><Td numeric>{r.avg}</Td><Td numeric>{r.total}</Td></Tr>)}
        {res.other && <Tr><Th scope="row">Other models</Th><Td numeric>—</Td><Td numeric>—</Td><Td numeric>—</Td><Td numeric>{formatMetric(metric, res.other.value)}</Td></Tr>}
      </Table>
    </Card>
  </Stack>;
}

/** Cost per 1M tokens: the overview's daily series (no per-model breakdown exists for this measure). */
function BlendedChart({ workspace, period, workspaceFilter, filters }: { workspace?: Workspace; period: UsagePeriod; workspaceFilter?: string; filters: UsageFilters }) {
  const base = workspace ? wsPath(workspace.id) : platformPath, info = metricInfo.blended;
  const q = useApi<UsageOverview>(`${base}/usage/overview?${usageQuery(period, workspaceFilter, filters)}`);
  if (q.isPending) return <p role="status">Loading chart…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const tile = q.data.tiles.blended_microusd_per_million, data = tile.daily.map(d => ({ label: shortDate(d.date), values: { value: chartNumber("blended", d.value) } }));
  return <Card title={`Daily ${info.label.toLowerCase()}`} description={`${formatMetric("blended", tile.value)} for the period · ${periodLabel(period)}. Final costs over the tokens of the same requests.`}>
    <Stack gap={4}>{activeDays(tile.daily) < 2 ? <p className={u.note}>Fewer than two days have final costs with tokens, so there is no chart.</p> : <div className={u.chartScroll}><LineChart size="lg" data={data} series={[{ key: "value", label: info.label }]} legend={false} summary={`${info.label} per UTC day.`} formatValue={chartLabeler("blended", tile.daily.map(d => d.value))} dataTable={{ caption: `${info.label} per day`, labelHeader: "UTC day" }} /></div>}
      <p className={u.note}>There is no per-model breakdown for this measure yet.</p></Stack>
  </Card>;
}

/* Admin/Auditor read-only frozen comparisons (#48, PR 1). No financial writes,
 * attribution, allocation, or variance computation happens in the browser.
 */
import { useEffect, useId, useRef, useState } from "react";
import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import type { Session } from "../../lib/api";
import { permissions } from "../../lib/permissions";
import { detailTime, utcDay, utcPeriod } from "../../lib/format";
import { formatCount } from "../../lib/reports";
import { categoryText, comparisonState, formatSignedMicroUsd as money, readReconciliation, readReconciliationPage, reasonText, reconciliationPath, varianceText, type ComparisonRow, type EstimateTotals, type ListedReconciliation, type Reconciliation } from "../../lib/provider-costs";
import { Button, ErrorNotice, Heading, NativeSelect, Stack, useApiScope } from "../../components/ui";
import { DataTable, type DataTableColumn } from "../../components/ui/data-table/data-table";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { Tabs, TabsList, Tab, TabsPanel } from "../../components/ui/tabs/tabs";
import { FilterToolbar, ToolbarField } from "../../components/templates/filter-toolbar";
import { InfoBanner } from "../../components/templates/notices";
import { StatTile, StatTileGrid } from "../../components/templates/stat-tile";
import s from "./provider-reconciliations.module.css";

type Props = { session: Session; record?: string; onSelect?: (record?: string) => void };
export function ProviderReconciliations(props: Props) {
  if (!permissions(props.session).platformRead) return <EmptyState title="Access denied" description="Provider-reported cost reconciliation requires Platform Admin or Auditor access." />;
  return <ComparisonView {...props} />;
}
function ComparisonView({ record, onSelect }: Props) {
  const [local, setLocal] = useState(record);
  const selected = onSelect ? record : local;
  const origin = useRef<string | undefined>(undefined);
  const select = (next?: string) => {
    if (next) origin.current = next;
    if (onSelect) onSelect(next); else setLocal(next);
  };
  return <Stack gap={4} className={s.view}>
    <Heading title="Provider-reported cost reconciliation" description="Anthropic · Read-only" />
    <InfoBanner title="Read-only">Frozen provider-reported charges against configured-rate estimates. Not a verified invoice or a complete payable balance. Priority Tier costs are not supplied. No estimates or holds are changed here.</InfoBanner>
    {selected ? <><div><Button variant="ghost" size="sm" onClick={() => select()}>Back to reconciliations</Button></div><Detail key={selected} id={selected} /></> : <ReconciliationList onSelect={select} restoreFocusId={origin.current} />}
  </Stack>;
}
const periodKey = (r: ListedReconciliation) => `${r.report.period_start}/${r.report.period_end}`;
const reportPeriod = (r: ListedReconciliation) => utcPeriod(r.report.period_start.slice(0, 10), r.report.period_end.slice(0, 10));
function ReconciliationList({ onSelect, restoreFocusId }: { onSelect: (id: string) => void; restoreFocusId?: string }) {
  const originButton = useRef<HTMLButtonElement>(null);
  const restored = useRef(false);
  const scope = useApiScope(), [period, setPeriod] = useState("");
  const q = useInfiniteQuery({ queryKey: ["api", scope, reconciliationPath, "period-metadata"], initialPageParam: 0, retry: false,
    queryFn: ({ pageParam, signal }) => readReconciliationPage(pageParam, signal), getNextPageParam: page => page.next });
  useEffect(() => {
    if (!restored.current && originButton.current) {
      originButton.current.focus({ preventScroll: true });
      restored.current = true;
    }
  }, [q.data, restoreFocusId]);
  // Any refusal hides cached financial/metadata rows, including a failed next page.
  const rows = q.error ? [] : q.data?.pages.flatMap(p => p.rows) ?? [];
  const periods = [...new Map(rows.map(row => [periodKey(row), reportPeriod(row)])).entries()];
  const columns: DataTableColumn<ListedReconciliation>[] = [
    { id: "period", header: "Report period (UTC)", rowHeader: true, cell: r => <Button ref={r.id === restoreFocusId ? originButton : undefined} size="sm" variant="ghost" aria-label={`Open reconciliation ${r.id}`} onClick={() => onSelect(r.id)}>{reportPeriod(r)}</Button> },
    { id: "provider", header: "Provider", cell: r => r.report.provider === "anthropic" ? "Anthropic" : r.report.provider },
    { id: "account", header: "Provider account", cell: r => r.account_id },
    { id: "observed", header: "Frozen at", cell: r => detailTime(r.observed_at, "UTC") },
    { id: "revision", header: "Snapshot", cell: r => r.id },
  ];
  return <Stack gap={4}>
    <FilterToolbar label="Reconciliation filters" start={<>
      <ToolbarField label="Provider"><NativeSelect size="sm" value="anthropic" onChange={() => {}}><option value="anthropic">Anthropic</option></NativeSelect></ToolbarField>
      <ToolbarField label="Report period"><NativeSelect size="sm" value={period} onChange={e => setPeriod(e.target.value)}><option value="">All loaded periods</option>{periods.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</NativeSelect></ToolbarField>
    </>} note="Period filters apply to loaded snapshots only. Load more to search additional coverage; immutable ID order, not period order." />
    <DataTable caption="Frozen reconciliations" stack className={s.table} columns={columns} data={rows.filter(r => !period || periodKey(r) === period)} getRowId={r => r.id} loading={q.isPending} error={q.error} onRetry={() => void q.refetch()}
      empty={<EmptyState size="compact" title={period ? "No loaded reconciliations match this period" : "No frozen reconciliations"} description={period ? "Choose another loaded period or load more snapshots." : "A Platform Admin must import an Anthropic report, attest mappings and freeze a snapshot before it can be compared here."} />}
      loadMore={{ hasMore: !q.error && !!q.hasNextPage, loading: q.isFetchingNextPage, onLoadMore: () => void q.fetchNextPage(), label: "Load more reconciliations" }} />
    {!q.error && q.data?.pages.some(page => page.limitReached) && <InfoBanner title="API paging limit reached">Additional snapshots may not be shown. Period filters still cover loaded snapshots only.</InfoBanner>}
  </Stack>;
}
function Detail({ id }: { id: string }) {
  const scope = useApiScope();
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => { heading.current?.focus({ preventScroll: true }); }, []);
  const q = useQuery({ queryKey: ["api", scope, `${reconciliationPath}/${encodeURIComponent(id)}`], queryFn: ({ signal }) => readReconciliation(id, signal), retry: false });
  return <Stack gap={4}>
    <h2 ref={heading} tabIndex={-1}>Reconciliation {id}</h2>
    {q.error ? <ErrorNotice error={q.error} retry={() => void q.refetch()} />
      : !q.data ? <p role="status">Loading frozen comparison…</p>
      : <SnapshotView reconciliation={q.data} />}
  </Stack>;
}
function Totals({ totals, label }: { totals: EstimateTotals; label: string }) {
  return <StatTileGrid label={label} columns={3}>
    <StatTile label="Settled configured-rate subtotal" value={money(totals.settled_estimate_microusd)} hint={totals.complete ? "Complete within this observed scope." : "Incomplete estimate; this is a known subtotal, not a complete cost."} />
    <StatTile label="Upstream attempts" value={formatCount(totals.attempts)} />
    <StatTile label="Unresolved attempts" value={formatCount(totals.unresolved_attempts)} />
  </StatTileGrid>;
}
function Holds({ totals }: { totals: EstimateTotals }) {
  const title = useId();
  return <section aria-labelledby={title}><Stack gap={3}>
    <h3 id={title}>Holds, separate from settled estimates</h3>
    <StatTileGrid label="Separate holds" columns={3}>
      <StatTile label="Held floor (at least)" value={money(totals.held_floor_microusd)} hint="Not an upper bound. Never added to settled estimates or imported charges." />
      <StatTile label="Holds upper bound" value={money(totals.holds_upper_bound_microusd)} />
      <StatTile label="Unbounded holds" value={formatCount(totals.unbounded_holds)} />
    </StatTileGrid>
  </Stack></section>;
}
const comparisonColumns: DataTableColumn<ComparisonRow>[] = [
  { id: "day", header: "Day (UTC)", rowHeader: true, cell: r => utcDay(r.day.slice(0, 10)) },
  { id: "model", header: "Historical model / tier", cell: r => <>{r.model ?? "Unknown model"}<span className={s.secondary}>{r.service_tier ?? "Unknown tier"}</span></> },
  { id: "category", header: "Category", cell: r => r.category ? categoryText(r.category) : "Not supplied" },
  { id: "provider-workspace", header: "Provider workspace", cell: r => r.reasons.includes("estimate_only") ? "Not supplied" : r.provider_workspace_id ?? "Provider default / missing identifier" },
  { id: "workspace", header: "Actual attribution (OMG workspace)", cell: r => r.reasons.includes("estimate_only") ? "Not supplied" : r.workspace_id ?? "Shared / unallocated" },
  { id: "imported", header: "Imported charges", numeric: true, cell: r => r.imported_microusd == null ? "Not supplied" : money(r.imported_microusd) },
  { id: "variance", header: "Variance (imported − estimate)", numeric: true, cell: varianceText },
  { id: "status", header: "Comparison", cell: r => <>{comparisonState(r)}<span className={s.secondary}>{r.reasons.map(reasonText).join(" · ")}</span></> },
];
function ComparisonLines({ rows }: { rows: ComparisonRow[] }) {
  const [model, setModel] = useState(""), [workspace, setWorkspace] = useState("");
  const models = [...new Set(rows.flatMap(row => row.model ? [row.model] : []))].sort();
  const workspaces = [...new Set(rows.flatMap(row => [row.workspace_id, ...(row.workspace_estimates?.map(e => e.workspace_id) ?? [])]).filter((id): id is string => !!id))].sort();
  const filtered = rows.filter(row => (!model || row.model === model) && (!workspace || row.workspace_id === workspace || row.workspace_estimates?.some(e => e.workspace_id === workspace)));
  return <Stack gap={4}>
    <FilterToolbar start={<>
      <ToolbarField label="Historical model"><NativeSelect size="sm" value={model} onChange={e => setModel(e.target.value)}><option value="">All non-personal models</option>{models.map(id => <option key={id} value={id}>{id}</option>)}</NativeSelect></ToolbarField>
      <ToolbarField label="OMG workspace"><NativeSelect size="sm" value={workspace} onChange={e => setWorkspace(e.target.value)}><option value="">All non-personal workspaces</option>{workspaces.map(id => <option key={id} value={id}>{id}</option>)}</NativeSelect></ToolbarField>
    </>} note="Filters change displayed lines only, never frozen totals or actual attribution. A shared actual bucket stays shared even when one workspace's estimate appears alongside it." />
    <DataTable key={`${model}/${workspace}`} caption="Provider comparison lines" stack className={s.table} columns={comparisonColumns} data={filtered} pageSize={20} getRowId={(_, i) => String(i)} empty={<EmptyState size="compact" title={model || workspace ? "No comparison lines match these filters" : "No non-personal comparison lines"} description="Personal activity, if supplied, appears only in Personal totals." />} />
  </Stack>;
}
type WorkspaceEstimate = { day: string; model: string | null; tier: string | null; workspace: string; totals: EstimateTotals; basis: string };
const estimateColumns: DataTableColumn<WorkspaceEstimate>[] = [
  { id: "workspace", header: "OMG workspace", rowHeader: true, cell: r => r.workspace },
  { id: "day", header: "Day (UTC)", cell: r => utcDay(r.day.slice(0, 10)) },
  { id: "model", header: "Historical model / tier", cell: r => <>{r.model ?? "Unknown model"}<span className={s.secondary}>{r.tier ?? "Unknown tier"}</span></> },
  { id: "settled", header: "Settled configured-rate subtotal", numeric: true, cell: r => money(r.totals.settled_estimate_microusd) },
  { id: "complete", header: "Coverage", cell: r => <>{r.totals.complete ? "Complete" : "Incomplete"}<span className={s.secondary}>{r.basis}</span></> },
];
function SnapshotView({ reconciliation: r }: { reconciliation: Reconciliation }) {
  const snapshot = r.snapshot, personalTitle = useId();
  const estimates = snapshot.rows.flatMap(row => (row.estimate ? [{ workspace_id: row.workspace_id!, totals: row.estimate }] : row.workspace_estimates ?? []).map(e => ({ day: row.day, model: row.model, tier: row.service_tier, workspace: e.workspace_id, totals: e.totals, basis: row.estimate ? "Estimate only" : "Alongside provider bucket; no proration" })));
  return <Stack gap={4}>
    <div className={s.facts}>
      <h3>{utcPeriod(snapshot.period.start.slice(0, 10), snapshot.period.end.slice(0, 10))}</h3>
      <p>UTC coverage [{snapshot.period.start}, {snapshot.period.end}) · Frozen at {detailTime(snapshot.observed_at, "UTC")}</p>
      <p>Provider account: {r.account_id} · Snapshot: {r.id}</p>
      <p>Import revision: {snapshot.import_id} · Mapping revision: {snapshot.mapping_id}</p>
      <p>Query version: <span>{snapshot.query_version}</span></p>
      <p>OMG uses attempt-start UTC; provider accounting buckets may differ. Unknown variance stays unknown; shared or ambiguous actual charges are never allocated.</p>
    </div>
    <StatTileGrid label="Snapshot-wide imported charges" columns={2}><StatTile label="Imported charges (signed)" value={money(snapshot.imported_microusd)} hint="Includes supplied adjustments. Not a payable balance." /></StatTileGrid>
    <Totals totals={snapshot.estimates} label="Snapshot-wide configured-rate estimates" />
    <Tabs defaultValue="comparison"><Stack gap={4}>
    <TabsList variant="pills" overflow="scroll" aria-label="Frozen comparison views"><Tab value="comparison">Comparison</Tab><Tab value="estimates">Estimates &amp; holds</Tab><Tab value="coverage">Unresolved coverage</Tab><Tab value="personal">Personal totals</Tab><Tab value="provenance">Source &amp; provenance</Tab></TabsList>
    <TabsPanel value="comparison"><ComparisonLines rows={snapshot.rows} /></TabsPanel>
    <TabsPanel value="estimates"><Stack gap={4}><Holds totals={snapshot.estimates} />
    <DataTable caption="Configured-rate estimates by workspace" stack className={s.table} columns={estimateColumns} data={estimates} pageSize={20} getRowId={(_, i) => String(i)} empty={<EmptyState size="compact" title="No shared-workspace estimates supplied" />} /></Stack></TabsPanel>
    <TabsPanel value="coverage"><Stack gap={4}><p>Do not add unresolved coverage across buckets or to estimates or holds. It overlaps the frozen estimates and may appear in multiple comparison buckets.</p>
    <DataTable caption="Unresolved coverage (overlapping, not additive)" stack className={s.table} columns={[
      { id: "day", header: "Day (UTC)", rowHeader: true, cell: row => utcDay(row.day.slice(0, 10)) },
      { id: "model", header: "Model", cell: row => row.model ?? "Unknown model" },
      { id: "unresolved", header: "Unresolved attempts", cell: row => formatCount(row.unresolved_coverage?.unresolved_attempts) },
    ] as DataTableColumn<ComparisonRow>[]} data={snapshot.rows.filter(row => !!row.unresolved_coverage && row.unresolved_coverage.unresolved_attempts !== "0")} pageSize={20} getRowId={(_, i) => String(i)} empty={<EmptyState size="compact" title="No overlapping unresolved coverage supplied" />} /></Stack></TabsPanel>
    <TabsPanel value="personal"><section aria-labelledby={personalTitle}><Stack gap={3}>
      <h3 id={personalTitle}>Personal workspaces, combined totals only</h3>
      <p>Subsets of snapshot-wide totals, not additional amounts. No owner, workspace, model, key or request detail. A small total can describe a single person.</p>
      <StatTileGrid label="Combined personal actual charges" columns={2}><StatTile label="Exclusively attributed imported charges" value={money(snapshot.personal_activity.exclusively_attributed_imported_microusd)} hint="Shared charges are never allocated to this total." /></StatTileGrid>
      <Totals totals={snapshot.personal_activity.estimates} label="Combined personal estimates" />
      <StatTileGrid label="Combined personal holds (separate subset)" columns={2}>
        <StatTile label="Personal held floor (at least)" value={money(snapshot.personal_activity.estimates.held_floor_microusd)} />
        <StatTile label="Personal holds upper bound" value={money(snapshot.personal_activity.estimates.holds_upper_bound_microusd)} />
      </StatTileGrid>
      <DataTable caption="Combined personal charges by category" stack className={s.table} columns={[
        { id: "category", header: "Category", rowHeader: true, cell: ([category]) => categoryText(category) },
        { id: "amount", header: "Imported charges", numeric: true, cell: ([, amount]) => money(amount) },
      ] as DataTableColumn<[string, string]>[]} data={Object.entries(snapshot.personal_activity.exclusively_attributed_imported_by_category)} getRowId={([category]) => category} empty={<EmptyState size="compact" title="No exclusively attributed personal categories supplied" />} />
    </Stack></section></TabsPanel>
    <TabsPanel value="provenance"><Stack gap={4}><DataTable caption="Category coverage" stack className={s.table} columns={[
      { id: "category", header: "Category", rowHeader: true, cell: row => categoryText(row.category) },
      { id: "status", header: "Source coverage", cell: row => row.status === "supplied" ? "Supplied" : "Not supplied" },
    ] as DataTableColumn<Reconciliation["snapshot"]["category_coverage"][number]>[]} data={snapshot.category_coverage} getRowId={row => row.category} />
    <DataTable caption="Frozen mapping provenance (shared scopes only)" stack className={s.table} columns={[
      { id: "scope", header: "Provider workspace", rowHeader: true, cell: row => row.provider_workspace_id ?? "Provider default / missing identifier" },
      { id: "connection", header: "OMG connection", cell: row => row.connection_id },
      { id: "workspace", header: "OMG workspace", cell: row => row.workspace_id ?? "Shared / unallocated" },
      { id: "period", header: "Effective interval (UTC, end exclusive)", cell: row => `${row.effective_start} – ${row.effective_end}` },
      { id: "exclusive", header: "Attested scope", cell: row => row.exclusive && !row.non_omg_traffic ? "Exclusive" : "Shared / unallocated" },
      { id: "evidence", header: "Evidence reference (not independently verified)", cell: row => row.evidence_reference },
    ] as DataTableColumn<Reconciliation["snapshot"]["mapping_provenance"][number]>[]} data={snapshot.mapping_provenance} pageSize={20} getRowId={(_, i) => String(i)} empty={<EmptyState size="compact" title="No shared-scope mapping provenance supplied" description="Personal attestations are retained in the immutable mapping revision, not exposed in this comparison." />} /></Stack></TabsPanel>
    </Stack></Tabs>
  </Stack>;
}

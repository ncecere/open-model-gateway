/*
 * Logs › Batches (workspace and Admin) and a batch's own page. One toolbar
 * row (status), compact rows: batch id + model | mode | status | progress |
 * cost so far | created, with Cancel and result downloads in the row menu.
 * The batch page puts Cancel in its header, a 4-stat summary row, the files,
 * and line outcomes (counts only) on demand.
 *
 * Privacy is the server's: members see the batches they created, workspace
 * admins the workspace's; Admin shows Team/Project batches and only totals
 * for personal workspaces. Metadata only: no lines or results are shown.
 */
import { useState } from "react";
import { Layers } from "lucide-react";
import { api, type Session, type Workspace } from "../lib/api";
import { batchCancelPath, batchPath, batchStatus, batchStatusLabel, batchStatusTone, batchStatuses, batchesPath, costSoFar, endpointLabel, isActive, isBatchStatus, modeHint, modeLabel, priceListLabel, progress, stopReason, type BatchDetail, type BatchPage, type BatchRow } from "../lib/batches";
import { fileContentPath } from "../lib/files";
import { pauseLabel, type BatchRouteWait } from "../lib/batch-scheduling";
import type { DashboardSearch } from "../lib/permissions";
import type { LogsScope } from "../lib/requests";
import { Button, ErrorNotice, Stack, useAction, useApi } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { FilterToolbar } from "../components/templates/filter-toolbar";
import { ActionMenu } from "../components/templates/action-menu";
import { CopyId } from "../components/templates/copy-id";
import { PercentBarCell } from "../components/templates/percent-bar-cell";
import { RecordPage } from "../components/templates/record-page";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { Badge, StatusBadge } from "../components/ui/badge/badge";
import { DataTable, type DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Time } from "../components/ui/time/time";
import s from "./shared.module.css";

export const batchSearch = (ws: string, id: string): DashboardSearch => ({ page: "batch-detail", ws, record: id });

function StatusCell({ b }: { b: BatchRow }) {
  const status = batchStatus(b), reason = stopReason(b.error_code) ?? (isActive(b) ? pauseLabel(b.waiting_reason) : null);
  return <StatusBadge tone={batchStatusTone(status)} size="sm" pulse={isActive(b)} title={reason ?? undefined}>{batchStatusLabel(status)}</StatusBadge>;
}
function ModeCell({ b }: { b: BatchRow }) {
  const price = priceListLabel(b);
  return <span className={s.badges}><Badge size="sm" title={modeHint(b.mode)}>{modeLabel(b.mode)}</Badge>{price && <Badge size="sm" tone={price.tone} title={price.hint}>{price.text}</Badge>}</span>;
}
function CostCell({ b }: { b: BatchRow }) {
  const cost = costSoFar(b);
  return <span title={cost.detail ?? undefined}>{cost.text}</span>;
}

/** The Batches tab of Logs. */
export function BatchesPanel({ scope }: { scope: LogsScope }) {
  const nav = useDashboardNavigation(), ask = useAction(), platform = scope.kind === "platform";
  const search: DashboardSearch = nav?.search ?? (platform ? { page: "platform-logs", tab: "batches" } : { page: "requests", ws: scope.workspace.id, tab: "batches" });
  const [offset, setOffset] = useState(0);
  const status = isBatchStatus(search.status) ? search.status : undefined;
  const page = useApi<BatchPage>(batchesPath(platform ? { kind: "platform" } : { kind: "workspace", ws: scope.workspace.id }, status, offset));
  const rows = page.data?.data ?? [];
  const go = (patch: Partial<DashboardSearch>) => { setOffset(0); nav?.navigate({ ...search, ...patch }); };
  const cancel = (b: BatchRow) => ask({ title: `Cancel ${b.id}?`, description: "No new lines start. Lines already running finish; the rest are listed in the error file.", danger: true, submitLabel: "Cancel batch", successNotice: "Cancelling batch.",
    run: (_, signal) => api(batchCancelPath(b.workspace_id, b.id), { method: "POST", signal }) });
  const canCancel = (b: BatchRow) => !platform && isActive(b) && !b.cancel_requested_at && (b.mine || (scope.kind === "workspace" && scope.workspace.capabilities.view_all_activity));
  const columns: DataTableColumn<BatchRow>[] = [
    { id: "batch", header: "Batch", rowHeader: true, hideable: false, cell: b => <span className={s.priceStack}>{platform ? <span className={s.primary}>{b.model}</span> : <ResourceLink search={batchSearch(b.workspace_id, b.id)} className={s.primary}>{b.model}</ResourceLink>}<CopyId value={b.id} label="batch ID" /></span> },
    ...(platform ? [{ id: "workspace", header: "Workspace", cell: (b: BatchRow) => b.workspace_name } satisfies DataTableColumn<BatchRow>] : []),
    { id: "mode", header: "Mode", cell: b => <ModeCell b={b} /> },
    { id: "status", header: "Status", cell: b => <StatusCell b={b} /> },
    { id: "progress", header: "Progress", cell: b => { const p = progress(b); return <PercentBarCell value={p.text} part={p.done} total={p.total} tone={b.failed > 0 ? "warning" : "primary"} />; } },
    { id: "cost", header: "Cost so far", numeric: true, cell: b => <CostCell b={b} /> },
    { id: "created", header: "Created", defaultHiddenNarrow: true, cell: b => <Time value={b.created_at} format="relative" /> },
  ];
  return <Stack gap={4}>
    <FilterToolbar facets={[{ id: "status", label: "Status", type: "select", placeholder: "Any status", options: batchStatuses.map(x => ({ value: x.value, label: x.label })) }]}
      values={{ status: status ? [status] : [] }} onChange={next => { const picked = Array.isArray(next.status) ? next.status[0] : undefined; go({ status: isBatchStatus(picked) ? picked : undefined }); }} />
    {page.error && !page.data ? <ErrorNotice error={page.error} retry={() => void page.refetch()} /> :
      <DataTable<BatchRow> caption="Batches" stack columns={columns} data={rows} getRowId={b => b.id} rowLabel={b => b.id} manual loading={page.isFetching} error={page.data ? page.error : undefined} onRetry={() => void page.refetch()}
        rowActions={b => <ActionMenu label={`Actions for ${b.id}`} actions={[
          { label: "Open", hidden: platform, render: <ResourceLink search={batchSearch(b.workspace_id, b.id)} /> },
          { label: "Download output", hidden: platform || !b.output_file_id, render: b.output_file_id ? <a href={fileContentPath(b.workspace_id, b.output_file_id)} download /> : undefined },
          { label: "Download errors", hidden: platform || !b.error_file_id, render: b.error_file_id ? <a href={fileContentPath(b.workspace_id, b.error_file_id)} download /> : undefined },
          { label: "Cancel…", danger: true, hidden: !canCancel(b), onSelect: () => cancel(b) },
        ]} />}
        cursor={{ hasPrevious: offset > 0, hasNext: !!page.data?.has_more, onPrevious: () => setOffset(Math.max(0, offset - 50)), onNext: () => setOffset(offset + 50) }}
        empty={<EmptyState size="compact" icon={<Layers />} title={status ? "No batches match this status" : "No batches yet"} description={status ? undefined : "Create one with POST /v1/batches from a file uploaded for batch."} />} />}
    {platform && page.data?.personal && <p className={s.note} title="Personal workspaces are private: only totals are shown.">Personal workspaces: {page.data.personal.active} running, {page.data.personal.finished} finished ({page.data.personal.failed} failed).</p>}
  </Stack>;
}

/** Why lines wait, per model route, with the batch's place in each queue. */
function WaitList({ waits }: { waits: BatchRouteWait[] }) {
  return <ul className={s.plainList}>{waits.map((w, i) => <li key={`${w.model}:${i}`}>
    <span className={s.primary}>{w.model}</span> · {pauseLabel(w.reason) ?? "Starting lines"}{w.queue > 1 ? ` · #${w.position} of ${w.queue} in queue` : ""} · {w.waiting_lines.toLocaleString("en-US")} waiting{w.running_lines ? ` · ${w.running_lines} running` : ""}
  </li>)}</ul>;
}

/** A batch's own page: summary, files and line outcomes. */
export function BatchDetailPage({ workspace, id }: { session: Session; workspace: Workspace; id: string }) {
  const ask = useAction();
  const detail = useApi<BatchDetail>(batchPath(workspace.id, id)), b = detail.data?.batch;
  const back = { label: "Batches", search: { page: "requests", ws: workspace.id, tab: "batches" } as DashboardSearch };
  if (detail.error && !b) return <ErrorNotice error={detail.error} retry={() => void detail.refetch()} />;
  if (!b) return <p role="status" className={s.note}>Loading batch…</p>;
  const status = batchStatus(b), p = progress(b), cost = costSoFar(b), price = priceListLabel(b), reason = stopReason(b.error_code) ?? (isActive(b) ? pauseLabel(b.waiting_reason) : null);
  const waits = isActive(b) ? detail.data?.scheduling ?? [] : [];
  const canCancel = isActive(b) && !b.cancel_requested_at && (b.mine || workspace.capabilities.view_all_activity);
  const cancel = () => ask({ title: `Cancel ${b.id}?`, description: "No new lines start. Lines already running finish; the rest are listed in the error file.", danger: true, submitLabel: "Cancel batch", successNotice: "Cancelling batch.",
    run: (_, signal) => api(batchCancelPath(workspace.id, b.id), { method: "POST", signal }) });
  const file = (fileId: string | null, label: string) => fileId ? <a href={fileContentPath(workspace.id, fileId)} download>{label}</a> : <span className={s.muted}>None</span>;
  return <RecordPage title={b.id} back={back}
    meta={<><StatusBadge tone={batchStatusTone(status)} pulse={isActive(b)}>{batchStatusLabel(status)}</StatusBadge><Badge title={modeHint(b.mode)}>{modeLabel(b.mode)}</Badge>{price && <Badge tone={price.tone} title={price.hint}>{price.text}</Badge>}</>}
    description={reason ?? undefined}
    actions={canCancel ? <Button variant="secondary" onClick={cancel}>Cancel batch</Button> : undefined}
    facts={[
      { label: "Model", value: b.model },
      { label: "Endpoint", value: endpointLabel(b.endpoint) },
      { label: "Created", value: <Time value={b.created_at} format="datetime" /> },
      { label: "Finished", value: b.completed_at ? <Time value={b.completed_at} format="datetime" /> : null },
      { label: "API key", value: b.key_name ?? null },
      { label: "Input", value: file(b.input_file_id, "Input file") },
      { label: "Output", value: file(b.output_file_id, "Output file") },
      { label: "Errors", value: file(b.error_file_id, "Error file") },
    ]}
    sections={[{ id: "scheduling", title: "Scheduling", hidden: !waits.length, content: <WaitList waits={waits} /> }, { id: "outcomes", title: "Line outcomes", hidden: !detail.data?.outcomes.length, content: <ul className={s.plainList}>{detail.data?.outcomes.map(o => <li key={`${o.state}:${o.code}`}>{batchStatusLabel(o.state)}{o.code ? ` · ${o.code.replace(/_/g, " ")}` : ""}: {o.lines.toLocaleString("en-US")}</li>)}</ul> }]}>
    <StatTileGrid columns={4} label="Batch summary">
      <StatTile label="Progress" value={p.text} hint={p.total ? `${Math.floor((p.done * 100) / p.total)}% of lines finished` : undefined} />
      <StatTile label="Completed" value={b.completed.toLocaleString("en-US")} />
      <StatTile label="Failed" value={b.failed.toLocaleString("en-US")} />
      <StatTile label="Cost so far" value={cost.text} hint={cost.detail ?? (b.mode === "gateway" ? "Standard prices per line" : undefined)} />
    </StatTileGrid>
  </RecordPage>;
}

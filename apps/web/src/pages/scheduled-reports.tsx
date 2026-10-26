import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, ApiError, type Session, type Workspace } from "../lib/api";
import { aggregateMeasureLabel, canReadReports, coverageLabel, countOrUnavailable, deliveryStatusLabel, deliveryStatusTone, dimensionLabel, recipientLabel, reportsPath, scheduleBody, scheduleFields, unavailableLabel, unavailableReasonLabel, type Aggregate, type ReportRun, type Schedule, type ScheduleList } from "../lib/scheduled-reports";
import { formatMicroUsd } from "../lib/governance";
import { Button, Alert, Empty, ErrorNotice, FormField, Heading, Inline, NativeSelect, Panel, Stack, Table, useAction, useApi, useApiScope } from "../components/ui";
import { StatusBadge } from "../components/ui/badge/badge";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { ResourceLink } from "../components/navigation-link";
import { TextLink } from "../components/ui/text-link/text-link";
import { detailTime, utcPeriod } from "../lib/format";
import type { DashboardSearch } from "../lib/permissions";
import s from "./shared.module.css";
const listSearch = (workspace?: Workspace): DashboardSearch => ({ page: workspace ? "costs" : "platform-costs", ws: workspace?.id, tab: "scheduled-reports" });
const runSearch = (workspace: Workspace | undefined, schedule: string, run: string): DashboardSearch => ({ page: workspace ? "workspace-report-run" : "platform-report-run", ws: workspace?.id, record: schedule, run });
const when = (value?: string | null) => value ? detailTime(value) : "Not initialized";
function Delivery({ value }: { value: ReportRun["delivery"] }) {
  if (!value) return <span>Delivery status unavailable</span>;
  return <Inline gap={2} align="center"><StatusBadge tone={deliveryStatusTone(value.status)} size="sm">{deliveryStatusLabel(value.status)}</StatusBadge><span className={s.secondary}>{value.sent} sent · {value.failed} failed · {value.waiting} waiting / {recipientLabel(value.recipients)}</span></Inline>;
}
export function ScheduledReports({ session, workspace }: { session: Session; workspace?: Workspace }) {
  if (!canReadReports(session, workspace)) return <Alert title="Access not available">Scheduled reports require workspace owner/admin access.</Alert>;
  return <ScheduleListView session={session} workspace={workspace} />;
}
function ScheduleListView({ session, workspace }: { session: Session; workspace?: Workspace }) {
  const base = reportsPath(workspace), q = useApi<ScheduleList>(`${base}?limit=100`), ask = useAction(), [selected, setSelected] = useState("");
  const write = workspace ? true : session.capabilities.platform_write, schedules = q.data?.data ?? [], chosen = schedules.some(r => r.id === selected) ? selected : schedules[0]?.id;
  const editor = (existing?: Schedule) => ask({ title: existing ? "Edit schedule" : "Create schedule", description: `${workspace?.kind === "personal" ? "Owner-only personal report; only you can receive it. " : workspace ? "Workspace-wide aggregates for current owners/admins. " : "Installation aggregates, optionally narrowed by historical cost center. Personal activity is collapsed. "}${existing ? "Changes affect future occurrences only; scope and historical cost-center filter are immutable." : "Created paused. Resume after reviewing email configuration. No historical replay."}`, submitLabel: existing ? "Save schedule" : "Create schedule", fields: scheduleFields(session, workspace, existing), run: (values, signal) => api(existing ? `${base}/${existing.id}` : base, { method: existing ? "PATCH" : "POST", body: scheduleBody(values, workspace, session, !!existing), signal }) });
  const change = (row: Schedule, action: "pause" | "resume" | "delete") => ask({ title: `${action[0]!.toUpperCase()}${action.slice(1)} schedule?`, description: action === "delete" ? "Deletion is final. Links to this schedule will stop working and unsent deliveries will be suppressed." : action === "pause" ? "Future generation and remaining email attempts stop. Existing authorized snapshots stay readable until expiry." : "Creator and recipient authority is checked again. Only the next future occurrence is scheduled; completed periods are not replayed.", submitLabel: `${action[0]!.toUpperCase()}${action.slice(1)} schedule`, danger: action === "delete", run: (_, signal) => api(`${base}/${row.id}`, { method: action === "delete" ? "DELETE" : "PATCH", ...action === "delete" ? {} : { body: { state: action === "pause" ? "paused" : "active" } }, signal }) });
  return <Stack gap={5}>
    <Heading title="Scheduled reports" description={workspace ? workspace.kind === "personal" ? "Owner-only personal aggregates" : `${workspace.name} · Workspace-wide aggregates` : "Installation and historical cost-center aggregates"} actions={write && <Button onClick={() => editor()}>Create schedule</Button>} />
    {!write && <p>Read-only · Platform Auditor</p>}
    {q.data && (!q.data.configuration.smtp_configured || !q.data.configuration.public_url_configured) && <Alert tone="warning" title={session.capabilities.platform_write ? "Email configuration required" : "Email delivery unavailable"}>{session.capabilities.platform_write ? <>Configure SMTP and an operator-controlled GATEWAY_PUBLIC_URL before enabling delivery. <ResourceLink search={{ page: "settings-email" }}>Email settings</ResourceLink></> : <>Email delivery isn't configured yet. Ask a Platform Admin to set it up.</>}</Alert>}
    <p>Weekly Monday–Monday or calendar-month UTC periods. Delivery follows the saved local time zone. Configured-rate estimates, not invoices; snapshots expire after 90 days.</p>
    {q.isPending && <p role="status">Loading schedules…</p>}{q.isError && <ErrorNotice error={q.error} retry={() => void q.refetch()} />}
    {q.data && <Table label="Report schedules" rows={schedules} rowKey={r => r.id} columns={[
      { title: "Schedule", render: r => <Button size="sm" variant="ghost" onClick={() => setSelected(r.id)}>{r.cadence} · {r.scope_kind.replaceAll("_", " ")} · {r.id.slice(0, 8)}</Button> },
      { title: "Allocation", render: r => r.cost_center_id ? `Historical cost center ${r.cost_center_id}` : "All allocations (including Unallocated)" },
      { title: "Delivery time", render: r => `${r.delivery_time} · ${r.timezone}` },
      { title: "Next occurrence", render: r => <span>{when(r.next_run_at)}{r.state === "paused" && " (paused; will be recalculated on resume)"}</span> },
      { title: "State", render: r => r.state === "active" ? <StatusBadge tone="success">Active</StatusBadge> : <StatusBadge tone="neutral">Paused</StatusBadge> },
      { title: "Recipients", render: r => `${r.recipient_user_ids.length} internal user${r.recipient_user_ids.length === 1 ? "" : "s"}` },
      { title: "Actions", hidden: !write, render: r => write && <Inline gap={2}><Button size="sm" variant="secondary" onClick={() => editor(r)}>Edit schedule</Button><Button size="sm" variant="secondary" onClick={() => change(r, r.state === "active" ? "pause" : "resume")}>{r.state === "active" ? "Pause" : "Resume"}</Button><Button size="sm" variant="ghost" onClick={() => change(r, "delete")}>Delete</Button></Inline> },
    ]} />}
    {q.data && !schedules.length && <Empty title="No scheduled reports yet">Create a paused schedule to get started.</Empty>}
    {q.data?.has_more && <Alert title="Schedule list limit reached">Only the first 100 schedules are shown.</Alert>}
    {chosen && <><FormField label="Recent runs for schedule"><NativeSelect value={chosen} onChange={e => setSelected(e.target.value)}>{schedules.map(r => <option key={r.id} value={r.id}>{r.cadence} · {r.id.slice(0, 8)}</option>)}</NativeSelect></FormField><RecentRuns key={chosen} base={base} schedule={chosen} workspace={workspace} /></>}
  </Stack>;
}
function RecentRuns({ base, schedule, workspace }: { base: string; schedule: string; workspace?: Workspace }) {
  const q = useApi<{ data: ReportRun[]; outcomes?: { category: string; created_at: string }[] }>(`${base}/${schedule}/runs?limit=20`);
  return <Stack gap={4}><h2>Recent runs</h2><p>Latest 20 occurrences. Skipped intervals are not combined into a report.</p><Button size="sm" variant="secondary" onClick={() => void q.refetch()}>Refresh runs</Button>
    {q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : q.isPending ? <p role="status">Loading runs…</p> : <><Table label="Recent report runs" rows={q.data.data} rowKey={r => r.id} columns={[
      { title: "UTC period", render: r => <ResourceLink search={runSearch(workspace, schedule, r.id)}>{utcPeriod(r.period_start.slice(0, 10), r.period_end.slice(0, 10))}</ResourceLink> },
      { title: "Observed", render: r => when(r.observed_at) },
      { title: "Delivery", render: r => <Delivery value={r.delivery} /> },
      { title: "Skipped intervals", render: r => r.skipped_intervals ?? "Unavailable" },
      { title: "Retention", render: r => r.expired ? "Expired" : `Expires ${when(r.expires_at)}` },
    ]} />{q.data.outcomes?.filter(o => o.category !== "success").map((o, i) => <Alert key={i} tone="warning" title={`Generation ${o.category.replaceAll("_", " ")}`}>{when(o.created_at)}</Alert>)}</>}
  </Stack>;
}
export function ReportRunPage({ session, workspace, schedule, run }: { session: Session; workspace?: Workspace; schedule: string; run: string }) {
  if (!canReadReports(session, workspace)) return <Alert title="Access not available">You cannot read this report.</Alert>;
  return <ReportRunView workspace={workspace} schedule={schedule} run={run} />;
}
function ReportRunView({ workspace, schedule, run }: { workspace?: Workspace; schedule: string; run: string }) {
  // Override the application's 15-second cache freshness: every page entry and
  // focus reauthorizes. Hide cached contents while the browser checks authority.
  const path = `${reportsPath(workspace)}/${encodeURIComponent(schedule)}/runs/${encodeURIComponent(run)}`, scope = useApiScope();
  const q = useQuery({ queryKey: ["api", scope, path], queryFn: ({ signal }) => api<ReportRun>(path, { signal }), staleTime: 0, gcTime: 0, retry: false, refetchOnMount: "always", refetchOnWindowFocus: "always" });
  const r = q.isError || q.isFetching && typeof window !== "undefined" ? undefined : q.data, snapshot = r?.snapshot;
  return <Stack gap={5} className={s.page}><ResourceLink search={listSearch(workspace)}>Back to Scheduled reports</ResourceLink><Heading title="Scheduled report" description="Immutable snapshot · Configured-rate estimates, not invoices" />
    {(q.isPending || q.isFetching) && <p role="status">Checking access and loading report…</p>}
    {q.isError && <><ErrorNotice error={q.error} retry={() => void q.refetch()} />{q.error instanceof ApiError && q.error.status === 404 && <p>This report is unavailable, deleted or outside your current access.</p>}</>}
    {r && (r.expired || !snapshot) ? <Alert title="Report expired">Snapshot retention is 90 days. Its contents and CSV are no longer available.</Alert> : r && snapshot && <>
      <p>{utcPeriod(r.period_start.slice(0, 10), r.period_end.slice(0, 10))} · Observed {when(r.observed_at)} · Revision {r.configuration_revision} · Expires {when(r.expires_at)}</p>
      <Inline><TextLink href={`${path}/csv`} download>Download aggregate CSV</TextLink><Button variant="secondary" size="sm" onClick={() => void q.refetch()}>Refresh report</Button></Inline>
      <Delivery value={r.delivery} /><Alert title="Holds and unknowns">Held amounts are retained floors, not additional settled charges or proven upper bounds. Unavailable values are never zero. Later reconciliation does not change this snapshot.</Alert>
      <Panel title="Full totals"><AggregateValues totals={snapshot.totals} /></Panel>
      <Panel title="Coverage"><DescriptionList layout="stacked" columns={3} items={Object.entries(snapshot.coverage).map(([key, value]) => ({ label: coverageLabel(key), value: countOrUnavailable(value) }))} /></Panel>
      {snapshot.breakdowns_truncated && <Alert tone="warning" title="Dimensions truncated">{snapshot.truncated_dimensions.map(d => `${dimensionLabel(d)}: first 100 entries`).join("; ")}. Full totals above are not truncated.</Alert>}
      {Object.entries(snapshot.breakdowns).map(([dimension, entries]) => <Stack key={dimension} gap={3}><h2>{dimensionLabel(dimension)}</h2><Table label={`${dimensionLabel(dimension)} aggregates`} rows={entries} rowKey={r => r.id ?? "aggregate"} empty={<EmptyState size="compact" title="No rows" />} columns={[
        { title: "Allocation", render: r => r.name }, { title: "Settled estimate", render: r => formatMicroUsd(r.totals.known_cost_microusd) }, { title: "Held floor", render: r => formatMicroUsd(r.totals.held_microusd) }, { title: "Attempts", render: r => countOrUnavailable(r.totals.attempts) }, { title: "Unresolved", render: r => countOrUnavailable(r.totals.unresolved_attempts) },
      ]} /></Stack>)}
      <Panel title="Unavailable measures"><DescriptionList layout="stacked" columns={2} items={Object.entries(snapshot.unavailable).map(([key, reason]) => ({ label: unavailableLabel(key), value: `Unavailable · ${unavailableReasonLabel(reason)}` }))} /></Panel>
    </>}
  </Stack>;
}
function AggregateValues({ totals }: { totals: Aggregate }) {
  return <StatTileGrid columns={3}>
    <StatTile label="Settled configured-rate estimate" value={formatMicroUsd(totals.known_cost_microusd)} />
    <StatTile label="Held floor" value={formatMicroUsd(totals.held_microusd)} />
    {(["attempts", "unresolved_attempts", "root_requests"] as const).map(key => <StatTile key={key} label={aggregateMeasureLabel(key)} value={countOrUnavailable(totals[key])} />)}
  </StatTileGrid>;
}

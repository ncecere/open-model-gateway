/*
 * Usage & costs › Cost records (workspace): one row per upstream attempt,
 * the time linking to the request page, "Cost status" for the accounting
 * state, inline URL-backed filters and a bounded CSV export.
 * Admin › Usage & costs › By workspace: per-workspace totals (request-level records stay inside each
 * workspace), with the platform's financial workspace actions.
 */
import { useState } from "react";
import { api, platformPath, platformWorkspacePath, wsPath, type Catalog, type Collection, type CostCenter, type Model, type Session, type Workspace } from "../../lib/api";
import { canReconcile, formatMicroUsd, type Breakdown, type Cost, type CostReport } from "../../lib/governance";
import { parseCheckboxValues } from "../../lib/forms";
import { formatCount, reconciliationBody, reconciliationFields } from "../../lib/reports";
import { permissions } from "../../lib/permissions";
import { saveCsv, usageCsv, EXPORT_MAX_ROWS } from "../../lib/usage-export";
import { filterCount, periodLabel, recordCostText, recordStatuses, recordStatusText, recordsQuery, usageFilters, usageStatuses, workloadLabels, type RecordFilters, type UsageContext, type UsagePeriod } from "../../lib/usage";
import { Button, Stack, useAction, useApi, useChoices } from "../../components/ui";
import { ActionMenu } from "../../components/templates/action-menu";
import { CopyId } from "../../components/templates/copy-id";
import { InfoBanner } from "../../components/templates/notices";
import { ReplacementLimitsDialog } from "../../components/scope-limits";
import { DataTable, type DataTableColumn } from "../../components/ui/data-table/data-table";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { meterSummary } from "./accounting";
import { tokensText } from "../../lib/requests";
import { tableTime } from "../../lib/format";
import { TableTime } from "../../components/templates/when";
import { ResourceLink } from "../../components/navigation-link";
import { useModelName } from "./filters";
import type { UsageNav } from "./shared";
import s from "../shared.module.css";
import u from "./usage.module.css";

const id = encodeURIComponent;
const PAGE = 50;

/**
 * The records filters from the URL (the bar is shared with Overview/Explore; see ./filters). Records match a model by
 * its API name, so the filtered model's name is looked up first; `unknownModel` when it can't be found.
 */
function useRecordFilters(workspace: Workspace | undefined, ctx: UsageContext, nav: UsageNav, workspaceFilter?: string): { filters: RecordFilters; ready: boolean; unknownModel: boolean; scopeText: string } {
  const f = usageFilters(nav.search, ctx), model = useModelName(workspace, f.model_id);
  const cost = recordStatuses.find(r => r.value === nav.search.cost_status);
  const unknownModel = !!f.model_id && !model.pending && !model.name;
  const statuses = f.status.map(v => usageStatuses.find(x => x.value === v)!.label.toLowerCase());
  const scopeText = [model.name && `model ${model.name}`, f.key_id && "one key", f.member && "one member", f.service_account_id && "one service account", f.cost_center_id && "one cost center", statuses.length && `status ${statuses.join(" or ")}`, cost && `cost "${cost.label}"`].filter(Boolean).join(", ");
  return { filters: { ...f, model: model.name, cost_status: cost?.value, workspace_id: workspaceFilter }, ready: !model.pending && !unknownModel, unknownModel, scopeText };
}
const UnknownModel = () => <InfoBanner tone="warning" title="Model filter not applied">The filtered model isn't available any more, so records can't be matched to it. Clear the model filter to see records.</InfoBanner>;

export function WorkspaceRecords({ session, workspace, ctx, period, nav }: { session: Session; workspace: Workspace; ctx: UsageContext; period: UsagePeriod; nav: UsageNav }) {
  const ask = useAction(), base = wsPath(workspace.id), offset = nav.search.offset ?? 0;
  const { filters, ready, unknownModel, scopeText } = useRecordFilters(workspace, ctx, nav);
  // One request with every filter: the server takes the key and a comma list of statuses.
  const main = useApi<Collection<Cost>>(`${base}/costs?${recordsQuery(period, filters, { platform: false, limit: PAGE, offset })}`, ready);
  const filtered = filterCount(filters) > 0 || !!filters.cost_status;
  const reconcile = (cost: Cost) => ask({ title: "Reconcile pinned usage", description: "Platform Admin action. Preserve every known original/raw and normalized count. Use authoritative evidence, including TTL allocations; the original pinned price is used. Unknown rates are not free.", fields: reconciliationFields(cost), submitLabel: "Reconcile usage", run: (v, signal) => api(`${base}/costs/${id(cost.id)}/reconcile`, { method: "POST", body: reconciliationBody(v), signal }) });
  const exportCsv = () => ask({ title: "Export CSV?", submitLabel: "Download CSV", successNotice: "CSV downloaded.",
    description: `Downloads up to ${EXPORT_MAX_ROWS.toLocaleString("en-US")} records from ${periodLabel(period)}${scopeText ? ` (${scopeText})` : ""}, newest first, with exact micro-USD amounts. ${workspace.kind !== "personal" && workspace.capabilities.view_all_activity ? `It covers everyone in ${workspace.name}.` : "It covers only your own keys."} No prompts or responses are included.`,
    run: async (_, signal) => { const blob = await usageCsv(workspace.id, EXPORT_MAX_ROWS, 0, signal, recordsQuery(period, filters, { platform: false })); signal.throwIfAborted(); saveCsv(blob, 0); } });
  const columns: DataTableColumn<Cost>[] = [
    // The time opens the request's page (records and Requests share formatters: "Oct 8, 2:30 AM", "145 in · 0 out").
    { id: "time", header: "Time", rowHeader: true, cell: c => <><ResourceLink search={{ page: "request-detail", ws: workspace.id, record: c.root_request_id }} aria-label={`Request ${c.root_request_id.slice(0, 8)}, ${tableTime(c.started_at)}`}><TableTime value={c.started_at} fallback={c.started_at} /></ResourceLink><span className={s.secondary}>{workloadLabels[c.workload_kind] ?? c.workload_kind}{c.attempt_number > 1 ? ` · attempt ${c.attempt_number}` : ""}</span></> },
    { id: "model", header: "Model", cell: c => <>{c.public_model}<span className={s.secondary}>{c.provider}</span></> },
    { id: "cost_status", header: "Cost status", cell: c => <>{recordStatusText(c)}{c.cost_microusd == null && c.unbounded_cost && c.unresolved_reason !== "unbounded_cost_or_unknown_rate" ? <span className={s.secondary}>No upper limit known</span> : null}</> },
    { id: "tokens", header: "Tokens", numeric: true, defaultHiddenNarrow: true, cell: c => tokensText(c.input_tokens, c.output_tokens, c.workload_kind) },
    { id: "cost", header: "Cost", numeric: true, cell: c => recordCostText(c) },
    { id: "request", header: "Request ID", defaultHidden: true, cell: c => <CopyId value={c.root_request_id} label="request ID" /> },
    { id: "cost_center", header: "Cost center", defaultHidden: true, cell: c => <>{c.cost_center_name ?? "Unallocated"}{c.cost_center_code && <span className={s.secondary}>{c.cost_center_code}</span>}</> },
    { id: "meters", header: "Meters", defaultHidden: true, cell: c => meterSummary(c) },
    { id: "provider_cost", header: "Provider-reported (for checking only)", defaultHidden: true, cell: c => c.provider_cost_microusd == null ? <span className={s.muted}>Not reported</span> : formatMicroUsd(c.provider_cost_microusd) },
  ];
  const canFix = permissions(session, workspace).reconcileCosts;
  return <Stack gap={4}>
    {unknownModel && <UnknownModel />}
    <div className={u.toolbar}><Button size="sm" variant="secondary" disabled={!ready} onClick={exportCsv}>Export CSV</Button></div>
    <DataTable<Cost> caption="Cost records" stack columns={columns} data={ready ? main.data?.data ?? [] : []} getRowId={c => c.id} columnsMenu columnsMenuMin={4} loading={ready && main.isPending} error={main.error ?? undefined} onRetry={() => void main.refetch()}
      rowActions={canFix ? (c: Cost) => canReconcile(c) ? <Button size="sm" variant="secondary" onClick={() => reconcile(c)}>Reconcile</Button> : null : undefined}
      empty={<EmptyState size="compact" title="No records match" description={`No requests in ${periodLabel(period)}${filtered ? " with these filters" : ""}. Records appear as soon as a key is used.`} />}
      cursor={{ hasPrevious: offset > 0, hasNext: !!main.data?.has_more && offset + PAGE <= 100000, onPrevious: () => nav.navigate({ offset: Math.max(0, offset - PAGE) || undefined }), onNext: () => nav.navigate({ offset: offset + PAGE }), label: main.data?.data.length ? `Rows ${(offset + 1).toLocaleString("en-US")}–${(offset + main.data.data.length).toLocaleString("en-US")}` : undefined }} />
    <p className={u.note}>One row per upstream attempt; the time opens its request. The Status filter matches the request's result; Cost status is the attempt's accounting state. Costs are estimates from configured prices, not provider invoices. An amount on hold is a floor, not a cap.</p>
  </Stack>;
}

/** Admin › Usage & costs › By workspace: per-workspace totals (personal workspaces as totals only) with the same inline filters. */
export function PlatformRecords({ session, ctx, period, nav, workspaceFilter }: { session: Session; ctx: UsageContext; period: UsagePeriod; nav: UsageNav; workspaceFilter?: string }) {
  const { filters, ready, unknownModel } = useRecordFilters(undefined, ctx, nav, workspaceFilter);
  const report = useApi<CostReport>(`${platformPath}/cost-report?${recordsQuery(period, filters, { platform: true })}`, ready);
  const columns: DataTableColumn<Breakdown>[] = [
    { id: "workspace", header: "Workspace", rowHeader: true, cell: r => r.name },
    { id: "spent", header: "Spent", numeric: true, cell: r => formatMicroUsd(r.totals.known_cost_microusd) },
    { id: "held", header: "On hold", numeric: true, cell: r => formatMicroUsd(r.totals.held_microusd) },
    { id: "requests", header: "Requests", numeric: true, defaultHiddenNarrow: true, cell: r => formatCount(r.totals.root_requests) },
    { id: "unknown", header: "Cost unknown (attempts)", numeric: true, defaultHiddenNarrow: true, cell: r => formatCount(r.totals.unresolved_attempts) },
  ];
  return <Stack gap={4}>
    {unknownModel && <UnknownModel />}
    <DataTable<Breakdown> caption="By workspace" stack columns={columns} data={ready ? report.data?.breakdowns.workspaces ?? [] : []} getRowId={r => r.id ?? r.name} columnsMenu loading={ready && report.isPending} error={report.error ?? undefined} onRetry={() => void report.refetch()}
      rowActions={session.capabilities.platform_write ? (r: Breakdown) => r.id ? <FinancialWorkspaceActions session={session} workspaceId={r.id} name={r.name} /> : null : undefined}
      empty={<EmptyState size="compact" title="No usage matches" description={`No workspace used models in ${periodLabel(period)} with these filters.`} />} />
    {report.data?.breakdowns_truncated && <p className={u.note}>Only the first 100 workspaces are listed. Narrow the filters to see the rest.</p>}
    <p className={u.note}>Request-level records stay inside each workspace. Personal workspaces appear as totals only, never with their keys or requests.</p>
  </Stack>;
}

function FinancialWorkspaceActions({ session, workspaceId, name }: { session: Session; workspaceId: string; name: string }) {
  const ask = useAction(), centers = useChoices<CostCenter>(`${platformPath}/cost-centers`, session.capabilities.platform_write), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`, session.capabilities.platform_write), models = useChoices<Model>(`${platformPath}/models`, session.capabilities.platform_write), path = platformWorkspacePath(workspaceId);
  const [limits, setLimits] = useState(false);
  const modelField = { name: "model_id", label: "Platform model", type: "select" as const, required: true, options: models.data?.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}` })) ?? [] };
  return <><ActionMenu label={`Administrative configuration for ${name}`} actions={[
    { label: "Assign future cost center…", disabled: !centers.isSuccess, disabledReason: "Loading authorized cost centers", onSelect: () => ask({ title: `Future allocation · ${name}`, description: "Applies to new requests only; past usage keeps its label. This does not fetch private workspace keys, activity or owner metadata.", fields: [{ name: "cost_center_id", label: "Cost center", type: "select", options: centers.data?.filter(c => !c.archived_at).map(c => ({ value: c.id, label: `${c.name} · ${c.code}` })) ?? [], help: "Blank intentionally sets Unallocated." }], submitLabel: "Set allocation", run: (v, signal) => api(path, { method: "PATCH", body: { cost_center_id: v.cost_center_id || null }, signal }) }) },
    { label: "Assign direct model…", disabled: !models.isSuccess, disabledReason: "Loading platform models", onSelect: () => ask({ title: `Direct model assignment · ${name}`, description: "Independent of catalog selection. Financial totals identify the workspace; private grant and credential inventories are not fetched.", fields: [modelField], submitLabel: "Assign direct model", run: (v, signal) => api(`${path}/models`, { method: "POST", body: { model_id: v.model_id }, signal }) }) },
    { label: "Replace catalog availability…", disabled: !catalogs.isSuccess, disabledReason: "Loading approved catalogs", onSelect: () => ask({ title: `Replace available catalogs · ${name}`, description: "Explicit replacement, never unioned with defaults. Empty selection intentionally disables catalog availability. Ineligible catalog sources retire; independent direct assignments survive.", fields: [{ name: "catalog_ids", label: "Replacement catalogs", type: "checkboxes", value: "[]", maxSelections: 200, options: catalogs.data?.map(c => ({ value: c.id, label: c.name })) ?? [] }], submitLabel: "Set replacement catalogs", run: (v, signal) => api(`${path}/catalogs`, { method: "PUT", body: { mode: "replace", catalog_ids: parseCheckboxValues(v.catalog_ids) }, signal }) }) },
    { label: "Set custom limits…", onSelect: () => setLimits(true) },
    { label: "Reset catalog inheritance…", onSelect: () => ask({ title: `Reset catalogs · ${name}?`, description: "Removes this workspace's custom catalog list. It will follow the default catalogs for its type again.", submitLabel: "Reset catalog inheritance", run: (_, signal) => api(`${path}/catalogs`, { method: "DELETE", signal }) }) },
    { label: "Reset policy inheritance…", onSelect: () => ask({ title: `Reset ceilings · ${name}?`, description: "Deletes the platform override without resetting consumption or local restrictions.", submitLabel: "Reset policy inheritance", run: (_, signal) => api(`${path}/policy`, { method: "DELETE", signal }) }) },
    { label: "Revoke direct model…", danger: true, disabled: !models.isSuccess, disabledReason: "Loading platform models", onSelect: () => ask({ title: `Revoke direct model · ${name}`, description: "Removes only a matching direct source. A live selected-catalog source is retained. No private grant inventory is fetched.", fields: [modelField], danger: true, submitLabel: "Revoke direct source", run: (v, signal) => api(`${path}/models/${id(v.model_id)}`, { method: "DELETE", signal }) }) },
  ]} />{limits && <ReplacementLimitsDialog workspaceId={workspaceId} name={name} onClose={() => setLimits(false)} />}</>;
}

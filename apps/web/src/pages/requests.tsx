/*
 * Requests: root requests of this workspace, newest first, with filters
 * (period, model, key, status, request ID search), a column chooser and row
 * density, and cursor pagination. Everything is in the URL, so a filtered
 * view can be shared and the request page's previous/next links stay inside
 * the same filters. A row opens the request's own page (no drawers).
 *
 * Privacy is the server's: members see requests made with their own keys,
 * workspace admins everyone's, personal workspaces only their owner. Metadata
 * only; prompts and responses are never stored or shown.
 */
import { useEffect, useMemo, useState } from "react";
import { ListTree } from "lucide-react";
import { wsPath, type Grant, type Workspace } from "../lib/api";
import type { DashboardSearch, RangePreset } from "../lib/permissions";
import { activeFilterCount, compactDateTime, costText, countText, latencyText, rangeLabels, requestFilters, requestQuery, requestStatuses, requestStatusLabel, requestStatusTone, tokensText, utcDate, workloadText, type RequestFilters, type RequestPage, type RequestRow } from "../lib/requests";
import { formatMicroUsd } from "../lib/governance";
import { NARROW_QUERY, useMediaQuery } from "../lib/bitop-utils";
import type { KeyRow } from "../lib/keys";
import { Heading, Stack, useApi, useChoices } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { DateControl } from "../components/date-control";
import { IconCell, LabIcon } from "../components/provider-icon";
import { CopyId } from "../components/templates/copy-id";
import { InfoBanner } from "../components/templates/notices";
import { FilterToolbar, ToolbarField } from "../components/templates/filter-toolbar";
import type { Facet, FilterValues } from "../components/ui/filter-bar/filter-bar";
import { ColumnChooser, DensityToggle, ViewDataTable, chooserColumns, tableViewFromSearch, tableViewToSearch, type TableView } from "../components/templates/table-view";
import { StatusBadge } from "../components/ui/badge/badge";
import type { DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { NativeSelect } from "../components/ui/input/input";
import type { Scope } from "./workspace";
import s from "./shared.module.css";
import rq from "./requests.module.css";

export function requestsDescription(workspace: Workspace) {
  if (workspace.kind === "personal") return "Requests made with your keys. Only you can see them.";
  return workspace.capabilities.view_all_activity ? `Every request made with ${workspace.name}'s keys.` : `Requests made with your keys in ${workspace.name}. Workspace admins see everyone's.`;
}
/** Search values of the request page for one row, keeping the list's filters for previous/next. */
export const requestPageSearch = (ws: string, id: string, filters: RequestFilters, view?: Pick<DashboardSearch, "cols" | "density">): DashboardSearch => ({ page: "request-detail", ws, record: id, ...filters, ...view });

/** The started time as the row's link: compact on one line, the full date and time (with zone) in the tooltip. */
function StartedLink({ row, search }: { row: RequestRow; search: DashboardSearch }) {
  const when = compactDateTime(row.started_at);
  return <ResourceLink className={rq.rowLink} search={search} aria-label={`Open request ${row.root_request_id.slice(0, 8)}, ${row.model}, started ${when.full}`}><time dateTime={row.started_at} title={when.full}>{when.text}</time></ResourceLink>;
}
const requestColumns = (ws: string, filters: RequestFilters, view: Pick<DashboardSearch, "cols" | "density">): DataTableColumn<RequestRow>[] => [
  // The started time is the row's link; the whole row is clickable (a stretched link), and it's the row's one tab stop.
  { id: "started", header: "Started", rowHeader: true, hideable: false, width: "8.5rem", cell: r => <StartedLink row={r} search={requestPageSearch(ws, r.root_request_id, filters, view)} /> },
  { id: "model", header: "Model", cell: r => <IconCell icon={<LabIcon model={r.model} />}><span className={rq.truncate} title={r.model}>{r.model}</span></IconCell> },
  { id: "status", header: "Status", cell: r => <StatusBadge tone={requestStatusTone(r.status)} size="sm">{requestStatusLabel(r.status)}</StatusBadge> },
  { id: "cost", header: "Cost", numeric: true, cell: r => <CostCell row={r} /> },
  { id: "latency", header: "Latency", numeric: true, cell: r => latencyText(r.latency_ms) },
  { id: "tokens", header: "Tokens", numeric: true, cell: r => tokensText(r.input_tokens, r.output_tokens, r.workload_kind) },
  { id: "attempts", header: "Attempts", numeric: true, cell: r => r.attempts > 1 ? `${r.attempts} (${r.attempts - 1} fallback${r.attempts > 2 ? "s" : ""})` : countText(r.attempts) },
  { id: "key", header: "Key", cell: r => <span className={rq.truncateKey} title={r.key.name}>{r.key.name}</span> },
  { id: "request", header: "Request ID", cell: r => <CopyId value={r.root_request_id} label="request ID" /> },
  { id: "workload", header: "Type", defaultHidden: true, cell: r => workloadText(r.workload_kind) },
  { id: "streamed", header: "Streamed", defaultHidden: true, cell: r => r.streamed ? "Yes" : "No" },
  { id: "cost_center", header: "Cost center", defaultHidden: true, cell: r => r.cost_center ? `${r.cost_center.name} · ${r.cost_center.code}` : "Unallocated" },
];
/** Known cost; an unknown cost shows what's on hold under it (a floor, not the price). */
function CostCell({ row }: { row: RequestRow }) {
  if (row.cost_microusd !== null) return <>{costText(row.cost_microusd, row.held_microusd)}</>;
  const held = row.held_microusd && /^\d+$/.test(row.held_microusd) && BigInt(row.held_microusd) > 0n ? row.held_microusd : null;
  return <span className={rq.costCell} title={costText(null, row.held_microusd)}><span>Unknown</span>{held && <span className={s.secondary}>{formatMicroUsd(held)} on hold</span>}</span>;
}
/** Most important first, so Cost and Latency stay in view on a laptop; the rest can be shown from Columns. */
export const requestColumnIds = ["started", "model", "status", "cost", "latency", "tokens", "attempts", "key", "request", "workload", "streamed", "cost_center"];
export const requestDefaultHidden = ["workload", "streamed", "cost_center"];
/** Low-priority columns also hidden by default on a phone (≤600px), where rows stack. */
export const requestNarrowHidden = ["tokens", "attempts", "key", "request"];
const defaultHiddenFor = (narrow: boolean) => narrow ? [...requestDefaultHidden, ...requestNarrowHidden] : requestDefaultHidden;
/** The table view in the URL. Hidden-by-default columns shown again are written as `cols=none` (nothing hidden). */
export function requestView(search: DashboardSearch, narrow = false): TableView {
  return search.cols === "none" ? { hidden: [], density: search.density ?? "comfortable" } : tableViewFromSearch(search, requestColumnIds, { hidden: defaultHiddenFor(narrow), density: "comfortable" });
}
export function requestViewSearch(view: TableView, narrow = false): Pick<DashboardSearch, "cols" | "density"> {
  const v = tableViewToSearch(view), same = [...view.hidden].sort().join(",") === [...defaultHiddenFor(narrow)].sort().join(",");
  return { cols: same ? undefined : v.cols ?? "none", density: v.density === "compact" ? "compact" : undefined };
}

export function Requests({ workspace }: Scope) {
  const nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? { page: "requests", ws: workspace.id }, narrow = useMediaQuery(NARROW_QUERY);
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const filters = requestFilters(search), view = requestView(search, narrow), viewSearch = { cols: search.cols, density: search.density };
  const { query, error: filterError } = requestQuery(filters);
  query.set("limit", "50"); if (search.cursor) query.set("cursor", search.cursor);
  const page = useApi<RequestPage>(`${wsPath(workspace.id)}/requests?${query}`, !filterError);
  // Filter options: this workspace's models and the keys the caller can see (members: their own).
  const models = useChoices<Grant>(`${wsPath(workspace.id)}/models`), keys = useChoices<KeyRow>(`${wsPath(workspace.id)}/keys`);
  // Previous pages of this visit (cursors are opaque and only go forward).
  const [previous, setPrevious] = useState<(string | undefined)[]>([]);
  const filterKey = JSON.stringify(filters);
  useEffect(() => setPrevious([]), [filterKey]);
  const columns = useMemo(() => requestColumns(workspace.id, filters, viewSearch), [workspace.id, filterKey, search.cols, search.density]);
  const modelOptions = [...new Map((models.data ?? []).map(g => [g.public_name, { value: g.public_name, label: g.display_name === g.public_name ? g.public_name : `${g.display_name} · ${g.public_name}` }])).values()];
  if (filters.model && !modelOptions.some(o => o.value === filters.model)) modelOptions.unshift({ value: filters.model, label: filters.model });
  const keyOptions = (keys.data ?? []).map(k => ({ value: k.id, label: k.name, group: k.service_account_id ? "Service accounts" : "People" }));
  if (filters.key_id && !keyOptions.some(o => o.value === filters.key_id)) keyOptions.unshift({ value: filters.key_id, label: "Selected key", group: "People" });
  const filtered = activeFilterCount(filters) > 0;
  const pageNumber = previous.length + 1, rows = page.data?.data ?? [], paged = !!search.cursor || !!page.data?.next_cursor;
  // Request ID search, period, model, key and status: one FilterToolbar row (in place behind "Filters" on a phone), Columns and density at its end.
  const periodActive = filters.range && filters.range !== "30d" ? 1 : 0, viewControls = !!rows.length || paged;
  const facets: Facet[] = [
    { id: "model", label: "Model", type: "select", placeholder: "Any model", options: modelOptions },
    { id: "key_id", label: "Key", type: "select", placeholder: "Any key", options: keyOptions },
    { id: "status", label: "Status", type: "toggle", multiple: true, allLabel: "All", options: requestStatuses },
  ];
  const facetValues: FilterValues = { model: filters.model ? [filters.model] : [], key_id: filters.key_id ? [filters.key_id] : [], status: filters.status ? filters.status.split(",") : [] };
  const onFacets = (next: FilterValues) => { const one = (v: unknown) => Array.isArray(v) && typeof v[0] === "string" ? v[0] : undefined; const several = Array.isArray(next.status) ? (next.status as string[]).join(",") : ""; go({ model: one(next.model), key_id: one(next.key_id), status: several || undefined, cursor: undefined }); };
  return <Stack gap={6} className={s.page}>
    <Heading title="Requests" description={requestsDescription(workspace)} />
    <div className={s.list}>
    <FilterToolbar search={{ label: "Search by request ID", placeholder: "Request ID (at least 4 characters)", value: search.q ?? "", onChange: next => go({ q: next || undefined, cursor: undefined }) }}
      start={<PeriodControl filters={filters} onChange={patch => go({ ...patch, cursor: undefined })} />} extraActive={periodActive}
      facets={facets} values={facetValues} onChange={onFacets}
      end={viewControls && <><ColumnChooser columns={chooserColumns(columns)} hidden={view.hidden} onHiddenChange={hidden => go(requestViewSearch({ ...view, hidden }, narrow))} defaultHidden={narrow ? [...requestDefaultHidden, ...requestNarrowHidden] : requestDefaultHidden} /><DensityToggle value={view.density} onChange={density => go(requestViewSearch({ ...view, density }, narrow))} /></>} />
    {filterError && <InfoBanner tone="warning" title="Filters not applied">{filterError}</InfoBanner>}
    <ViewDataTable<RequestRow> caption="Requests" className={rq.requests} stack columns={columns} data={rows} getRowId={r => r.root_request_id} manual
      view={view} onViewChange={next => go(requestViewSearch(next, narrow))} hideViewControls
      loading={page.isFetching} error={page.error ?? models.error ?? keys.error} onRetry={() => { void page.refetch(); void models.refetch(); void keys.refetch(); }}
      empty={<EmptyState size="compact" icon={<ListTree />} title={filtered || filterError ? "No requests match these filters." : "No requests yet."} description={filtered ? "Change the period or clear filters to see more." : workspace.kind !== "personal" && !workspace.capabilities.view_all_activity ? "Requests you make with your keys in this workspace appear here." : "Requests made with this workspace's keys appear here."} />}
      cursor={paged || rows.length ? { hasPrevious: !!search.cursor, hasNext: !!page.data?.next_cursor, label: !paged || !rows.length ? undefined : search.cursor && previous.length === 0 ? "Later rows" : `Rows ${((pageNumber - 1) * 50 + 1).toLocaleString("en-US")}–${((pageNumber - 1) * 50 + rows.length).toLocaleString("en-US")}`,
        onPrevious: () => { const back = previous.at(-1); setPrevious(previous.slice(0, -1)); go({ cursor: back }); },
        onNext: () => { if (!page.data?.next_cursor) return; setPrevious([...previous, search.cursor]); go({ cursor: page.data.next_cursor }); } } : undefined} />
    </div>
    <p className={s.note}>Request metadata only: prompts and responses are never stored. Costs are estimates from configured prices; a cost that isn't known yet shows what's on hold. Times are in your time zone; periods are UTC days.</p>
  </Stack>;
}

/** Period presets (UTC days) or custom dates; the end date is shown inclusive and sent exclusive. */
export function PeriodControl({ filters, onChange }: { filters: RequestFilters; onChange: (patch: Pick<DashboardSearch, "range" | "start_date" | "end_date">) => void }) {
  const value = filters.range ?? "30d";
  const inclusiveEnd = filters.end_date ? utcDate(Date.parse(`${filters.end_date}T00:00:00Z`) - 86_400_000) : "";
  const today = utcDate(Date.now());
  const setDates = (start: string, endInclusive: string) => onChange({ range: "custom", start_date: start || undefined, end_date: endInclusive ? utcDate(Date.parse(`${endInclusive}T00:00:00Z`) + 86_400_000) : undefined });
  return <>
    <ToolbarField label="Period"><NativeSelect size="sm" value={value} onChange={event => { const v = event.target.value as RangePreset; if (v === "custom") setDates(filters.start_date ?? utcDate(Date.now() - 6 * 86_400_000), inclusiveEnd || today); else onChange({ range: v === "30d" ? undefined : v, start_date: undefined, end_date: undefined }); }}>
      {(Object.keys(rangeLabels) as (keyof typeof rangeLabels)[]).map(r => <option key={r} value={r}>{rangeLabels[r]} (UTC)</option>)}
      <option value="custom">Custom dates…</option>
    </NativeSelect></ToolbarField>
    {value === "custom" && <><ToolbarField label="From (UTC)"><DateControl id="requests-from" name="from" value={filters.start_date ?? ""} onChange={v => setDates(v, inclusiveEnd)} /></ToolbarField><ToolbarField label="To (UTC, included)"><DateControl id="requests-to" name="to" value={inclusiveEnd} onChange={v => setDates(filters.start_date ?? "", v)} /></ToolbarField></>}
  </>;
}

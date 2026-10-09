/*
 * Logs (Workspace › Logs and Admin › Usage & spend › Logs): OpenRouter-style
 * request logs as pages, not drawers. Pill tabs Requests | Generations |
 * Sessions, a row of summary tiles for the current filters, one FilterToolbar
 * (period, model, key, status, finish reason, streamed, session; Admin adds
 * Workspace) and a ViewDataTable with a column chooser and density. A row
 * opens its own routed page (request, or session) with previous/next inside
 * the same filters. Everything is in the URL.
 *
 * Privacy is the server's: in a workspace, members see requests made with
 * their own keys, workspace admins everyone's, personal workspaces only their
 * owner. Admin shows Team and Project workspaces only; personal workspaces
 * never appear as rows (their totals stay in Usage & costs). Metadata only:
 * prompts and responses are never stored or shown.
 */
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { ListTree, MessagesSquare } from "lucide-react";
import { platformPath, wsPath, type Grant, type Model, type Workspace } from "../lib/api";
import type { DashboardSearch, RangePreset } from "../lib/permissions";
import { activeFilterCount, compactDateTime, countText, finishReasonLabel, finishReasonTone, finishReasons, jobStateTone, jobText, latencyText, logPaths, logTab, logsSearch, rangeLabels, rateText, requestFilters, requestQuery, requestStatuses, requestStatusLabel, requestStatusTone, requestTarget, servedModel, servedModelText, sessionTarget, tokensText, tpsText, ttftText, utcDate, validSessionId, workloadText, type GenerationPage, type GenerationRow, type LogMetrics, type LogTab, type LogsScope, type RequestFilters, type RequestPage, type RequestRow, type SessionPage, type SessionRow } from "../lib/requests";
import { formatMicroUsd, formatUsd } from "../lib/governance";
import { Money } from "../components/templates/money";
import { NARROW_QUERY, useMediaQuery } from "../lib/bitop-utils";
import type { KeyRow } from "../lib/keys";
import type { DirectoryWorkspace } from "../lib/people";
import { Heading, Stack, useApi, useChoices } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { DateControl } from "../components/date-control";
import { IconCell, LabIcon, ProviderIcon } from "../components/provider-icon";
import { CopyId } from "../components/templates/copy-id";
import { UpstreamModel } from "../components/templates/upstream-model";
import { InfoBanner } from "../components/templates/notices";
import { FilterToolbar, ToolbarField, type ToolbarChip } from "../components/templates/filter-toolbar";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { TypeTabs } from "../components/templates/type-tabs";
import type { Facet, FilterValues } from "../components/ui/filter-bar/filter-bar";
import { ColumnChooser, DensityToggle, ViewDataTable, chooserColumns, tableViewFromSearch, tableViewToSearch, type TableView } from "../components/templates/table-view";
import { StatusBadge } from "../components/ui/badge/badge";
import type { DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Input, NativeSelect } from "../components/ui/input/input";
import { BatchesPanel } from "./batches";
import s from "./shared.module.css";
import rq from "./requests.module.css";

export function requestsDescription(workspace: Workspace) {
  if (workspace.kind === "personal") return "Requests made with your keys. Only you can see them.";
  return workspace.capabilities.view_all_activity ? `Every request made with ${workspace.name}'s keys.` : `Requests made with your keys in ${workspace.name}. Workspace admins see everyone's.`;
}
const platformDescription = "Requests in every team and project. Personal workspaces never appear here.";
type View = Pick<DashboardSearch, "cols" | "density">;
/** Search values of the request page for one row, keeping the list's filters for previous/next. */
export const requestPageSearch = (ws: string, id: string, filters: RequestFilters, view?: View): DashboardSearch => ({ page: "request-detail", ws, record: id, ...filters, workspace_id: undefined, ...view });

/** The started time as the row's link: compact on one line, the full date and time (with zone) in the tooltip. */
function RowLink({ at, search, label }: { at: string; search: DashboardSearch; label: string }) {
  const when = compactDateTime(at);
  return <ResourceLink className={rq.rowLink} search={search} aria-label={`${label}, started ${when.full}`}><time dateTime={at} title={when.full}>{when.text}</time></ResourceLink>;
}
/** Known cost; an unknown cost shows what's on hold under it (a floor, not the price). */
function CostCell({ cost, held }: { cost: string | null; held: string | null }) {
  if (cost !== null) return <Money value={cost} />;
  const onHold = held && /^\d+$/.test(held) && BigInt(held) > 0n ? held : null;
  return <span className={rq.costCell} title={onHold ? `Unknown · ${formatMicroUsd(onHold)} on hold` : "Unknown"}><span>Unknown</span>{onHold && <span className={s.secondary}>{formatUsd(onHold)} on hold</span>}</span>;
}
const FinishBadge = ({ reason }: { reason: string | null | undefined }) => reason == null ? <span className={s.secondary}>None</span> : <StatusBadge tone={finishReasonTone(reason)} size="sm">{finishReasonLabel(reason)}</StatusBadge>;
/** Time to first token; a request that wasn't streamed has none ("—", the reason in the tooltip), not a repeated phrase in every row. */
const TtftCell = ({ ms, streamed }: { ms: number | null | undefined; streamed: boolean }) => ms == null && !streamed ? <span className={s.secondary} title="Not streamed">—</span> : <>{ttftText(ms, streamed)}</>;
const optionalCount = (v: string | null | undefined) => v == null ? "Unknown" : countText(v);
function KeyApp({ name, app }: { name: string; app?: string | null }) {
  return <span className={rq.costCell} style={{ alignItems: "flex-start" }}><span className={rq.truncateKey} title={name}>{name}</span>{app && <span className={`${s.secondary} ${rq.truncateKey}`} title={`App: ${app}`}>{app}</span>}</span>;
}
/** "1,234 in" over "567 out" (two short lines keep the default row inside 1440); other texts as one line. */
function TokensCell({ text }: { text: string }) {
  const parts = text.split(" · ");
  if (parts.length !== 2 || !parts[0]!.endsWith(" in")) return <>{text}</>;
  return <span className={rq.costCell} title={text}><span>{parts[0]}<span className="sr-only"> · </span></span><span className={s.secondary}>{parts[1]}</span></span>;
}
const WorkspaceCell = ({ w }: { w?: { name: string; kind: string } }) => w ? <span className={rq.truncateKey} title={`${w.name} · ${w.kind}`}>{w.name}</span> : <>Unknown</>;

// ----- Requests -----
const requestColumns = (scope: LogsScope, filters: RequestFilters, view: View): DataTableColumn<RequestRow>[] => [
  // The started time is the row's link; the whole row is clickable (a stretched link), and it's the row's one tab stop.
  { id: "started", header: "Started", rowHeader: true, hideable: false, width: "8rem", cell: r => <RowLink at={r.started_at} search={requestTarget(scope, r.root_request_id, filters, view)} label={`Open request ${r.root_request_id.slice(0, 8)}, ${r.model}`} /> },
  { id: "model", header: "Model", cell: r => <IconCell icon={<LabIcon model={r.model} />}><span className={rq.truncate} title={servedModel(r) ? `${r.model} → ${servedModelText(servedModel(r))}` : r.model}>{r.model}</span></IconCell> },
  ...(scope.kind === "platform" ? [{ id: "workspace", header: "Workspace", cell: (r: RequestRow) => <WorkspaceCell w={r.workspace} /> }] : []),
  { id: "key", header: "Key / app", cell: r => <KeyApp name={r.key.name} app={r.app} /> },
  { id: "tokens", header: "Tokens", numeric: true, cell: r => <TokensCell text={tokensText(r.input_tokens, r.output_tokens, r.workload_kind)} /> },
  { id: "cost", header: "Cost", numeric: true, cell: r => <CostCell cost={r.cost_microusd} held={r.held_microusd} /> },
  { id: "latency", header: "Latency", numeric: true, cell: r => latencyText(r.latency_ms) },
  { id: "ttft", header: "TTFT", label: "Time to first token", numeric: true, cell: r => <TtftCell ms={r.time_to_first_token_ms} streamed={r.streamed} /> },
  { id: "speed", header: "Speed", label: "Speed (tokens per second)", numeric: true, cell: r => r.tokens_per_second ? tpsText(r.tokens_per_second) : <span className={s.secondary}>—</span> },
  { id: "finish", header: "Finish", label: "Finish reason", cell: r => <FinishBadge reason={r.finish_reason} /> },
  // Fallbacks show under the status (the Attempts column is optional), so the default row fits at 1440.
  { id: "status", header: "Status", cell: r => <span className={rq.costCell} style={{ alignItems: "flex-start" }}><StatusBadge tone={requestStatusTone(r.status)} size="sm">{requestStatusLabel(r.status)}</StatusBadge>{r.attempts > 1 && <span className={s.secondary} title={`${r.attempts} attempts`}>{r.attempts - 1} fallback{r.attempts > 2 ? "s" : ""}</span>}</span> },
  { id: "attempts", header: "Attempts", numeric: true, defaultHidden: true, cell: r => r.attempts > 1 ? `${r.attempts} (${r.attempts - 1} fallback${r.attempts > 2 ? "s" : ""})` : countText(r.attempts) },
  { id: "cached", header: "Cached", label: "Cached input tokens", numeric: true, defaultHidden: true, cell: r => optionalCount(r.cached_input_tokens) },
  { id: "reasoning", header: "Reasoning", label: "Reasoning tokens", numeric: true, defaultHidden: true, cell: r => optionalCount(r.reasoning_tokens) },
  { id: "request", header: "Request ID", defaultHidden: true, cell: r => <CopyId value={r.root_request_id} label="request ID" /> },
  { id: "session", header: "Session", defaultHidden: true, cell: r => r.session_id ? <span className={rq.truncateKey} title={r.session_id}>{r.session_id}</span> : <span className={s.secondary}>None</span> },
  { id: "workload", header: "Type", defaultHidden: true, cell: r => workloadText(r.workload_kind) },
  { id: "job", header: "Job", label: "Async job state", defaultHidden: true, cell: r => r.job ? <StatusBadge tone={jobStateTone(r.job.state)} size="sm">{jobText(r.job)}</StatusBadge> : <span className={s.secondary}>—</span> },
  { id: "streamed", header: "Streamed", defaultHidden: true, cell: r => r.streamed ? "Yes" : "No" },
  { id: "cost_center", header: "Cost center", defaultHidden: true, cell: r => r.cost_center ? `${r.cost_center.name} · ${r.cost_center.code}` : "Unallocated" },
];
export const requestColumnIds = ["started", "model", "workspace", "key", "tokens", "cost", "latency", "ttft", "speed", "finish", "status", "attempts", "cached", "reasoning", "request", "session", "workload", "job", "streamed", "cost_center"];
/** Rows stay short (ui-principles 4): streaming telemetry and fallbacks are one click away under Columns. */
export const requestDefaultHidden = ["ttft", "speed", "attempts", "cached", "reasoning", "request", "session", "workload", "job", "streamed", "cost_center"];
/** Low-priority columns also hidden by default on a phone (≤600px), where rows stack. */
export const requestNarrowHidden = ["tokens", "attempts", "key", "request", "ttft", "speed", "finish"];

// ----- Generations (one row per upstream attempt) -----
const generationColumns = (scope: LogsScope, filters: RequestFilters, view: View): DataTableColumn<GenerationRow>[] => [
  { id: "started", header: "Started", rowHeader: true, hideable: false, width: "8.5rem", cell: g => <RowLink at={g.started_at} search={requestTarget(scope, g.root_request_id, filters, view)} label={`Open request ${g.root_request_id.slice(0, 8)} (attempt ${g.attempt_number}), ${g.model}`} /> },
  { id: "model", header: "Model", cell: g => <IconCell icon={<LabIcon model={g.model} />}><span className={rq.truncate} title={g.model}>{g.model}</span></IconCell> },
  { id: "provider", header: "Provider", cell: g => <IconCell icon={<ProviderIcon profile={g.connection.provider} size="sm" />}><span className={rq.truncateKey} title={`${g.connection.name} · ${servedModelText(servedModel(g))}`}>{g.connection.name}</span></IconCell> },
  ...(scope.kind === "platform" ? [{ id: "workspace", header: "Workspace", cell: (g: GenerationRow) => <WorkspaceCell w={g.workspace} /> }] : []),
  { id: "key", header: "Key / app", cell: g => <KeyApp name={g.key.name} app={g.app} /> },
  { id: "tokens", header: "Tokens", numeric: true, cell: g => tokensText(g.input_tokens, g.output_tokens, g.workload_kind) },
  { id: "cost", header: "Cost", numeric: true, cell: g => <CostCell cost={g.cost_microusd} held={g.held_microusd} /> },
  { id: "latency", header: "Latency", numeric: true, cell: g => latencyText(g.latency_ms) },
  { id: "ttft", header: "TTFT", label: "Time to first token", numeric: true, cell: g => <TtftCell ms={g.time_to_first_token_ms} streamed={g.streamed} /> },
  { id: "speed", header: "Speed", label: "Speed (tokens per second)", numeric: true, cell: g => g.tokens_per_second ? tpsText(g.tokens_per_second) : <span className={s.secondary}>—</span> },
  { id: "finish", header: "Finish", label: "Finish reason", cell: g => <FinishBadge reason={g.finish_reason} /> },
  { id: "status", header: "Status", cell: g => <StatusBadge tone={requestStatusTone(g.status)} size="sm">{requestStatusLabel(g.status)}</StatusBadge> },
  { id: "attempt", header: "Attempt", numeric: true, cell: g => g.attempt_number > 1 ? `${g.attempt_number} (fallback)` : "1" },
  { id: "error", header: "Error", defaultHidden: true, cell: g => g.error_code ? <code className={s.mono}>{g.error_code}</code> : <span className={s.secondary}>None</span> },
  // The model the provider reported serving, else the route's configured id (marked "configured").
  { id: "upstream", header: "Upstream model", defaultHidden: true, cell: g => <UpstreamModel row={g} compact /> },
  { id: "cached", header: "Cached", label: "Cached input tokens", numeric: true, defaultHidden: true, cell: g => optionalCount(g.cached_input_tokens) },
  { id: "reasoning", header: "Reasoning", label: "Reasoning tokens", numeric: true, defaultHidden: true, cell: g => optionalCount(g.reasoning_tokens) },
  { id: "generation_time", header: "Generation", label: "Generation time", numeric: true, defaultHidden: true, cell: g => latencyText(g.generation_ms) },
  { id: "session", header: "Session", defaultHidden: true, cell: g => g.session_id ? <span className={rq.truncateKey} title={g.session_id}>{g.session_id}</span> : <span className={s.secondary}>None</span> },
  { id: "attempt_id", header: "Attempt ID", defaultHidden: true, cell: g => <CopyId value={g.execution_id} label={`attempt ${g.attempt_number} ID`} /> },
];
const generationColumnIds = ["started", "model", "provider", "workspace", "key", "tokens", "cost", "latency", "ttft", "speed", "finish", "status", "attempt", "error", "upstream", "cached", "reasoning", "generation_time", "session", "attempt_id"];
const generationDefaultHidden = ["ttft", "speed", "error", "upstream", "cached", "reasoning", "generation_time", "session", "attempt_id"];
const generationNarrowHidden = ["provider", "key", "tokens", "ttft", "speed", "finish", "attempt"];

// ----- Sessions -----
const sessionColumns = (scope: LogsScope, filters: RequestFilters): DataTableColumn<SessionRow>[] => [
  { id: "last", header: "Last activity", rowHeader: true, hideable: false, width: "8.5rem", cell: r => <RowLink at={r.last_at} search={sessionTarget(scope, r.session_id, r.workspace.id, filters)} label={`Open session ${r.session_id}`} /> },
  { id: "session", header: "Session ID", cell: r => <code className={`${s.mono} ${rq.truncate}`} title={r.session_id}>{r.session_id}</code> },
  ...(scope.kind === "platform" ? [{ id: "workspace", header: "Workspace", cell: (r: SessionRow) => <WorkspaceCell w={r.workspace} /> }] : []),
  { id: "app", header: "App", cell: r => r.app ? <span className={rq.truncateKey} title={r.app}>{r.app}</span> : <span className={s.secondary}>Unknown</span> },
  { id: "model", header: "Primary model", cell: r => r.last_model ? <IconCell icon={<LabIcon model={r.last_model} />}><span className={rq.truncate} title={r.models.join(", ")}>{r.last_model}{r.model_count > 1 ? ` +${r.model_count - 1}` : ""}</span></IconCell> : "Unknown" },
  { id: "requests", header: "Requests", numeric: true, cell: r => countText(r.requests) },
  { id: "failed", header: "Failed", numeric: true, cell: r => countText(r.failed_requests) },
  { id: "tokens", header: "Tokens", numeric: true, cell: r => tokensText(r.input_tokens, r.output_tokens) },
  { id: "cost", header: "Cost", numeric: true, cell: r => r.cost_microusd !== null ? <Money value={r.cost_microusd} /> : <span className={rq.costCell} title={`Some requests' costs aren't known yet · at least ${formatMicroUsd(r.known_cost_microusd)}`}><span>At least {formatUsd(r.known_cost_microusd)}</span>{r.held_microusd !== "0" && <span className={s.secondary}>{formatUsd(r.held_microusd)} on hold</span>}</span> },
  { id: "first", header: "First seen", defaultHidden: true, cell: r => { const w = compactDateTime(r.first_at); return <time dateTime={r.first_at} title={w.full}>{w.text}</time>; } },
  { id: "keys", header: "Keys", numeric: true, defaultHidden: true, cell: r => countText(r.keys) },
];
const sessionColumnIds = ["last", "session", "workspace", "app", "model", "requests", "failed", "tokens", "cost", "first", "keys"];
const sessionDefaultHidden = ["first", "keys"];
const sessionNarrowHidden = ["app", "failed", "tokens"];

const tables = {
  requests: { ids: requestColumnIds, hidden: requestDefaultHidden, narrow: requestNarrowHidden },
  generations: { ids: generationColumnIds, hidden: generationDefaultHidden, narrow: generationNarrowHidden },
  sessions: { ids: sessionColumnIds, hidden: sessionDefaultHidden, narrow: sessionNarrowHidden },
  // Batches keep their own compact table (pages/batches.tsx).
  batches: { ids: [], hidden: [], narrow: [] },
} satisfies Record<LogTab, { ids: string[]; hidden: string[]; narrow: string[] }>;
/** Admin › Logs adds a Workspace column; it hides Finish by default (still under Columns) so the default row fits at 1440. */
export const platformRequestHidden = ["finish"];
const defaultHiddenFor = (tab: LogTab, narrow: boolean, platform = false) => {
  const base = platform && tab === "requests" ? [...tables[tab].hidden, ...platformRequestHidden] : tables[tab].hidden;
  return narrow ? [...new Set([...base, ...tables[tab].narrow])] : base;
};
/** The table view in the URL. Hidden-by-default columns shown again are written as `cols=none` (nothing hidden). */
export function logView(search: DashboardSearch, tab: LogTab, narrow = false, platform = false): TableView {
  return search.cols === "none" ? { hidden: [], density: search.density ?? "comfortable" } : tableViewFromSearch(search, tables[tab].ids, { hidden: defaultHiddenFor(tab, narrow, platform), density: "comfortable" });
}
export function logViewSearch(view: TableView, tab: LogTab, narrow = false, platform = false): View {
  const v = tableViewToSearch(view), same = [...view.hidden].sort().join(",") === [...defaultHiddenFor(tab, narrow, platform)].sort().join(",");
  return { cols: same ? undefined : v.cols ?? "none", density: v.density === "compact" ? "compact" : undefined };
}
export const requestView = (search: DashboardSearch, narrow = false) => logView(search, "requests", narrow);
export const requestViewSearch = (view: TableView, narrow = false) => logViewSearch(view, "requests", narrow);

/** Previous/Next over opaque forward-only cursors kept in the URL. */
function useCursorPaging(search: DashboardSearch, filterKey: string, go: (patch: Partial<DashboardSearch>) => void, nextCursor: string | null | undefined, rows: number, pageSize = 50) {
  const [previous, setPrevious] = useState<(string | undefined)[]>([]);
  useEffect(() => setPrevious([]), [filterKey]);
  const paged = !!search.cursor || !!nextCursor, page = previous.length + 1;
  if (!paged && !rows) return undefined;
  return {
    hasPrevious: !!search.cursor, hasNext: !!nextCursor,
    label: !paged || !rows ? undefined : search.cursor && previous.length === 0 ? "Later rows" : `Rows ${((page - 1) * pageSize + 1).toLocaleString("en-US")}–${((page - 1) * pageSize + rows).toLocaleString("en-US")}`,
    onPrevious: () => { const back = previous.at(-1); setPrevious(previous.slice(0, -1)); go({ cursor: back }); },
    onNext: () => { if (!nextCursor) return; setPrevious([...previous, search.cursor]); go({ cursor: nextCursor }); },
  };
}

type TableProps = { scope: LogsScope; search: DashboardSearch; filters: RequestFilters; query: URLSearchParams; enabled: boolean; narrow: boolean; go: (patch: Partial<DashboardSearch>) => void; filtered: boolean; tools: (columns: { id: string; label: string; hideable?: boolean }[], view: TableView) => void };
function emptyText(scope: LogsScope, what: string, filtered: boolean) {
  if (filtered) return { title: `No ${what} match these filters.`, description: "Change the period or clear filters to see more." };
  if (scope.kind === "platform") return { title: `No ${what} yet.`, description: `${what[0]!.toUpperCase()}${what.slice(1)} from team and project workspaces appear here.` };
  const own = scope.workspace.kind !== "personal" && !scope.workspace.capabilities.view_all_activity;
  return { title: `No ${what} yet.`, description: own ? `${what[0]!.toUpperCase()}${what.slice(1)} you make with your keys in this workspace appear here.` : `${what[0]!.toUpperCase()}${what.slice(1)} made with this workspace's keys appear here.` };
}

/** The Requests table (also used on a session's page, fixed to that session). */
export function RequestsTable({ scope, search, filters, query, enabled, narrow, go, filtered, tools, caption = "Requests" }: TableProps & { caption?: string }) {
  const view = logView(search, "requests", narrow, scope.kind === "platform"), viewSearch = { cols: search.cols, density: search.density }, filterKey = JSON.stringify(filters);
  const q = new URLSearchParams(query); q.set("limit", "50"); if (search.cursor) q.set("cursor", search.cursor);
  const page = useApi<RequestPage>(`${logPaths(scope).requests}?${q}`, enabled);
  const columns = useMemo(() => requestColumns(scope, filters, viewSearch), [scope.kind, filterKey, search.cols, search.density]); // eslint-disable-line react-hooks/exhaustive-deps
  const rows = page.data?.data ?? [], cursor = useCursorPaging(search, filterKey, go, page.data?.next_cursor, rows.length);
  useEffect(() => tools(chooserColumns(columns), view), [columns, JSON.stringify(view)]); // eslint-disable-line react-hooks/exhaustive-deps
  const empty = emptyText(scope, "requests", filtered);
  return <ViewDataTable<RequestRow> caption={caption} className={rq.requests} stack columns={columns} data={rows} getRowId={r => r.root_request_id} manual
    view={view} onViewChange={next => go(logViewSearch(next, "requests", narrow, scope.kind === "platform"))} hideViewControls
    loading={page.isFetching} error={page.error} onRetry={() => void page.refetch()}
    empty={<EmptyState size="compact" icon={<ListTree />} title={empty.title} description={empty.description} />} cursor={cursor} />;
}
function GenerationsTable({ scope, search, filters, query, enabled, narrow, go, filtered, tools }: TableProps) {
  const view = logView(search, "generations", narrow), viewSearch = { cols: search.cols, density: search.density }, filterKey = JSON.stringify(filters);
  const q = new URLSearchParams(query); q.set("limit", "50"); if (search.cursor) q.set("cursor", search.cursor);
  const page = useApi<GenerationPage>(`${logPaths(scope).generations}?${q}`, enabled);
  const columns = useMemo(() => generationColumns(scope, filters, viewSearch), [scope.kind, filterKey, search.cols, search.density]); // eslint-disable-line react-hooks/exhaustive-deps
  const rows = page.data?.data ?? [], cursor = useCursorPaging(search, filterKey, go, page.data?.next_cursor, rows.length);
  useEffect(() => tools(chooserColumns(columns), view), [columns, JSON.stringify(view)]); // eslint-disable-line react-hooks/exhaustive-deps
  const empty = emptyText(scope, "generations", filtered);
  return <ViewDataTable<GenerationRow> caption="Generations (upstream attempts)" className={rq.requests} stack columns={columns} data={rows} getRowId={g => g.execution_id} manual
    view={view} onViewChange={next => go(logViewSearch(next, "generations", narrow))} hideViewControls
    loading={page.isFetching} error={page.error} onRetry={() => void page.refetch()}
    empty={<EmptyState size="compact" icon={<ListTree />} title={empty.title} description={empty.description} />} cursor={cursor} />;
}
function SessionsTable({ scope, search, filters, query, enabled, narrow, go, filtered, tools }: TableProps) {
  const view = logView(search, "sessions", narrow), filterKey = JSON.stringify(filters);
  const q = new URLSearchParams(query); q.set("limit", "50"); if (search.cursor) q.set("cursor", search.cursor);
  const page = useApi<SessionPage>(`${logPaths(scope).sessions}?${q}`, enabled);
  const columns = useMemo(() => sessionColumns(scope, filters), [scope.kind, filterKey]); // eslint-disable-line react-hooks/exhaustive-deps
  const rows = page.data?.data ?? [], cursor = useCursorPaging(search, filterKey, go, page.data?.next_cursor, rows.length);
  useEffect(() => tools(chooserColumns(columns), view), [columns, JSON.stringify(view)]); // eslint-disable-line react-hooks/exhaustive-deps
  return <ViewDataTable<SessionRow> caption="Sessions" className={rq.requests} stack columns={columns} data={rows} getRowId={r => `${r.workspace.id}:${r.session_id}`} manual
    view={view} onViewChange={next => go(logViewSearch(next, "sessions", narrow))} hideViewControls
    loading={page.isFetching} error={page.error} onRetry={() => void page.refetch()}
    empty={<EmptyState size="compact" icon={<MessagesSquare />} title={filtered ? "No sessions match these filters." : "No sessions yet."} description={<>Requests are grouped into sessions when the client sends a session ID: an <code className={s.mono}>X-Session-Id</code> header, or OpenAI <code className={s.mono}>metadata.session_id</code> / <code className={s.mono}>user</code>, or Anthropic <code className={s.mono}>metadata.user_id</code>. An <code className={s.mono}>X-Title</code> header names the app.</>} />} cursor={cursor} />;
}

/** Summary tiles for the current filters (requests, error rate, latency, time to first token, speed). */
export function LogMetricsTiles({ scope, query, enabled }: { scope: LogsScope; query: URLSearchParams; enabled: boolean }) {
  const m = useApi<LogMetrics>(`${logPaths(scope).metrics}${query.size ? `?${query}` : ""}`, enabled), d = m.data;
  // Loading shows an ellipsis; a failed summary is unknown (never zero).
  const value = (v: ReactNode | null | undefined) => m.isError ? null : !enabled || m.isPending ? "…" : v;
  // Nothing to measure (no requests, or none streamed) is "—", not "Unknown": unknown means a value exists but wasn't reported.
  const noRequests = d?.requests === "0" ? "—" : null, noStreamed = d && (d.requests === "0" || d.ttft_requests === "0") ? "—" : null;
  return <StatTileGrid columns={5} label="Summary for these filters">
    <StatTile label="Requests" value={value(d ? countText(d.requests) : null)} hint={d ? `${countText(d.failed)} failed · ${countText(d.completed)} finished` : m.isError ? "Couldn't load the summary" : undefined} />
    <StatTile label="Error rate" value={value(d?.error_rate == null ? d?.completed === "0" ? "—" : null : rateText(d.error_rate))} hint={d?.error_rate == null ? "No finished requests" : "Failed ÷ finished"} />
    <StatTile label="Latency" value={value(d?.latency_p50_ms == null ? d?.completed === "0" ? "—" : null : latencyText(d.latency_p50_ms))} hint={d?.latency_p95_ms == null ? "Median" : `Median · p95 ${latencyText(d.latency_p95_ms)}`} />
    <StatTile label="Time to first token" value={value(d?.avg_time_to_first_token_ms == null ? noStreamed : latencyText(d.avg_time_to_first_token_ms))} hint={d ? d.ttft_requests === "0" ? "No streamed requests" : `Average · ${countText(d.ttft_requests)} streamed` : undefined} />
    <StatTile label="Speed" value={value(d?.tokens_per_second == null ? noRequests : tpsText(d.tokens_per_second))} hint={<span title="Output tokens per second after the first token">Output tokens/s</span>} />
  </StatTileGrid>;
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
/** Exact session ID filter: committed on Enter or when leaving the field. */
function SessionField({ value, onChange }: { value?: string; onChange: (next?: string) => void }) {
  const [text, setText] = useState(value ?? "");
  useEffect(() => setText(value ?? ""), [value]);
  const commit = () => { const next = text.trim(); if (next !== (value ?? "")) onChange(next || undefined); };
  return <ToolbarField label="Session" hint="Exact client session ID"><Input size="sm" value={text} placeholder="Session ID" aria-invalid={text.trim() !== "" && !validSessionId(text.trim()) ? true : undefined} onChange={e => setText(e.target.value)} onBlur={commit} onKeyDown={e => { if (e.key === "Enter") commit(); }} /></ToolbarField>;
}

/** The whole Logs page for a scope. */
export function LogsPage({ scope }: { scope: LogsScope }) {
  const nav = useDashboardNavigation(), narrow = useMediaQuery(NARROW_QUERY);
  const search: DashboardSearch = nav?.search ?? (scope.kind === "platform" ? { page: "platform-logs" } : { page: "requests", ws: scope.workspace.id });
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const tab = logTab(search.tab), platform = scope.kind === "platform";
  const filters = requestFilters(platform ? search : { ...search, workspace_id: undefined });
  const { query, error: filterError } = requestQuery(filters);
  // Filter options: models, the keys the caller can see (workspace only) and, on Admin, team and project workspaces.
  const models = useChoices<Grant>(scope.kind === "workspace" ? `${wsPath(scope.workspace.id)}/models` : "", scope.kind === "workspace");
  const platformModels = useChoices<Model>(`${platformPath}/models`, platform);
  const keys = useChoices<KeyRow>(scope.kind === "workspace" ? `${wsPath(scope.workspace.id)}/keys` : "", scope.kind === "workspace");
  const workspaces = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`, platform);
  const [tools, setTools] = useState<{ columns: { id: string; label: string; hideable?: boolean }[]; view: TableView }>();
  const modelOptions = [...new Map([...(models.data ?? []), ...(platformModels.data ?? [])].map(g => [g.public_name, { value: g.public_name, label: g.display_name === g.public_name ? g.public_name : `${g.display_name} · ${g.public_name}` }])).values()];
  if (filters.model && !modelOptions.some(o => o.value === filters.model)) modelOptions.unshift({ value: filters.model, label: filters.model });
  const keyOptions = (keys.data ?? []).map(k => ({ value: k.id, label: k.name, group: k.service_account_id ? "Service accounts" : "People" }));
  if (filters.key_id && !keyOptions.some(o => o.value === filters.key_id)) keyOptions.unshift({ value: filters.key_id, label: "Selected key", group: "People" });
  const workspaceOptions = (workspaces.data ?? []).filter(w => w.kind !== "personal").map(w => ({ value: w.id, label: w.name, group: w.kind === "team" ? "Teams" : "Projects" }));
  if (filters.workspace_id && !workspaceOptions.some(o => o.value === filters.workspace_id)) workspaceOptions.unshift({ value: filters.workspace_id, label: "Selected workspace", group: "Teams" });
  const filtered = activeFilterCount(filters) > 0;
  const facets: Facet[] = [
    { id: "model", label: "Model", type: "select", placeholder: "Any model", options: modelOptions },
    ...(platform ? [{ id: "workspace_id", label: "Workspace", type: "select", placeholder: "Any team or project", options: workspaceOptions } satisfies Facet] : [{ id: "key_id", label: "Key", type: "select", placeholder: "Any key", options: keyOptions } satisfies Facet]),
    // A compact multi-select (five statuses as a segmented control took a row of its own).
    { id: "status", label: "Status", type: "select", multiple: true, placeholder: "Any status", options: requestStatuses },
    { id: "finish_reason", label: "Finish reason", type: "select", multiple: true, placeholder: "Any", options: finishReasons },
    { id: "streamed", label: "Streamed", type: "toggle", allLabel: "Any", options: [{ value: "true", label: "Streamed" }, { value: "false", label: "Not streamed" }] },
    { id: "workload", label: "Jobs", type: "toggle", allLabel: "All requests", options: [{ value: "jobs", label: "Video and batch jobs" }] },
  ];
  const list = (v?: string) => v ? v.split(",") : [];
  const facetValues: FilterValues = { model: list(filters.model), key_id: list(filters.key_id), workspace_id: list(filters.workspace_id), status: list(filters.status), finish_reason: list(filters.finish_reason), streamed: list(filters.streamed), workload: list(filters.workload) };
  const onFacets = (next: FilterValues) => {
    const one = (v: unknown) => Array.isArray(v) && typeof v[0] === "string" ? v[0] : undefined, several = (v: unknown) => Array.isArray(v) && v.length ? (v as string[]).join(",") : undefined;
    go({ model: one(next.model), key_id: platform ? undefined : one(next.key_id), workspace_id: platform ? one(next.workspace_id) : undefined, status: several(next.status), finish_reason: several(next.finish_reason), streamed: one(next.streamed) as DashboardSearch["streamed"], workload: one(next.workload) === "jobs" ? "jobs" : undefined, cursor: undefined });
  };
  const chips: ToolbarChip[] = filters.session_id ? [{ key: "session", label: "Session", text: filters.session_id, onRemove: () => go({ session_id: undefined, cursor: undefined }) }] : [];
  const periodActive = filters.range && filters.range !== "30d" ? 1 : 0;
  const props: TableProps = { scope, search, filters, query, enabled: !filterError, narrow, go, filtered: filtered || !!filterError, tools: (columns, view) => setTools({ columns, view }) };
  const defaults = defaultHiddenFor(tab, narrow, platform);
  return <Stack gap={6} className={s.page}>
    <Heading title="Logs" description={scope.kind === "platform" ? platformDescription : requestsDescription(scope.workspace)} />
    {tab !== "batches" && <LogMetricsTiles scope={scope} query={query} enabled={!filterError} />}
    <TypeTabs label="Log view" value={tab} onChange={next => nav?.navigate(next === "batches" ? { ...(scope.kind === "platform" ? { page: "platform-logs" } : { page: "requests", ws: scope.workspace.id }), tab: "batches" } : logsSearch(scope, filters, next as LogTab))}
      items={[{ value: "requests", label: "Requests" }, { value: "generations", label: "Generations" }, { value: "sessions", label: "Sessions" }, { value: "batches", label: "Batches" }]}>
      {tab === "batches" ? <BatchesPanel scope={scope} /> : <div className={s.list}>
        {/* One row at 1440 (ui-principles 3): Search, Period, Model, Status, More filters (key/workspace, finish, streamed, session) … Columns, density. */}
        <FilterToolbar search={{ label: "Search by request ID (at least 4 characters)", placeholder: "Request ID", value: search.q ?? "", onChange: next => go({ q: next || undefined, cursor: undefined }) }}
          start={<PeriodControl filters={filters} onChange={patch => go({ ...patch, cursor: undefined })} />} extraActive={periodActive} extraChips={chips}
          moreStart={<SessionField value={filters.session_id} onChange={session_id => go({ session_id, cursor: undefined })} />} moreStartActive={filters.session_id ? 1 : 0}
          facets={facets} values={facetValues} onChange={onFacets} more={["key_id", "workspace_id", "finish_reason", "streamed"]}
          end={tools && <><ColumnChooser columns={tools.columns} hidden={tools.view.hidden} onHiddenChange={hidden => go(logViewSearch({ ...tools.view, hidden }, tab, narrow, platform))} defaultHidden={defaults} /><DensityToggle value={tools.view.density} onChange={density => go(logViewSearch({ ...tools.view, density }, tab, narrow))} /></>} />
        {filterError && <InfoBanner tone="warning" title="Filters not applied">{filterError}</InfoBanner>}
        {tab === "requests" ? <RequestsTable {...props} /> : tab === "generations" ? <GenerationsTable {...props} /> : <SessionsTable {...props} />}
      </div>}
    </TypeTabs>
    <p className={s.note}>Metadata only: prompts and responses are never stored. Costs are estimates; periods are UTC days.</p>
  </Stack>;
}
export const platformLogsScope: LogsScope = { kind: "platform" };
export const workspaceLogsScope = (workspace: Workspace): LogsScope => ({ kind: "workspace", workspace });

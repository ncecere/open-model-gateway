/*
 * One request on its own page (the replacement for OpenRouter's drawer):
 * "Back to Logs" and previous/next within the list's filters (j/k), tiles for
 * cost (or what's on hold), tokens, latency, time to first token / generation
 * time and attempts, copyable IDs, the finish reason, the client session (a
 * link to the session's page) and app, the data policy of the route that
 * served it (the current setting, not a record of the request), and a
 * timeline of every upstream attempt with its route, provider, status, finish
 * reason, error code, latency, time to first token, tokens, cost and the
 * failover reason. Metadata only; prompts and responses are never stored.
 *
 * Used in a workspace (Workspace › Logs) and on Admin (Usage & spend › Logs,
 * Team/Project requests only). Privacy is the server's: members open only
 * their own keys' requests, workspace admins everyone's, personal workspaces
 * only their owner; Admin never opens a personal workspace's request.
 */
import { DetailTime } from "../components/templates/when";
import { useEffect } from "react";
import { FileQuestion } from "lucide-react";
import { ApiError, platformPath, wsPath, type Session } from "../lib/api";
import { formatMicroUsd } from "../lib/governance";
import { costText, countText, dataPolicyOf, finishReasonLabel, latencyText, logPaths, logsSearch, requestFilters, requestQuery, requestStatusLabel, requestStatusTone, requestTarget, sessionTarget, timelineStatus, tokensText, tpsText, unresolvedText, workloadText, type LogsScope, type RequestAttempt, type RequestDetail, type RequestPage } from "../lib/requests";
import type { DashboardSearch } from "../lib/permissions";
import { Button, ErrorNotice, Stack, useApi } from "../components/ui";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { ProviderIcon, WithIcon } from "../components/provider-icon";
import { BackLink } from "../components/templates/form-page";
import { CopyId, shortId } from "../components/templates/copy-id";
import { DataPolicyBadge } from "../components/templates/data-policy-badge";
import { PrevNext } from "../components/templates/prev-next";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { Timeline, type TimelineItem } from "../components/templates/timeline";
import { StatusBadge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { PageHeader } from "../components/ui/page-header/page-header";
import { Time } from "../components/ui/time/time";
import type { Scope } from "./workspace";
import s from "./shared.module.css";

const stateLabel = (state: string) => state === "started" ? "In progress" : requestStatusLabel(state);
export function attemptItem(a: RequestAttempt, attempts: RequestAttempt[], workload?: string): TimelineItem {
  const previous = attempts.find(x => x.attempt_number === a.attempt_number - 1);
  const reason = unresolvedText(a.cost_microusd === null ? a.unresolved_reason : null);
  return {
    id: a.execution_id, status: timelineStatus(a.state), durationMs: a.latency_ms,
    title: <WithIcon icon={<ProviderIcon profile={a.connection.provider} size="sm" />}>{a.connection.name} · {a.deployment.upstream_model}</WithIcon>,
    marker: a.attempt_number > 1 ? "Fallback" : undefined, markerTone: "warning",
    meta: <>{stateLabel(a.state)}{a.finish_reason && a.finish_reason !== "error" ? <> · {finishReasonLabel(a.finish_reason)}</> : null}{a.error_code ? <> · <code className={s.mono}>{a.error_code}</code></> : null} · {tokensText(a.input_tokens, a.output_tokens, a.workload_kind ?? workload)} · {costText(a.cost_microusd, a.held_microusd)}</>,
    detail: <Stack gap={1}>
      {(a.time_to_first_token_ms != null || a.generation_ms != null) && <span>{a.time_to_first_token_ms != null ? `First token after ${latencyText(a.time_to_first_token_ms)} · ` : ""}{a.generation_ms != null ? `Generation ${latencyText(a.generation_ms)}` : ""}{attemptSpeed(a) ? ` · ${attemptSpeed(a)}` : ""}</span>}
      {(a.cached_input_tokens != null || a.reasoning_tokens != null) && <span className={s.muted}>{a.cached_input_tokens != null ? `${countText(a.cached_input_tokens)} cached input tokens` : ""}{a.cached_input_tokens != null && a.reasoning_tokens != null ? " · " : ""}{a.reasoning_tokens != null ? `${countText(a.reasoning_tokens)} reasoning tokens` : ""}</span>}
      {a.attempt_number > 1 && <span>Tried after attempt {a.attempt_number - 1}{previous ? ` (${previous.connection.name})` : ""} ended{a.failover_reason ? <> with <code className={s.mono}>{a.failover_reason}</code></> : ""}.</span>}
      {reason && <span className={s.muted}>Cost unknown: {reason}.</span>}
      <span className={s.muted}>Attempt ID <CopyId value={a.execution_id} label={`attempt ${a.attempt_number} ID`} /> · Started <DetailTime value={a.started_at} fallback={a.started_at} /></span>
    </Stack>,
  };
}

/** Output tokens per second after the first token (as the server computes it for successful generation). */
function attemptSpeed(a: RequestAttempt): string | undefined {
  if (a.state !== "succeeded" || a.generation_ms == null || a.output_tokens == null || !/^\d+$/.test(a.output_tokens) || (a.workload_kind && a.workload_kind !== "generation")) return;
  const ms = a.generation_ms - (a.time_to_first_token_ms ?? 0); if (ms <= 0) return;
  const hundredths = (BigInt(a.output_tokens) * 100_000n) / BigInt(ms);
  return tpsText(`${hundredths / 100n}.${String(hundredths % 100n).padStart(2, "0")}`);
}
const FULL_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/** A short id as shown in lists (8+ hex characters, optional dashes) that the list search can resolve. */
export const isShortRequestId = (id: string) => !FULL_ID.test(id) && /^[0-9a-f][0-9a-f-]{7,35}$/i.test(id);
/** The longest window the requests list allows (93 days), so a short id from an older list still resolves. */
export function shortIdLookup(id: string, now = Date.now()) {
  const day = 86_400_000, iso = (t: number) => new Date(t).toISOString().slice(0, 10);
  return new URLSearchParams({ q: id.toLowerCase(), start_date: iso(now - 92 * day), end_date: iso(now + day), limit: "2" });
}

/** Same not-found state as the key page: plain words, Back to Logs, no previous/next. */
function RequestNotFound({ scope, back }: { scope: LogsScope; back: { label: string; search: DashboardSearch } }) {
  const description = scope.kind === "platform" ? "Admin shows team and project requests only; personal workspaces never appear here." : scope.workspace.kind === "personal" ? "Only the owner sees requests in a personal workspace." : scope.workspace.capabilities.view_all_activity ? "Check the ID; it may belong to another workspace." : "Members see only requests made with their own keys.";
  return <Stack gap={6} className={s.page}>
    <PageHeader title="Request not found" breadcrumbs={<BackLink {...back} />} />
    <EmptyState icon={<FileQuestion />} titleAs="h2" title="This request doesn't exist or you can't see it" description={description}
      action={<Button variant="secondary" render={<ResourceLink search={back.search} />}>Back to Logs</Button>} />
  </Stack>;
}

/** `/requests/3cbfb500`: resolves a unique short id to the full one; otherwise the not-found state. */
export function RequestDetailPage({ workspace, id }: Scope & { id: string }) {
  return <RequestRoute scope={{ kind: "workspace", workspace }} id={id} />;
}
/** Admin › Logs › request (Team/Project requests only). */
export function PlatformRequestDetailPage({ id }: { session: Session; id: string }) {
  return <RequestRoute scope={{ kind: "platform" }} id={id} />;
}
const scopeFilters = (scope: LogsScope, search: DashboardSearch) => requestFilters(scope.kind === "platform" ? search : { ...search, workspace_id: undefined });
function RequestRoute({ scope, id }: { scope: LogsScope; id: string }) {
  const nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? requestTarget(scope, id, {});
  const short = isShortRequestId(id), full = FULL_ID.test(id);
  const lookup = useApi<RequestPage>(`${logPaths(scope).requests}?${shortIdLookup(id)}`, short);
  const match = lookup.data?.data.length === 1 ? lookup.data.data[0]!.root_request_id : undefined;
  useEffect(() => { if (match && nav) nav.navigate({ ...search, record: match }); }, [match]); // eslint-disable-line react-hooks/exhaustive-deps
  const back = { label: "Logs", search: logsSearch(scope, scopeFilters(scope, search), "requests", { cols: search.cols, density: search.density }) };
  if (full) return <RequestDetail scope={scope} id={id} />;
  if (short && (lookup.isPending || match)) return <p role="status">Finding request {id}…</p>;
  if (short && lookup.isError && !(lookup.error instanceof ApiError && [400, 403, 404].includes(lookup.error.status))) return <ErrorNotice error={lookup.error} retry={() => void lookup.refetch()} />;
  return <RequestNotFound scope={scope} back={back} />;
}

function RequestDetail({ scope, id }: { scope: LogsScope; id: string }) {
  const nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? requestTarget(scope, id, {});
  const filters = scopeFilters(scope, search), view = { cols: search.cols, density: search.density };
  // Neighbours follow the list's filters; filters the server would reject are left out rather than failing the page.
  const built = requestQuery(filters), query = built.error ? requestQuery({ ...filters, q: undefined, start_date: undefined, end_date: undefined, session_id: undefined }).query : built.query;
  const detailPath = scope.kind === "platform" ? `${platformPath}/logs/requests/${encodeURIComponent(id)}` : `${wsPath(scope.workspace.id)}/requests/${encodeURIComponent(id)}`;
  const q = useApi<RequestDetail>(`${detailPath}${query.size ? `?${query}` : ""}`);
  const title = `Request ${id.slice(0, 8)}`;
  useResourceName(title);
  const back = { label: "Logs", search: logsSearch(scope, filters, "requests", view) };
  const r = q.data, served = r?.attempts.at(-1);
  const target = (other: string | null) => other ? { label: shortId(other), render: <ResourceLink search={requestTarget(scope, other, filters, view)} /> } : null;
  const ttft = r?.time_to_first_token_ms ?? served?.time_to_first_token_ms ?? null, generation = r?.generation_ms ?? served?.generation_ms ?? null;
  const speed = r?.tokens_per_second ? tpsText(r.tokens_per_second) : served ? attemptSpeed(served) : undefined;
  const held = r && r.cost_microusd === null && r.held_microusd && r.held_microusd !== "0" ? r.held_microusd : null;
  // Not found and not allowed look the same (the server answers 404 or 403): say so plainly, with no previous/next.
  if (q.error instanceof ApiError && (q.error.status === 404 || q.error.status === 403 || q.error.status === 400)) return <RequestNotFound scope={scope} back={back} />;
  return <Stack gap={6} className={s.page}>
    <PageHeader title={title} meta={r && <StatusBadge tone={requestStatusTone(r.status)}>{requestStatusLabel(r.status)}</StatusBadge>} breadcrumbs={<BackLink {...back} />} description={r ? `${r.model} · ${r.key.name}` : undefined} actions={<PrevNext noun="request" shortcuts prev={target(r?.prev_id ?? null)} next={target(r?.next_id ?? null)} />} />
    {q.isPending ? <p role="status">Loading request…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <>
      <StatTileGrid columns={5} label="Request summary">
        <StatTile label={r!.cost_microusd === null ? "Cost (not final)" : "Cost"} value={r!.cost_microusd === null ? null : formatMicroUsd(r!.cost_microusd)} hint={held ? `${formatMicroUsd(held)} on hold until the cost is known` : r!.cost_microusd === null ? "Not known yet" : "Estimated from configured prices"} />
        <StatTile label="Tokens" value={tokensText(r!.input_tokens, r!.output_tokens, r!.workload_kind) === "Unknown" ? null : tokensText(r!.input_tokens, r!.output_tokens, r!.workload_kind)} hint={tokensText(r!.input_tokens, r!.output_tokens, r!.workload_kind) === "Not applicable" ? "Speech is metered by audio or characters, not tokens" : "Input · output"} />
        <StatTile label="Latency" value={r!.latency_ms === null ? null : latencyText(r!.latency_ms)} hint={r!.streamed ? "Streamed · until the stream ended" : "Until the response completed"} />
        {r!.streamed ? <StatTile label="Time to first token" value={ttft === null ? null : latencyText(ttft)} hint={[generation !== null ? `Generation ${latencyText(generation)}` : null, speed].filter(Boolean).join(" · ") || "Not reported"} />
          : <StatTile label="Generation time" value={generation === null ? null : latencyText(generation)} hint={speed ?? "Upstream time of the attempt that served it"} />}
        <StatTile label="Attempts" value={countText(r!.attempt_count)} hint={r!.attempt_count > 1 ? `${r!.attempt_count - 1} fallback${r!.attempt_count > 2 ? "s" : ""}` : "No fallback"} />
      </StatTileGrid>
      <Card title="Details" titleAs="h2"><DescriptionList dividers items={[
        { label: "Request ID", value: <CopyId value={r!.root_request_id} label="request ID" head={13} tail={12} /> },
        { label: "Model", value: r!.upstream_model && r!.upstream_model !== r!.model ? <>{r!.model} <span className={s.secondary}>→ {r!.upstream_model}</span></> : r!.model },
        ...(scope.kind === "platform" && r!.workspace ? [{ label: "Workspace", value: `${r!.workspace.name} · ${r!.workspace.kind === "project" ? "Project" : "Team"}` }] : []),
        { label: "Key", value: scope.kind === "workspace" ? <ResourceLink search={{ page: "key-detail", ws: scope.workspace.id, record: r!.key.id }}>{r!.key.name}</ResourceLink> : r!.key.name },
        { label: "Finish reason", value: r!.finish_reason ? finishReasonLabel(r!.finish_reason) : r!.status === "in_progress" ? "Not finished yet" : "None reported" },
        { label: "Session", value: r!.session_id ? <ResourceLink search={sessionTarget(scope, r!.session_id, r!.workspace?.id ?? r!.workspace_id, filters)}>{r!.session_id}</ResourceLink> : <span className={s.secondary}>None (the client sent no session ID)</span> },
        ...(r!.app ? [{ label: "App", value: r!.app }] : []),
        ...(r!.cached_input_tokens != null || r!.reasoning_tokens != null ? [{ label: "Token detail", value: [r!.cached_input_tokens != null ? `${countText(r!.cached_input_tokens)} cached input` : null, r!.reasoning_tokens != null ? `${countText(r!.reasoning_tokens)} reasoning` : null].filter(Boolean).join(" · ") }] : []),
        { label: "Started", value: <DetailTime value={r!.started_at} fallback={r!.started_at} /> },
        { label: "Completed", value: r!.completed_at ? <DetailTime value={r!.completed_at} fallback={r!.completed_at} /> : "Not yet" },
        { label: "Type", value: `${workloadText(r!.workload_kind)}${r!.streamed ? " · streamed" : ""}` },
        { label: "Data policy", value: <><DataPolicyBadge policy={dataPolicyOf(served?.data_policy)} /> <span className={s.secondary}>Current setting of the route that served it, not a record of this request.</span></> },
        ...(r!.cost_center ? [{ label: "Cost center", value: `${r!.cost_center.name} · ${r!.cost_center.code}` }] : []),
      ]} /></Card>
      <Card title="Attempts" titleAs="h2" description="Each upstream attempt in order. A fallback is tried only when an earlier attempt fails before anything was returned.">
        <Timeline label="Upstream attempts" items={r!.attempts.map(a => attemptItem(a, r!.attempts, r!.workload_kind))} showDurationBars showTotal empty="No attempts are visible for this request." />
      </Card>
      <p className={s.note}>Prompts and responses are never stored or shown here.</p>
    </>}
  </Stack>;
}

/*
 * One client session on its own page (Logs › Sessions › a row): totals,
 * the app and models it used, and its requests (newest first, linking to
 * each request's page). A session is the optional ID a client sends
 * (X-Session-Id, OpenAI metadata.session_id / user, Anthropic
 * metadata.user_id); it groups requests inside one workspace and is never an
 * identity or a permission. Same privacy as the requests themselves: members
 * see only their own requests in a session, Admin only team and project
 * workspaces.
 */
import { MessagesSquare } from "lucide-react";
import type { Session } from "../lib/api";
import { ApiError } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { countText, logPaths, logsSearch, requestFilters, requestQuery, tokensText, validSessionId, type LogsScope, type RequestFilters, type SessionRow } from "../lib/requests";
import { formatUsd } from "../lib/governance";
import { Money } from "../components/templates/money";
import { NARROW_QUERY, useMediaQuery } from "../lib/bitop-utils";
import { Button, ErrorNotice, Stack, useApi } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { DetailTime } from "../components/templates/when";
import { BackLink } from "../components/templates/form-page";
import { CopyId } from "../components/templates/copy-id";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { PageHeader } from "../components/ui/page-header/page-header";
import { RequestsTable } from "./logs";
import type { Scope } from "./workspace";
import s from "./shared.module.css";

export function SessionDetailPage({ workspace }: Scope) {
  return <SessionView scope={{ kind: "workspace", workspace }} />;
}
export function PlatformSessionDetailPage(_: { session: Session }) {
  return <SessionView scope={{ kind: "platform" }} />;
}
const durationText = (from: string, to: string) => {
  const ms = Date.parse(to) - Date.parse(from);
  if (!Number.isFinite(ms) || ms < 0) return "Unknown";
  const minutes = Math.round(ms / 60_000);
  return minutes < 1 ? "Under a minute" : minutes < 120 ? `${minutes} min` : `${Math.round(minutes / 60)} h`;
};

function SessionView({ scope }: { scope: LogsScope }) {
  const nav = useDashboardNavigation(), narrow = useMediaQuery(NARROW_QUERY);
  const search: DashboardSearch = nav?.search ?? (scope.kind === "platform" ? { page: "platform-log-session" } : { page: "session-detail", ws: scope.workspace.id });
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const id = search.session_id, ws = scope.kind === "platform" ? search.workspace_id : scope.workspace.id;
  // The session page keeps the period only; its requests are this session's.
  const all = requestFilters(scope.kind === "platform" ? search : { ...search, workspace_id: undefined });
  const period: RequestFilters = { range: all.range, start_date: all.start_date, end_date: all.end_date };
  const { query, error } = requestQuery(period);
  const valid = !!id && !!ws && validSessionId(id) && !error;
  const summary = useApi<SessionRow>(valid ? `${logPaths(scope).session(ws!, id!)}${query.size ? `?${query}` : ""}` : "", valid);
  useResourceName(id ? `Session ${id.length > 24 ? `${id.slice(0, 24)}…` : id}` : "Session");
  const back = { label: "Logs", search: logsSearch(scope, period, "sessions") };
  const missing = !valid || (summary.error instanceof ApiError && [400, 403, 404].includes(summary.error.status));
  if (missing) return <Stack gap={6} className={s.page}>
    <PageHeader title="Session not found" breadcrumbs={<BackLink {...back} />} />
    <EmptyState icon={<MessagesSquare />} titleAs="h2" title="This session doesn't exist in this period or you can't see it"
      description={scope.kind === "platform" ? "Admin shows team and project sessions only." : "Members see only sessions of requests made with their own keys. Try a longer period."}
      action={<Button variant="secondary" render={<ResourceLink search={back.search} />}>Back to Logs</Button>} />
  </Stack>;
  const r = summary.data;
  const sessionFilters: RequestFilters = { ...period, session_id: id, ...(scope.kind === "platform" ? { workspace_id: ws } : {}) };
  const tableQuery = requestQuery(sessionFilters).query;
  return <Stack gap={6} className={s.page}>
    {/* Title like "Request d166ee7c": the ID once, in the title (not again as the subtitle). */}
    <PageHeader title={`Session ${id!.length > 40 ? `${id!.slice(0, 40)}…` : id}`} breadcrumbs={<BackLink {...back} />} />
    {summary.isPending ? <p role="status">Loading session…</p> : summary.isError ? <ErrorNotice error={summary.error} retry={() => void summary.refetch()} /> : <>
      <StatTileGrid columns={4} label="Session summary">
        <StatTile label="Requests" value={countText(r!.requests)} hint={`${countText(r!.failed_requests)} failed · ${countText(r!.attempts)} attempts`} />
        <StatTile label={r!.cost_microusd === null ? "Cost (not final)" : "Cost"} value={r!.cost_microusd === null ? <Money value={r!.known_cost_microusd} prefix="At least " /> : <Money value={r!.cost_microusd} />} hint={r!.held_microusd !== "0" ? `${formatUsd(r!.held_microusd)} on hold` : "Estimated from configured prices"} />
        <StatTile label="Tokens" value={tokensText(r!.input_tokens, r!.output_tokens) === "Unknown" ? null : tokensText(r!.input_tokens, r!.output_tokens)} hint="Input · output" />
        <StatTile label="Duration" value={durationText(r!.first_at, r!.last_at)} hint="First to last request start" />
      </StatTileGrid>
      <Card title="Details" titleAs="h2"><DescriptionList dividers items={[
        { label: "Session ID", value: <CopyId value={r!.session_id} label="session ID" head={200} tail={0} /> },
        ...(scope.kind === "platform" ? [{ label: "Workspace", value: `${r!.workspace.name} · ${r!.workspace.kind === "project" ? "Project" : "Team"}` }] : []),
        { label: "App", value: r!.app ?? <span className={s.secondary} title="The client sent no X-Title header">Unknown</span> },
        { label: "Models", value: r!.models.join(", ") + (r!.model_count > r!.models.length ? ` and ${r!.model_count - r!.models.length} more` : "") },
        { label: "First request", value: <DetailTime value={r!.first_at} fallback={r!.first_at} /> },
        { label: "Last request", value: <DetailTime value={r!.last_at} fallback={r!.last_at} /> },
      ]} /></Card>
    </>}
    <Card title="Requests" titleAs="h2" description="Newest first.">
      <RequestsTable caption="Session requests" scope={scope} search={search} filters={sessionFilters} query={tableQuery} enabled={valid} narrow={narrow} go={go} filtered tools={() => {}} />
    </Card>
    <p className={s.note}>A session ID is a label the client sends; it is never used for access.</p>
  </Stack>;
}

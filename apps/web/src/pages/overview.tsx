/*
 * Workspace Overview: this month at a glance, access and a short list of
 * recent requests. The header holds the page's one primary action, Create key,
 * and Settings (Grounded team overview); the sidebar already lists every page,
 * so there is no grid of navigation cards.
 * Scope follows /me: workspace admins and the Personal owner see the whole
 * workspace; members see their own activity only (the server filters).
 *
 * Provenance: Grounded web/src/pages/team/overview/{page,stats,member,recent}.tsx
 * (read-only reference).
 */
import { ArrowRight, KeyRound, Settings, Activity } from "lucide-react";
import type { ReactNode } from "react";
import { wsPath, type Session, type Workspace } from "../lib/api";
import { formatMicroUsd, type CostReport } from "../lib/governance";
import { formatCount, reportQuery } from "../lib/reports";
import { latencyText, requestStatusLabel, requestStatusTone, tokensText, type RequestPage } from "../lib/requests";
import { canView, inWorkspacePortal, type Page } from "../lib/permissions";
import { authorityLabel } from "../lib/access";
import { kindLabels, platformRoleLabels } from "../lib/people";
import { useRef, useState } from "react";
import type { Grant, ServiceAccount } from "../lib/api";
import { keyModelOptions } from "../lib/key-models";
import { permissions } from "../lib/permissions";
import { CreateKeyDialog } from "./keys";
import { ErrorNotice, Heading, Stack, useApi, useChoices } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { RoleBadge } from "../components/people";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { AccessCard } from "../components/effective-access";
import { Badge, StatusBadge } from "../components/ui/badge/badge";
import { Button } from "../components/ui/button/button";
import { Card } from "../components/ui/card/card";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { PageHeader } from "../components/ui/page-header/page-header";
import { Time } from "../components/ui/time/time";
import type { Scope } from "./workspace";
import s from "./shared.module.css";
import h from "./home-portal.module.css";

/** The header line: who the workspace is shared with and whose activity this page shows. */
export function overviewDescription(workspace: Workspace) {
  if (workspace.kind === "personal") return "Only you can see this workspace's keys and requests.";
  return `${kindLabels[workspace.kind]} · showing ${workspace.capabilities.view_all_activity ? "everyone's" : "your"} activity`;
}
export function Overview({ session, workspace }: Scope) {
  const canDetails = inWorkspacePortal(session, workspace), p = permissions(session, workspace);
  const grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`, canDetails), accounts = useChoices<ServiceAccount>(`${wsPath(workspace.id)}/service-accounts`, canDetails && p.manageServiceAccounts);
  const options = keyModelOptions(grants.data ?? []), activeAccounts = accounts.data?.filter(a => !a.disabled_at) ?? [];
  const [creating, setCreating] = useState(false), trigger = useRef<HTMLButtonElement>(null);
  const canIssue = p.createUserKey || p.manageServiceAccounts && activeAccounts.length > 0;
  const ready = grants.isSuccess && options.length > 0 && (!p.manageServiceAccounts || accounts.isSuccess);
  const settings = canView("workspace-settings", session, workspace) && <Button variant="secondary" render={<ResourceLink search={{ page: "workspace-settings", ws: workspace.id }} />}><Settings aria-hidden />Settings</Button>;
  const create = canDetails && canIssue && <Button ref={trigger} disabled={!ready} title={grants.isSuccess && !options.length ? "Add a model first" : undefined} onClick={() => setCreating(true)}><KeyRound aria-hidden />Create key</Button>;
  const q = useApi<CostReport>(`${wsPath(workspace.id)}/cost-report?${reportQuery({ ws: workspace.id }, false)}`, canDetails);
  return <Stack gap={6} className={s.page}>
    <PageHeader title={workspace.name} description={overviewDescription(workspace)} meta={workspace.kind === "personal" ? <Badge variant="outline">Private</Badge> : <RoleBadge role={workspace.role} />} actions={(settings || create) && <>{settings}{create}</>} />
    <section aria-label="This month (UTC)">
      {q.isPending ? <p role="status">Loading this month's totals…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <StatTileGrid columns={3} label="This month (UTC)">
        <StatTile label="Spent this month" value={formatMicroUsd(q.data.totals.known_cost_microusd)} hint="Estimated from configured prices" render={<ResourceLink search={{ page: "costs", ws: workspace.id }} />} />
        <StatTile label="On hold" value={formatMicroUsd(q.data.totals.held_microusd)} hint={q.data.totals.unresolved_attempts !== "0" ? `${formatCount(q.data.totals.unresolved_attempts)} cost unknown` : undefined} />
        <StatTile label="Requests" value={formatCount(q.data.totals.root_requests)} hint={q.data.totals.attempts !== q.data.totals.root_requests ? `${formatCount(q.data.totals.attempts)} attempts including retries` : undefined} />
      </StatTileGrid>}
    </section>
    {canDetails && <AccessCard workspace={workspace} />}
    {canDetails && <RecentActivity workspace={workspace} />}
    {creating && <CreateKeyDialog session={session} workspace={workspace} options={options} accounts={activeAccounts} onClose={navigated => { setCreating(false); if (!navigated) requestAnimationFrame(() => trigger.current?.focus()); }} />}
  </Stack>;
}
/**
 * The five latest requests the caller may see (the server scopes members to their own keys): one row per request
 * (not per upstream attempt), each opening its request page.
 */
function RecentActivity({ workspace }: { workspace: Workspace }) {
  const q = useApi<RequestPage>(`${wsPath(workspace.id)}/requests?limit=5`);
  const body: ReactNode = q.isPending ? <p role="status" className={h.activityRow}>Loading recent requests…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : q.data.data.length === 0 ? <EmptyState size="compact" icon={<Activity />} title="No requests yet." description={workspace.capabilities.view_all_activity && workspace.kind !== "personal" ? "Requests made with this workspace's keys appear here." : "Requests made with your keys appear here."} /> :
    <ul className={h.activity} aria-label="Recent requests">{q.data.data.map(r => <li key={r.root_request_id} className={h.activityRow}><span className={h.activityText}><ResourceLink className={s.primary} search={{ page: "request-detail", ws: workspace.id, record: r.root_request_id }}>{r.model}</ResourceLink><StatusBadge tone={requestStatusTone(r.status)}>{requestStatusLabel(r.status)}</StatusBadge><span className={s.muted}>{tokensText(r.input_tokens, r.output_tokens, r.workload_kind)} · {latencyText(r.latency_ms)}</span></span><span className={h.activityWhen}><Time value={r.started_at} format="relative" /></span></li>)}</ul>;
  return <Card title="Recent activity" description={workspace.capabilities.view_all_activity ? "The latest requests in this workspace." : "Your latest requests in this workspace."} actions={<Button variant="ghost" size="sm" render={<ResourceLink search={{ page: "requests", ws: workspace.id }} />}>All requests <ArrowRight aria-hidden /></Button>} flush>{body}</Card>;
}
export function Profile({ session }: { session: Session }) {
  const mine = session.workspaces.filter(w => inWorkspacePortal(session, w));
  // Same cards and description lists as the rest of the portal (no bare bullets or explanatory paragraph).
  const name = session.user.display_name?.trim();
  return <Stack gap={6} className={s.page}><Heading title="Your profile" description="Your platform role and the workspaces you belong to." />
    <Card title="Identity" titleAs="h2"><DescriptionList dividers items={[...(name ? [{ label: "Name", value: name }] : []), { label: "Email", value: session.user.email }, { label: "Platform role", value: <span title="Admin and Auditor include User access; workspace roles are separate.">{platformRoleLabels[session.user.platform_role] ?? "No platform role"}</span> }, { label: "Installation", value: session.installation.name }]} /></Card>
    <Card title="Your workspaces" titleAs="h2"><DescriptionList dividers items={mine.map(w => ({ key: w.id, label: <ResourceLink search={{ page: "overview", ws: w.id }}>{w.name}</ResourceLink>, value: authorityLabel(w) }))} /></Card>
  </Stack>;
}

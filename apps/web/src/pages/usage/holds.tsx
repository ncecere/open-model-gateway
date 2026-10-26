/*
 * Usage & costs › Holds (`?tab=holds`): the aged unknown-hold review queue (#101), read-only.
 * An aged unknown hold is money a gateway keeps set aside for an attempt whose cost has been unknown for 24 hours
 * or more. The amount is a floor ("at least"), never an upper bound, and nothing here refunds or releases it: a
 * Platform Admin settles a hold only through the existing evidence-backed reconcile dialog (Cost records' flow).
 *
 * The gateway authorizes every row: a workspace admin sees the whole workspace, a member only their own keys', a
 * personal owner their own; Admin sees Team/Project rows plus one identity-free personal aggregate (no drill-down).
 * Rows page by keyset cursor ("Load more"); rows leave the queue as holds resolve, so there are no page numbers.
 */
import { useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import { api, platformPath, wsPath, type Session, type Workspace } from "../../lib/api";
import type { BillingUsage } from "../../lib/governance";
import { ageText, canReconcileHold, floorAmount, floorText, holdRequestLink, holdsPath, holdsQuery, holdsScopeText, holdsUnavailable, mergeHoldPages, reasonText, type HoldRow, type HoldsPage, type HoldTotals } from "../../lib/holds";
import { formatCount } from "../../lib/reports";
import type { RequestDetail } from "../../lib/requests";
import { workloadLabels } from "../../lib/usage";
import { Button, Stack, useAction, useApiScope } from "../../components/ui";
import { InfoBanner } from "../../components/templates/notices";
import { CopyId, shortId } from "../../components/templates/copy-id";
import { StatTile, StatTileGrid } from "../../components/templates/stat-tile";
import { TableTime } from "../../components/templates/when";
import { ResourceLink } from "../../components/navigation-link";
import { Badge } from "../../components/ui/badge/badge";
import { DataTable, type DataTableColumn } from "../../components/ui/data-table/data-table";
import { EmptyState } from "../../components/ui/empty-state/empty-state";
import { toast } from "../../components/ui/toast/toast";
import { reconcileAction } from "./records";
import s from "../shared.module.css";
import u from "./usage.module.css";

const enc = encodeURIComponent;
const kindLabel = (kind: string) => kind === "project" ? "Project" : kind === "team" ? "Team" : "Personal";

/** The most useful numbers first: how many, the known floor (not an upper bound), how many have no limit, how old. */
function Totals({ totals }: { totals: HoldTotals }) {
  return <StatTileGrid label="Aged unknown hold totals" columns={4}>
    <StatTile label="Aged unknown holds" value={formatCount(totals.count)} />
    <StatTile label="Known floor (at least)" value={floorAmount(totals.known_floor_microusd)} hint="Adds only holds with a known floor. Not an upper bound, and never added to spend." />
    <StatTile label="Unbounded" value={formatCount(totals.unbounded_count)} hint="Holds with no known upper limit." />
    <StatTile label="Oldest" value={ageText(totals.oldest_age_hours)} />
  </StatTileGrid>;
}

/** Every personal workspace combined. Identity-free by design: no workspace, owner, key or request, and nothing to open. */
function PersonalAggregate({ totals }: { totals: HoldTotals }) {
  const none = totals.count === "0";
  return <div role="group" aria-label="Personal workspaces, combined" className={u.personal}>
    <h2 className={u.personalTitle}>Personal workspaces, combined</h2>
    {none ? <p className={u.personalFacts}>None</p> : <p className={u.personalFacts}>
      <span>{formatCount(totals.count)} {totals.count === "1" ? "hold" : "holds"}</span>
      <span>{floorText(totals.known_floor_microusd)}</span>
      <span>{formatCount(totals.unbounded_count)} unbounded</span>
      <span>oldest {ageText(totals.oldest_age_hours)}</span>
    </p>}
    <p className={u.note}>One total for every personal workspace; no workspace, owner or request is shown. A small count can describe a single person.</p>
  </div>;
}

export function HoldsTab({ session, workspace }: { session: Session; workspace?: Workspace }) {
  const path = holdsPath(workspace?.id), scope = useApiScope(), ask = useAction(), [busy, setBusy] = useState<string>();
  const q = useInfiniteQuery({
    queryKey: ["api", scope, path, "holds"], retry: false, initialPageParam: null as string | null,
    queryFn: ({ pageParam, signal }) => api<HoldsPage>(`${path}?${holdsQuery(pageParam)}`, { signal }),
    getNextPageParam: page => page.has_more && page.next_cursor ? page.next_cursor : undefined,
  });
  const view = mergeHoldPages(q.data?.pages ?? []);
  if (holdsUnavailable(q.error)) return <EmptyState size="compact" title="Holds review isn't available" description="This gateway is an older version without the aged unknown-hold queue. Upgrade the gateway to review holds here." />;

  // The existing reconcile dialog needs the pinned original observations, which the queue row doesn't carry: read them
  // from the request's own attempt (the same record page), then open Cost records' dialog on the existing endpoint.
  const reconcile = async (row: HoldRow, opener: HTMLElement) => {
    setBusy(row.execution_id);
    try {
      const detail = await api<RequestDetail>(workspace ? `${wsPath(workspace.id)}/requests/${enc(row.root_request_id)}` : `${platformPath}/logs/requests/${enc(row.root_request_id)}`);
      const attempt = detail.attempts.find(a => a.execution_id === row.execution_id);
      if (!attempt) { toast.error("Can't reconcile this hold", "Its attempt record couldn't be found. Reload and try again."); return; }
      ask({ ...reconcileAction(wsPath(row.workspace.id), { id: attempt.execution_id, input_tokens: attempt.input_tokens, output_tokens: attempt.output_tokens, billing_usage: attempt.billing_usage as BillingUsage | null }), returnFocus: opener });
    } catch (error) { toast.error("Can't open reconciliation", error instanceof Error ? error.message : "Try again."); } finally { setBusy(undefined); }
  };
  const columns: DataTableColumn<HoldRow>[] = [
    { id: "age", header: "Unknown for", rowHeader: true, hideable: false, cell: r => <>{ageText(r.age_hours)}<span className={s.secondary}>since <TableTime value={r.unknown_since} fallback={r.unknown_since} /></span></> },
    { id: "floor", header: "Floor", numeric: true, cell: r => <span className={u.holdCell}>{floorText(r.held_microusd)}{r.unbounded_cost && <Badge tone="warning" className={u.holdBadge} title="No upper limit is known for this hold">Unbounded</Badge>}</span> },
    { id: "reason", header: "Reason", cell: r => <span className={u.holdCell}>{reasonText(r.unresolved_reason)}</span> },
    { id: "model", header: "Model", cell: r => <>{r.public_model}<span className={s.secondary}>{r.provider} · {workloadLabels[r.workload_kind as keyof typeof workloadLabels] ?? r.workload_kind}</span></> },
    ...(workspace ? [] : [{ id: "workspace", header: "Workspace", cell: (r: HoldRow) => <>{r.workspace.name}<span className={s.secondary}>{kindLabel(r.workspace.kind)}</span></> }]),
    { id: "request", header: "Request", cell: r => { const link = holdRequestLink(session, r, workspace); return link ? <ResourceLink search={link} aria-label={`Request ${shortId(r.root_request_id)}`}>{shortId(r.root_request_id)}</ResourceLink> : <CopyId value={r.root_request_id} label="request ID" />; } },
  ];
  const showReconcile = view.rows.some(r => canReconcileHold(session, r));
  const settled = !q.isPending && !q.error;
  return <Stack gap={4}>
    <InfoBanner title="How to read holds">
      An aged unknown hold is money kept set aside for an attempt whose cost has been unknown for 24 hours or more. The amount is a floor (&ldquo;at least&rdquo;), not an upper bound, and an unbounded hold has no known limit. Nothing is refunded or released here and these amounts are never added to spend: a hold settles only when a Platform Admin reconciles it with evidence. {holdsScopeText(workspace)}. Holds aren&apos;t filtered by period.
    </InfoBanner>
    {settled && view.rows.length > 0 && <Totals totals={view.totals} />}
    {settled && !workspace && view.personal && <PersonalAggregate totals={view.personal} />}
    {settled && view.rows.length === 0 ? <EmptyState size="compact" title="No aged unknown holds" description="Nothing in this view has had an unknown cost for 24 hours or more." />
      : <DataTable<HoldRow> caption="Aged unknown holds" stack columns={columns} data={view.rows} getRowId={r => r.execution_id} loading={q.isPending} error={q.error ?? undefined} onRetry={() => void q.refetch()}
        rowActions={showReconcile ? (r: HoldRow) => canReconcileHold(session, r) ? <Button size="sm" variant="secondary" disabled={busy === r.execution_id} aria-label={`Reconcile request ${r.root_request_id.slice(0, 8)}`} onClick={e => void reconcile(r, e.currentTarget)}>Reconcile</Button> : null : undefined}
        loadMore={{ hasMore: !!q.hasNextPage, loading: q.isFetchingNextPage, onLoadMore: () => void q.fetchNextPage() }} />}
    {settled && view.rows.length > 0 && <p className={u.note}>Showing {formatCount(String(view.rows.length))} of {formatCount(view.totals.count)}. Oldest first.</p>}
  </Stack>;
}

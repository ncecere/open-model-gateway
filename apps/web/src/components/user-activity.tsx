/*
 * Admin user › Activity: the changes a person made (platform audit filtered by
 * actor, sign-ins hidden by default) and their own-key usage over the last 30
 * days. Provenance: Grounded web/src/pages/admin/people/user-activity.tsx
 * (read-only reference): When / Action (label + code) / Target, a "Hide
 * sign-ins" switch and Columns.
 *
 * Privacy follows the platform audit and cost report, enforced by the gateway:
 * the audit never returns personal-workspace events (anyone's), and usage is
 * aggregate only. A workspace is named only when it is a Team or Project in
 * the shared directory; personal usage is one "Personal workspace (private)"
 * total, never a name, key or request. Nothing here fetches keys or requests.
 */
import { useMemo, useState, type ReactNode } from "react";
import { Activity as ActivityIcon, FileClock } from "lucide-react";
import { platformPath, type Audit, type Catalog, type CostCenter, type GroupMapping, type Model, type PlatformUser, type Provider, type Workspace } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import type { CostReport, Totals } from "../lib/governance";
import { formatMicroUsd } from "../lib/governance";
import { formatCount } from "../lib/reports";
import { auditEventLabel, kindLabels, last30Days, resourceTypeLabel, splitUsage, type DirectoryWorkspace } from "../lib/people";
import { DateTime, ErrorNotice, Stack, StatCard, useApi, useChoices } from "./ui";
import { DirectoryTable, PersonCell, copyIdAction, signInFacet } from "./people";
import { ResourceLink } from "./navigation-link";
import { Badge } from "./ui/badge/badge";
import { Card } from "./ui/card/card";
import { CellText, type DataTableColumn } from "./ui/data-table/data-table";
import { EmptyState } from "./ui/empty-state/empty-state";
import { Table, Td, Tr } from "./ui/table/table";
import s from "../pages/shared.module.css";
import p from "./people.module.css";

type Target = { name: string; search?: DashboardSearch };
const named = <T extends { id: string },>(rows: T[] | undefined, name: (row: T) => string, search?: (row: T) => DashboardSearch) => new Map<string, Target>((rows ?? []).map(r => [r.id, { name: name(r), search: search?.(r) }]));
const enc = encodeURIComponent;
const link = (t: Target) => t.search ? <ResourceLink search={t.search}>{t.name}</ResourceLink> : t.name;

/** Names for audit targets from platform lists the Admin/Auditor can already read (shared with the audit log's cache). */
function useAuditTargets() {
  const users = useChoices<PlatformUser>(`${platformPath}/users`), shared = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`);
  const centers = useChoices<CostCenter>(`${platformPath}/cost-centers`), mappings = useChoices<GroupMapping>(`${platformPath}/oidc/group-mappings`);
  const models = useChoices<Model>(`${platformPath}/models`), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`), providers = useChoices<Provider>(`${platformPath}/providers`);
  const spaces = named<Workspace>(shared.data?.filter(w => w.kind !== "personal"), w => w.name, w => ({ page: "workspace-detail", record: w.id, kind: w.kind as "team" | "project" }));
  const lookup: Record<string, Map<string, Target>> = {
    user: named(users.data, u => u.email ?? "Retained identity", u => ({ page: "user-detail", record: u.id })), workspace: spaces,
    cost_center: named(centers.data, c => c.name, () => ({ page: "cost-centers" })), group_mapping: named(mappings.data, m => m.group_value, () => ({ page: "oidc" })),
    model: named(models.data, m => m.display_name || m.public_name, m => ({ page: "model-detail", record: m.id })), catalog: named(catalogs.data, c => c.name, c => ({ page: "catalog-detail", record: c.id })), provider: named(providers.data, v => v.name, v => ({ page: "provider-detail", record: v.id })),
  };
  const target = (e: Audit): Target | undefined => (e.resource_type && e.resource_id ? lookup[e.resource_type]?.get(e.resource_id) : undefined) ?? (e.target_name ? { name: e.target_name } : undefined);
  return { target, spaces };
}

/** "Recent actions": platform-audit events this person performed. */
export function UserActions({ user, name }: { user: Pick<PlatformUser, "id">; name: string }) {
  const [hide, setHide] = useState(true), { target, spaces } = useAuditTargets();
  const columns: DataTableColumn<Audit>[] = [
    { id: "when", header: "When", rowHeader: true, hideable: false, cell: e => <DateTime value={e.created_at} /> },
    { id: "action", header: "Action", cell: e => <CellText primary={auditEventLabel(e)} secondary={<span className={s.mono}>{e.action}</span>} /> },
    { id: "target", header: "Target", cell: e => { const t = target(e); return <CellText primary={t ? link(t) : <Badge size="sm" variant="outline">{resourceTypeLabel(e.resource_type)}</Badge>} secondary={t ? resourceTypeLabel(e.resource_type) : undefined} />; } },
    { id: "workspace", header: "Team or project", cell: e => { const w = e.workspace_id ? spaces.get(e.workspace_id) : undefined; return w ? link(w) : <span className={s.muted}>—</span>; } },
  ];

  return <Card title="Recent actions" description={`Changes ${name} made, newest first. Personal-workspace events are never listed.`}>
    <DirectoryTable<Audit> path={`${platformPath}/audit?actor_user_id=${enc(user.id)}`} label={`Actions by ${name}`} storageKey="user-activity" rowKey={e => e.id} rowLabel={e => `${auditEventLabel(e)} event`} columns={columns}
      facets={[signInFacet]} facetState={{ values: { hide_sign_ins: hide ? ["true"] : [] }, onChange: next => setHide(Array.isArray(next.hide_sign_ins) && next.hide_sign_ins[0] === "true") }}
      rowActions={e => [copyIdAction(e.resource_id, "Copy target ID"), copyIdAction(e.id, "Copy event ID")]}
      empty={{ icon: <FileClock />, title: hide ? "No changes recorded." : "No activity recorded.", description: hide ? "Sign-ins are hidden." : undefined }} />
  </Card>;
}

const money = (v: string) => formatMicroUsd(v);
function UsageRow({ name, kind, totals }: { name: ReactNode; kind?: ReactNode; totals: Totals }) {
  return <Tr><Td>{name}</Td><Td>{kind ?? <span className={s.muted}>—</span>}</Td><Td numeric>{formatCount(totals.attempts)}</Td><Td numeric>{money(totals.known_cost_microusd)}</Td><Td numeric>{money(totals.held_microusd)}</Td></Tr>;
}
/**
 * "Usage (last 30 days)": the platform cost report filtered to this person's own
 * keys. Team/Project rows are named; everything else is one private personal total.
 */
export function UserUsage({ user }: { user: Pick<PlatformUser, "id"> }) {
  const range = useMemo(() => last30Days(), []);
  const report = useApi<CostReport>(`${platformPath}/cost-report?start_date=${range.start_date}&end_date=${range.end_date}&actor_user_id=${enc(user.id)}`);
  const shared = useChoices<DirectoryWorkspace>(`${platformPath}/workspaces`);
  const description = "Requests with this person's own API keys, in UTC. Estimates from configured rates, not invoices.";
  if (report.isPending) return <Card title="Usage (last 30 days)" description={description}><p role="status">Loading usage…</p></Card>;
  if (report.isError) return <Card title="Usage (last 30 days)" description={description}><ErrorNotice error={report.error} retry={() => void report.refetch()} /></Card>;
  const r = report.data, t = r.totals, none = t.attempts === "0";
  const names = new Map((shared.data ?? []).map(w => [w.id, { name: w.name, kind: w.kind }]));
  const split = shared.isSuccess && !r.breakdowns_truncated ? splitUsage(r.breakdowns.workspaces, names) : undefined;
  return <Card title="Usage (last 30 days)" description={description}>
    <Stack gap={4}>
      <div className={p.usageTotals}>
        <StatCard label="Upstream attempts" value={formatCount(t.attempts)} hint={t.unresolved_attempts !== "0" ? `${formatCount(t.unresolved_attempts)} with unresolved cost` : undefined} />
        <StatCard label="Known estimated cost" value={money(t.known_cost_microusd)} hint={t.unresolved_attempts !== "0" ? "A lower bound while costs are unresolved" : undefined} />
        <StatCard label="Held reservations" value={money(t.held_microusd)} />
      </div>
      {none ? <EmptyState size="compact" icon={<ActivityIcon />} title="No usage in the last 30 days." /> : !split ? <p className={s.muted}>{shared.isPending ? "Loading the per-workspace breakdown…" : "Per-workspace breakdown unavailable; totals only."}</p> :
        <Table caption="Usage by workspace" columns={["Workspace", "Kind", { label: "Attempts", numeric: true }, { label: "Known cost", numeric: true }, { label: "Held", numeric: true }]}>
          {split.shared.map(w => <UsageRow key={w.id} name={<PersonCell name={w.name} shape="square"><CellText primary={<ResourceLink search={{ page: "workspace-detail", record: w.id, kind: w.kind as "team" | "project" }}>{w.name}</ResourceLink>} /></PersonCell>} kind={<Badge>{kindLabels[w.kind]}</Badge>} totals={w.totals} />)}
          {split.personal && <UsageRow name={<CellText primary="Personal workspace (private)" secondary="Totals only" />} totals={split.personal} />}
        </Table>}
    </Stack>
  </Card>;
}

export function UserActivity({ user, name }: { user: Pick<PlatformUser, "id">; name: string }) {
  return <Stack gap={6}><UserActions user={user} name={name} /><UserUsage user={user} /></Stack>;
}

/*
 * Key safety (docs/key-safety.md).
 * - Admin › Records › Key safety: High / Medium / Low tiles (keys counted by
 *   their worst finding) plus a personal-keys tile (counts only), then one
 *   toolbar row and compact rows: "API key in <workspace>", finding badges
 *   (max three), last used, expiry and one fix. Admin views never show shared
 *   key names or holders; a fix opens the key's own page when you administer
 *   that workspace, otherwise the team/project page.
 * - Workspace pieces: KeyRiskBadge (API keys list) and KeyFindings (key page
 *   Overview). Fixes reuse the existing key actions: rotate with a new expiry,
 *   Limits / Access tabs, disable and revoke (confirmed).
 */
import type { ReactNode } from "react";
import { ShieldCheck } from "lucide-react";
import { platformPath, wsPath, type Session, type Workspace } from "../lib/api";
import { canDisable, canRevoke, canRotate, canEditKeyLimits, type KeyRow } from "../lib/keys";
import { adminKeyLabel, filterSafetyRows, findingFix, findingHint, findingLabels, fixHint, fixLabels, fixTab, holderLabel, keySafetyPath, rowFixes, severities, severityLabels, severityTone, type Finding, type Fix, type SafetyReport, type SafetyRow, type SafetyThresholds, type Severity } from "../lib/key-safety";
import { inWorkspacePortal, type DashboardSearch } from "../lib/permissions";
import { Button, ErrorNotice, Heading, Stack, useAction, useApi, type Action } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { HintBadge } from "../components/templates/hint-badge";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { FilterToolbar } from "../components/templates/filter-toolbar";
import { Card } from "../components/ui/card/card";
import { CellText, DataTable, type DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Time } from "../components/ui/time/time";
import { keyActions } from "./keys";
import s from "./shared.module.css";
import k from "./key-safety.module.css";

/** Workspace findings for the keys the viewer can list (one key on its page). */
export const useKeySafety = (workspace: Workspace, keyId?: string, enabled = true) => useApi<SafetyReport>(keySafetyPath(wsPath(workspace.id), { key_id: keyId }), enabled);

export function FindingBadge({ finding, thresholds }: { finding: Finding; thresholds?: SafetyThresholds }) {
  return <HintBadge tone={severityTone[finding.severity]} hint={findingHint(finding, thresholds)}>{findingLabels[finding.code]}</HintBadge>;
}
/** At most three badges, the rest as "+n" with their names as the tooltip. */
export function FindingBadges({ row, thresholds, max = 3 }: { row: SafetyRow; thresholds?: SafetyThresholds; max?: number }) {
  const rest = row.findings.slice(max);
  return <span className={s.badges}>{row.findings.slice(0, max).map(f => <FindingBadge key={f.code} finding={f} thresholds={thresholds} />)}{rest.length > 0 && <HintBadge dot={false} variant="outline" hint={rest.map(f => findingLabels[f.code]).join(" · ")}>+{rest.length}</HintBadge>}</span>;
}
/** The API keys list's small badge: worst severity, findings as its tooltip. */
export function KeyRiskBadge({ row }: { row?: SafetyRow }) {
  if (!row) return null;
  return <HintBadge tone={severityTone[row.severity]} hint={row.findings.map(f => findingLabels[f.code]).join(" · ")}>{severityLabels[row.severity]} risk</HintBadge>;
}

/** Whether a fix is available to this viewer on this key (same rules as the key page). */
function fixAllowed(fix: Fix, session: Session, workspace: Workspace, key: KeyRow): boolean {
  switch (fix) {
    case "set_expiry": case "rotate": return canRotate(session, workspace, key);
    case "add_budget": return canEditKeyLimits(session, workspace, key);
    case "restrict_models": return true;
    case "disable": return canDisable(session, workspace, key);
    case "revoke": return canRevoke(session, workspace, key);
  }
}
function fixAction(fix: Fix, workspace: Workspace, key: KeyRow): Action | undefined {
  if (fix === "set_expiry" || fix === "rotate") return keyActions.rotate(workspace.id, key);
  if (fix === "disable") return keyActions.disable(workspace.id, key);
  if (fix === "revoke") return keyActions.revoke(workspace.id, key);
}
/** One fix button: a confirmation dialog (rotate, disable, revoke) or the key page tab (limits, access). */
export function FixButton({ fix, session, workspace, keyRow, size = "sm" }: { fix: Fix; session: Session; workspace: Workspace; keyRow: KeyRow; size?: "sm" | "md" }) {
  const ask = useAction(), action = fixAction(fix, workspace, keyRow), tab = fixTab[fix];
  if (!fixAllowed(fix, session, workspace, keyRow)) return null;
  const variant = fix === "revoke" ? "danger" : "secondary";
  if (tab) return <Button size={size} variant={variant} title={fixHint[fix]} render={<ResourceLink search={{ page: "key-detail", ws: workspace.id, record: keyRow.id, tab }} />}>{fixLabels[fix]}</Button>;
  return action ? <Button size={size} variant={variant} title={fixHint[fix]} onClick={() => ask(action)}>{fixLabels[fix]}</Button> : null;
}

/** Key page Overview: this key's findings, each with its fix. Nothing when the key is clean. */
export function KeyFindings({ session, workspace, keyRow }: { session: Session; workspace: Workspace; keyRow: KeyRow }) {
  const q = useKeySafety(workspace, keyRow.id), row = q.data?.data.find(r => r.key.id === keyRow.id);
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (!row) return null;
  return <Card title="Safety" titleAs="h2">
    <ul className={k.findings} aria-label="Findings">{row.findings.map(f => <li key={f.code} className={k.finding}>
      <FindingBadge finding={f} thresholds={q.data?.thresholds} />
      <span className={s.muted}>{findingHint(f, q.data?.thresholds)}</span>
      <span className={k.fix}><FixButton fix={findingFix[f.code]} session={session} workspace={workspace} keyRow={keyRow} /></span>
    </li>)}</ul>
  </Card>;
}

/** Admin fix target: the key page for a workspace you administer, else the team/project page. */
function adminFix(session: Session, row: SafetyRow): { label: string; title?: string; search: DashboardSearch } {
  const ws = session.workspaces.find(w => w.id === row.key.workspace.id), fix = rowFixes(row)[0]!;
  const manages = !!ws && inWorkspacePortal(session, ws) && (ws.capabilities.view_all_activity || row.key.holder === "service_account" && ws.capabilities.manage_service_accounts);
  if (manages) return { label: fixLabels[fix], title: fixHint[fix], search: { page: "key-detail", ws: ws!.id, record: row.key.id, tab: fixTab[fix] } };
  return { label: `Open ${row.key.workspace.kind === "project" ? "project" : "team"}`, title: "Only its workspace admins can change this key.", search: { page: "workspace-detail", record: row.key.workspace.id, kind: row.key.workspace.kind === "project" ? "project" : "team" } };
}
const severityFacet = { id: "severity", label: "Severity", type: "toggle" as const, multiple: true, allLabel: "All", options: severities.map(v => ({ value: v, label: severityLabels[v] })) };
const count = (n: number | undefined) => n === undefined ? null : n.toLocaleString("en-US");

export function KeySafetyPage({ session }: { session: Session }) {
  const nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? { page: "key-safety" };
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const report = useApi<SafetyReport>(keySafetyPath(platformPath), session.capabilities.platform_read);
  const data = report.data, rows = filterSafetyRows(data?.data ?? [], search.q, search.severity);
  const pick = (severity: Severity) => go({ severity: search.severity === severity ? undefined : severity });
  const columns: DataTableColumn<SafetyRow>[] = [
    { id: "key", header: "Key", rowHeader: true, hideable: false, cell: r => <CellText primary={adminKeyLabel(r)} secondary={holderLabel[r.key.holder]} /> },
    { id: "findings", header: "Findings", cell: r => <FindingBadges row={r} thresholds={data?.thresholds} /> },
    { id: "last_used", header: "Last used", cell: r => r.key.last_used_at ? <Time value={r.key.last_used_at} format="relative" /> : <span className={s.muted}>Never</span> },
    { id: "expires", header: "Expires", defaultHiddenNarrow: true, cell: r => r.key.expires_at ? <Time value={r.key.expires_at} format="date" /> : <span className={s.muted}>Never</span> },
    { id: "action", header: <span className="sr-only">Fix</span>, label: "Fix", hideable: false, cell: r => { const fix = adminFix(session, r); return <Button size="sm" variant="secondary" title={fix.title} render={<ResourceLink search={fix.search} />}>{fix.label}</Button>; } },
  ];
  let body: ReactNode;
  if (report.isError) body = <ErrorNotice error={report.error} retry={() => void report.refetch()} />;
  else body = <DataTable<SafetyRow> caption="Keys that need attention" columns={columns} data={rows} getRowId={r => r.key.id} rowLabel={adminKeyLabel} manual loading={report.isPending}
    empty={<EmptyState size="compact" icon={<ShieldCheck />} title={data?.data.length ? "No keys match" : "No keys need attention"} action={data?.data.length ? <Button size="sm" variant="ghost" onClick={() => go({ q: undefined, severity: undefined })}>Clear filters</Button> : undefined} />} />;
  return <Stack gap={6} className={s.page}>
    <Heading title="Key safety" description="Team and project keys that need attention." />
    <StatTileGrid columns={4} label="Keys by worst finding">
      {(["high", "medium", "low"] as const).map(v => <StatTile key={v} label={severityLabels[v]} value={report.isPending ? "…" : count(data?.summary[v])} hint={search.severity === v ? "Filtered" : undefined} onClick={() => pick(v)} />)}
      <StatTile label="Personal keys" value={report.isPending ? "…" : count(data?.personal?.flagged)} hint={data?.personal ? `of ${count(data.personal.keys)} need attention · counts only` : undefined} />
    </StatTileGrid>
    <div className={s.list}>
      <FilterToolbar search={{ label: "Search keys", placeholder: "Workspace or finding", value: search.q ?? "", onChange: q => go({ q: q || undefined }), debounceMs: 250 }}
        facets={[severityFacet]} values={{ severity: search.severity?.split(",") ?? [] }} onChange={v => go({ severity: Array.isArray(v.severity) && v.severity.length ? v.severity.join(",") : undefined })}
        note={data?.truncated ? "Showing the 500 most severe keys." : undefined} />
      {body}
      {data && rows.length > 0 && <p className={s.note} role="status">{rows.length} of {data.summary.flagged} key{data.summary.flagged === 1 ? "" : "s"} · unused after {data.thresholds.unused_days} days, rotate after {data.thresholds.rotation_days}</p>}
    </div>
  </Stack>;
}

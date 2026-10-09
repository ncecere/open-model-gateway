/*
 * Usage & costs › Storage: GB-days kept in the gateway's file store for the
 * period (1 GB = 2^30 bytes for 24 hours), from hourly usage records. Storage
 * is **not charged**: the cost reads "Not charged" (a deliberate state), never
 * "Unknown" and never an estimated $0.
 *
 * Workspace: needs workspace-wide visibility (admins, a personal owner); by
 * purpose plus stored now against the quota. Admin: one row per workspace;
 * personal workspaces show totals only.
 */
import { platformPath, wsPath, type Workspace } from "../../lib/api";
import { storageText } from "../../lib/limits";
import { kindLabels } from "../../lib/people";
import type { UsagePeriod } from "../../lib/usage";
import { Badge, ErrorNotice, useApi } from "../../components/ui";
import { StorageBar, bytesText } from "../../components/templates/storage-bar";
import { Card } from "../../components/ui/card/card";
import { Table, Td, Th, Tr } from "../../components/ui/table/table";
import s from "../shared.module.css";

type Amount = { byte_seconds: string; gb_days: string };
type PurposeAmount = Amount & { purpose: string };
export type StorageUsage = Amount & { cost_state: "not_charged"; total: Amount; by_purpose: PurposeAmount[]; recorded_through: string;
  current?: { used_bytes: number; quota_bytes: number | null };
  workspaces?: (Amount & { workspace_id: string; name: string; kind: "personal" | "team" | "project"; current_bytes: number; quota_bytes: number | null; by_purpose: PurposeAmount[] | null })[] };

const gbDays = (value: string) => `${value} GB-day${value === "1" ? "" : "s"}`;
const STORAGE_PURPOSES: Record<string, string> = { batch_input: "Batch input", batch_output: "Batch output", user_file: "User files", export: "Exports", video_output: "Video outputs" };
export const purposeName = (p: string) => STORAGE_PURPOSES[p] ?? p;

export function StorageUsageCard({ workspace, period, workspaceFilter }: { workspace?: Workspace; period: UsagePeriod; workspaceFilter?: string }) {
  const visible = !workspace || workspace.capabilities.view_all_activity;
  const base = workspace ? wsPath(workspace.id) : platformPath;
  const q = useApi<StorageUsage>(`${base}/usage/storage?start_date=${period.start_date}&end_date=${period.end_date}`, visible);
  if (!visible) return null;
  const notCharged = <Badge>Not charged</Badge>;
  if (q.isPending) return <Card title="Storage" actions={notCharged}><p role="status" className={s.note}>Loading storage…</p></Card>;
  if (q.isError) return <Card title="Storage"><ErrorNotice error={q.error} retry={() => void q.refetch()} /></Card>;
  const d = q.data;
  // An older gateway (no storage usage): show nothing rather than guess.
  if (!d?.total || !Array.isArray(d.by_purpose)) return null;
  const rows = (d.workspaces ?? []).filter(w => !workspaceFilter || w.workspace_id === workspaceFilter);
  const total = workspaceFilter ? rows[0] ?? { gb_days: "0", byte_seconds: "0" } : d.total;
  return <Card title="Storage" description={`${gbDays(total.gb_days)} this period · cost: not charged`} actions={notCharged}>
    {workspace && d.current && <StorageBar used={d.current.used_bytes} quota={d.current.quota_bytes} size="sm" />}
    {workspace ? d.by_purpose.length > 0 && <Table caption="Storage by purpose" stack density="compact" columns={["Purpose", { label: "Usage", numeric: true, width: "10rem" }, { label: "Cost", width: "8rem" }]}>
      {d.by_purpose.map(p => <Tr key={p.purpose}><Th scope="row">{purposeName(p.purpose)}</Th><Td numeric>{gbDays(p.gb_days)}</Td><Td><span className={s.muted}>Not charged</span></Td></Tr>)}
    </Table>
      : rows.length > 0 ? <Table caption="Storage by workspace" stack density="compact" columns={["Workspace", { label: "Usage", numeric: true, width: "10rem" }, { label: "Stored now", numeric: true, width: "12rem" }, { label: "Cost", width: "8rem" }]}>
        {rows.map(w => <Tr key={w.workspace_id}><Th scope="row"><span className={s.primary}>{w.name}</span><span className={s.secondary}>{kindLabels[w.kind]}</span></Th><Td numeric>{gbDays(w.gb_days)}</Td><Td numeric>{bytesText(w.current_bytes)} / {storageText(w.quota_bytes)}</Td><Td><span className={s.muted}>Not charged</span></Td></Tr>)}
      </Table> : <p className={s.note}>No files stored in this period.</p>}
  </Card>;
}

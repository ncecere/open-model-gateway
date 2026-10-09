/*
 * Settings › Limits, read-only: the limits that apply to this workspace, with
 * stacked budgets (each with its reset rule) and budget meters where the
 * caller may see usage. Effective access (layers and per-model reasons) is
 * its own Settings tab, "Access" (WorkspaceAccess), not stacked under this.
 * Personal workspaces show this (their limits are set by the platform;
 * ux-api-contract §1), and so do shared-workspace members (no edit
 * affordances, P2-13). Team/Project admins get the editable tighten-only view
 * (pages/governance.tsx `Governance`).
 *
 * Per-key caps live on each key's page. Grounded reference:
 * web/src/pages/team/usage.tsx (read-only meters, "Platform admins set the limits").
 */
import { wsPath, type Workspace } from "../lib/api";
import type { PolicyResponse } from "../lib/governance";
import { formatMicroUsd } from "../lib/governance";
import { limitsOf, limitsSummary, periodName, rateRows, rateText, resetText } from "../lib/limits";
import { ErrorNotice, Stack, useApi } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { BudgetMeters } from "../components/scope-limits";
import { Card } from "../components/ui/card/card";
import { Table, Td, Tr } from "../components/ui/table/table";
import s from "./shared.module.css";

export function EffectiveLimits({ workspace }: { workspace: Workspace }) {
  const q = useApi<PolicyResponse>(`${wsPath(workspace.id)}/policy`), personal = workspace.kind === "personal";
  if (q.isPending) return <p role="status">Loading limits…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const effective = limitsOf(q.data.effective), local = q.data.provenance ? limitsOf(q.data.provenance.local) : undefined;
  const earlier = personal && !!local && limitsSummary(local, "") !== "";
  return <Stack gap={6}>
    <Card title="Limits" description={personal ? "Set by a Platform Admin." : "Set by a Platform Admin and this workspace's admins."} flush>
      <Table caption={`Limits for ${workspace.name}`} stack columns={["Limit", { label: "Applies", width: "16rem" }]}>
        {rateRows.map(r => <Tr key={r.key}><Td><span className={s.primary}>{r.label}</span></Td><Td>{rateText(effective[r.key], r.unit === "per min" ? "per minute" : "at once")}</Td></Tr>)}
        {effective.budgets.length === 0 ? <Tr><Td><span className={s.primary}>Budget</span></Td><Td>No budget</Td></Tr> : effective.budgets.map(b => <Tr key={b.period}><Td><span className={s.primary}>{periodName[b.period]} budget</span><span className={s.secondary}>{resetText(b.period)}</span></Td><Td>{formatMicroUsd(b.amount_microusd)}</Td></Tr>)}
      </Table>
    </Card>
    <BudgetMeters windows={q.data.budgets ?? []} kind={workspace.kind} mode={q.data.mode} />
    {earlier && <p className={s.note}>Your earlier cap still applies: {limitsSummary(local)}.</p>}
    {workspace.capabilities.issue_own_key && <p className={s.note}>Cap a single key from <ResourceLink search={{ page: "keys", ws: workspace.id }}>API keys</ResourceLink>.</p>}
  </Stack>;
}

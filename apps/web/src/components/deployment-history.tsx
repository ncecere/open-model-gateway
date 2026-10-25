import { useState, type ReactNode } from "react";
import { platformPath, type DeploymentHistoryPage } from "../lib/api";
import { auditActionLabels } from "../lib/people";
import { Button, DateTime, ErrorNotice, Inline, Stack, useApi } from "./ui";
import { Tab, Tabs, TabsList, TabsPanel } from "./ui/tabs/tabs";
import { DescriptionList } from "./ui/description-list/description-list";

/** History is fetched only when its panel is opened; authorization stays in Rust. */
export function DeploymentConfigurationTabs({ id, tab, children }: { id: string; tab?: string; children: ReactNode }) {
  return <Tabs defaultValue={tab === "history" ? "history" : "configuration"}>
    <TabsList variant="pills" aria-label="Route configuration"><Tab value="configuration">Configuration</Tab><Tab value="history">History</Tab></TabsList>
    <TabsPanel value="configuration">{children}</TabsPanel>
    <TabsPanel value="history"><DeploymentHistory key={id} id={id} /></TabsPanel>
  </Tabs>;
}

/** Cursor positions, not offsets or current-row reconstruction. Each page reauthorizes. */
export function DeploymentHistory({ id }: { id: string }) {
  const [cursors, setCursors] = useState<(string | null)[]>([null]);
  const cursor = cursors[cursors.length - 1];
  const path = `${platformPath}/deployments/${encodeURIComponent(id)}/configuration/history?limit=25${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`;
  const q = useApi<DeploymentHistoryPage>(path);
  const page = !q.isError ? q.data : undefined;
  return <Stack gap={4}>
    <p>Deployment enablement changes. Older events have values unavailable; they are never reconstructed from current settings. This history does not include prices or routing policy changes.</p>
    {q.isPending && <p role="status">Loading configuration history…</p>}
    {q.isError && <ErrorNotice error={q.error} retry={() => void q.refetch()} />}
    {page && <>
      {page.data.length === 0 && <p>No configuration history recorded.</p>}
      <ol aria-label="Configuration changes">{page.data.map(change => <li key={`${change.created_at}:${change.id}`}>
        <Stack gap={2}>
          <p><strong>{auditActionLabels[change.action] ?? change.action}</strong> · <DateTime value={change.created_at} /></p>
          <p>{change.values_available && change.after ? `${change.before ? change.before.enabled ? "Enabled" : "Disabled" : "Not configured"} → ${change.after.enabled ? "Enabled" : "Disabled"}` : "Values unavailable"}</p>
          <p>{change.actor.label}{change.actor.admin_token_id && " · Admin API token"}</p>
          {change.revision !== null && <p>Revision {change.revision}</p>}
          <details><summary>Change details</summary><DescriptionList items={[
            { label: "Change / audit ID", value: <code>{change.id}</code> },
            { label: "Deployment ID", value: <code>{change.target.id}</code> },
            { label: "Actor ID", value: change.actor.id ? <code>{change.actor.id}</code> : "Unavailable or system" },
            ...(change.actor.admin_token_id ? [{ label: "Admin API token ID", value: <code>{change.actor.admin_token_id}</code> }] : []),
            { label: "Snapshot schema", value: change.schema_version === null ? "Values unavailable" : String(change.schema_version) },
          ]} /></details>
        </Stack>
      </li>)}</ol>
    </>}
    <Inline gap={2}>
      <Button variant="secondary" disabled={cursors.length === 1 || q.isFetching} onClick={() => setCursors(previous => previous.slice(0, -1))}>Newer changes</Button>
      <Button variant="secondary" disabled={!page?.next_cursor || q.isFetching} onClick={() => { if (page?.next_cursor) setCursors(previous => [...previous, page.next_cursor]); }}>Older changes</Button>
    </Inline>
  </Stack>;
}

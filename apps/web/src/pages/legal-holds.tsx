/*
 * Admin › Records › Legal holds (#75). While a hold is active, the gateway
 * removes none of that person's or workspace's data: departed-user cleanup,
 * request-log compaction, file expiry and archiving wait. A hold grants no
 * extra access. Holds are placed from a user's or a team/project's page
 * ("Place legal hold…") with a short reference code, never free text, and
 * released here.
 */
import { useState } from "react";
import { Scale } from "lucide-react";
import { api, API, type Session } from "../lib/api";
import { Button, DateTime, ErrorNotice, Heading, Stack, useAction, useApi } from "../components/ui";
import { Badge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Table, Td, Tr } from "../components/ui/table/table";
import { ResourceLink } from "../components/navigation-link";
import type { Field } from "../lib/forms";
import s from "./shared.module.css";

export type LegalHold = { id: string; target: { kind: "user" | "workspace"; id: string; label: string | null }; reference: string; created_at: string; created_by: string | null; released_at: string | null };
export const holdsPath = `${API}/platform/legal-holds`;
export const referenceField: Field = { name: "reference", label: "Reference", required: true, maxLength: 64, help: "A short code such as a case or ticket number. No names or descriptions.", validate: v => /^[A-Za-z0-9][A-Za-z0-9._:/#-]{0,63}$/.test(v) ? undefined : "Letters, digits and . _ : / # - only, starting with a letter or digit." };
/** "Place legal hold…" for a user or a team/project. */
export function placeHold(ask: ReturnType<typeof useAction>, target: { user_id: string } | { workspace_id: string }, name: string) {
  ask({ title: `Place a legal hold on ${name}?`, description: "While it's active, none of this data is removed: cleanup, log compaction, file expiry and archiving wait. It gives no extra access.", fields: [referenceField], submitLabel: "Place hold", successNotice: "Legal hold placed.", run: (v, signal) => api(holdsPath, { method: "POST", body: { ...target, reference: v.reference }, signal }) });
}

export function LegalHolds({ session }: { session: Session }) {
  const [all, setAll] = useState(false), ask = useAction(), writable = session.capabilities.platform_write;
  const q = useApi<{ data: LegalHold[] }>(all ? `${holdsPath}?state=all` : holdsPath);
  const target = (h: LegalHold) => <ResourceLink search={h.target.kind === "user" ? { page: "user-detail", record: h.target.id } : { page: "workspace-detail", record: h.target.id }}>{h.target.label ?? "Retained identity"}</ResourceLink>;
  const release = (h: LegalHold) => ask({ title: `Release hold ${h.reference}?`, description: "Normal cleanup, compaction, expiry and archiving resume for this scope (unless another hold covers it).", danger: true, submitLabel: "Release hold", successNotice: "Legal hold released.", run: (_, signal) => api(`${holdsPath}/${encodeURIComponent(h.id)}/release`, { method: "POST", signal }) });
  const body = q.isPending ? <p role="status">Loading holds…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : q.data.data.length === 0 ? <EmptyState size="compact" icon={<Scale />} title={all ? "No legal holds." : "No active legal holds."} description="Place one from a user's or a team's or project's page." /> :
    <Table caption="Legal holds" columns={["Reference", "Applies to", "Placed", "Status", ""]}>{q.data.data.map(h => <Tr key={h.id}>
      <Td><code>{h.reference}</code></Td>
      <Td>{target(h)} <Badge size="sm" variant="outline">{h.target.kind === "user" ? "User" : "Workspace"}</Badge></Td>
      <Td><DateTime value={h.created_at} />{h.created_by && <span className={s.muted}> by {h.created_by}</span>}</Td>
      <Td>{h.released_at ? <>Released <DateTime value={h.released_at} /></> : <Badge tone="warning">Active</Badge>}</Td>
      <Td>{writable && !h.released_at && <Button size="sm" variant="ghost" aria-label={`Release ${h.reference}`} onClick={() => release(h)}>Release</Button>}</Td>
    </Tr>)}</Table>;
  return <Stack gap={6} className={s.page}>
    <Heading title="Legal holds" description="Data the gateway keeps while a matter is open. A hold gives no extra access." />
    <Card title={all ? "All holds" : "Active holds"} titleAs="h2" actions={<Button size="sm" variant="secondary" onClick={() => setAll(!all)}>{all ? "Active only" : "Include released"}</Button>} flush={!!q.data?.data.length}>{body}</Card>
  </Stack>;
}

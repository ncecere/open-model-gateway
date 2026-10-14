/*
 * "Your sessions" on Your profile (#79): where you're signed in to this
 * dashboard, with a way to sign one out, or every other one at once.
 * The server returns only a device label (browser and OS family) and times,
 * never an address. Signing a session out ends dashboard access only; API
 * keys are separate. The session in use is marked and not offered here; use
 * the regular Sign out.
 */
import { LogOut, MonitorSmartphone } from "lucide-react";
import { API, api } from "../lib/api";
import { Button, ErrorNotice, useAction, useApi } from "./ui";
import { Badge } from "./ui/badge/badge";
import { Card } from "./ui/card/card";
import { EmptyState } from "./ui/empty-state/empty-state";
import { Table, Td, Tr } from "./ui/table/table";
import { Time } from "./ui/time/time";

export type DashboardSession = { id: string; client: string | null; created_at: string; last_seen_at: string | null; expires_at: string; current: boolean };

const path = `${API}/me/sessions`;
const device = (s: DashboardSession) => s.client ?? "Unknown browser";
/** Two sessions can share a label; the sign-in time tells their buttons and dialogs apart. */
const named = (s: DashboardSession) => `${device(s)}, signed in ${new Date(s.created_at).toLocaleString("en-US", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" })} UTC`;

export function YourSessions() {
  const q = useApi<{ data: DashboardSession[] }>(path), ask = useAction();
  const others = q.data?.data.filter(s => !s.current) ?? [];
  const signOutOthers = others.length > 0 && <Button size="sm" variant="secondary" onClick={() => ask({ title: "Sign out your other sessions?", description: `Ends ${others.length === 1 ? "1 other session" : `${others.length} other sessions`} now. This browser stays signed in, and your API keys keep working.`, submitLabel: "Sign out others", danger: true, successNotice: "Other sessions signed out.", run: (_, signal) => api(`${path}/revoke-others`, { method: "POST", signal }) })}><LogOut aria-hidden /> Sign out others</Button>;
  const body = q.isPending ? <p role="status">Loading sessions…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : q.data.data.length === 0 ? <EmptyState size="compact" icon={<MonitorSmartphone />} title="No active sessions." /> :
    <Table caption="Your dashboard sessions" columns={["Device", "Signed in", "Last active", ""]}>{q.data.data.map(s => <Tr key={s.id}>
      <Td>{device(s)} {s.current && <Badge size="sm" variant="outline">This browser</Badge>}</Td>
      <Td><Time value={s.created_at} format="relative" /></Td>
      <Td>{s.last_seen_at ? <Time value={s.last_seen_at} format="relative" /> : "—"}</Td>
      <Td>{!s.current && <Button size="sm" variant="ghost" aria-label={`Sign out ${named(s)}`} onClick={() => ask({ title: `Sign out ${named(s)}?`, description: "Ends that session now; it has to sign in again.", submitLabel: "Sign out", danger: true, successNotice: "Session signed out.", run: (_, signal) => api(`${path}/${encodeURIComponent(s.id)}`, { method: "DELETE", signal }) })}>Sign out</Button>}</Td>
    </Tr>)}</Table>;
  return <Card title="Sessions" titleAs="h2" description="Where you're signed in to this dashboard." actions={signOutOthers || undefined} flush={!!q.data?.data.length}>{body}</Card>;
}

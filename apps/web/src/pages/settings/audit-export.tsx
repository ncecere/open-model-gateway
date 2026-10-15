/*
 * Admin › Settings › Audit export (#106, #107): where the audit log streams
 * for the SIEM, read-only. The destination is the server operator's (a signed
 * HTTPS webhook, GATEWAY_AUDIT_WEBHOOK_URL with
 * GATEWAY_AUDIT_WEBHOOK_SECRET_ENV, or syslog over TLS,
 * GATEWAY_AUDIT_SYSLOG_URL), so a Platform Admin can't redirect or stop the
 * export; they can see whether it keeps up and send a test.
 */
import { useState } from "react";
import { Send } from "lucide-react";
import { api, API, type Session } from "../../lib/api";
import { Badge, Button, DateTime, ErrorNotice, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { Alert } from "../../components/ui/alert/alert";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { toast } from "../../components/ui/toast/toast";
import { SettingsPage } from "./shared";

export type AuditExportStatus = {
  configured: boolean; transport: "webhook" | "syslog" | null; enabled: boolean; enabled_at: string | null; destination_host: string | null; schema: string;
  queued: number; oldest_queued_at: string | null; last_success_at: string | null;
  last_error: string | null; last_error_at: string | null; retrying: boolean; next_attempt_at: string | null; delivered_total: string;
  next_seq: string; last_heartbeat_at: string | null;
};
const path = `${API}/platform/settings/audit-export`;
export const exportErrors: Record<string, string> = {
  connection: "Couldn't connect to the receiver.",
  tls: "The TLS handshake with the receiver failed (certificate, name or client certificate).",
  timeout: "The receiver didn't answer in time.",
  http_4xx: "The receiver refused the delivery (4xx). Check its signing secret and URL.",
  http_5xx: "The receiver failed (5xx).",
  http_other: "The receiver answered with an unexpected status.",
};
const errorText = (e: string | null) => e ? exportErrors[e] ?? e : "None";
const transports = { webhook: "HTTPS webhook", syslog: "Syslog over TLS" } as const;

export function AuditExportSettingsPage({ session }: { session: Session }) {
  const q = useApi<AuditExportStatus>(path), writable = session.capabilities.platform_write;
  const [testing, setTesting] = useState(false);
  async function test() {
    setTesting(true);
    try {
      const r = await api<{ ok: boolean; error: string | null }>(`${path}/test`, { method: "POST" });
      if (r.ok) toast.success("Test delivery accepted by the receiver."); else toast.error(`Test delivery failed: ${errorText(r.error)}`);
    } catch (error) { toast.error(error instanceof Error ? error.message : "Test delivery failed."); } finally { setTesting(false); }
  }
  const description = "Streams the platform audit log to your SIEM, as signed HTTPS deliveries or as syslog over TLS. The server operator sets the destination; it can't be changed here.";
  if (q.isPending) return <SettingsPage title="Audit export" description={description}><p role="status">Loading…</p></SettingsPage>;
  if (q.isError) return <SettingsPage title="Audit export" description={description}><ErrorNotice error={q.error} retry={() => void q.refetch()} /></SettingsPage>;
  const s = q.data;
  const badge = !s.configured ? <Badge>Not set up</Badge> : s.retrying ? <Badge tone="bad">Retrying</Badge> : <Badge tone="good">Delivering</Badge>;
  return <SettingsPage title="Audit export" description={description}>
    {!s.configured && <Alert tone="info" title="Not set up">To export, the server operator sets <code>GATEWAY_AUDIT_WEBHOOK_URL</code> and <code>GATEWAY_AUDIT_WEBHOOK_SECRET_ENV</code>, or <code>GATEWAY_AUDIT_SYSLOG_URL</code>. Events are exported from then on; earlier ones stay in the dashboard and backups.</Alert>}
    {s.configured && s.retrying && <Alert tone="warning" title="Deliveries are failing">{errorText(s.last_error)} Events wait in the queue and are retried; none are dropped.</Alert>}
    <Card title="Status" titleAs="h2" actions={<>{badge}{s.configured && writable && <Button size="sm" variant="secondary" loading={testing} onClick={() => void test()}><Send aria-hidden /> Send test delivery</Button>}</>}>
      <DescriptionList dividers items={[
        { label: "Transport", value: s.transport ? transports[s.transport] : "None" },
        { label: "Destination", value: s.destination_host ?? "None" },
        { label: "Exporting since", value: s.enabled_at ? <DateTime value={s.enabled_at} /> : "—" },
        { label: "Waiting", value: s.queued === 0 ? "None" : <>{s.queued.toLocaleString()} events{s.oldest_queued_at && <>, oldest <DateTime value={s.oldest_queued_at} /></>}</> },
        { label: "Last delivery", value: s.last_success_at ? <DateTime value={s.last_success_at} /> : "Never" },
        { label: "Last error", value: s.last_error_at ? <>{errorText(s.last_error)} <DateTime value={s.last_error_at} /></> : "None" },
        { label: "Delivered", value: `${Number(s.delivered_total).toLocaleString()} events` },
        { label: "Next sequence number", value: Number(s.next_seq).toLocaleString() },
        { label: "Last heartbeat", value: s.last_heartbeat_at ? <DateTime value={s.last_heartbeat_at} /> : "None yet (sent after a minute without events)" },
        { label: "Format", value: <code>{s.schema}</code> },
      ]} />
    </Card>
  </SettingsPage>;
}

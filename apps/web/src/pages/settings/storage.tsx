/*
 * Admin › Settings › Data & privacy › Storage: the server-configured file store
 * (backend, bucket/endpoint host, encryption key id, health) and per-purpose
 * retention, with an allow toggle only for purposes that hold customer content.
 * Toggles need a configured, healthy store (the server probes before enabling).
 * Auditors see values only. See docs/file-storage.md.
 */
import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { HardDrive } from "lucide-react";
import { api, type Session } from "../../lib/api";
import { formatBytes } from "../../lib/bitop-format";
import { settingsPath, storageBackendLabel, storageBody, storageDraft, storageErrors, storageErrorsOf, type StorageDraft, type StorageSettings, type StorageTestResult } from "../../lib/settings";
import { Badge, Button, DateTime, ErrorNotice, Input, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { Alert } from "../../components/ui/alert/alert";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { Switch } from "../../components/ui/switch/switch";
import { Table, Td, Th, Tr } from "../../components/ui/table/table";
import { toast } from "../../components/ui/toast/toast";
import st from "./settings.module.css";

const path = `${settingsPath}/storage`;

function HealthBadge({ s }: { s: StorageSettings }) {
  if (s.backend === "off") return <Badge>Off</Badge>;
  const h = s.health;
  if (!h || !h.current) return <Badge>Not tested</Badge>;
  return <span className={st.locked}>{h.ok ? <Badge tone="good">Healthy</Badge> : <Badge tone="bad">Failing</Badge>}<DateTime value={h.checked_at} />{!h.ok && h.error && <span className={st.note}>{storageErrors[h.error]}</span>}</span>;
}

function location(s: StorageSettings): string {
  const l = s.location;
  if (!l || l.kind !== "s3") return storageBackendLabel(s);
  return `${l.bucket} · ${l.endpoint_host ?? `s3.${l.region}.amazonaws.com`}${l.endpoint_host && !l.endpoint_tls ? " (HTTP)" : ""}`;
}

export function StorageSection({ session }: { session: Session }) {
  const q = useApi<StorageSettings>(path), client = useQueryClient(), writable = session.capabilities.platform_write;
  const saved = q.data ? storageDraft(q.data) : undefined;
  const [edits, setEdits] = useState<StorageDraft>(), form = edits ?? saved;
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [submitted, setSubmitted] = useState(false);
  const [testing, setTesting] = useState(false), [test, setTest] = useState<StorageTestResult | { failed: unknown }>();
  if (q.isError) return <Card title="Storage"><ErrorNotice error={q.error} retry={() => void q.refetch()} /></Card>;
  if (!q.data || !form || !saved) return <Card title="Storage"><p role="status">Loading storage…</p></Card>;
  const s = q.data, off = s.backend === "off";
  const dirty = JSON.stringify(form) !== JSON.stringify(saved);
  const errors = storageErrorsOf(s, form), invalid = Object.keys(errors).length > 0;
  const set = (g: keyof StorageDraft, patch: Partial<StorageDraft[keyof StorageDraft]>) => setEdits({ ...form, [g]: { ...form[g], ...patch } });
  async function save() {
    setSubmitted(true); if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try { await api(path, { method: "PUT", body: storageBody(form) }); await client.invalidateQueries({ queryKey: ["api"] }); setEdits(undefined); setSubmitted(false); toast.success("Storage settings saved"); }
    catch (caught) { setError(caught); void client.invalidateQueries({ queryKey: ["api"] }); } finally { setBusy(false); }
  }
  async function runTest() {
    setTesting(true); setTest(undefined);
    try { setTest(await api<StorageTestResult>(`${path}/test`, { method: "POST" })); void client.invalidateQueries({ queryKey: ["api"] }); }
    catch (caught) { setTest({ failed: caught }); } finally { setTesting(false); }
  }
  const actions = writable && !off ? <Button variant="secondary" size="sm" loading={testing} onClick={() => void runTest()}><HardDrive aria-hidden /> Test storage</Button> : undefined;
  return <Card title="Storage" description="Encrypted file storage for batch files, exports, branding and videos." actions={actions}>
    <div className={st.form}>
      <DescriptionList dividers items={[
        { label: "Backend", value: off ? <span className={st.locked}><Badge>Off</Badge><code className={st.code}>GATEWAY_FILE_STORE</code></span> : location(s) },
        ...(s.encryption ? [{ label: "Encryption key", value: <span className={st.locked}><code className={st.code}>{s.encryption.key_id}</code>{s.encryption.decrypt_only_keys > 0 && <span className={st.note}>+{s.encryption.decrypt_only_keys} decrypt-only</span>}</span> }] : []),
        { label: "Health", value: <HealthBadge s={s} /> },
      ]} />
      {test && ("failed" in test ? <ErrorNotice error={test.failed} /> : test.ok ? <Alert tone="success" title="Storage works">Wrote, read and deleted a test object{test.round_trip_ms !== null ? ` in ${test.round_trip_ms} ms` : ""}.</Alert> : <Alert tone="danger" title="Storage test failed">{test.error ? storageErrors[test.error] : "The test didn't complete."}</Alert>)}
      {error !== undefined && <ErrorNotice error={error} />}
      <Table caption="Storage by purpose" stack density="compact" columns={["Purpose", { label: "Keep for", width: "10rem" }, { label: "Allowed", width: "8rem" }, { label: "Stored", numeric: true, width: "8rem" }]}>
        {s.groups.map(g => {
          const row = form[g.group], editable = writable && g.retention_editable;
          return <Tr key={g.group}>
            <Th scope="row">{g.label}</Th>
            <Td>{g.retention_days === null && !g.retention_editable ? "No expiry" : editable
              ? <span className={st.days}><Input aria-label={`${g.label} retention in days`} aria-invalid={submitted && !!errors[g.group]} inputMode="numeric" value={row.days} maxLength={3} disabled={busy} onChange={e => set(g.group, { days: e.target.value })} /><span className={st.unit}>days</span></span>
              : `${g.retention_days} days`}</Td>
            <Td>{!g.toggle ? <span className={st.note}>{off ? "Store off" : "Always"}</span> : writable
              ? <Switch label={<span className="sr-only">Allow {g.label.toLowerCase()}</span>} checked={row.enabled} disabled={busy || (off && !row.enabled)} onCheckedChange={checked => set(g.group, { enabled: checked })} />
              : g.enabled ? <Badge tone="good">Allowed</Badge> : <Badge>Off</Badge>}</Td>
            <Td numeric>{g.objects ? formatBytes(g.bytes) : "—"}</Td>
          </Tr>;
        })}
      </Table>
      {submitted && invalid && <p className={st.note} role="alert">{Object.values(errors)[0]}</p>}
      {writable && dirty && <div className={st.status}>
        <span className={st.note}>Turning a purpose on runs a storage test first.</span>
        <span className={st.locked}><Button variant="secondary" disabled={busy} onClick={() => { setEdits(undefined); setSubmitted(false); setError(undefined); }}>Discard</Button><Button loading={busy} onClick={() => void save()}>Save storage</Button></span>
      </div>}
    </div>
  </Card>;
}

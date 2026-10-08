/*
 * Admin › Settings › Email: the SMTP relay that sends invitations (and, later,
 * alerts). The password is never entered here: only an `env:NAME` reference
 * on the server allowlist, like provider credentials. "Send test email" goes
 * to the signed-in admin's own verified address; the server rate-limits and
 * audits it. Without a relay, invitations fall back to copying the code.
 */
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Send } from "lucide-react";
import { api, ApiError, type Session } from "../../lib/api";
import { deliveryErrors, emailBody, emailDraft, emailErrors, settingsPath, tlsLabels, tlsPorts, type EmailDraft, type EmailSettings, type EmailTestResult, type TlsMode } from "../../lib/settings";
import { Badge, Button, DateTime, ErrorNotice, FormField, Input, NativeSelect, Stack, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { Alert } from "../../components/ui/alert/alert";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { Switch } from "../../components/ui/switch/switch";
import { toast } from "../../components/ui/toast/toast";
import { readOnlyNote, SaveControls, SettingsPage } from "./shared";
import st from "./settings.module.css";

type Draft = EmailDraft & { enabled: boolean };
const draftOf = (e: EmailSettings): Draft => ({ ...emailDraft(e), enabled: e.configured });
const statusBadge = (e: EmailSettings) => e.status === "ready" ? <Badge tone="good">Ready</Badge> : e.status === "credential_unavailable" ? <Badge tone="bad">Password unavailable</Badge> : <Badge>Not set up</Badge>;
/** Server reasons that belong to one field. */
const reasonField: Record<string, [keyof EmailDraft, string]> = {
  plaintext_requires_loopback: ["tls", "Unencrypted delivery only works with a relay on this machine. Choose STARTTLS or implicit TLS."],
  credential_reference_not_allowed: ["password_ref", "This variable isn't on the server allowlist (GATEWAY_SECRET_ENV_ALLOWLIST). Ask whoever runs the gateway to add it."],
};

export function EmailSettingsPage({ session }: { session: Session }) {
  const path = `${settingsPath}/email`, q = useApi<EmailSettings>(path), client = useQueryClient(), writable = session.capabilities.platform_write;
  const saved = q.data ? draftOf(q.data) : undefined;
  const [edits, setEdits] = useState<Draft>(), form = edits ?? saved;
  const [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [rejected, setRejected] = useState<Partial<Record<keyof EmailDraft, string>>>({});
  const [testing, setTesting] = useState(false), [test, setTest] = useState<EmailTestResult | { failed: unknown }>();
  const dirty = !!form && !!saved && (form.enabled !== saved.enabled || form.enabled && JSON.stringify(form) !== JSON.stringify(saved));
  const errors = form?.enabled ? emailErrors(form) : {}, invalid = Object.keys(errors).length > 0, shown = { ...(submitted ? errors : {}), ...rejected };
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => { if (!form) return; setEdits({ ...form, [k]: v }); if (k in rejected) setRejected({}); };
  async function save() {
    setSubmitted(true); if (!form || invalid || busy) return;
    setBusy(true); setError(undefined); setRejected({});
    try {
      await api(path, { method: "PUT", body: form.enabled ? emailBody(form) : { host: null } });
      await client.invalidateQueries({ queryKey: ["api"] });
      setEdits(undefined); setSubmitted(false); setTest(undefined); toast.success(form.enabled ? "Email settings saved" : "Email delivery turned off");
    } catch (caught) {
      const placed = caught instanceof ApiError && caught.reason ? reasonField[caught.reason] : undefined;
      if (placed) setRejected({ [placed[0]]: placed[1] }); else setError(caught);
    } finally { setBusy(false); }
  }
  async function sendTest() {
    setTesting(true); setTest(undefined);
    try { const result = await api<EmailTestResult>(`${path}/test`, { method: "POST" }); setTest(result); void client.invalidateQueries({ queryKey: ["api"] }); }
    catch (caught) { setTest({ failed: caught }); } finally { setTesting(false); }
  }
  const page = (body: ReactNode) => <SettingsPage title="Email" description="An SMTP relay for sending invitations. Without one, invitations show a code to copy and send yourself.">{body}</SettingsPage>;
  if (q.isError) return page(<ErrorNotice error={q.error} retry={() => void q.refetch()} />);
  if (!form || !q.data) return page(<p role="status">Loading settings…</p>);
  const e = q.data;
  const status = <Card title="Status" description={e.configured ? `Delivering through ${e.host}:${e.port}.` : "No relay is set up, so no email is sent."}>
    <Stack gap={4}>
      <DescriptionList dividers items={[
        { label: "Delivery", value: statusBadge(e) },
        { label: "Last test", value: e.last_test ? <span className={st.locked}>{e.last_test.ok ? <Badge tone="good">Delivered</Badge> : <Badge tone="bad">Failed</Badge>}<DateTime value={e.last_test.at} />{e.last_test.error && <span className={st.note}>{deliveryErrors[e.last_test.error]}</span>}</span> : "Not tested" },
        { label: "Invitation link", value: e.public_url_configured ? "Emails point to this gateway's Accept invitation page." : "GATEWAY_PUBLIC_URL isn't set, so emails ask people to open the gateway themselves." },
      ]} />
      {e.status === "credential_unavailable" && <Alert tone="warning" title="The password can't be read">The referenced variable isn't set on the server or isn't on its allowlist. Email won't send until it is.</Alert>}
      {writable && <div className={st.status}>
        <p className={st.note}>Sends a short test message to {session.user.email}. At most 2 a minute.</p>
        <Button variant="secondary" loading={testing} disabled={!e.configured || dirty} onClick={() => void sendTest()}><Send aria-hidden /> Send test email</Button>
      </div>}
      {test && ("failed" in test ? <ErrorNotice error={test.failed} /> : test.ok ? <Alert tone="success" title="Test email sent">Sent to {test.recipient}. Check that inbox, including spam.</Alert> : <Alert tone="danger" title="Test email failed">{test.error ? deliveryErrors[test.error] : "The message wasn't delivered."}</Alert>)}
      {writable && dirty && e.configured && <p className={st.note}>Save or discard your changes before sending a test.</p>}
    </Stack>
  </Card>;
  if (!writable) return page(<>
    {status}
    {e.configured && <Card title="Relay" description={readOnlyNote}><DescriptionList dividers items={[
      { label: "Host", value: e.host }, { label: "Port", value: String(e.port) }, { label: "Security", value: e.tls ? tlsLabels[e.tls] : "—" },
      { label: "Username", value: e.username ?? "None" }, { label: "Password reference", value: e.password_ref ? <code className={st.code}>{e.password_ref}</code> : "None" },
      { label: "From", value: e.from_name ? `${e.from_name} <${e.from_address}>` : e.from_address },
    ]} /></Card>}
  </>);
  return page(<div className={st.form} data-dirty={dirty ? "true" : undefined}>
    {status}
    {error !== undefined && <ErrorNotice error={error} />}
    <Card title="Relay" description="One connection per message, with STARTTLS or implicit TLS. Unencrypted delivery is only possible to a relay on this machine.">
      <Stack gap={5}>
        <Switch label="Send email" description="Turning this off clears the relay settings." checked={form.enabled} disabled={busy} onCheckedChange={checked => set("enabled", checked)} />
        {form.enabled && <>
          <div className={st.grid} data-columns="2">
            <FormField label="Host" description="Host name or IP address only, without a port." error={shown.host}><Input value={form.host} placeholder="smtp.example.com" maxLength={253} autoComplete="off" disabled={busy} onChange={ev => set("host", ev.target.value)} /></FormField>
            <FormField label="Port" error={shown.port}><Input inputMode="numeric" value={form.port} maxLength={5} disabled={busy} onChange={ev => set("port", ev.target.value)} /></FormField>
            <FormField label="Security" description="STARTTLS usually uses port 587; implicit TLS 465." error={shown.tls}><NativeSelect value={form.tls} disabled={busy} onChange={ev => { const tls = ev.target.value as TlsMode; if (!form) return; setEdits({ ...form, tls, port: form.port === String(tlsPorts[form.tls]) ? String(tlsPorts[tls]) : form.port }); setRejected({}); }}>{(Object.keys(tlsLabels) as TlsMode[]).map(m => <option key={m} value={m}>{tlsLabels[m]}</option>)}</NativeSelect></FormField>
          </div>
          <div className={st.grid} data-columns="2">
            <FormField label="Username" labelHint="Optional" description="Leave empty if the relay doesn't need sign-in." error={shown.username}><Input value={form.username} maxLength={256} autoComplete="off" disabled={busy} onChange={ev => set("username", ev.target.value)} /></FormField>
            <FormField label="Password reference" labelHint={form.username.trim() ? undefined : "Optional"} description={<>A server variable holding the password, written <code>env:NAME</code>, on <code>GATEWAY_SECRET_ENV_ALLOWLIST</code>. The password itself is never entered or stored here.</>} error={shown.password_ref}><Input value={form.password_ref} placeholder="env:SMTP_PASSWORD" maxLength={132} autoComplete="off" spellCheck={false} disabled={busy} onChange={ev => set("password_ref", ev.target.value)} /></FormField>
          </div>
          <div className={st.grid} data-columns="2">
            <FormField label="From address" error={shown.from_address}><Input type="email" value={form.from_address} placeholder="gateway@example.com" maxLength={320} disabled={busy} onChange={ev => set("from_address", ev.target.value)} /></FormField>
            <FormField label="From name" labelHint="Optional" error={shown.from_name}><Input value={form.from_name} placeholder={session.installation.name} maxLength={120} disabled={busy} onChange={ev => set("from_name", ev.target.value)} /></FormField>
          </div>
        </>}
      </Stack>
    </Card>
    <SaveControls writable dirty={dirty} invalid={submitted && invalid} busy={busy} saveLabel="Save email settings" onSave={() => void save()} onDiscard={() => { setEdits(undefined); setSubmitted(false); setError(undefined); setRejected({}); }} />
  </div>);
}

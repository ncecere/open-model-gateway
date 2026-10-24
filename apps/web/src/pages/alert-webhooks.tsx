import { useId, useLayoutEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Plus, Webhook } from "lucide-react";
import { api, ApiError, type RequestOptions, type Session } from "../lib/api";
import { deliveryErrorText, deliveryLabels, deliveryStates, webhookBody, webhookDraft, webhookDraftErrors, webhookError, webhookItemPath, webhookPath, type AlertWebhook, type WebhookDeliveries, type WebhookDraft } from "../lib/alert-webhooks";
import { Alert, Button, ErrorNotice, FormField, Heading, Input, Stack, Status, useAction, useApi } from "../components/ui";
import { LocalTable } from "../components/people";
import { CellText, DataTable } from "../components/ui/data-table/data-table";
import { Dialog } from "../components/ui/dialog/dialog";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { TableTime } from "../components/templates/when";
import { CopyId } from "../components/templates/copy-id";
import { DiscardChangesDialog, NavigationGuard } from "../components/navigation-guard";
import { toast } from "../components/ui/toast/toast";
import s from "./shared.module.css";

/** Session Origin/CSRF and live authority remain the server's responsibility. Never retry a write. */
function useWebhookWrite(session: Session) {
  const client = useQueryClient(), allowed = useRef(session.capabilities.platform_write), retired = useRef(false), mounted = useRef(false);
  const controllers = useRef(new Set<AbortController>());
  allowed.current = session.capabilities.platform_write;
  useLayoutEffect(() => {
    mounted.current = true;
    const abort = () => { for (const request of controllers.current) request.abort(); };
    const retire = () => { retired.current = true; abort(); };
    window.addEventListener("omg:access-refresh", retire); window.addEventListener("omg:unauthorized", retire);
    return () => { mounted.current = false; abort(); window.removeEventListener("omg:access-refresh", retire); window.removeEventListener("omg:unauthorized", retire); };
  }, []);
  return async (path: string, options: RequestOptions) => {
    if (!mounted.current || !allowed.current || retired.current) throw webhookError(new ApiError(403, "403", "Access denied"));
    const controller = new AbortController(); controllers.current.add(controller);
    try {
      const result = await api(path, { ...options, signal: options.signal ? AbortSignal.any([options.signal, controller.signal]) : controller.signal });
      controller.signal.throwIfAborted();
      void client.invalidateQueries({ queryKey: ["api"] }); return result;
    } catch (error) {
      if (error instanceof ApiError && error.status === 403) {
        void client.invalidateQueries({ queryKey: ["session"] }); window.dispatchEvent(new Event("omg:access-refresh"));
      }
      throw webhookError(error);
    } finally { controllers.current.delete(controller); }
  };
}
type Write = ReturnType<typeof useWebhookWrite>;

/** Admin › Settings › Alerts › Webhooks. No workspace recipient delegation or payload editor. */
export function AlertWebhooks({ session }: { session: Session }) {
  const readable = session.capabilities.platform_read, writable = session.capabilities.platform_write;
  const q = useApi<{ data: AlertWebhook[] }>(webhookPath, readable), ask = useAction(), write = useWebhookWrite(session);
  const [editing, setEditing] = useState<AlertWebhook | "new">(), [selected, setSelected] = useState<AlertWebhook>();
  const [error, setError] = useState<unknown>(), [queued, setQueued] = useState(false), [testing, setTesting] = useState<string>();
  const inFlight = useRef(false), createTrigger = useRef<HTMLButtonElement>(null);
  const showDeliveries = (d: AlertWebhook) => { setQueued(false); setSelected(d); };
  const toggle = (d: AlertWebhook) => ask({
    title: `${d.enabled ? "Disable" : "Enable"} ${d.label}?`, submitLabel: d.enabled ? "Disable webhook" : "Enable webhook",
    description: d.enabled ? "Waiting deliveries become ineligible. Re-enabling does not replay them. An already-authorized send may finish." : "Only future eligible installation transitions are queued. The operator's URL and reference approvals are checked again; there is no historical backfill.",
    successNotice: d.enabled ? "Webhook disabled" : "Webhook enabled",
    run: (_, signal) => write(webhookItemPath(d.id), { method: "PATCH", body: { enabled: !d.enabled }, signal }),
  });
  const remove = (d: AlertWebhook) => ask({
    title: `Delete ${d.label}?`, danger: true, submitLabel: "Delete webhook", successNotice: "Webhook deleted",
    description: "This cannot be undone. The URL and signing references are scrubbed; retained delivery history is not erased. Waiting deliveries become ineligible. An already-authorized send may finish.",
    run: (_, signal) => write(webhookItemPath(d.id), { method: "DELETE", signal }),
  });
  async function test(d: AlertWebhook) {
    if (!writable || !d.enabled || inFlight.current) return;
    inFlight.current = true; setTesting(d.id); setError(undefined); setQueued(false);
    try { await write(`${webhookItemPath(d.id)}/test`, { method: "POST" }); setQueued(true); setSelected(d); }
    catch (caught) { setError(caught); } finally { inFlight.current = false; setTesting(undefined); }
  }
  if (!readable) return <Alert tone="warning" title="Access not available">Platform Admin or Auditor access is required.</Alert>;
  return <Stack gap={5}>
    <Heading title="Webhooks" description="Signed notifications to operator-approved receivers. Up to ten saved destinations, initially disabled."
      actions={writable && <Button ref={createTrigger} onClick={() => setEditing("new")}><Plus aria-hidden /> Create webhook</Button>} />
    <Alert tone="info" title="Verify signatures and prevent replay">
      Signatures use HMAC-SHA256 over exactly &lt;timestamp&gt;.&lt;raw body&gt;. Receivers must verify signatures in constant time, accept timestamps only within ±300 seconds, check X-OMG-Delivery matches the signed delivery_id, and durably deduplicate by delivery_id. The signing secret itself is never shown; only its reference is configured.
    </Alert>
    <p className={s.note}>Only eligible installation transitions are sent. Workspace rules, personal incidents, key-expiry reminders and admission-ceiling incidents are excluded. Delivery may duplicate or arrive out of order; failed and exhausted deliveries cannot be replayed.</p>
    {error !== undefined && <ErrorNotice error={error} />}
    <LocalTable<AlertWebhook> rows={q.data?.data} loading={q.isFetching} error={q.error ? webhookError(q.error) : undefined} retry={() => void q.refetch()}
      label="Alert webhooks" storageKey="alert-webhooks" rowKey={d => d.id} rowLabel={d => d.label} columns={[
        { id: "name", header: "Name", accessor: d => d.label, rowHeader: true, hideable: false, cell: d => <CellText primary={d.label} secondary={d.signing_configured ? d.overlap_active ? "Signing configured · overlap active" : "Signing configured" : "Signing not configured"} /> },
        { id: "enabled", header: "Status", accessor: d => d.enabled ? "enabled" : "disabled", cell: d => <Status enabled={d.enabled} /> },
        { id: "delivery", header: "Delivery status", cell: d => <CellText primary={<>Last success: <TableTime value={d.last_success_at} fallback="None recorded" /></>} secondary={`Last recorded error: ${deliveryErrorText(d.last_error)}`} /> },
        { id: "waiting", header: "Waiting", numeric: true, cell: d => d.waiting },
        { id: "exhausted", header: "Exhausted", numeric: true, cell: d => d.exhausted },
      ]} rowActions={d => [
        { label: "Deliveries", onSelect: () => showDeliveries(d) },
        { label: "Edit…", hidden: !writable, onSelect: () => setEditing(d) },
        { label: d.enabled ? "Disable…" : "Enable…", hidden: !writable, onSelect: () => toggle(d) },
        { label: testing === d.id ? "Queueing test…" : "Send test", hidden: !writable, disabled: !d.enabled || !!testing, disabledReason: !d.enabled ? "Enable this saved webhook first." : undefined, onSelect: () => void test(d) },
        { label: "Delete…", danger: true, hidden: !writable, onSelect: () => remove(d) },
      ]} empty={{ icon: <Webhook aria-hidden />, title: "No webhooks yet.", description: "Ask the operator to approve a receiver URL and signing reference before creating a destination." }} />
    <Button variant="secondary" onClick={() => void q.refetch()} disabled={q.isFetching}>Refresh webhooks</Button>
    {writable && editing && <WebhookForm key={typeof editing === "string" ? "new" : editing.id} saved={editing === "new" ? undefined : editing} write={write} onClose={() => { setEditing(undefined); requestAnimationFrame(() => createTrigger.current?.focus()); }} />}
    {selected && <DeliveryPanel destination={selected} writable={writable} queued={queued} onClose={() => { setSelected(undefined); setQueued(false); }} />}
  </Stack>;
}

function WebhookForm({ saved, write, onClose }: { saved?: AlertWebhook; write: Write; onClose: () => void }) {
  const id = useId(), [draft, setDraft] = useState(() => webhookDraft(saved)), [submitted, setSubmitted] = useState(false);
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [discarding, setDiscarding] = useState(false);
  const inFlight = useRef(false), completed = useRef(false), mounted = useRef(true);
  useLayoutEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const dirty = JSON.stringify(draft) !== JSON.stringify(webhookDraft(saved));
  const errors = webhookDraftErrors(draft, saved), invalid = Object.keys(errors).length > 0;
  const close = () => { if (!busy) { if (dirty) setDiscarding(true); else onClose(); } };
  const field = (key: keyof WebhookDraft, label: string, description?: string, readOnly = false) => <FormField label={label} description={description} error={submitted ? errors[key] : undefined}>
    <Input value={draft[key]} readOnly={readOnly} disabled={busy} maxLength={key === "label" ? 120 : key === "url" ? 2048 : 160} autoComplete="off" spellCheck={false} autoFocus={key === "label"} onChange={event => setDraft(previous => ({ ...previous, [key]: event.target.value }))} />
  </FormField>;
  async function save() {
    setSubmitted(true); if (invalid || inFlight.current || completed.current) return;
    inFlight.current = true; setBusy(true); setError(undefined);
    try {
      await write(saved ? webhookItemPath(saved.id) : webhookPath, { method: saved ? "PATCH" : "POST", body: webhookBody(draft, saved) });
      if (!mounted.current) return;
      completed.current = true; toast.success(saved ? "Webhook saved" : "Webhook created disabled"); onClose();
    } catch (caught) { if (mounted.current) setError(caught); } finally { inFlight.current = false; if (mounted.current) setBusy(false); }
  }
  return <><Dialog open title={saved ? "Edit webhook" : "Create webhook"} description={saved ? "URLs are immutable. Create a new destination to change receivers." : "Saving does not contact the receiver or resolve signing secrets. The destination starts disabled."} hideClose={busy} onOpenChange={open => { if (!open) close(); }} footer={<><Button variant="secondary" disabled={busy} onClick={close}>Cancel</Button><Button type="submit" form={id} loading={busy}>{saved ? "Save webhook" : "Create webhook"}</Button></>}>
    <form id={id} noValidate aria-busy={busy} data-dirty={dirty ? "true" : undefined} onSubmit={event => { event.preventDefault(); void save(); }}><Stack gap={4}>
      {field("label", "Name")}
      {field("url", "URL", "Must exactly match the operator URL allowlist. HTTPS required except development-only loopback HTTP. No credentials, query or fragment. URL approval does not override egress policy.", !!saved)}
      {field("current", "Signing reference", "An operator-approved env:NAME, never the signing secret itself. Deploy the variable to every worker first.")}
      {saved ? <>
        {field("previous", "Previous signing reference", "Optional rotation overlap: use a different approved reference. Receivers accept either signature during overlap.")}
        {field("until", "Overlap ends (RFC3339)", "Set both previous fields for at most the next 24 hours, or clear both to end overlap. Include the time zone, for example 2026-11-01T12:00:00Z.")}
      </> : <p className={s.note}>After creation, Edit lets you rotate references and optionally overlap the previous key for up to 24 hours.</p>}
      {error !== undefined && <ErrorNotice error={error} />}
    </Stack></form>
  </Dialog><NavigationGuard dirty={dirty} allow={() => completed.current} /><DiscardChangesDialog open={discarding} onOpenChange={setDiscarding} onDiscard={onClose} /></>;
}

function DeliveryPanel({ destination, writable, queued, onClose }: { destination: AlertWebhook; writable: boolean; queued: boolean; onClose: () => void }) {
  const q = useApi<WebhookDeliveries>(`${webhookItemPath(destination.id)}/deliveries`), d = q.data?.destination;
  return <Dialog open title={`Deliveries · ${destination.label}`} size="xl" description="At most 50 newest status records. No payloads, signatures or secret material. Refresh to see worker progress; tests make one attempt only." onOpenChange={open => { if (!open) onClose(); }} footer={<><Button variant="secondary" disabled={q.isFetching} onClick={() => void q.refetch()}>Refresh deliveries</Button><Button onClick={onClose}>Close deliveries</Button></>}>
    <Stack gap={4}>
      {queued && <Alert tone="info" title="Test queued">This is not a delivery confirmation. Refresh deliveries to check its outcome. No automatic retry is made for a test.</Alert>}
      {q.error ? <ErrorNotice error={webhookError(q.error)} retry={() => void q.refetch()} /> : <>
        {d && <DescriptionList items={[
          { label: "Status", value: <Status enabled={d.enabled} /> },
          { label: "Signing", value: d.signing_configured ? "Configured (secret never shown)" : "Not configured" },
          { label: "Overlap", value: d.overlap_active ? <>Active until <TableTime value={d.previous_secret_until} /></> : "Inactive" },
          ...(writable ? [{ label: "URL", value: <span className={s.secondary}>{d.url}</span> }, { label: "Signing reference", value: <span className={s.secondary}>{d.current_secret_ref}</span> }, ...(d.previous_secret_ref ? [{ label: "Previous signing reference", value: <span className={s.secondary}>{d.previous_secret_ref}</span> }] : [])] : []),
        ]} />}
        {q.data && <p className={s.note}>{deliveryStates.map(state => `${deliveryLabels[state]}: ${q.data!.counts[state]}`).join(" · ")}</p>}
        <DataTable caption="Webhook deliveries" stack data={q.data?.data ?? []} getRowId={row => row.id} loading={q.isFetching} columns={[
          { id: "state", header: "Outcome", rowHeader: true, cell: row => deliveryLabels[row.state] ?? "Unknown" },
          { id: "type", header: "Type", cell: row => row.test ? "Test" : "Alert transition" },
          { id: "attempts", header: "Attempts", numeric: true, cell: row => row.attempt_count },
          { id: "created", header: "Created", cell: row => <TableTime value={row.created_at} /> },
          { id: "next", header: "Next attempt / completion", cell: row => <TableTime value={row.next_attempt_at ?? row.completed_at} /> },
          { id: "error", header: "Last error", cell: row => deliveryErrorText(row.last_error) },
          { id: "id", header: "Delivery ID", cell: row => <CopyId value={row.id} label="delivery ID" /> },
        ]} empty="No deliveries recorded. Enabling sends future eligible transitions only." />
      </>}
    </Stack>
  </Dialog>;
}

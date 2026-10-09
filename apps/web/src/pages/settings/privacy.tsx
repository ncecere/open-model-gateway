/*
 * Admin › Settings › Data & privacy: what OpenRouter may do with prompts, how
 * long request metadata keeps its details, and the fixed fact that prompts and
 * responses are never stored. A setting the server environment sets is shown
 * locked with its variable; the save sends it back unchanged.
 */
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, type Session } from "../../lib/api";
import { settingsPath, type PrivacySettings } from "../../lib/settings";
import { Badge, ErrorNotice, FormField, Input, Stack, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { RadioGroup } from "../../components/ui/radio-group/radio-group";
import { toast } from "../../components/ui/toast/toast";
import { EnvironmentLock, readOnlyNote, SaveControls, SettingsPage } from "./shared";
import st from "./settings.module.css";

type Policy = "deny" | "allow";
type Draft = { policy: Policy; retention: string };
const draftOf = (p: PrivacySettings): Draft => ({ policy: p.openrouter_data_collection.stored, retention: p.request_log_retention_days.stored === null ? "" : String(p.request_log_retention_days.stored) });
const policyOptions: { value: Policy; label: string; description: string }[] = [
  { value: "deny", label: "Deny", description: "Only providers that don't store or train on prompts. Free (:free) models are unavailable. Recommended." },
  { value: "allow", label: "Allow", description: "Providers that may store or train on prompts are allowed too, including free models." },
];
export function retentionError(value: string, min: number, max: number): string | undefined {
  const v = value.trim(); if (!v) return;
  if (!/^\d{1,4}$/.test(v) || Number(v) < min || Number(v) > max) return `Enter ${min}–${max} days, or leave it empty to keep details.`;
  return;
}
const retentionText = (days: number | null) => days === null ? "Kept (no automatic compaction)" : `${days} days`;

export function PrivacySettingsPage({ session }: { session: Session }) {
  const path = `${settingsPath}/privacy`, q = useApi<PrivacySettings>(path), client = useQueryClient(), writable = session.capabilities.platform_write;
  const saved = q.data ? draftOf(q.data) : undefined;
  const [edits, setEdits] = useState<Draft>(), form = edits ?? saved;
  const [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const dirty = !!form && !!saved && JSON.stringify(form) !== JSON.stringify(saved);
  const r = q.data?.request_log_retention_days, retention = form && r ? retentionError(form.retention, r.minimum, r.maximum) : undefined, invalid = !!retention;
  async function save() {
    setSubmitted(true); if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try {
      await api(path, { method: "PUT", body: { openrouter_data_collection: form.policy, request_log_retention_days: form.retention.trim() ? Number(form.retention.trim()) : null } });
      await client.invalidateQueries({ queryKey: ["api"] });
      setEdits(undefined); setSubmitted(false); toast.success("Settings saved");
    } catch (caught) { setError(caught); void client.invalidateQueries({ queryKey: ["api"] }); } finally { setBusy(false); }
  }
  const page = (body: ReactNode) => <SettingsPage title="Data & privacy" description="What providers may do with prompts, and how long request details are kept.">{body}</SettingsPage>;
  if (q.isError) return page(<ErrorNotice error={q.error} retry={() => void q.refetch()} />);
  if (!form || !q.data || !r) return page(<p role="status">Loading settings…</p>);
  const dc = q.data.openrouter_data_collection, edit = writable;
  return page(<div className={st.form} data-dirty={dirty ? "true" : undefined}>
    {error !== undefined && <ErrorNotice error={error} />}
    <Card title="OpenRouter data collection" description={<>Sent with every OpenRouter request as <code>provider.data_collection</code>. Other providers are unaffected.</>}>
      {dc.locked || !edit
        ? <Stack gap={3}><DescriptionList dividers items={[{ label: "Data collection", value: policyOptions.find(o => o.value === dc.value)?.label ?? dc.value }]} />{dc.locked ? <p className={st.note}><EnvironmentLock variable={dc.variable} /> Change it in the server environment; it applies after a restart.</p> : <p className={st.note}>{readOnlyNote}</p>}</Stack>
        : <RadioGroup<Policy> legend="Data collection" options={policyOptions} value={form.policy} disabled={busy} onValueChange={v => setEdits({ ...form, policy: v })} />}
    </Card>
    <Card title="Request log retention" description="After this many days, settled requests drop their error code and latency. Usage, cost and the audit log are kept.">
      {r.locked || !edit
        ? <Stack gap={3}><DescriptionList dividers items={[{ label: "Compact details after", value: retentionText(r.value) }]} />{r.locked ? <p className={st.note}><EnvironmentLock variable={r.variable} /> Change it in the server environment; it applies after a restart.</p> : <p className={st.note}>{readOnlyNote}</p>}</Stack>
        : <FormField label="Compact details after" labelHint="Optional" description={`${r.minimum}–${r.maximum} days. Leave empty to keep details.`} error={submitted ? retention : undefined}><span className={st.days}><Input inputMode="numeric" value={form.retention} maxLength={4} placeholder="Keep" disabled={busy} onChange={e => setEdits({ ...form, retention: e.target.value })} /><span className={st.unit}>days</span></span></FormField>}
    </Card>
    <Card title="Prompts and responses" description="Request logs hold metadata only: model, key, tokens, cost, timing and status.">
      <DescriptionList dividers items={[{ label: "Prompt and response bodies", value: <Badge tone="good">Never stored</Badge> }]} />
    </Card>
    <SaveControls writable={edit && !(dc.locked && r.locked)} dirty={dirty} invalid={submitted && invalid} busy={busy} saveLabel="Save settings" onSave={() => void save()} onDiscard={() => { setEdits(undefined); setSubmitted(false); setError(undefined); }} />
  </div>);
}

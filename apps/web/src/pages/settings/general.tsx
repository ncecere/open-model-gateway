/*
 * Admin › Settings › General: the installation's display name, help and logo
 * links, and the longest lifetime a person may give a new key. Times are UTC.
 * Provenance: layout follows Grounded web/src/pages/admin/settings/general.tsx
 * (read-only reference).
 */
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError, type Session } from "../../lib/api";
import { daysError, httpsUrlError, KEY_DAYS, nameError, settingsPath, type GeneralSettings } from "../../lib/settings";
import { ErrorNotice, FormField, Input, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { toast } from "../../components/ui/toast/toast";
import { readOnlyNote, SaveControls, SettingsPage } from "./shared";
import st from "./settings.module.css";

type Draft = { display_name: string; support_url: string; logo_url: string; days: string };
const draftOf = (g: GeneralSettings): Draft => ({ display_name: g.display_name, support_url: g.support_url ?? "", logo_url: g.logo_url ?? "", days: String(g.human_key_max_lifetime_days) });
export function generalErrors(d: Draft): Partial<Record<keyof Draft, string>> {
  const out: Partial<Record<keyof Draft, string>> = {};
  const name = nameError(d.display_name); if (name) out.display_name = name;
  const support = httpsUrlError(d.support_url); if (support) out.support_url = support;
  const logo = httpsUrlError(d.logo_url); if (logo) out.logo_url = logo;
  const days = daysError(d.days, KEY_DAYS.min, KEY_DAYS.max, "the lifetime"); if (days) out.days = days;
  return out;
}

export function GeneralSettingsPage({ session }: { session: Session }) {
  const path = `${settingsPath}/general`, q = useApi<GeneralSettings>(path), client = useQueryClient(), writable = session.capabilities.platform_write;
  const saved = q.data ? draftOf(q.data) : undefined;
  const [edits, setEdits] = useState<Draft>(), form = edits ?? saved;
  const [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const dirty = !!form && !!saved && JSON.stringify(form) !== JSON.stringify(saved);
  const errors = form ? generalErrors(form) : {}, invalid = Object.keys(errors).length > 0, shown = submitted ? errors : {};
  const set = (k: keyof Draft, v: string) => form && setEdits({ ...form, [k]: v });
  async function save() {
    setSubmitted(true); if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try {
      await api<GeneralSettings>(path, { method: "PUT", body: { display_name: form.display_name.trim(), support_url: form.support_url.trim() || null, logo_url: form.logo_url.trim() || null, human_key_max_lifetime_days: Number(form.days.trim()) } });
      await client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] });
      setEdits(undefined); setSubmitted(false); toast.success("Settings saved");
    } catch (caught) { setError(caught); } finally { setBusy(false); }
  }
  const page = (body: ReactNode) => <SettingsPage title="General" description="Name, help link, logo and key lifetime.">{body}</SettingsPage>;
  if (q.isError) return page(<ErrorNotice error={q.error} retry={() => void q.refetch()} />);
  if (!form || !q.data) return page(<p role="status">Loading settings…</p>);
  if (!writable) return page(<>
    <Card title="Installation" description={readOnlyNote}><DescriptionList dividers items={[{ label: "Display name", value: q.data.display_name }, { label: "Support link", value: q.data.support_url ?? "Not set" }, { label: "Logo", value: q.data.logo_url ?? "Not set" }]} /></Card>
    <Card title="API keys"><DescriptionList dividers items={[{ label: "Longest lifetime for a new personal key", value: `${q.data.human_key_max_lifetime_days} days` }]} /></Card>
    <Card title="Time"><DescriptionList dividers items={[{ label: "Time zone", value: "UTC" }]} /></Card>
  </>);
  return page(<div className={st.form} data-dirty={dirty ? "true" : undefined}>
    {error !== undefined && <ErrorNotice error={error instanceof ApiError && error.status === 400 ? new Error("The settings weren't saved: check the highlighted fields and try again.") : error} />}
    <Card title="Installation" description="Shown in page titles, emails and to everyone who signs in.">
      <div className={st.grid}>
        <FormField label="Display name" error={shown.display_name}><Input value={form.display_name} maxLength={120} disabled={busy} onChange={e => set("display_name", e.target.value)} /></FormField>
        <FormField label="Support link" labelHint="Optional" description="Starts with https://." error={shown.support_url}><Input type="url" inputMode="url" placeholder="https://help.example.com" value={form.support_url} maxLength={2048} disabled={busy} onChange={e => set("support_url", e.target.value)} /></FormField>
        <FormField label="Logo URL" labelHint="Optional" description="An https:// image address." error={shown.logo_url}><Input type="url" inputMode="url" placeholder="https://cdn.example.com/logo.svg" value={form.logo_url} maxLength={2048} disabled={busy} onChange={e => set("logo_url", e.target.value)} /></FormField>
      </div>
    </Card>
    <Card title="API keys" description="For keys people create or rotate from now on. Existing keys keep their expiry.">
      <FormField label="Longest lifetime for a new personal key" description={`${KEY_DAYS.min}–${KEY_DAYS.max} days.`} error={shown.days}><span className={st.days}><Input inputMode="numeric" value={form.days} maxLength={3} disabled={busy} onChange={e => set("days", e.target.value)} /><span className={st.unit}>days</span></span></FormField>
    </Card>
    <Card title="Time" description="Budgets, reports and periods use UTC days.">
      <DescriptionList dividers items={[{ label: "Time zone", value: "UTC" }]} />
    </Card>
    <SaveControls writable dirty={dirty} invalid={submitted && invalid} busy={busy} saveLabel="Save settings" onSave={() => void save()} onDiscard={() => { setEdits(undefined); setSubmitted(false); setError(undefined); }} />
  </div>);
}

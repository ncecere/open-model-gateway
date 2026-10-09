/*
 * Admin › Settings › General: the installation's display name, help link,
 * uploaded logo (stored in the encrypted file store, shown in the sidebar and on
 * the sign-in page instead of the Portal mark), and the longest lifetime a
 * person may give a new key. Times are UTC. The old external logo URL is
 * deprecated: kept by the API, never shown.
 * Provenance: layout follows Grounded web/src/pages/admin/settings/general.tsx
 * (read-only reference).
 */
import { useId, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError, type Session } from "../../lib/api";
import { daysError, httpsUrlError, KEY_DAYS, LOGO_MAX_BYTES, LOGO_TYPES, logoFileError, logoPath, nameError, settingsPath, uploadLogo, type GeneralSettings } from "../../lib/settings";
import { Button, ErrorNotice, FormField, Input, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { toast } from "../../components/ui/toast/toast";
import { InstallationLogo } from "../../components/layout/installation-logo";
import { ResourceLink } from "../../components/navigation-link";
import { Upload } from "lucide-react";
import { readOnlyNote, SaveControls, SettingsPage } from "./shared";
import st from "./settings.module.css";

type Draft = { display_name: string; support_url: string; days: string };
const draftOf = (g: GeneralSettings): Draft => ({ display_name: g.display_name, support_url: g.support_url ?? "", days: String(g.human_key_max_lifetime_days) });
export function generalErrors(d: Draft): Partial<Record<keyof Draft, string>> {
  const out: Partial<Record<keyof Draft, string>> = {};
  const name = nameError(d.display_name); if (name) out.display_name = name;
  const support = httpsUrlError(d.support_url); if (support) out.support_url = support;
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
      await api<GeneralSettings>(path, { method: "PUT", body: { display_name: form.display_name.trim(), support_url: form.support_url.trim() || null, logo_url: q.data?.logo_url ?? null, human_key_max_lifetime_days: Number(form.days.trim()) } });
      await client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] });
      setEdits(undefined); setSubmitted(false); toast.success("Settings saved");
    } catch (caught) { setError(caught); } finally { setBusy(false); }
  }
  const page = (body: ReactNode) => <SettingsPage title="General" description="Name, help link, logo and key lifetime.">{body}</SettingsPage>;
  if (q.isError) return page(<ErrorNotice error={q.error} retry={() => void q.refetch()} />);
  if (!form || !q.data) return page(<p role="status">Loading settings…</p>);
  if (!writable) return page(<>
    <Card title="Installation" description={readOnlyNote}><DescriptionList dividers items={[{ label: "Display name", value: q.data.display_name }, { label: "Support link", value: q.data.support_url ?? "Not set" }, { label: "Logo", value: <LogoPreview settings={q.data} /> }]} /></Card>
    <Card title="API keys"><DescriptionList dividers items={[{ label: "Longest lifetime for a new personal key", value: `${q.data.human_key_max_lifetime_days} days` }]} /></Card>
    <Card title="Time"><DescriptionList dividers items={[{ label: "Time zone", value: "UTC" }]} /></Card>
  </>);
  return page(<div className={st.form} data-dirty={dirty ? "true" : undefined}>
    {error !== undefined && <ErrorNotice error={error instanceof ApiError && error.status === 400 ? new Error("The settings weren't saved: check the highlighted fields and try again.") : error} />}
    <Card title="Installation" description="Shown in page titles, emails and to everyone who signs in.">
      <div className={st.grid}>
        <FormField label="Display name" error={shown.display_name}><Input value={form.display_name} maxLength={120} disabled={busy} onChange={e => set("display_name", e.target.value)} /></FormField>
        <FormField label="Support link" labelHint="Optional" description="Starts with https://." error={shown.support_url}><Input type="url" inputMode="url" placeholder="https://help.example.com" value={form.support_url} maxLength={2048} disabled={busy} onChange={e => set("support_url", e.target.value)} /></FormField>
        <LogoRow settings={q.data} />
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

/** The logo at sign-in size, or the Portal mark, with a short caption. */
export function LogoPreview({ settings }: { settings: GeneralSettings }) {
  const logo = settings.logo;
  return <span className={st.logoPreview}>
    <span className={st.logoBox}><InstallationLogo logo={logo} alt={settings.display_name} className={st.logoImage} /></span>
    <span className={st.logoCaption}>{logo ? `Custom · ${logo.width} × ${logo.height} px` : "Open Model Gateway (default)"}</span>
  </span>;
}

/**
 * Admin › Settings › General › Logo: preview, Upload (PNG, JPEG or WebP; the server checks the bytes) and Remove.
 * Saved at once, outside the settings draft. Needs the file store; without it, one line links to Storage.
 */
export function LogoRow({ settings }: { settings: GeneralSettings }) {
  const client = useQueryClient(), input = useRef<HTMLInputElement>(null), labelId = useId();
  const [busy, setBusy] = useState<"upload" | "remove">(), [error, setError] = useState<string>();
  const upload = settings.logo_upload, available = upload?.available ?? false, maxBytes = upload?.max_bytes ?? LOGO_MAX_BYTES;
  const done = async (message: string) => { await client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] }); toast.success(message); };
  async function choose(file: File | undefined) {
    if (input.current) input.current.value = "";
    if (!file || busy) return;
    const invalid = logoFileError(file, maxBytes); setError(invalid); if (invalid) return;
    setBusy("upload");
    try { await uploadLogo(file); await done("Logo updated"); }
    catch (caught) { setError(caught instanceof ApiError && caught.status !== 0 && caught.status < 500 ? caught.message : "The logo wasn't saved. Try again."); }
    finally { setBusy(undefined); }
  }
  async function remove() {
    if (busy) return; setBusy("remove"); setError(undefined);
    try { await api(logoPath, { method: "DELETE" }); await done("Logo removed"); }
    catch (caught) { setError(caught instanceof ApiError ? caught.message : "The logo wasn't removed. Try again."); }
    finally { setBusy(undefined); }
  }
  return <div className={st.logoRow} role="group" aria-labelledby={labelId} data-logo={settings.logo ? "custom" : "portal"}>
    <span id={labelId} className={st.logoLabel}>Logo</span>
    <div className={st.logoControls}>
      <LogoPreview settings={settings} />
      {available && <><input ref={input} type="file" hidden accept={LOGO_TYPES.join(",")} onChange={e => void choose(e.target.files?.[0])} />
        <Button size="sm" variant="secondary" loading={busy === "upload"} disabled={!!busy} onClick={() => input.current?.click()}><Upload aria-hidden /> Upload</Button></>}
      {settings.logo && <Button size="sm" variant="ghost" loading={busy === "remove"} disabled={!!busy} onClick={() => void remove()}>Remove</Button>}
    </div>
    {available
      ? <p className={st.note}>PNG, JPEG or WebP up to {Math.round(maxBytes / 1024)} KiB; square, 64 px or larger.</p>
      : <p className={st.note}>Needs file storage: <ResourceLink search={{ page: "settings-privacy" }}>Data &amp; privacy › Storage</ResourceLink>.</p>}
    {settings.logo_url && !settings.logo && <p className={st.note} data-notice="logo-url">Logo URL is no longer shown; upload a logo.</p>}
    {error && <p className={st.error} role="alert">{error}</p>}
  </div>;
}

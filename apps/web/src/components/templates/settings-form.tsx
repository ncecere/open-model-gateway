import { useEffect, useId, useRef, useState } from "react";
import { DateControl } from "../date-control";
import { NavigationGuard } from "../navigation-guard";
export { NavigationGuard } from "../navigation-guard";
import { useQueryClient } from "@tanstack/react-query";
import { type Field, type Values, validateFields } from "../../lib/forms";
import { Button, CheckboxesField, ErrorNotice, FormField, Input, NativeSelect, Textarea, Stack } from "../ui";
import { Form } from "../ui/field/field";
import { StickySaveBar } from "./sticky-save-bar";
import { AlertDialog } from "../ui/dialog/dialog";
import { useDashboardNavigation } from "../navigation-link";
import { DescriptionList } from "../ui/description-list/description-list";
import { Switch } from "../ui/switch/switch";
import s from "./templates.module.css";

/** How a saved field value reads when it can't be edited: option labels, "Not set" for empty, never a disabled control. */
export function fieldDisplay(field: Field, value: string | undefined): string {
  const raw = value ?? field.value ?? "";
  if (field.type === "checkboxes") {
    let ids: string[] = []; try { const parsed: unknown = JSON.parse(raw || "[]"); if (Array.isArray(parsed)) ids = parsed.map(String); } catch { /* malformed: none */ }
    return ids.length ? ids.map(id => field.options?.find(o => o.value === id)?.label ?? id).join(", ") : "None";
  }
  if (field.type === "switch") return raw === "true" ? "On" : "Off";
  if (field.type === "select") return field.options?.find(o => o.value === raw)?.label ?? (raw || "Not set");
  return raw.trim() ? raw : "Not set";
}
/** Read-only rendering of settings fields (review rule 12: a description list, no disabled form, no per-section banner). */
export function ReadOnlyFields({ fields, values }: { fields: Field[]; values?: Values }) {
  return <DescriptionList dividers items={fields.filter(f => !f.visibleWhen || f.visibleWhen(values ?? Object.fromEntries(fields.map(x => [x.name, x.value ?? ""])))).map(f => ({ label: f.label, value: fieldDisplay(f, values?.[f.name]) }))} />;
}
export function SettingsForm({ fields, writable, onSave, description, allowPristineSubmit = false }: { fields: Field[]; writable: boolean; onSave: (values: Values, signal: AbortSignal) => Promise<unknown>; description?: string; /** Only enable when an absent replacement header can be explicitly created. */ allowPristineSubmit?: boolean }) {
  const initial = () => Object.fromEntries(fields.map(f => [f.name, f.value ?? (f.type === "checkboxes" ? "[]" : "")]));
  const [values, setValues] = useState<Values>(initial), [saved, setSaved] = useState<Values>(initial), [errors, setErrors] = useState<Record<string, string>>({}), [error, setError] = useState<unknown>(), [busy, setBusy] = useState(false), [notice, setNotice] = useState("");
  const [pristineSubmitted, setPristineSubmitted] = useState(false);
  const dirty = JSON.stringify(values) !== JSON.stringify(saved), creatingOverride = allowPristineSubmit && !pristineSubmitted, canSubmit = writable && (dirty || creatingOverride), id = useId(), client = useQueryClient(), nav = useDashboardNavigation(), controller = useRef<AbortController | undefined>(undefined);
  const incoming = JSON.stringify(initial()), applied = useRef(incoming);
  useEffect(() => {
    // Query invalidation/reset must refresh pristine controls, not just labels.
    // Preserve a pending draft; once discarded, apply the latest server values.
    if (busy || dirty || incoming === applied.current) return;
    applied.current = incoming;
    const next = JSON.parse(incoming) as Values;
    setValues(next); setSaved(next); setErrors({}); setError(undefined); setNotice("");
  }, [incoming, dirty, busy]);
  useEffect(() => () => controller.current?.abort(), []);
  useEffect(() => { if (!allowPristineSubmit) setPristineSubmitted(false); }, [allowPristineSubmit]);
  const changeValue = (name: string, value: string) => {
    if (!writable || busy || values[name] === value) return;
    setValues(prev => ({ ...prev, [name]: value }));
    // Validation can depend on other fields. Release controlled invalid state so
    // Base UI lets the next submit revalidate the current values.
    setErrors({});
  };
  async function submit(event: React.FormEvent) {
    event.preventDefault(); if (!canSubmit || busy) return;
    const validation = validateFields(fields, values); setErrors(validation); if (Object.keys(validation).length) { document.getElementById(`${id}-${Object.keys(validation)[0]}`)?.focus(); return; }
    const request = new AbortController(); controller.current = request; setBusy(true); setError(undefined); setNotice("");
    try { await onSave(values, request.signal); if (request.signal.aborted) return; setSaved(values); if (creatingOverride) setPristineSubmitted(true); setNotice("Settings saved."); void client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] }); }
    catch (error) { if (!request.signal.aborted) setError(error); } finally { if (!request.signal.aborted) setBusy(false); }
  }
  if (!writable) return <Stack gap={4}>{description && <p>{description}</p>}<ReadOnlyFields fields={fields} values={saved} /></Stack>;
  return <Form noValidate onSubmit={event => void submit(event)} className={s.settings} data-dirty={dirty ? "true" : undefined}><Stack gap={6}>{description && <p>{description}</p>}{fields.filter(f => !f.visibleWhen || f.visibleWhen(values)).map(f => {
    const fieldId = `${id}-${f.name}`, common = { id: fieldId, name: f.name, required: f.required || f.requiredWhen?.(values), disabled: !writable || busy, value: values[f.name] ?? "", onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>) => changeValue(f.name, event.target.value) };
    const help = f.helpFor?.(values) ?? f.help;
    if (f.type === "switch") return <Switch key={f.name} id={fieldId} label={f.label} description={help} checked={values[f.name] === "true"} disabled={!writable || busy} onCheckedChange={checked => changeValue(f.name, String(checked))} />;
    if (f.type === "checkboxes") return <CheckboxesField key={f.name} field={{ ...f, help }} id={fieldId} value={values[f.name]} error={errors[f.name]} disabled={!writable || busy} onChange={value => changeValue(f.name, value)} />;
    return <FormField key={f.name} label={f.label} labelHint={f.hint ?? (!(f.required || f.requiredWhen?.(values)) ? "Optional" : undefined)} description={help} error={errors[f.name]}>{f.type === "date" ? <DateControl id={fieldId} name={f.name} value={values[f.name] ?? ""} disabled={!writable || busy} onChange={value => changeValue(f.name, value)} /> : f.type === "select" ? <NativeSelect {...common}><option value="">Choose…</option>{f.options?.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect> : f.type === "textarea" ? <Textarea {...common} rows={4} /> : <Input {...common} type={f.type ?? "text"} inputMode={f.inputMode} maxLength={f.maxLength} min={f.min} max={f.max} step={f.type === "number" ? 1 : undefined} />}</FormField>;
  })}{error !== undefined && <ErrorNotice error={error} />}{notice && <p role="status">{notice}</p>}</Stack><StickySaveBar open={canSubmit} message={dirty ? "Unsaved changes" : "Set limits for this workspace"}>{dirty && <Button variant="ghost" disabled={busy} onClick={() => { setValues(saved); setErrors({}); setError(undefined); }}>Discard</Button>}<Button type="submit" loading={busy}>{creatingOverride ? "Save custom settings" : "Save settings"}</Button></StickySaveBar>{nav && <NavigationGuard dirty={writable && dirty} />}</Form>;
}

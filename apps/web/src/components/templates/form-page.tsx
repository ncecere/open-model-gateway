/*
 * FormPage: a long create form on a page of its own (Add model), with a
 * "Back to …" link, titled sections in two columns, and Cancel + submit under
 * the form. Short forms stay in ActionDialog.
 *
 * Provenance: layout and section pattern adapted from Grounded
 * web/src/components/templates/form-page.tsx and takeover.tsx (read-only
 * reference). OMG's version is a routed page, not a takeover over a list, and
 * reuses OMG's NavigationGuard instead of Grounded's close guard.
 *
 *   <FormPage label="Add model" title="Add model" back={{ label: "Models", search: { page: "models" } }}
 *     onCancel={…} onSubmit={…} submitLabel="Add model" busy={busy} dirty={dirty} error={error}>
 *     <FormSection title="Source"><FieldControls … /></FormSection>
 *   </FormPage>
 */
import { ArrowLeft } from "lucide-react";
import { useEffect, useId, useRef, type ReactNode } from "react";
import type { DashboardSearch } from "../../lib/permissions";
import { parseCheckboxValues, type Field, type Values } from "../../lib/forms";
import { Button, ErrorNotice, FormField, Input, NativeSelect, Textarea } from "../ui";
import { Card, CardBody } from "../ui/card/card";
import { Checkbox, CheckboxGroup } from "../ui/checkbox/checkbox";
import { Form } from "../ui/field/field";
import { Stack } from "../ui/layout/layout";
import { PageHeader } from "../ui/page-header/page-header";
import { DateControl } from "../date-control";
import { NavigationGuard } from "../navigation-guard";
import { ResourceLink } from "../navigation-link";
import { useResourceName } from "../layout/breadcrumbs";
import s from "../../pages/shared.module.css";
import styles from "./templates.module.css";

/** "← Back to Models": a real link (new tab, copy) that navigates in place. */
export function BackLink({ label, search }: { label: string; search: DashboardSearch }) {
  return <ResourceLink search={search} className={styles.backLink}><ArrowLeft aria-hidden size={14} />Back to {label}</ResourceLink>;
}

export type FormPageProps = {
  /** Plain-text page name for the breadcrumb and the form's accessible name. */
  label: string;
  title: ReactNode;
  description?: ReactNode;
  back: { label: string; search: DashboardSearch };
  onCancel: () => void;
  onSubmit: () => void;
  submitLabel: ReactNode;
  busy?: boolean;
  /** Unsaved edits: leaving any way asks "Leave without saving?" first. */
  dirty: boolean;
  /** Allow leaving without asking (e.g. after a successful save, while navigating to the result). */
  allowLeave?: () => boolean;
  /** Request-level error, shown above the actions. Field errors belong to their fields. */
  error?: unknown;
  /** Show a placeholder while the form's options load. */
  loading?: ReactNode;
  children: ReactNode;
};

export function FormPage({ label, title, description, back, onCancel, onSubmit, submitLabel, busy = false, dirty, allowLeave, error, loading, children }: FormPageProps) {
  const id = useId(), ref = useRef<HTMLFormElement>(null);
  useResourceName(label);
  // A form page starts on its first field (Grounded initialFocus="field"). The shell focuses the
  // page heading after navigation; run after it so the field wins.
  useEffect(() => {
    if (loading) return;
    const timer = setTimeout(() => ref.current?.querySelector<HTMLElement>("input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled])")?.focus({ preventScroll: true }), 0);
    return () => clearTimeout(timer);
  }, [!!loading]);
  return <Stack gap={6} className={`${s.page} ${styles.takeover}`}>
    <PageHeader title={title} description={description} breadcrumbs={<BackLink {...back} />} />
    <Card><CardBody>{loading ? <div role="status">{loading}</div> : <Form ref={ref} id={id} noValidate aria-label={label} aria-busy={busy} className={styles.formBody} data-dirty={dirty ? "true" : undefined} onSubmit={event => { event.preventDefault(); if (!busy) onSubmit(); }}>{children}</Form>}</CardBody></Card>
    {error !== undefined && <ErrorNotice error={error} />}
    <div className={styles.formActions}><Button variant="secondary" onClick={onCancel}>Cancel</Button><Button type="submit" form={id} loading={busy} disabled={!!loading}>{submitLabel}</Button></div>
    <NavigationGuard dirty={dirty} allow={allowLeave} />
  </Stack>;
}

/** A titled group of fields inside a FormPage, in two columns (one on narrow screens). */
export function FormSection({ title, description, children }: { title: string; description?: ReactNode; children: ReactNode }) {
  return <fieldset className={styles.formSection}><legend className={styles.formSectionTitle}>{title}</legend>{description && <p className={`${s.note} ${styles.formSectionNote}`}>{description}</p>}<div className={styles.formSectionBody}>{children}</div></fieldset>;
}

/** Spans both columns of a FormSection. */
export function Wide({ children }: { children: ReactNode }) { return <div className={styles.wide}>{children}</div>; }

/**
 * Field[] controls with controlled values and errors, the same contract as
 * ActionDialog and SettingsForm (validateFields, string values, JSON checkbox values).
 */
export function FieldControls({ fields, values, errors, disabled, idPrefix, onChange }: { fields: Field[]; values: Values; errors: Record<string, string>; disabled?: boolean; idPrefix: string; onChange: (name: string, value: string) => void }) {
  return <>{fields.filter(f => !f.visibleWhen || f.visibleWhen(values)).map(f => {
    const fieldId = `${idPrefix}-${f.name}`, required = f.required || f.requiredWhen?.(values);
    if (f.type === "checkboxes") {
      let selected: string[]; try { selected = parseCheckboxValues(values[f.name] ?? "[]"); } catch { selected = []; }
      return <Wide key={f.name}><CheckboxGroup legend={f.label} description={f.help} error={errors[f.name]} id={fieldId} tabIndex={-1} value={selected} disabled={disabled} onValueChange={next => onChange(f.name, JSON.stringify(next))} orientation={f.options && f.options.length > 4 ? "vertical" : "horizontal"}>{f.options?.map(o => <Checkbox key={o.value} value={o.value} label={o.label} />)}</CheckboxGroup></Wide>;
    }
    // validateFields is authoritative: native constraints would let Base UI block submit with generic
    // browser messages, so required is announced with aria-required and ranges are checked in code.
    const common = { id: fieldId, name: f.name, "aria-required": required || undefined, disabled, value: values[f.name] ?? "", onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>) => onChange(f.name, event.target.value) };
    const control = f.type === "date" ? <DateControl id={fieldId} name={f.name} value={values[f.name] ?? ""} disabled={disabled} onChange={value => onChange(f.name, value)} />
      : f.type === "select" ? <NativeSelect {...common}><option value="">Choose…</option>{f.options?.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect>
      : f.type === "textarea" ? <Textarea {...common} rows={3} autoComplete="off" />
      : <Input {...common} type={f.type ?? "text"} inputMode={f.inputMode ?? (f.type === "number" ? "numeric" : undefined)} maxLength={f.maxLength} placeholder={f.placeholder} autoComplete="off" spellCheck={false} />;
    const field = <FormField key={f.name} name={f.name} label={f.label} labelHint={f.hint ?? (!required ? "Optional" : undefined)} description={f.help} error={errors[f.name]}>{control}</FormField>;
    return f.type === "textarea" ? <Wide key={f.name}>{field}</Wide> : field;
  })}</>;
}

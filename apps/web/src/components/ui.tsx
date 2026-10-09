import { createContext, Fragment, useContext, useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, type Collection, type OneTimeToken, type Session } from "../lib/api";
import { parseCheckboxValues, validateFields, type Field, type Values } from "../lib/forms";
import { detailTime } from "../lib/format";
import { DataTable, type DataTableColumn } from "./ui/data-table/data-table";
import type { Facet } from "./ui/filter-bar/filter-bar";
import { FilterToolbar } from "./templates/filter-toolbar";
import { useStoredColumns } from "./templates/table-view";
import { Card, CardBody } from "./ui/card/card";
import { PageHeader } from "./templates/page-header";
import { Badge as BitopBadge } from "./ui/badge/badge";
import { EmptyState } from "./ui/empty-state/empty-state";
import { Alert, ErrorAlert } from "./ui/alert/alert";
import { Button } from "./ui/button/button";
import { Input, NativeSelect, Textarea } from "./ui/input/input";
import { Field as FormField, Form } from "./ui/field/field";
import { Checkbox, CheckboxGroup } from "./ui/checkbox/checkbox";
import { RadioGroup } from "./ui/radio-group/radio-group";
import { Switch } from "./ui/switch/switch";
import { WithIcon } from "./provider-icon";
import { IconSelect } from "./icon-select";
import { Dialog } from "./ui/dialog/dialog";
import { Stack, Inline } from "./ui/layout/layout";
import { Time } from "./ui/time/time";
import { toast } from "./ui/toast/toast";
import { DateControl } from "./date-control";
import { DiscardChangesDialog, NavigationGuard } from "./navigation-guard";
import { useDashboardNavigation } from "./navigation-link";
import s from "../pages/shared.module.css";
export { Button, Input, NativeSelect, Textarea, FormField, Alert, Stack, Inline };
export { StatCard } from "./ui/stat-card/stat-card";
export function ErrorNotice({ error, retry }: { error: unknown; retry?: () => void }) { return <ErrorAlert error={error} title={error instanceof ApiError && error.status === 403 ? "Access denied" : error instanceof ApiError && error.status === 404 ? "Not found" : "Unable to complete request"} onRetry={retry} />; }
export function Empty({ title = "Nothing here yet", children }: { title?: string; children?: ReactNode }) { return <EmptyState title={title} description={children} titleAs="h3" />; }
export function Badge({ children, tone = "neutral" }: { children: ReactNode; tone?: "neutral" | "good" | "bad" }) { return <BitopBadge tone={tone === "good" ? "success" : tone === "bad" ? "danger" : "neutral"}>{children}</BitopBadge>; }
export function Status({ enabled }: { enabled: boolean }) { return <Badge tone={enabled ? "good" : "neutral"}>{enabled ? "Enabled" : "Disabled"}</Badge>; }
/** Relative time for tables ("3 hours ago") with the detail format in its tooltip ("Oct 8, 2026, 2:30 AM EDT", lib/format). */
export function DateTime({ value }: { value?: string | null }) { return <Time value={value} format="relative" fallback="—" title={value ? detailTime(value) : undefined} />; }
export function Id({ value }: { value?: string | null }) { return value ? <code className={s.secondary} title={value}>{value}</code> : <span className={s.secondary}>—</span>; }
const HeadingLevel = createContext<"h1" | "h2">("h1");
export function SectionHeadings({ children }: { children: ReactNode }) { return <HeadingLevel.Provider value="h2">{children}</HeadingLevel.Provider>; }
export function Heading({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) { return <PageHeader title={title} titleAs={useContext(HeadingLevel)} description={description} actions={actions} />; }
export function Panel({ title, children }: { title?: string; children: ReactNode }) { return <Card title={title}>{title ? children : <CardBody>{children}</CardBody>}</Card>; }
const ApiScopeContext = createContext<string | undefined>(undefined);
export function ApiScopeProvider({ session, children }: { session: Session; children: ReactNode }) {
  const scope = JSON.stringify(session);
  return <ApiScopeContext.Provider key={scope} value={scope}>{children}</ApiScopeContext.Provider>;
}
/** The live authorization scope that keys every API query (for pages that build several queries at once). */
export const useApiScope = () => useContext(ApiScopeContext);
export function useApi<T>(path: string, enabled = true) { const scope = useContext(ApiScopeContext); return useQuery({ queryKey: ["api", scope, path], queryFn: ({ signal }) => api<T>(path, { signal }), enabled, retry: false }); }
export function useCollection<T>(path: string, enabled = true) { return useApi<Collection<T>>(path, enabled); }
export function useChoices<T>(path: string, enabled = true) {
  const scope = useContext(ApiScopeContext);
  return useQuery({ queryKey: ["api", scope, path, "choices"], enabled, retry: false, queryFn: async ({ signal }) => {
    const rows: T[] = [];
    for (let offset = 0; offset < 20000; offset += 200) {
      const page = await api<Collection<T>>(`${path}${path.includes("?") ? "&" : "?"}limit=200&offset=${offset}`, { signal }); rows.push(...page.data);
      if (page.has_more === false || page.data.length < 200) return rows;
    }
    throw new Error("Too many options to load. Narrow the selection before continuing.");
  } });
}
export type Column<T> = { title: string; render: (row: T) => ReactNode; hidden?: boolean; narrow?: boolean; numeric?: boolean };
const columnsOf = <T,>(columns: Column<T>[]): DataTableColumn<T>[] => columns.map((col, i) => ({ id: col.title, header: col.title, cell: col.render, rowHeader: i === 0, hideable: i !== 0, defaultHidden: col.hidden, defaultHiddenNarrow: col.narrow, numeric: col.numeric }));
export function Table<T>({ rows, columns, rowKey, label }: { rows: T[]; columns: Column<T>[]; rowKey: (row: T) => string; label: string }) { return <DataTable caption={label} stack columns={columnsOf(columns)} data={rows} getRowId={rowKey} columnsMenu columnsMenuMin={5} />; }
/** An offset-paged collection; with search and/or the status filter, a FilterToolbar above it (both in the URL). */
export function CollectionTable<T>({ path, columns, rowKey, label, empty, pageSize = 50, searchable = false, statusFilter = false }: { path: string; columns: Column<T>[]; rowKey: (row: T) => string; label: string; empty: string; pageSize?: number; searchable?: boolean; statusFilter?: boolean }) {
  const navigation = useDashboardNavigation();
  const [local, setLocal] = useState<{ offset?: number; q?: string; enabled?: "true" | "false" }>({ offset: 0 });
  const state = navigation?.search ?? local, offset = state.offset ?? 0, q = searchable ? state.q ?? "" : "", enabled = statusFilter ? state.enabled : undefined;
  const change = (next: Partial<typeof local>) => navigation ? navigation.navigate({ ...navigation.search, ...next }) : setLocal(prev => ({ ...prev, ...next }));
  const params = new URLSearchParams({ limit: String(pageSize), offset: String(offset) }); if (q) params.set("q", q); if (enabled) params.set("enabled", enabled);
  const query = useCollection<T>(`${path}${path.includes("?") ? "&" : "?"}${params}`);
  const tableColumns = columnsOf(columns), storage = `omg.enterprise.columns.${label}`, cols = useStoredColumns(tableColumns, storage, 5), filters = searchable || statusFilter;
  return <div className={s.list}>
    {filters && <FilterToolbar search={searchable ? { label: `Search ${label.toLowerCase()}`, placeholder: "Name or identifier", value: q, onChange: next => change({ q: next || undefined, offset: undefined }) } : undefined}
      facets={statusFilter ? [statusFacet] : []} values={enabled ? { enabled: [enabled] } : {}} onChange={next => change({ enabled: (Array.isArray(next.enabled) ? next.enabled[0] : undefined) as "true" | "false" | undefined, offset: undefined })} end={cols.menu} />}
    <DataTable caption={label} stack columns={tableColumns} data={query.data?.data ?? []} getRowId={rowKey} manual {...filters ? { hiddenColumns: cols.hidden, onHiddenColumnsChange: cols.setHidden } : { columnsMenu: true, columnsMenuMin: 5, columnsStorageKey: storage }}
      loading={query.isFetching} error={query.error} onRetry={() => void query.refetch()} empty={<EmptyState size="compact" title={q || enabled ? `No ${label.toLowerCase()} match these filters` : `No ${label.toLowerCase()} yet`} description={q || enabled ? undefined : empty} action={q || enabled ? <Button size="sm" variant="ghost" onClick={() => change({ q: undefined, enabled: undefined, offset: undefined })}>Clear filters</Button> : undefined} />} cursor={{ hasPrevious: offset > 0, hasNext: !!query.data && (query.data.has_more ?? query.data.data.length === pageSize) && offset + pageSize <= 100000, onPrevious: () => change({ offset: Math.max(0, offset - pageSize) }), onNext: () => change({ offset: offset + pageSize }), label: query.data?.data.length ? (() => { const rows = query.data.data.length, more = query.data.has_more ?? rows === pageSize; return `Rows ${offset + 1}\u2013${offset + rows}${more ? "" : ` of ${offset + rows}`}`; })() : undefined }} />
  </div>;
}
const statusFacet: Facet = { id: "enabled", label: "Status", type: "toggle", allLabel: "All", options: [{ value: "true", label: "Enabled" }, { value: "false", label: "Disabled" }] };
export type Action = { title: string; returnFocus?: HTMLElement; description?: string; submitLabel?: string; fields?: Field[]; danger?: boolean; secretLabel?: string; /** What the one-time secret is called in the dialog ("key", "invite code"; default "token"). */ secretNoun?: string; successNotice?: string; navigates?: boolean; run: (values: Values, signal: AbortSignal) => Promise<unknown>; after?: (result: unknown) => void };
const ActionContext = createContext<(action: Action) => void>(() => { throw new Error("Missing action provider"); });
export const useAction = () => useContext(ActionContext);
export function ActionProvider({ children }: { children: ReactNode }) {
  const [action, setAction] = useState<Action | null>(null);
  const ask = useCallback((next: Action) => setAction({ ...next, returnFocus: next.returnFocus ?? (document.activeElement instanceof HTMLElement ? document.activeElement : undefined) }), []);
  return <ActionContext.Provider value={ask}>{children}{action && <ActionDialog action={action} onClose={success => { setAction(null); if (success) toast.success(action.successNotice ?? "Changes saved."); }} />}</ActionContext.Provider>;
}
function ActionDialog({ action, onClose }: { action: Action; onClose: (success: boolean) => void }) {
  const client = useQueryClient(), fields = action.fields ?? [], id = useId();
  const [values, setValues] = useState<Values>(() => Object.fromEntries(fields.map(field => [field.name, field.value ?? (field.type === "checkboxes" ? "[]" : "")])));
  const initial = useRef(values), dirty = JSON.stringify(values) !== JSON.stringify(initial.current);
  const [discarding, setDiscarding] = useState(false), [errors, setErrors] = useState<Record<string, string>>({}), [error, setError] = useState<unknown>(), [busy, setBusy] = useState(false), [token, setToken] = useState<string>(), [copied, setCopied] = useState(false), [copyError, setCopyError] = useState(false);
  const mounted = useRef(true), inFlight = useRef(false), completed = useRef(false), controller = useRef<AbortController | undefined>(undefined);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; controller.current?.abort(); }; }, []);
  useEffect(() => { if (!dirty || token) return; const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; }; window.addEventListener("beforeunload", warn); return () => window.removeEventListener("beforeunload", warn); }, [dirty, token]);
  const changeValue = (name: string, value: string) => {
    if (busy || values[name] === value) return;
    setValues(prev => ({ ...prev, [name]: value }));
    // Validation can depend on other fields. Release controlled invalid state so
    // Base UI lets the next submit revalidate the current values.
    setErrors({});
  };
  const discard = () => { controller.current?.abort(); setToken(undefined); setValues({}); onClose(!!token); };
  const close = () => { if (busy) return; if (!token && dirty) setDiscarding(true); else discard(); };
  async function submit(event: React.FormEvent) {
    event.preventDefault(); if (inFlight.current) return;
    const validation = validateFields(fields, values); setErrors(validation);
    if (Object.keys(validation).length) { document.getElementById(`${id}-${Object.keys(validation)[0]}`)?.focus(); return; }
    const request = new AbortController(); controller.current = request; inFlight.current = true; setBusy(true); setError(undefined);
    try {
      // Deliberately not useMutation: one-time credentials never enter mutation caches.
      const result = await action.run(Object.fromEntries(Object.entries(values).map(([key, value]) => [key, value.trim()])), request.signal);
      if (!mounted.current || request.signal.aborted) return;
      void client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] });
      completed.current = true;
      if (action.secretLabel) { const secret = result as OneTimeToken; if (typeof secret?.token !== "string" || !secret.token) throw new Error("No token was returned. Check the list before trying again."); setValues({}); setToken(secret.token); }
      else { action.after?.(result); onClose(true); }
    } catch (caught) { if (mounted.current && !request.signal.aborted) { setError(caught); if (caught instanceof ApiError && caught.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); } }
    finally { inFlight.current = false; if (mounted.current) setBusy(false); }
  }
  return <><Dialog open title={token ? action.secretLabel! : action.title} description={action.description} hideClose={busy} onOpenChange={open => { if (!open) close(); }} finalFocus={() => action.returnFocus?.isConnected ? action.returnFocus : true} footer={token ? <Button onClick={discard}>I have saved it</Button> : <><Button variant="secondary" disabled={busy} onClick={close}>Cancel</Button><Button type="submit" form={id} variant={action.danger ? "danger" : "primary"} loading={busy}>{action.submitLabel ?? "Save"}</Button></>}>
    {token ? <Stack gap={4}><Alert tone="warning" title="Shown only once">Store it securely. Closing this dialog clears it from the page.</Alert><FormField label={`One-time ${action.secretNoun ?? "token"}`}><Textarea readOnly value={token} rows={4} spellCheck={false} autoComplete="off" autoFocus onFocus={event => event.currentTarget.select()} /></FormField><Button variant="secondary" onClick={async () => { try { await navigator.clipboard.writeText(token); setCopied(true); setCopyError(false); } catch { setCopyError(true); } }}>{copied ? "Copied" : `Copy ${action.secretNoun ?? "token"}`}</Button><p role="status">{copyError ? `Clipboard unavailable. Select and copy the ${action.secretNoun ?? "token"} manually.` : copied ? `Copied. Clear your clipboard after storing the ${action.secretNoun ?? "token"}.` : ""}</p></Stack> : <Form id={id} noValidate onSubmit={event => void submit(event)} aria-busy={busy} data-dirty={dirty && !token ? "true" : undefined}><Stack gap={4}>{fields.filter(field => !field.visibleWhen || field.visibleWhen(values)).map((field, index) => {
      const fieldId = `${id}-${field.name}`;
      const common = { id: fieldId, name: field.name, required: field.required || field.requiredWhen?.(values), disabled: busy, value: values[field.name] ?? "", autoFocus: !action.danger && index === 0, onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>) => changeValue(field.name, event.target.value) };
      const help = field.helpFor?.(values) ?? field.help;
      if (field.type === "switch") return <Switch key={field.name} id={fieldId} label={field.label} description={help} checked={values[field.name] === "true"} disabled={busy} onCheckedChange={checked => changeValue(field.name, String(checked))} />;
      if (field.type === "checkboxes") return <CheckboxesField key={field.name} field={{ ...field, help }} id={fieldId} value={values[field.name] ?? "[]"} error={errors[field.name]} disabled={busy} onChange={value => changeValue(field.name, value)} />;
      if (field.type === "select" && field.display === "icon-select") return <IconSelectField key={field.name} field={{ ...field, help }} id={fieldId} value={values[field.name] ?? ""} error={errors[field.name]} disabled={busy} onChange={value => changeValue(field.name, value)} />;
      if (field.type === "select" && field.display === "cards") return <CardChoiceField key={field.name} field={{ ...field, help }} id={fieldId} value={values[field.name] ?? ""} error={errors[field.name]} disabled={busy} onChange={value => changeValue(field.name, value)} />;
      return <FormField key={field.name} name={field.name} label={field.label} labelHint={field.hint ?? (!(field.required || field.requiredWhen?.(values)) ? "Optional" : undefined)} description={help} error={errors[field.name]}>{field.type === "date" ? <DateControl id={fieldId} name={field.name} value={values[field.name] ?? ""} disabled={busy} onChange={value => changeValue(field.name, value)} /> : field.type === "select" ? <NativeSelect {...common}><option value="">Choose…</option>{field.options?.map(option => <option key={option.value} value={option.value}>{option.label}</option>)}</NativeSelect> : field.type === "textarea" ? <Textarea {...common} rows={4} autoComplete="off" spellCheck={false} /> : <Input {...common} type={field.type ?? "text"} inputMode={field.inputMode} min={field.min} max={field.max} step={field.type === "number" ? 1 : undefined} maxLength={field.maxLength} placeholder={field.placeholder} autoComplete="off" spellCheck={field.type === "password" ? false : undefined} />}</FormField>;
    })}{error !== undefined && <ErrorNotice error={error} />}</Stack></Form>}
  </Dialog><NavigationGuard dirty={dirty && !token} allow={() => completed.current || !!action.navigates && inFlight.current} /><DiscardChangesDialog open={discarding} onOpenChange={setDiscarding} onDiscard={discard} /></>;
}
/** A select field as Bitop radio cards: native radio semantics (arrow keys, one tab stop) with optional decorative option icons. */
export function CardChoiceField({ field, id, value, error, disabled, onChange }: { field: Field; id: string; value: string; error?: string; disabled?: boolean; onChange: (value: string) => void }) {
  return <RadioGroup legend={field.label} description={field.help} error={error} id={id} tabIndex={-1} name={field.name} variant="card" value={value} disabled={disabled} onValueChange={next => onChange(String(next))} options={field.options?.map(option => ({ value: option.value, label: option.icon ? <WithIcon icon={option.icon}>{option.label}</WithIcon> : option.label })) ?? []} />;
}
/** A select field as a compact dropdown with each option's icon in the list and the trigger. The field wires help and errors to the trigger. */
export function IconSelectField({ field, id, value, error, disabled, onChange }: { field: Field; id: string; value: string; error?: string; disabled?: boolean; onChange: (value: string) => void }) {
  return <FormField name={field.name} description={field.help} error={error}><IconSelect label={field.label} id={id} value={value} disabled={disabled} onChange={onChange} items={field.options?.map(option => ({ value: option.value, label: option.label, icon: option.icon ?? null })) ?? []} /></FormField>;
}
export function CheckboxesField({ field, id, value, error, disabled, onChange }: { field: Field; id: string; value: string; error?: string; disabled?: boolean; autoFocus?: boolean; onChange: (value: string) => void }) {
  let selected: string[]; try { selected = parseCheckboxValues(value); } catch { selected = []; }
  return <CheckboxGroup legend={field.label} description={field.help} error={error} id={id} tabIndex={-1} value={selected} onValueChange={next => onChange(JSON.stringify(next))} disabled={disabled}><Stack gap={3}>{field.options?.map((option, i, all) => <Fragment key={option.value}>{option.group && option.group !== all[i - 1]?.group && <span className={s.groupHeading}>{option.group}</span>}<Checkbox value={option.value} label={option.label} /></Fragment>)}</Stack></CheckboxGroup>;
}
export function RowActions({ children }: { children: ReactNode }) { return <Inline gap={2}>{children}</Inline>; }

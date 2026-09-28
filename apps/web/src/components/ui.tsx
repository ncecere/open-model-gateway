import { createContext, useContext, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, type Collection, type OneTimeToken, type Session } from "../lib/api";
import { parseCheckboxValues, validateFields, type Field, type Values } from "../lib/forms";
import { Table as BitopTable, Tr, Td } from "./ui/table/table";
import { Card, CardBody } from "./ui/card/card";
import { PageHeader } from "./ui/page-header/page-header";
import { Badge as BitopBadge } from "./ui/badge/badge";
import { EmptyState } from "./ui/empty-state/empty-state";
import { Button } from "./ui/button/button";
import { Input, NativeSelect, Textarea } from "./ui/input/input";
import { Field as FormField } from "./ui/field/field";
import { Checkbox, CheckboxGroup } from "./ui/checkbox/checkbox";
import { useDashboardNavigation } from "./navigation-link";
export { Button, Input, NativeSelect, Textarea, FormField };
export { StatCard } from "./ui/stat-card/stat-card";

export function ErrorNotice({ error, retry }: { error: unknown; retry?: () => void }) {
  return <div className="notice error" role="alert"><strong>{error instanceof ApiError && error.status === 403 ? "Access denied" : error instanceof ApiError && error.status === 404 ? "Not found" : "Unable to complete request"}</strong><p>{error instanceof Error ? error.message : "An unexpected error occurred."}</p>{retry && <button className="button secondary" onClick={retry}>Try again</button>}</div>;
}
export function Empty({ title = "Nothing here yet", children }: { title?: string; children?: ReactNode }) {
  return <EmptyState title={title} description={children} titleAs="h3" />;
}
export function Badge({ children, tone = "neutral" }: { children: ReactNode; tone?: "neutral" | "good" | "bad" }) { return <BitopBadge tone={tone === "good" ? "success" : tone === "bad" ? "danger" : "neutral"}>{children}</BitopBadge>; }
export function Status({ enabled }: { enabled: boolean }) { return <Badge tone={enabled ? "good" : "neutral"}>{enabled ? "Enabled" : "Disabled"}</Badge>; }
export function DateTime({ value }: { value?: string | null }) { return value ? <time dateTime={value}>{new Date(value).toLocaleString()}</time> : <span className="muted">—</span>; }
export function Id({ value }: { value?: string | null }) { return value ? <code className="identifier" title={value}>{value}</code> : <span className="muted">—</span>; }
const HeadingLevel = createContext<"h1" | "h2">("h1");
export function SectionHeadings({ children }: { children: ReactNode }) { return <HeadingLevel.Provider value="h2">{children}</HeadingLevel.Provider>; }
export function Heading({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) { const titleAs = useContext(HeadingLevel); return <PageHeader title={title} titleAs={titleAs} description={description} actions={actions} className="page-heading" />; }
export function Panel({ title, children }: { title?: string; children: ReactNode }) { return <Card title={title} className="gateway-panel">{title ? children : <CardBody>{children}</CardBody>}</Card>; }
const ApiScopeContext = createContext<string | undefined>(undefined);
export function ApiScopeProvider({ session, children }: { session: Session; children: ReactNode }) {
  // Never reuse an administrator’s cached rows after an identity or role change,
  // including authentication changes made in another browser tab.
  const scope = JSON.stringify([session.user.id, session.user.platform_admin, session.organizations.map((org) => [org.id, org.role, org.membership_role, org.authority_source, org.capabilities]), session.workspaces.map((ws) => [ws.id, ws.role, ws.kind, ws.membership_role, ws.authority_source, ws.capabilities])]);
  return <ApiScopeContext.Provider key={scope} value={scope}>{children}</ApiScopeContext.Provider>;
}
export function useApi<T>(path: string, enabled = true) {
  const scope = useContext(ApiScopeContext);
  return useQuery({ queryKey: scope ? ["api", scope, path] : ["api", path], queryFn: ({ signal }) => api<T>(path, { signal }), enabled, retry: false });
}
export function useCollection<T>(path: string, enabled = true) { return useApi<Collection<T>>(path, enabled); }
export function useChoices<T>(path: string, enabled = true) {
  const scope = useContext(ApiScopeContext);
  return useQuery({ queryKey: scope ? ["api", scope, path, "choices"] : ["api", path, "choices"], enabled, retry: false, queryFn: async ({ signal }) => {
    const rows: T[] = [];
    for (let offset = 0; offset < 20000; offset += 200) {
      const page = await api<Collection<T>>(`${path}${path.includes("?") ? "&" : "?"}limit=200&offset=${offset}`, { signal });
      rows.push(...page.data);
      if (page.data.length < 200) return rows;
    }
    throw new Error("Too many options to load. Ask an administrator to narrow the catalog.");
  } });
}
export type Column<T> = { title: string; render: (row: T) => ReactNode };
export function Table<T>({ rows, columns, rowKey, label }: { rows: T[]; columns: Column<T>[]; rowKey: (row: T) => string; label: string }) {
  return <BitopTable caption={label} columns={columns.map((col) => col.title)} maxHeight="38rem" stickyHeader>{rows.map((row) => <Tr key={rowKey(row)}>{columns.map((col) => <Td key={col.title}>{col.render(row)}</Td>)}</Tr>)}</BitopTable>;
}
export function CollectionTable<T>({ path, columns, rowKey, label, empty, pageSize = 100, searchable = false, statusFilter = false }: { path: string; columns: Column<T>[]; rowKey: (row: T) => string; label: string; empty: string; pageSize?: number; searchable?: boolean; statusFilter?: boolean }) {
  const navigation = useDashboardNavigation();
  const [local, setLocal] = useState<{ offset: number; q?: string; enabled?: "true" | "false" }>({ offset: 0 });
  const state = searchable && navigation ? navigation.search : local;
  const offset = Math.min(100000, Math.max(0, state.offset ?? 0));
  const q = searchable ? state.q ?? "" : "";
  const enabled = statusFilter ? state.enabled : undefined;
  const [draft, setDraft] = useState(q);
  useEffect(() => { setDraft(q); }, [q]);
  const change = (next: { offset: number; q?: string; enabled?: "true" | "false" }) => {
    if (searchable && navigation) navigation.navigate({ ...navigation.search, ...next });
    else setLocal(next);
  };
  const params = new URLSearchParams({ limit: String(pageSize), offset: String(offset) });
  if (q) params.set("q", q);
  if (enabled) params.set("enabled", enabled);
  const query = useCollection<T>(`${path}${path.includes("?") ? "&" : "?"}${params}`);
  return <Panel><div className="table-toolbar"><h2>{label}</h2><button className="button ghost" disabled={query.isFetching} onClick={() => void query.refetch()}>{query.isFetching ? "Refreshing…" : "Refresh"}</button></div>
    {searchable && <form className="collection-filters" onSubmit={event => { event.preventDefault(); change({ q: draft.trim() || undefined, enabled, offset: 0 }); }}>
      <FormField label={`Search ${label.toLowerCase()}`}><Input type="search" name="q" value={draft} maxLength={200} onChange={event => setDraft(event.target.value)} placeholder="Search by name…" autoComplete="off" /></FormField>
      {statusFilter && <FormField label="Status"><NativeSelect name="enabled" value={enabled ?? ""} onChange={event => change({ q: q || undefined, enabled: event.target.value === "true" ? "true" : event.target.value === "false" ? "false" : undefined, offset: 0 })}><option value="">All statuses</option><option value="true">Enabled</option><option value="false">Disabled</option></NativeSelect></FormField>}
      <Button type="submit">Search</Button>{(q || enabled) && <Button variant="secondary" onClick={() => { setDraft(""); change({ q: undefined, enabled: undefined, offset: 0 }); }}>Clear filters</Button>}
    </form>}
    {query.isPending ? <p className="loading" role="status">Loading {label.toLowerCase()}…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : query.data.data.length ? <Table rows={query.data.data} columns={columns} rowKey={rowKey} label={label} /> : <Empty title={q || enabled ? "No matching results" : offset ? "No more results" : `No ${label.toLowerCase()} yet`}>{q || enabled ? "Try a different search or clear the filters. Searches include all authorized results, not just this page." : empty}</Empty>}
    <div className="pagination"><button className="button secondary" disabled={offset === 0 || query.isFetching} onClick={() => change({ q: q || undefined, enabled, offset: Math.max(0, offset - pageSize) })}>Previous</button><span>Page {Math.floor(offset / pageSize) + 1}</span><button className="button secondary" disabled={!query.data || query.data.data.length < pageSize || query.isFetching || offset + pageSize > 100000} onClick={() => change({ q: q || undefined, enabled, offset: offset + pageSize })}>Next</button></div></Panel>;
}

export type Action = { title: string; returnFocus?: HTMLElement; description?: string; submitLabel?: string; fields?: Field[]; danger?: boolean; secretLabel?: string; successNotice?: string; run: (values: Values) => Promise<unknown>; after?: (result: unknown) => void };
const ActionContext = createContext<(action: Action) => void>(() => { throw new Error("Missing action provider"); });
export function useAction() { return useContext(ActionContext); }
export function ActionProvider({ children }: { children: ReactNode }) {
  const [action, setAction] = useState<Action | null>(null);
  const [notice, setNotice] = useState("");
  return <ActionContext.Provider value={(next) => { setNotice(""); setAction({ ...next, returnFocus: next.returnFocus ?? (document.activeElement instanceof HTMLElement ? document.activeElement : undefined) }); }}>{children}{notice && <div role="status" className="toast">{notice}<button aria-label="Dismiss notification" onClick={() => setNotice("")}>×</button></div>}{action && <ActionDialog action={action} onClose={(success) => { setAction(null); if (success) setNotice(action.successNotice ?? "Changes saved."); }} />}</ActionContext.Provider>;
}
function Modal({ title, children, onClose, busy, returnFocus }: { title: string; children: ReactNode; onClose: () => void; busy: boolean; returnFocus?: HTMLElement }) {
  const ref = useRef<HTMLDialogElement>(null);
  const id = useId();
  useEffect(() => {
    const dialog = ref.current;
    dialog?.showModal();
    return () => {
      dialog?.close();
      queueMicrotask(() => { if (returnFocus?.isConnected && !returnFocus.matches(":disabled")) returnFocus.focus(); });
    };
  }, [returnFocus]);
  return <dialog ref={ref} className="dialog" aria-labelledby={id} onCancel={(event) => { event.preventDefault(); if (!busy) onClose(); }}><div className="dialog-heading"><h2 id={id}>{title}</h2><button className="button ghost" aria-label="Close dialog" disabled={busy} onClick={onClose}>×</button></div>{children}</dialog>;
}
function ActionDialog({ action, onClose }: { action: Action; onClose: (success: boolean) => void }) {
  const client = useQueryClient();
  const fields = action.fields ?? [];
  const [values, setValues] = useState<Values>(() => Object.fromEntries(fields.map((field) => [field.name, field.value ?? (field.type === "checkboxes" ? "[]" : "")])));
  const initialValues = useRef(values);
  const [discarding, setDiscarding] = useState(false);
  const dirty = Object.keys(values).some(key => values[key] !== initialValues.current[key]);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const [token, setToken] = useState<string>();
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const id = useId();
  const mounted = useRef(true);
  const inFlight = useRef(false);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => {
    if (!dirty || token) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty, token]);
  const discard = () => { setToken(undefined); setValues({}); onClose(!!token); };
  const close = () => { if (discarding) setDiscarding(false); else if (!token && dirty) setDiscarding(true); else discard(); };
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (inFlight.current) return;
    const validation = validateFields(fields, values);
    setErrors(validation);
    if (Object.keys(validation).length) { document.getElementById(`${id}-${Object.keys(validation)[0]}`)?.focus(); return; }
    inFlight.current = true; setBusy(true); setError(undefined);
    try {
      // Deliberately not useMutation: a one-time token must never enter mutation caches.
      const result = await action.run(Object.fromEntries(Object.entries(values).map(([key, value]) => [key, value.trim()])));
      void client.invalidateQueries({ queryKey: ["api"] });
      void client.invalidateQueries({ queryKey: ["session"] });
      if (!mounted.current) return;
      if (action.secretLabel) {
        const secret = result as OneTimeToken;
        if (typeof secret?.token !== "string" || !secret.token) throw new Error("The gateway did not return a token. Check the resource list before trying again.");
        setValues({}); setToken(secret.token);
      } else { action.after?.(result); onClose(true); }
    } catch (caught) { if (mounted.current) setError(caught); }
    finally { inFlight.current = false; if (mounted.current) setBusy(false); }
  }
  return <Modal title={token ? action.secretLabel! : action.title} busy={busy} onClose={close} returnFocus={action.returnFocus}>{discarding ? <div className="form-stack"><p role="alert">Discard the unsaved values in this form? Closing does not undo any previously submitted changes.</p><div className="dialog-footer"><Button variant="secondary" autoFocus onClick={() => setDiscarding(false)}>Keep editing</Button><Button variant="danger" onClick={discard}>Discard changes</Button></div></div> : token ? <div className="form-stack"><p className="notice">Copy this token now. It will not be shown again. Store it securely; closing this dialog clears it from this page.</p><label htmlFor={`${id}-token`}>One-time token</label><textarea id={`${id}-token`} className="secret" readOnly value={token} rows={4} spellCheck={false} autoComplete="off" autoFocus onFocus={(event) => event.currentTarget.select()} /><div className="actions"><button className="button" onClick={async () => { try { await navigator.clipboard.writeText(token); setCopied(true); setCopyError(false); } catch { setCopyError(true); } }}>{copied ? "Copied" : "Copy token"}</button><button className="button secondary" onClick={close}>I have saved it</button></div><p role="status">{copyError ? "Clipboard unavailable. Select the token and copy it manually." : copied ? "Copied to your clipboard. Clear your clipboard after storing it securely." : ""}</p></div> : <form className="form-stack" aria-busy={busy} noValidate onSubmit={(event) => void submit(event)}>{action.description && <p className={action.danger ? "notice warning" : "muted"}>{action.description}</p>}{fields.filter((field) => !field.visibleWhen || field.visibleWhen(values)).map((field, index) => {
    const fieldId = `${id}-${field.name}`;
    const common = { id: fieldId, name: field.name, required: field.required, disabled: busy, value: values[field.name] ?? "", autoFocus: index === 0, onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>) => setValues((previous) => ({ ...previous, [field.name]: event.target.value })) };
    if (field.type === "checkboxes") return <CheckboxesField key={field.name} field={field} id={fieldId} value={values[field.name] ?? "[]"} error={errors[field.name]} disabled={busy} autoFocus={index === 0} onChange={(value) => setValues((previous) => ({ ...previous, [field.name]: value }))} />;
    return <FormField key={field.name} name={field.name} label={field.label} labelHint={!field.required ? "Optional" : undefined} description={field.help} error={errors[field.name]}>{field.type === "select" ? <NativeSelect {...common}><option value="">Choose…</option>{field.options?.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</NativeSelect> : field.type === "textarea" ? <Textarea {...common} rows={4} autoComplete="off" spellCheck={false} /> : <Input {...common} type={field.type ?? "text"} inputMode={field.inputMode} min={field.min} max={field.max} step={field.type === "number" ? 1 : undefined} maxLength={field.maxLength} placeholder={field.placeholder} autoComplete="off" spellCheck={field.type === "password" ? false : undefined} />}</FormField>;
  })}{error !== undefined && <ErrorNotice error={error} />}<div className="dialog-footer"><Button variant="secondary" disabled={busy} onClick={close}>Cancel</Button><Button type="submit" variant={action.danger ? "danger" : "primary"} loading={busy}>{busy ? "Saving…" : action.submitLabel ?? "Save"}</Button></div></form>}</Modal>;
}
// Gateway composition keeps the vendor primitives untouched. Base UI renders
// this as a group, not a native fieldset. Explicit labels and a focus target also
// work before hydration and when there are no granted options to render.
export function CheckboxesField({ field, id, value, error, disabled, autoFocus, onChange }: { field: Field; id: string; value: string; error?: string; disabled?: boolean; autoFocus?: boolean; onChange: (value: string) => void }) {
  let selected: string[] = [];
  try { selected = parseCheckboxValues(value); } catch { /* Submit reports malformed values instead of accepting them. */ }
  const labelId = `${id}-label`;
  const descriptionId = `${id}-description`;
  const errorId = `${id}-error`;
  return <CheckboxGroup id={id} tabIndex={-1} name={field.name} legend={<span id={labelId}>{field.label}{field.required && " (required)"}</span>} aria-labelledby={labelId} disabled={disabled} value={selected} onValueChange={(next) => onChange(JSON.stringify(next))} aria-invalid={!!error || undefined} aria-describedby={[field.help && descriptionId, error && errorId].filter(Boolean).join(" ") || undefined} description={field.help && <span id={descriptionId}>{field.help}</span>} error={error && <span id={errorId}>{error}</span>} className="gateway-checkbox-group"><div className="gateway-checkbox-options">{field.options?.map((option, index) => <Checkbox key={option.value} value={option.value} label={<span id={`${id}-option-${index}`}>{option.label}</span>} aria-labelledby={`${id}-option-${index}`} disabled={disabled} autoFocus={autoFocus && index === 0} />)}</div></CheckboxGroup>;
}
export function RowActions({ children }: { children: ReactNode }) { return <div className="row-actions">{children}</div>; }

/*
 * API keys: one row per key with its status pill (Active / Disabled / Revoked /
 * Expired; no strikethrough), spending against the key's budget
 * with its reset period, last use, expiry and restrictions; search by name or
 * ID, a status filter, and bulk disable (confirmed; revoked and expired keys
 * can't be selected). A row opens the key's own
 * page (pages/key-detail.tsx). Create key is a dialog: name, who it's for,
 * expiry and budget presets with Custom, the budget's reset period, extra
 * stacked budgets, optional rate limits, and model access. Limits travel in
 * the create request itself (validated and stored with the key in one
 * transaction; a rejection creates nothing). Secrets are shown once and never
 * cached.
 *
 * Members see their own keys; workspace admins every key (the server decides).
 * Disabled keys can be enabled again; revoked keys never.
 */
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, Plus, X } from "lucide-react";
import { api, wsPath, type Grant, type Member, type OneTimeToken, type ServiceAccount } from "../lib/api";
import { permissions, type DashboardSearch } from "../lib/permissions";
import { ownKeyHelp } from "../lib/access";
import { expiryField } from "../lib/forms";
import { formatMicroUsd, type BudgetPeriod, type Policy, type PolicyResponse } from "../lib/governance";
import { keyModelOptions, keyModelRestrictionHelp, keyModelSummary, keyRotationDescription, selectedModelIds } from "../lib/key-models";
import { canDisable, canEnable, canRevoke, canRotate, expiryError, expiryPresets, filterKeys, issuedTo, keyListStatus, keyPill, keyStatus, type KeyRow } from "../lib/keys";
import { draftErrors, draftLimits, hasErrors, limitsBody, limitsOf, limitsSaveError, limitsSummary, mergeErrors, newBudgetKey, periodName, rateRows, rejectionErrors, resetText, stackPeriods, type BudgetDraft, type DraftErrors, type LimitsDraft, type Parent, type RateKey } from "../lib/limits";
import { Alert, Button, ErrorNotice, FormField, Heading, Input, NativeSelect, Stack, Textarea, useAction, useApi, useChoices, type Action } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { NavigationGuard } from "../components/navigation-guard";
import { ActionMenu, type ActionItem } from "../components/templates/action-menu";
import { CopyId } from "../components/templates/copy-id";
import { PeriodBadge, PeriodPills } from "../components/templates/period-pills";
import { PresetChoice, presetChoiceText, type PresetValue } from "../components/templates/preset-choice";
import { StatusPill } from "../components/templates/status-pill";
import { UsageBar } from "../components/templates/usage-bar";
import { FilterToolbar } from "../components/templates/filter-toolbar";
import { useStoredColumns } from "../components/templates/table-view";
import type { Facet } from "../components/ui/filter-bar/filter-bar";
import { copyIdAction } from "../components/people";
import { IconButton } from "../components/ui/button/button";
import { Checkbox, CheckboxGroup } from "../components/ui/checkbox/checkbox";
import { CellText, DataTable, type DataTableColumn } from "../components/ui/data-table/data-table";
import { NARROW_QUERY, useMediaQuery } from "../lib/bitop-utils";
import { Dialog } from "../components/ui/dialog/dialog";
import { Disclosure } from "../components/ui/disclosure/disclosure";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { RadioGroup } from "../components/ui/radio-group/radio-group";
import { Time } from "../components/ui/time/time";
import { toast } from "../components/ui/toast/toast";
import type { Scope } from "./workspace";
import { KeyRiskBadge, useKeySafety } from "./key-safety";
import { findingsByKey } from "../lib/key-safety";
import s from "./shared.module.css";
import k from "./keys.module.css";

const keyPath = (ws: string, id: string) => `${wsPath(ws)}/keys/${encodeURIComponent(id)}`;
/** Confirmed key actions shared by the list and the key page. */
export const keyActions = {
  disable: (ws: string, key: KeyRow): Action => ({ title: `Disable ${key.name}?`, description: "Requests using this key fail until it's enabled again. Its limits and usage are kept, and you can enable it later.", danger: true, submitLabel: "Disable key", successNotice: "Key disabled.", run: (_, signal) => api(keyPath(ws, key.id), { method: "PATCH", body: { disabled: true }, signal }) }),
  enable: (ws: string, key: KeyRow): Action => ({ title: `Enable ${key.name}?`, description: "Requests using this key work again straight away.", submitLabel: "Enable key", successNotice: "Key enabled.", run: (_, signal) => api(keyPath(ws, key.id), { method: "PATCH", body: { disabled: false }, signal }) }),
  revoke: (ws: string, key: KeyRow): Action => ({ title: `Revoke ${key.name}?`, description: "Requests using this key fail immediately. This can't be undone: a revoked key can never be enabled again.", danger: true, submitLabel: "Revoke key", successNotice: "Key revoked.", run: (_, signal) => api(keyPath(ws, key.id), { method: "DELETE", signal }) }),
  rotate: (ws: string, key: KeyRow): Action => ({ title: `Rotate ${key.name}?`, description: keyRotationDescription, danger: true, fields: [expiryField], submitLabel: "Rotate key", secretLabel: "Save your replacement key", secretNoun: "key", run: (v, signal) => api(`${keyPath(ws, key.id)}/rotate`, { method: "POST", body: { expires_in_days: Number(v.expires_in_days) }, signal }) }),
};
/** Disables keys one at a time; a failure names the keys that weren't disabled. */
export function bulkDisableAction(ws: string, keys: KeyRow[]): Action {
  return { title: `Disable ${keys.length} key${keys.length === 1 ? "" : "s"}?`, description: `${keys.map(x => x.name).join(", ")}. Requests using ${keys.length === 1 ? "it" : "them"} fail until enabled again. Limits and usage are kept.`, danger: true, submitLabel: `Disable ${keys.length === 1 ? "key" : `${keys.length} keys`}`, successNotice: `${keys.length === 1 ? "Key" : `${keys.length} keys`} disabled.`,
    run: async (_, signal) => {
      const failed: string[] = [];
      for (const key of keys) { signal.throwIfAborted(); try { await api(keyPath(ws, key.id), { method: "PATCH", body: { disabled: true }, signal }); } catch (error) { if (signal.aborted) throw error; failed.push(`${key.name} (${(error as Error).message})`); } }
      if (failed.length) throw new Error(`Disabled ${keys.length - failed.length} of ${keys.length}. Not disabled: ${failed.join("; ")}.`);
    } };
}
export function keyMenu(session: Scope["session"], workspace: Scope["workspace"], key: KeyRow, ask: (a: Action) => void, search?: DashboardSearch): ActionItem[] {
  return [
    { label: "Open key", render: <ResourceLink search={{ ...search, page: "key-detail", ws: workspace.id, record: key.id }} /> },
    copyIdAction(key.id, "Copy key ID"),
    { label: "Rotate key…", hidden: !canRotate(session, workspace, key), onSelect: () => ask(keyActions.rotate(workspace.id, key)) },
    { label: "Enable key…", hidden: !canEnable(session, workspace, key), onSelect: () => ask(keyActions.enable(workspace.id, key)) },
    { label: "Disable key…", danger: true, hidden: !canDisable(session, workspace, key), onSelect: () => ask(keyActions.disable(workspace.id, key)) },
    { label: "Revoke key…", danger: true, hidden: !canRevoke(session, workspace, key), onSelect: () => ask(keyActions.revoke(workspace.id, key)) },
  ];
}
/** Only keys that can still be used again can be picked for bulk actions; revoked and expired keys are final. */
const selectableKey = (key: KeyRow) => { const st = keyStatus(key); return st === "active" || st === "disabled"; };
const keyStatuses = [{ value: "active", label: "Active" }, { value: "disabled", label: "Disabled" }, { value: "expired", label: "Expired" }, { value: "revoked", label: "Revoked" }];
const statusFacet: Facet = { id: "status", label: "Status", type: "toggle", allLabel: "All", options: keyStatuses };
/** Keys with key-safety findings (docs/key-safety.md). */
const riskFacet: Facet = { id: "risk", label: "Safety", type: "toggle", allLabel: "All", options: [{ value: "attention", label: "Needs attention" }] };

export function Keys({ session, workspace }: Scope) {
  const ask = useAction(), nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? { page: "keys", ws: workspace.id }, p = permissions(session, workspace);
  const go = (patch: Partial<DashboardSearch>) => nav?.navigate({ ...search, ...patch });
  const keys = useChoices<KeyRow>(`${wsPath(workspace.id)}/keys`);
  const accounts = useChoices<ServiceAccount>(`${wsPath(workspace.id)}/service-accounts`, p.manageServiceAccounts), grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`), options = keyModelOptions(grants.data ?? []);
  const [creating, setCreating] = useState(false), [selected, setSelected] = useState<string[]>([]), createTrigger = useRef<HTMLButtonElement>(null);
  // The dialog unmounts on close, so focus is put back on its trigger explicitly (Escape, Cancel, "I have saved it").
  const closeCreate = (navigated = false) => { setCreating(false); if (!navigated) requestAnimationFrame(() => createTrigger.current?.focus()); };
  // One failure, one error: the list, models and service accounts load together.
  const failure = keys.error ?? grants.error ?? accounts.error, retryAll = () => { void keys.refetch(); void grants.refetch(); if (p.manageServiceAccounts) void accounts.refetch(); };
  // Active by default; "All" is explicit (status=all), so revoked keys don't fill the first page.
  const status = keyListStatus(search.status);
  // Workspace admins see every key: name the people who hold them (members see only their own keys).
  const members = useChoices<Member>(`${wsPath(workspace.id)}/members`, workspace.kind !== "personal" && workspace.capabilities.view_all_activity);
  const safety = useKeySafety(workspace), risks = useMemo(() => findingsByKey(safety.data), [safety.data]);
  const rows = useMemo(() => { const list = filterKeys(keys.data ?? [], status, search.q); return search.risk === "attention" ? list.filter(x => risks.has(x.id)) : list; }, [keys.data, status, search.q, search.risk, risks]);
  const activeAccounts = accounts.data?.filter(a => !a.disabled_at) ?? [];
  const canIssue = p.createUserKey || p.manageServiceAccounts && activeAccounts.length > 0, noModels = grants.isSuccess && options.length === 0;
  const ready = grants.isSuccess && !grants.isFetching && (!p.manageServiceAccounts || accounts.isSuccess);
  const rowSearch = { status: search.status, q: search.q, risk: search.risk }, narrow = useMediaQuery(NARROW_QUERY);
  // Selection is ours (not DataTable's): revoked and expired keys can't be picked, and with no
  // selectable rows there is no checkbox column and no "0 of 0 selected" footer (review #17).
  const selectableIds = rows.filter(selectableKey).map(x => x.id), picked = selected.filter(id => selectableIds.includes(id));
  const allPicked = selectableIds.length > 0 && picked.length === selectableIds.length;
  const toggle = (id: string, on: boolean) => setSelected(on ? [...selected.filter(x => x !== id), id] : selected.filter(x => x !== id));
  const selectColumn: DataTableColumn<KeyRow> = { id: "select", label: "Select", hideable: false, width: "2.5rem",
    header: <Checkbox label={<span className="sr-only">Select all keys that can be disabled</span>} checked={allPicked} indeterminate={picked.length > 0 && !allPicked} onCheckedChange={on => setSelected(on ? selectableIds : [])} />,
    cell: key => selectableKey(key) ? <Checkbox label={<span className="sr-only">Select {key.name}</span>} checked={picked.includes(key.id)} onCheckedChange={on => toggle(key.id, on)} /> : null };
  const columns: DataTableColumn<KeyRow>[] = [
    // On a phone rows become cards (review #27): the name leads the card, the checkbox comes after the details.
    ...(selectableIds.length && !narrow ? [selectColumn] : []),
    { id: "name", header: "Key", rowHeader: true, hideable: false, cell: key => { const st = keyStatus(key); return <span className={k.name} data-status={st}><ResourceLink search={{ ...rowSearch, page: "key-detail", ws: workspace.id, record: key.id }}>{key.name}</ResourceLink><span className={s.secondary}>{issuedTo(key, session, accounts.data, members.data)}</span></span>; } },
    { id: "status", header: "Status", cell: key => <span className={s.badges}><StatusPill {...keyPill(keyStatus(key))} explain /><KeyRiskBadge row={risks.get(key.id)} /></span> },
    { id: "usage", header: "Spent", label: "Spent (includes on hold)", cell: key => key.usage ? <span className={k.usage}><UsageBar label={`${key.name} budget`} size="sm" used={key.usage.used_microusd} limit={key.usage.limit_microusd} />{key.usage.limit_microusd !== null && <PeriodBadge period={key.usage.period} />}</span> : <span className={s.muted}>Unknown</span> },
    { id: "restrictions", header: "Restrictions", defaultHiddenNarrow: true, cell: key => { const m = keyModelSummary(key, options); return <CellText primary={<span title={m.models.join(", ") || undefined}>{m.label}</span>} secondary={key.usage?.limit_microusd ? "Budget limit" : "No key budget"} />; } },
    { id: "last_used", header: "Last used", cell: key => key.last_used_at ? <Time value={key.last_used_at} format="relative" /> : <span className={s.muted}>{key.last_used_at === null ? "Never" : "Unknown"}</span> },
    { id: "expires", header: "Expires", cell: key => <Time value={key.expires_at} format="date" fallback="—" /> },
    { id: "created", header: "Created", defaultHidden: true, cell: key => <Time value={key.created_at} format="date" fallback="—" /> },
    { id: "id", header: "Key ID", defaultHidden: true, cell: key => <CopyId value={key.id} label="key ID" /> },
    ...(selectableIds.length && narrow ? [selectColumn] : []),
  ];
  const cols = useStoredColumns(columns, "omg.enterprise.columns.keys");
  return <Stack gap={6} className={s.page}>
    <Heading title="API keys" description={workspace.capabilities.view_all_activity && workspace.kind !== "personal" ? "Keys for calling this workspace's models from code." : "Your keys for calling this workspace's models from code."}
      actions={<Button ref={createTrigger} disabled={!ready || !canIssue || noModels || !!failure} onClick={() => setCreating(true)}>Create key</Button>} />
    {noModels && <Alert tone="info" title="Add a model first">This workspace has no models yet, so a new key couldn't call anything. {p.manageGrants ? <ResourceLink search={{ page: "grants", ws: workspace.id }}>Add models</ResourceLink> : "Ask a workspace admin to add models."}</Alert>}
    {!p.createUserKey && <Alert tone="info">{ownKeyHelp(workspace)}</Alert>}
    {grants.isFetching && <p role="status" className={s.note}>Loading this workspace&apos;s models…</p>}
    {failure ? <ErrorNotice error={failure} retry={retryAll} /> : <div className={s.list}>
    {/* No keys at all: just the empty state (nothing to search or filter yet). */}
    {keys.data?.length !== 0 && <FilterToolbar search={{ label: "Search keys", placeholder: "Name or key ID", value: search.q ?? "", onChange: next => go({ q: next || undefined }), debounceMs: 250 }}
      facets={[statusFacet, riskFacet]} values={{ status: status ? [status] : [], risk: search.risk ? [search.risk] : [] }} onChange={next => { const picked = Array.isArray(next.status) ? next.status[0] : undefined, risk = Array.isArray(next.risk) && next.risk[0] === "attention" ? "attention" as const : undefined; go({ status: picked === "active" ? undefined : picked ?? "all", risk }); }} end={cols.menu} />}
    {picked.length > 0 && (() => { const eligible = rows.filter(x => picked.includes(x.id) && canDisable(session, workspace, x)), clear = () => setSelected([]); return <div className={k.bulk} role="group" aria-label="Bulk actions">
      <span className={k.bulkCount}>{picked.length} selected</span>
      <Button size="sm" variant="danger" disabled={!eligible.length} title={eligible.length ? undefined : "Only active keys you manage can be disabled"} onClick={() => ask({ ...bulkDisableAction(workspace.id, eligible), after: clear })}>{eligible.length ? `Disable ${eligible.length === 1 ? "key" : `${eligible.length} keys`}…` : "Nothing to disable"}</Button>
      <Button size="sm" variant="ghost" className={k.bulkClear} onClick={clear}>Clear selection</Button>
    </div>; })()}
    <DataTable<KeyRow> caption="API keys" stack columns={columns} data={rows} getRowId={x => x.id} rowLabel={x => x.name} hiddenColumns={cols.hidden} onHiddenColumnsChange={cols.setHidden} manual
      rowActions={key => <ActionMenu label={`Actions for ${key.name}`} actions={keyMenu(session, workspace, key, ask, rowSearch)} />}
      loading={keys.isFetching}
      empty={<EmptyState size="compact" icon={<KeyRound />} title={keys.data?.length ? status === "active" && !search.q && !search.risk ? "No active keys" : search.risk && !search.q ? "No keys need attention" : "No keys match these filters" : "No API keys yet"} description={keys.data?.length ? status === "active" && !search.q && !search.risk ? "Disabled, expired and revoked keys are hidden." : undefined : "Create a key to call this workspace's models from your code."}
        action={keys.data?.length ? status === "active" && !search.q && !search.risk ? <Button size="sm" variant="secondary" onClick={() => go({ status: "all" })}>Show all keys ({keys.data.length})</Button> : <Button size="sm" variant="ghost" onClick={() => go({ status: undefined, q: undefined, risk: undefined })}>Clear filters</Button> : undefined} />} />
    </div>}
    {creating && <CreateKeyDialog session={session} workspace={workspace} options={options} accounts={activeAccounts} onClose={closeCreate} />}
  </Stack>;
}

const limitPresets = ["10", "50", "100"];
type Owner = "me" | "service";
/** Create key: name, owner, expiry and budget presets (+ Custom), reset period, more stacked budgets, model access. */
export function CreateKeyDialog({ session, workspace, options, accounts, onClose }: Scope & { options: { value: string; label: string }[]; accounts: ServiceAccount[]; /** `navigated`: the dialog opened another page, so focus goes to its heading, not back to the trigger. */ onClose: (navigated?: boolean) => void }) {
  const client = useQueryClient(), nav = useDashboardNavigation(), formId = useId(), p = permissions(session, workspace);
  const policy = useApi<PolicyResponse>(`${wsPath(workspace.id)}/policy`);
  const [name, setName] = useState(""), [owner, setOwner] = useState<Owner>(p.createUserKey ? "me" : "service"), [account, setAccount] = useState(accounts[0]?.id ?? "");
  const [expiry, setExpiry] = useState<PresetValue>({ kind: "preset", preset: "30" }), [limit, setLimit] = useState<PresetValue>({ kind: "preset", preset: "" }), [period, setPeriod] = useState<BudgetPeriod>("month");
  const [extra, setExtra] = useState<BudgetDraft[]>([]), [modelMode, setModelMode] = useState<"inherit" | "selected">("inherit"), [models, setModels] = useState<string[]>([]);
  const [rates, setRates] = useState<Record<RateKey, string>>({ requests_per_minute: "", tokens_per_minute: "", concurrent_requests: "", concurrent_jobs: "" }), [rejected, setRejected] = useState<DraftErrors>();
  const [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [result, setResult] = useState<{ id: string; token: string; policy: Policy | null }>(), [copied, setCopied] = useState(false);
  const limitText = presetChoiceText(limit).trim(), maxDays = owner === "service" ? 365 : session.installation.key_max_lifetime_days ?? 365;
  const budgets: BudgetDraft[] = [...(limitText ? [{ key: "primary", period, amount: limitText }] : []), ...extra];
  const draft: LimitsDraft = { ...rates, budgets };
  const parents: Parent[] = policy.data?.provenance ? [{ label: "platform", limits: limitsOf(policy.data.provenance.platform) }, { label: "workspace", limits: limitsOf(policy.data.provenance.local) }] : [];
  // Client checks first; a server rejection (named reason) is shown on its field until that part of the form changes.
  const budgetErrors = mergeErrors(draftErrors(draft, "tighten", parents), rejected);
  const anyLimit = budgets.length > 0 || rateRows.some(r => rates[r.key].trim());
  const errors = {
    name: !name.trim() ? "Enter a name." : name.trim().length > 120 ? "Use at most 120 characters." : undefined,
    // Personal keys follow the installation maximum (Admin › Settings › General); service keys keep 365 days.
    expiry: (expiry.kind === "custom" ? expiryError(expiry.text) : undefined) ?? (Number(presetChoiceText(expiry).trim()) > maxDays ? `Personal keys can last at most ${maxDays} days here.` : undefined),
    account: owner === "service" && !account ? "Choose a service account." : undefined,
    models: (() => { const r = selectedModelIds(modelMode, models, options); return "error" in r ? r.error : undefined; })(),
  };
  const invalid = Object.values(errors).some(Boolean) || hasErrors(budgetErrors);
  const dirty = !result && (!!name || limitText !== "" || extra.length > 0 || rateRows.some(r => rates[r.key].trim()) || modelMode !== "inherit" || expiry.kind === "custom" || expiry.preset !== "30");
  const shown = (message?: string) => submitted ? message : undefined;
  const setRate = (key: RateKey, value: string) => { setRates({ ...rates, [key]: value }); setRejected(undefined); };
  const setExtraBudgets = (next: BudgetDraft[]) => { setExtra(next); setRejected(undefined); };
  async function submit() {
    setSubmitted(true);
    if (invalid || busy) return;
    setBusy(true); setError(undefined); setRejected(undefined);
    const ids = selectedModelIds(modelMode, models, options) as { model_ids: string[] | null };
    try {
      const days = Number(presetChoiceText(expiry).trim());
      // Limits are validated and stored with the key in one transaction: any rejection creates nothing. Nothing set = inherit.
      const limits = anyLimit ? limitsBody(draftLimits(draft)) : {};
      const created = await api<OneTimeToken & { policy?: Policy | null }>(`${wsPath(workspace.id)}/keys`, { method: "POST", body: { name: name.trim(), expires_in_days: days, ...ids, ...(owner === "service" ? { service_account_id: account } : {}), ...limits } });
      if (typeof created?.token !== "string" || !created.token) throw new Error("No token was returned. Check the list before trying again.");
      void client.invalidateQueries({ queryKey: ["api"] });
      setResult({ id: created.id, token: created.token, policy: created.policy ?? null });
    } catch (caught) {
      const placed = rejectionErrors(caught, draft);
      if (placed) { setRejected(placed); setError(new Error("The key wasn't created: a limit isn't allowed here. See the highlighted field.")); }
      else setError(limitsSaveError(caught));
    }
    finally { setBusy(false); }
  }
  const close = (navigated = false) => { if (!busy) { setResult(undefined); onClose(navigated); } };
  if (result) return <Dialog open title="Save your API key" description="Use it in your code to call models in this workspace. It can't sign in to this dashboard." onOpenChange={open => { if (!open) close(); }}
    footer={<><Button variant="secondary" onClick={() => { const id = result.id; close(true); nav?.navigate({ page: "key-detail", ws: workspace.id, record: id }); }}>Open key page</Button><Button onClick={() => close()}>I have saved it</Button></>}>
    <Stack gap={4}>
      <Alert tone="warning" title="Shown only once">Store this key securely. Closing this dialog clears it from the page.</Alert>
      <p className={s.note}>{result.policy ? `Key limits: ${limitsSummary(limitsOf(result.policy))}. Workspace and platform limits also apply.` : "No key limits: workspace and platform limits apply."}</p>
      <FormField label="API key"><Textarea readOnly value={result.token} rows={3} spellCheck={false} autoComplete="off" autoFocus onFocus={event => event.currentTarget.select()} /></FormField>
      <Button variant="secondary" onClick={async () => { try { await navigator.clipboard.writeText(result.token); setCopied(true); toast.success("Copied", "Clear your clipboard after storing the key."); } catch { toast.error("Clipboard unavailable", "Select and copy the key manually."); } }}>{copied ? "Copied" : "Copy key"}</Button>
    </Stack>
  </Dialog>;
  const used = new Set(budgets.map(b => b.period));
  return <><Dialog open size="lg" title="Create API key" description={`Use this key in your code to call models in ${workspace.name}. It can't sign in to this dashboard.`} hideClose={busy} onOpenChange={open => { if (!open) close(); }}
    footer={<><Button variant="secondary" disabled={busy} onClick={() => close()}>Cancel</Button><Button type="submit" form={formId} loading={busy}>Create key</Button></>}>
    <form id={formId} noValidate aria-busy={busy} data-dirty={dirty ? "true" : undefined} onSubmit={event => { event.preventDefault(); void submit(); }}>
      <Stack gap={5}>
        <FormField label="Name" error={shown(errors.name)}><Input value={name} maxLength={120} autoFocus autoComplete="off" disabled={busy} onChange={event => setName(event.target.value)} /></FormField>
        {p.manageServiceAccounts && accounts.length > 0 && <RadioGroup legend="Who is this key for?" orientation="horizontal" value={owner} disabled={busy} onValueChange={v => setOwner(v as Owner)} options={[...(p.createUserKey ? [{ value: "me" as Owner, label: "Me" }] : []), { value: "service" as Owner, label: "A service account" }]} />}
        {owner === "service" && <FormField label="Service account" error={shown(errors.account)}><NativeSelect value={account} disabled={busy} onChange={event => setAccount(event.target.value)}><option value="">Choose…</option>{accounts.map(a => <option key={a.id} value={a.id}>{a.name}</option>)}</NativeSelect></FormField>}
        <PresetChoice legend="Expires in" description="Can't be changed later." presets={expiryPresets.filter(d => d <= maxDays).map(d => ({ value: String(d), label: d === 365 ? "1 year" : `${d} days` }))} value={expiry} onChange={setExpiry} disabled={busy} custom={{ label: "Custom expiry (days)", inputMode: "numeric", maxLength: 3, placeholder: "1–365", error: shown(errors.expiry) }} />
        <PresetChoice legend="Spending limit" description="Includes what's on hold." presets={[{ value: "", label: "No limit" }, ...limitPresets.map(v => ({ value: v, label: `$${v}` }))]} value={limit} onChange={next => { setLimit(next); setRejected(undefined); }} disabled={busy} custom={{ label: "Custom limit (USD)", prefix: "$", inputMode: "decimal", maxLength: 32, error: shown(budgetErrors.budgets.primary) }} error={limit.kind === "preset" ? shown(budgetErrors.budgets.primary) : undefined} />
        {/* The reset period only matters once there is a limit (progressive disclosure, no disabled control with a hint). */}
        {limitText && <PeriodPills label="Reset period" value={period} onChange={next => { setPeriod(next); setRejected(undefined); }} periods={stackPeriods.filter(x => x === period || !extra.some(b => b.period === x))} showReset disabled={busy} />}
        {extra.map(b => <div key={b.key} className={k.extraBudget}>
          <FormField label="Also limit"><NativeSelect value={b.period} disabled={busy} onChange={event => setExtraBudgets(extra.map(x => x.key === b.key ? { ...x, period: event.target.value as BudgetPeriod } : x))}>{stackPeriods.filter(x => x === b.period || !used.has(x)).map(x => <option key={x} value={x}>{periodName[x]} budget</option>)}</NativeSelect></FormField>
          <FormField label="Amount (USD)" description={resetText(b.period)} error={shown(budgetErrors.budgets[b.key])}><Input value={b.amount} inputMode="decimal" maxLength={32} autoComplete="off" disabled={busy} startIcon={<span aria-hidden>$</span>} onChange={event => setExtraBudgets(extra.map(x => x.key === b.key ? { ...x, amount: event.target.value } : x))} /></FormField>
          <IconButton icon={<X aria-hidden />} label={`Remove ${periodName[b.period].toLowerCase()} budget`} disabled={busy} onClick={() => setExtraBudgets(extra.filter(x => x.key !== b.key))} />
        </div>)}
        {limitText && used.size < stackPeriods.length && <span><Button size="sm" variant="ghost" disabled={busy} onClick={() => { const next = (["day", "week", "month", "lifetime"] as BudgetPeriod[]).find(x => !used.has(x)); if (next) setExtraBudgets([...extra, { key: newBudgetKey(), period: next, amount: "" }]); }}><Plus aria-hidden /> Add a budget for another period</Button></span>}
        <Disclosure title="Rate limits" summary={rateRows.some(r => rates[r.key].trim()) ? rateRows.filter(r => rates[r.key].trim()).map(r => `${rates[r.key].trim()} ${r.key === "concurrent_jobs" ? "jobs at once" : r.unit === "at once" ? "at once" : r.key === "requests_per_minute" ? "RPM" : "TPM"}`).join(" · ") : "Optional · inherited when blank"} defaultOpen={false}>
          <div className={k.rates}>{rateRows.map(r => { const cap = parents.map(x => x.limits[r.key]).filter((v): v is number => v !== null).sort((a, b) => a - b)[0]; return <FormField key={r.key} label={r.label} description={cap !== undefined ? `At most ${cap.toLocaleString("en-US")} (inherited)` : r.description} error={shown(budgetErrors.rates[r.key])}>
            <Input value={rates[r.key]} inputMode="numeric" maxLength={32} autoComplete="off" disabled={busy} placeholder="No key limit" onChange={event => setRate(r.key, event.target.value)} />
          </FormField>; })}</div>
        </Disclosure>
        {submitted && budgetErrors.form.map(e => <p key={e} className={s.dangerText}>{e}</p>)}
        <RadioGroup legend="Models" description={keyModelRestrictionHelp} value={modelMode} disabled={busy} onValueChange={v => setModelMode(v as "inherit" | "selected")} options={[{ value: "inherit", label: "All models in this workspace", description: "Updates as models are added or removed." }, { value: "selected", label: "Only selected models" }]} />
        {modelMode === "selected" && <CheckboxGroup legend="Selected models" error={shown(errors.models)} value={models} onValueChange={setModels} disabled={busy}>{options.map(o => <Checkbox key={o.value} value={o.value} label={o.label} />)}</CheckboxGroup>}
        {policy.data && parents.some(x => x.limits.budgets.length) && <p className={s.note}>Inherited budgets: {parents.flatMap(x => x.limits.budgets.map(b => `${x.label} ${formatMicroUsd(b.amount_microusd)} ${periodName[b.period].toLowerCase()}`)).join(" · ")}. A key budget can't be higher than the inherited one for the same period.</p>}
        {error !== undefined && <ErrorNotice error={error} />}
      </Stack>
    </form>
  </Dialog><NavigationGuard dirty={dirty && !busy} /></>;
}

/*
 * Limits of one scope as one Grounded-style table with stacked budgets
 * (ux-api-contract "Stacked budgets"): a row per rate limit, then one row per
 * budget period (Daily / Weekly / Monthly / Lifetime) with its plain-language
 * reset rule. Columns: the inherited value, this scope's value (inline input)
 * and the effective result. Budget rows are added and removed in place.
 *
 * `LimitsTable` is the table alone (also used by Admin › Limits for the
 * installation ceiling and type defaults). `ScopeLimits` loads a policy and
 * saves it, in three modes:
 *
 * - "replacement": the platform override of one Team/Project. Choose the live
 *   type defaults or an override (prefilled with the defaults; blank means no
 *   limit). Choosing the defaults again deletes the override after confirming.
 * - "local" / "key": tighten-only restrictions. Blank inherits; a rate can't
 *   exceed a parent rate, a budget can't exceed a parent budget for the same
 *   period, and saved caps can only be lowered (lib/limits.ts mirrors the
 *   server; its 400/403 replies are explained in plain words).
 *
 * Money is exact integer micro-USD (BigInt). Provenance: layout adapted from
 * Grounded web/src/pages/admin/limits/{platform,fields}.tsx (read-only reference).
 */
import { useId, useState } from "react";
import { Plus, X } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { api, platformWorkspacePath, type WorkspaceKind } from "../lib/api";
import { dollarsToMicroUsd, formatMicroUsd, type BudgetPeriod, type BudgetWindow, type PolicyResponse } from "../lib/governance";
import { budgetFor, composeLimits, draftErrors, draftLimits, draftLimitsValid, draftOf, hasErrors, limitsBody, limitsOf, limitsSaveError, limitsSummary, lockedBudget, mergeErrors, newBudgetKey, nextPeriod, noLimits, periodName, rateRows, rateText, rejectionErrors, resetText, sameDraft, stackPeriods, type DraftErrors, type InvalidRows, type Limits, type LimitsDraft, type Parent, type RateKey } from "../lib/limits";
import { resetsAt } from "../lib/usage";
import { kindLabels } from "../lib/people";
import { Button, ErrorNotice, NativeSelect, Stack, useAction, useApi } from "./ui";
import { NavigationGuard } from "./navigation-guard";
import { UsageBar } from "./templates/usage-bar";
import { Card } from "./ui/card/card";
import { Field } from "./ui/field/field";
import { Input } from "./ui/input/input";
import { IconButton } from "./ui/button/button";
import { RadioGroup } from "./ui/radio-group/radio-group";
import { Dialog } from "./ui/dialog/dialog";
import { StickySaveBar } from "./templates/sticky-save-bar";
import { Table, Td, Tr, type TableColumn } from "./ui/table/table";
import { toast } from "./ui/toast/toast";
import s from "../pages/shared.module.css";
import l from "./scope-limits.module.css";

export type LimitsMode = "replacement" | "local" | "key";
const amountText = (amount: string | null) => amount === null ? "No budget" : formatMicroUsd(amount);

export type LimitsTableProps = {
  caption: string;
  /** This scope's column header, e.g. "This workspace". */
  scopeLabel: string;
  draft: LimitsDraft;
  onChange?: (draft: LimitsDraft) => void;
  /** Editable inputs (otherwise the saved values are shown). */
  editing: boolean;
  busy?: boolean;
  errors?: DraftErrors;
  /** Inherited column: header and the composed parent limits. Omit for scopes without a parent. */
  inherited?: { label: string; limits: Limits | undefined };
  /** Effective column: the composed result of the draft's valid rows; rows in `invalid` show "—" until fixed. Omit to hide. */
  effective?: { limits: Limits | undefined; invalid?: InvalidRows };
  mode?: "free" | "tighten";
  stored?: Limits;
  /** Shown instead of a value when this scope's cell is read-only and empty. */
  emptyText?: string;
  /** Placeholder of an empty rate input (default: inherited value or "No limit"). */
  placeholder?: (key: RateKey) => string;
};

/** Rate rows plus stacked budget rows (period + amount) with inherited and effective columns. */
export function LimitsTable({ caption, scopeLabel, draft, onChange, editing, busy, errors, inherited, effective, mode = "free", stored = noLimits, emptyText = "Not set", placeholder }: LimitsTableProps) {
  const set = (patch: Partial<LimitsDraft>) => onChange?.({ ...draft, ...patch });
  const setBudget = (key: string, patch: Partial<LimitsDraft["budgets"][number]>) => set({ budgets: draft.budgets.map(b => b.key === key ? { ...b, ...patch } : b) });
  const columns: TableColumn[] = ["Limit", ...(inherited ? [{ label: inherited.label, width: "10rem" }] : []), { label: scopeLabel, width: "15rem" }, ...(effective ? [{ label: "Effective", width: "10rem" }] : [])];
  const parentRate = (key: RateKey) => inherited ? inherited.limits ? rateText(inherited.limits[key]) : "—" : undefined;
  // Budget rows: this scope's rows, plus inherited periods this scope doesn't set (read-only, "Add" offers a cap).
  const inheritedOnly = stackPeriods.filter(p => budgetFor(inherited?.limits, p) !== null && !draft.budgets.some(b => b.period === p));
  const rows = [...draft.budgets.map(b => ({ period: b.period, row: b })), ...inheritedOnly.map(period => ({ period, row: undefined }))].sort((a, b) => stackPeriods.indexOf(a.period) - stackPeriods.indexOf(b.period));
  const canAdd = editing && !!nextPeriod(draft);
  const add = (period?: BudgetPeriod) => { const p = period ?? nextPeriod(draft); if (p) set({ budgets: [...draft.budgets, { key: newBudgetKey(), period: p, amount: "" }] }); };
  return <>
    {errors && errors.form.length > 0 && <ul className={l.formErrors} role="alert">{errors.form.map(e => <li key={e}>{e}</li>)}</ul>}
    <Table caption={caption} stack columns={columns}>
      {rateRows.map(r => <Tr key={r.key}>
        <Td><span className={s.primary}>{r.label}</span><span className={s.secondary}>{r.description}</span></Td>
        {inherited && <Td>{parentRate(r.key)}</Td>}
        <Td>{editing ? <Field label={`${r.label} · ${scopeLabel}`} hideLabel error={errors?.rates[r.key]} className={l.amountField}><span className={l.amount}><GroupedIntegerInput className={l.amountInput} placeholder={placeholder?.(r.key) ?? (inherited?.limits?.[r.key] != null ? `Inherited: ${inherited.limits[r.key]!.toLocaleString("en-US")}` : "No limit")} value={draft[r.key]} disabled={busy} onChange={value => set({ [r.key]: value })} /><span aria-hidden className={l.unit}>{r.unit}</span></span></Field> : draft[r.key].trim() ? Number(draft[r.key]).toLocaleString("en-US") : <span className={s.muted}>{emptyText}</span>}</Td>
        {effective && <Td>{effective.limits && !effective.invalid?.rates.includes(r.key) ? rateText(effective.limits[r.key]) : "—"}</Td>}
      </Tr>)}
      {rows.map(({ period, row }) => {
        const parent = budgetFor(inherited?.limits, period), locked = !!row && lockedBudget(mode, stored, row);
        const used = new Set(draft.budgets.filter(b => b !== row).map(b => b.period));
        const head = row && editing
          ? <div className={l.budgetHead}><Field label={`Budget period · ${scopeLabel}`} hideLabel className={l.periodField}><NativeSelect size="sm" value={row.period} disabled={busy || locked} onChange={event => setBudget(row.key, { period: event.target.value as BudgetPeriod })}>{stackPeriods.filter(p => p === row.period || !used.has(p)).map(p => <option key={p} value={p}>{periodName[p]} budget</option>)}</NativeSelect></Field><span className={s.secondary}>{resetText(row.period)}</span></div>
          : <><span className={s.primary}>{periodName[period]} budget</span><span className={s.secondary}>{resetText(period)}</span></>;
        const cell = row && editing
          ? <Field label={`${periodName[row.period]} budget (USD) · ${scopeLabel}`} hideLabel error={errors?.budgets[row.key]} className={l.amountField}><span className={l.amount}><Input size="sm" className={l.amountInput} inputMode="decimal" maxLength={32} autoComplete="off" placeholder={parent ? `At most ${formatMicroUsd(parent)}` : "Amount in USD"} startIcon={<span aria-hidden>$</span>} value={row.amount} disabled={busy} onChange={event => setBudget(row.key, { amount: event.target.value })} /><IconButton size="sm" icon={<X aria-hidden />} label={locked ? `Saved ${periodName[row.period].toLowerCase()} budget can only be lowered` : `Remove ${periodName[row.period].toLowerCase()} budget`} disabled={busy || locked} onClick={() => set({ budgets: draft.budgets.filter(b => b.key !== row.key) })} /></span>{locked && <span className={l.cellNote}>Saved budgets can only be lowered.</span>}</Field>
          : row ? amountText(row.amount.trim() ? safeMicro(row.amount) : null)
          : <>{editing ? <Button size="sm" variant="ghost" disabled={busy} onClick={() => add(period)}><Plus aria-hidden /> Add {periodName[period].toLowerCase()} cap</Button> : <span className={s.muted}>{emptyText}</span>}</>;
        return <Tr key={row?.key ?? `inherited-${period}`}>
          <Td>{head}</Td>
          {inherited && <Td>{inherited.limits ? amountText(parent) : "—"}</Td>}
          <Td>{cell}</Td>
          {effective && <Td>{effective.limits && !effective.invalid?.periods.includes(period) ? amountText(budgetFor(effective.limits, period)) : "—"}</Td>}
        </Tr>;
      })}
      {rows.length === 0 && <Tr><Td><span className={s.primary}>Budget</span><span className={s.secondary}>Add a daily, weekly, monthly or lifetime budget. Each one is enforced on its own.</span></Td>{inherited && <Td>No budget</Td>}<Td><span className={s.muted}>No budget</span></Td>{effective && <Td>No budget</Td>}</Tr>}
    </Table>
    {editing && <div className={l.addRow}><Button size="sm" variant="secondary" disabled={!canAdd || busy} onClick={() => add()}><Plus aria-hidden /> Add budget</Button><span className={s.note}>One budget per period. Every budget applies on its own; spending counts toward all of them.</span></div>}
  </>;
}
function safeMicro(dollars: string): string | null { try { return dollarsToMicroUsd(dollars); } catch { return null; } }

const layerName = (kind: string, mode: "inherit" | "replace" | undefined): Record<BudgetWindow["layer"], string> => ({ platform: mode === "replace" ? "Platform override" : `${kind} default`, local: "Workspace", key: "Key" });
/** Budget meters per applicable layer and period (spent plus on hold in each current window). */
export function BudgetMeters({ windows, kind, mode, title = "Budget used this period" }: { windows: BudgetWindow[]; kind: WorkspaceKind; mode?: "inherit" | "replace"; title?: string }) {
  if (!windows.length) return null;
  const names = layerName(kindLabels[kind], mode);
  return <Card title={title} description="Spent plus on hold in each budget's current window. Costs that aren't known yet are never counted as zero.">
    <div className={l.meters}>{windows.map(w => { const period = w.period ?? w.budget_period, amount = w.amount_microusd ?? w.monthly_budget_microusd, label = `${names[w.layer]} ${periodName[period].toLowerCase()} budget`; return <div key={`${w.layer}-${period}`}>
      {w.usage_visible ? <UsageBar label={label} showLabel used={w.used_microusd} limit={amount} period={period} description={<>{w.unresolved_usage ? "Some costs aren't known yet, so this is at least the amount shown. " : ""}{resetsAt(w.window_end)}</>} />
        : <p className={s.note}>{label}: {formatMicroUsd(amount)}. Only workspace admins see how much of it is used.</p>}
    </div>; })}</div>
  </Card>;
}

/**
 * Admin › Usage & costs › By workspace › "Set custom limits…": the platform override of one workspace with stacked budgets, in place over the
 * breakdown (the same table as the workspace's Limits tab). Prefilled with the current override, or with the live type
 * defaults when the workspace inherits them; blank means no limit. Saving replaces the override's rates and full budget
 * set; spending so far is never reset.
 */
export function ReplacementLimitsDialog({ workspaceId, name, onClose }: { workspaceId: string; name: string; onClose: () => void }) {
  const path = `${platformWorkspacePath(workspaceId)}/policy`, query = useApi<PolicyResponse>(path), client = useQueryClient(), formId = useId();
  const [draft, setDraft] = useState<LimitsDraft>(), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [rejected, setRejected] = useState<DraftErrors>();
  // A reply without a policy (an older or misbehaving gateway) is treated as unavailable, never as "no limits".
  const data = query.data?.policy ? query.data : undefined, provenance = data?.provenance;
  const typeDefault = provenance ? provenance.type_default ? limitsOf(provenance.type_default) : data?.mode === "replace" ? undefined : limitsOf(provenance.platform) : undefined;
  const saved = data ? draftOf(data.mode === "replace" || !typeDefault ? limitsOf(data.policy) : typeDefault) : undefined;
  const form = draft ?? saved, errors = form ? draftErrors(form, "free") : undefined, invalid = !!errors && hasErrors(errors);
  const local = provenance ? limitsOf(provenance.local) : undefined;
  // Valid rows keep their live preview while others are being fixed.
  const valid = form && errors ? draftLimitsValid(form, errors) : undefined, effective = valid ? composeLimits(valid.limits, local) : undefined;
  const dirty = !!draft && !!saved && (!sameDraft(draft, saved) || data?.mode !== "replace");
  async function save() {
    if (!form || invalid || busy) return;
    setBusy(true); setError(undefined); setRejected(undefined);
    try { await api(path, { method: "PUT", body: limitsBody(draftLimits(form)) }); await client.invalidateQueries({ queryKey: ["api"] }); toast.success("Limits saved", name); onClose(); }
    catch (caught) { const placed = rejectionErrors(caught, form); if (placed) setRejected(placed); else setError(limitsSaveError(caught)); }
    finally { setBusy(false); }
  }
  return <><Dialog open size="lg" title={`Set custom limits · ${name}`} hideClose={busy} onOpenChange={open => { if (!open && !busy) onClose(); }}
    description="Replaces the type defaults for this workspace only. A blank field means no limit here (it doesn't fall back to the default). Workspace caps, key limits, installation limits and spending so far still apply."
    footer={<><Button variant="secondary" disabled={busy} onClick={onClose}>Cancel</Button><Button type="submit" form={formId} loading={busy} disabled={!form || invalid}>Save custom limits</Button></>}>
    <form id={formId} noValidate aria-busy={busy} onSubmit={event => { event.preventDefault(); void save(); }}>
      {query.isPending ? <p role="status">Loading limits…</p> : query.isError ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : !form || !errors ? <ErrorNotice error={new Error("Limit sources are unavailable from this gateway.")} /> : <Stack gap={4}>
        {data?.mode === "replace" ? <p className={s.note}>This workspace already has custom limits. Saving replaces them.</p> : <p className={s.note}>This workspace follows its type defaults; they're filled in below as a starting point.</p>}
        {error !== undefined && <ErrorNotice error={error} />}
        <LimitsTable caption={`Custom limits for ${name}`} scopeLabel="Custom limits" draft={form} onChange={next => { setDraft(next); setRejected(undefined); }} editing busy={busy} errors={mergeErrors(errors, rejected)}
          inherited={{ label: "Type default", limits: typeDefault }} effective={{ limits: effective, invalid: valid?.invalid }} emptyText="No limit" placeholder={key => `No limit${typeDefault?.[key] != null ? ` · default ${typeDefault[key]!.toLocaleString("en-US")}` : ""}`} />
      </Stack>}
    </form>
  </Dialog><NavigationGuard dirty={dirty && !busy} /></>;
}

/** Read-only renderings carry their reason in the card description (rule 12: no per-section "Read-only" banners). */
/** Group separators ("100,000") for a whole number, otherwise the text as typed. */
export const groupedInteger = (raw: string) => /^\d+$/.test(raw.trim()) && Number.isSafeInteger(Number(raw.trim())) ? Number(raw.trim()).toLocaleString("en-US") : raw;
/**
 * A whole-number limit input that reads "100,000" when it isn't focused, like the read-only cells (review #45). The
 * draft keeps the plain digits: typed separators and spaces are dropped.
 */
function GroupedIntegerInput({ value, onChange, placeholder, disabled, className }: { value: string; onChange: (value: string) => void; placeholder?: string; disabled?: boolean; className?: string }) {
  const [focused, setFocused] = useState(false);
  return <Input size="sm" className={className} inputMode="numeric" maxLength={40} autoComplete="off" placeholder={placeholder} value={focused ? value : groupedInteger(value)} disabled={disabled}
    onFocus={() => setFocused(true)} onBlur={() => setFocused(false)} onChange={event => onChange(event.target.value.replace(/[,\s]/g, ""))} />;
}
export function ScopeLimits({ path, mode, writable, kind = "team", scopeLabel, readOnlyReason }: { path: string; mode: LimitsMode; writable: boolean; kind?: WorkspaceKind; scopeLabel?: string; readOnlyReason?: string }) {
  const query = useApi<PolicyResponse>(path), client = useQueryClient(), ask = useAction();
  const [draft, setDraft] = useState<LimitsDraft>(), [choice, setChoice] = useState<"defaults" | "override">();
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(), [rejected, setRejected] = useState<DraftErrors>();
  if (query.isPending) return <p role="status">Loading limits…</p>;
  if (query.isError) return <ErrorNotice error={query.error} retry={() => void query.refetch()} />;
  const data = query.data, noun = kindLabels[kind], lower = noun.toLowerCase();
  if (!("provenance" in data) || !data.provenance) return <ErrorNotice error={new Error("Limit sources are unavailable from this gateway.")} />;
  const replaced = data.mode === "replace", serverChoice = replaced ? "override" : "defaults";
  const platform = limitsOf(data.provenance.platform), local = limitsOf(data.provenance.local), stored = limitsOf(data.policy);
  // Type defaults: served to platform readers; while inheriting they are the platform layer itself.
  const typeDefault = data.provenance.type_default ? limitsOf(data.provenance.type_default) : replaced ? undefined : platform;
  const saved = draftOf(stored), picked = mode === "replacement" ? choice ?? serverChoice : "override";
  const form = draft ?? saved, editing = writable && picked === "override";
  const platformLabel = data.provenance.platform_source === "workspace_override" ? "platform override" : `${noun} default`;
  const parents: Parent[] = mode === "local" ? [{ label: platformLabel, limits: platform }] : mode === "key" ? [{ label: platformLabel, limits: platform }, { label: "workspace", limits: local }] : [];
  const errors = draftErrors(form, mode === "replacement" ? "free" : "tighten", parents, mode === "replacement" ? noLimits : stored), invalid = picked === "override" && hasErrors(errors);
  const edit = (next: LimitsDraft) => { setDraft(next); setRejected(undefined); };
  const dirty = mode === "replacement" ? picked !== serverChoice || picked === "override" && !sameDraft(form, saved) : !sameDraft(form, saved);
  // The draft's valid rows (invalid ones are left out and shown as "—"), so unchanged rows keep their effective value.
  const valid = picked === "defaults" ? undefined : draftLimitsValid(form, errors, mode === "replacement" ? noLimits : stored);
  const draftLayer = picked === "defaults" ? typeDefault : valid!.limits;
  const inherited = mode === "replacement" ? typeDefault : mode === "local" ? platform : composeLimits(platform, local);
  // Live preview; installation-wide ceilings may additionally apply and are listed under Effective access.
  const effective = !draftLayer ? undefined : mode === "replacement" ? composeLimits(draftLayer, local) : mode === "local" ? composeLimits(platform, draftLayer) : composeLimits(platform, local, draftLayer);
  const scopeName = scopeLabel ?? (mode === "replacement" ? `This ${lower}` : mode === "key" ? "This key" : "This workspace");
  const reset = () => { setDraft(undefined); setChoice(undefined); setError(undefined); setRejected(undefined); };
  async function save() {
    if (busy || !dirty || invalid) return;
    if (mode === "replacement" && picked === "defaults") {
      ask({ title: `Use ${noun} defaults again?`, description: `Deletes this ${lower}'s override. Current and future ${noun} default changes apply automatically. Spending so far isn't reset.`, submitLabel: `Use ${noun} defaults`, run: async (_, signal) => { await api(path, { method: "DELETE", signal }); }, after: () => { reset(); toast.success("Limits saved", `${noun} defaults apply`); } });
      return;
    }
    setBusy(true); setError(undefined); setRejected(undefined);
    try { await api(path, { method: "PUT", body: limitsBody(draftLimits(form)) }); await client.invalidateQueries({ queryKey: ["api"] }); reset(); toast.success("Limits saved"); }
    catch (caught) {
      // A named rejection is shown on the field it concerns; anything else as a notice.
      const placed = rejectionErrors(caught, form);
      if (placed) setRejected(placed); else setError(limitsSaveError(caught));
    }
    finally { setBusy(false); }
  }
  const description = mode === "replacement"
    ? picked === "defaults" ? `This ${lower} uses the ${noun} defaults and follows any change to them. Workspace caps and key limits still apply on top.` : `Saved values replace the ${noun} defaults for this ${lower}. A blank field means no limit. Workspace caps and key limits still apply on top.`
    : writable ? `The lowest limit always applies. Leave a field blank to use the inherited limit. Limits can only be tightened: a saved cap can be lowered, never raised or removed here.` : `${readOnlyReason ? `${readOnlyReason} ` : ""}The lowest limit always applies: platform, then ${mode === "key" ? "workspace, then this key" : "this workspace, then each key"}.`;
  return <Stack gap={6}>
    {mode === "replacement" && <RadioGroup legend="Limits source" variant="card" orientation="horizontal" value={picked} disabled={!writable || busy} onValueChange={value => { setChoice(value as "defaults" | "override"); if (value === "override" && !replaced && !draft) setDraft(draftOf(typeDefault ?? noLimits)); }} options={[
      { value: "defaults", label: `Use ${noun} defaults`, description: typeDefault ? `Current ${noun} defaults: ${limitsSummary(typeDefault)}.` : `The ${noun} defaults, updated automatically.` },
      { value: "override", label: `Override for this ${lower}`, description: `Replaces the ${noun} defaults for this ${lower} only. Blank fields mean no limit.` },
    ]} />}
    {error !== undefined && <ErrorNotice error={error} />}
    <Card title={mode === "replacement" ? `${noun} limits` : mode === "key" ? "Key limits" : "Workspace limits"} description={description} flush>
      <LimitsTable caption={`${scopeName} limits`} scopeLabel={scopeName} draft={picked === "defaults" ? draftOf(typeDefault ?? noLimits) : form} onChange={edit} editing={editing} busy={busy} errors={picked === "override" ? mergeErrors(errors, rejected) : undefined}
        inherited={{ label: mode === "replacement" ? `${noun} default` : "Inherited", limits: inherited }} effective={{ limits: effective, invalid: valid?.invalid }} mode={mode === "replacement" ? "free" : "tighten"} stored={mode === "replacement" ? noLimits : stored}
        emptyText={picked === "defaults" ? "Default" : mode === "replacement" ? "No limit" : "Not set"} placeholder={mode === "replacement" ? key => `No limit${typeDefault?.[key] != null ? ` · default ${typeDefault[key]!.toLocaleString("en-US")}` : ""}` : undefined} />
    </Card>
    <BudgetMeters windows={data.budgets ?? []} kind={kind} mode={data.mode} />
    {mode !== "key" && <p className={s.note}>Installation-wide limits may also apply. Raising a limit never resets spending, and requests whose cost isn't known yet stay on hold and count toward budgets.</p>}
    {writable && <><StickySaveBar open={dirty} message={invalid ? "Not saved: fix the highlighted limits" : rejected ? "Not saved: see the highlighted limit" : mode === "replacement" && picked === "defaults" ? `Unsaved: use the ${noun} defaults` : mode === "replacement" && !replaced ? `Unsaved: create an override for this ${lower}` : "Unsaved changes"}><Button variant="secondary" disabled={busy} onClick={reset}>Discard</Button><Button loading={busy} disabled={invalid} onClick={() => void save()}>Save limits</Button></StickySaveBar><NavigationGuard dirty={dirty && !busy} /></>}
  </Stack>;
}

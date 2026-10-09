/*
 * Pricing v3 editor: OpenRouter-style price lines grouped by meter. Operators
 * type US dollars per display unit; the body carries exact integer micro-USD
 * per batch (lib/pricing.ts). Used in a dialog on routes and inline on Add model.
 */
import { Fragment, useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Download, Plus, X } from "lucide-react";
import { ApiError, api, platformPath, type Deployment } from "../lib/api";
import type { Meter, Price, WorkloadKind } from "../lib/governance";
import { METER_SPECS, METERS, acceptImportedCeilings, convertUsd, countNoun, draftBody, draftFromPrice, draftFromSuggestion, draftSummary, emptyDraft, exactAlternatives, formatUsd, markAllFree, meterModes, mergeImport, newRow, priceBound, priceDisplayLines, priceTokenCeilings, tokenCeilings, unpricedMeters, formatAudio, rowKey, tierLabel, unitFor, usdToMicroUsd, validateDraft, workloadLabels, type ImportInfo, type MeterDraft, type MeterMode, type PriceDraft, type PriceSuggestion, type RateRow } from "../lib/pricing";
import { formatCount } from "../lib/reports";
import { Alert, Button, ErrorNotice, FormField, Input, NativeSelect } from "./ui";
import { Badge } from "./ui/badge/badge";
import { Dialog } from "./ui/dialog/dialog";
import { toast } from "./ui/toast/toast";
import { DiscardChangesDialog, NavigationGuard } from "./navigation-guard";
import s from "../pages/shared.module.css";
import styles from "./price-editor.module.css";

/** DOM id for an editor error key ("input_tokens.r3.usd" → "<prefix>-input_tokens-r3-usd"). */
export const priceFieldId = (prefix: string, key: string) => `${prefix}-${key.replace(/[^A-Za-z0-9_-]/g, "-")}`;
export function focusFirstPriceError(prefix: string, errors: Record<string, string>) { const first = Object.keys(errors)[0]; if (first) document.getElementById(priceFieldId(prefix, first))?.focus(); }

type EditorProps = { draft: PriceDraft; onChange: (draft: PriceDraft) => void; errors: Record<string, string>; disabled?: boolean; idPrefix: string };
/** The controlled line editor. Every change clears review highlighting for what was touched. */
export function PriceLinesEditor({ draft, onChange, errors, disabled, idPrefix }: EditorProps) {
  const fid = (key: string) => priceFieldId(idPrefix, key);
  const setMeter = (meter: Meter, next: Partial<MeterDraft>) => onChange({ ...draft, meters: { ...draft.meters, [meter]: { ...draft.meters[meter], ...next } } });
  const hidden = METERS.filter(m => !draft.shown.includes(m)), ceilings = tokenCeilings(draft);
  const canMarkFree = draft.shown.some(m => draft.meters[m].mode !== "not_applicable" && draft.meters[m].mode !== "free");
  return <div className={styles.editor}>
    {draft.import && <ImportNotice info={draft.import} disabled={disabled} onUseImported={() => onChange(acceptImportedCeilings(draft))} />}
    {(ceilings.input || ceilings.output) ? <div className={styles.grid}>
      {ceilings.input && <FormField name="input_token_limit" label="Hard upstream input token ceiling" description="Includes cache tokens. Reserved with the output ceiling against tokens-per-minute limits, so keep both within them." error={errors["limits.input"]}><Input id={fid("limits.input")} inputMode="numeric" autoComplete="off" disabled={disabled} value={draft.inputTokenLimit} onChange={e => onChange({ ...draft, inputTokenLimit: e.target.value })} /></FormField>}
      {ceilings.output && <FormField name="output_token_limit" label="Hard upstream output token ceiling" description={draft.workload === "generation" || draft.workload === "systemone" ? "Must be positive." : "Zero is valid when the workload produces no output tokens."} error={errors["limits.output"]}><Input id={fid("limits.output")} inputMode="numeric" autoComplete="off" disabled={disabled} value={draft.outputTokenLimit} onChange={e => onChange({ ...draft, outputTokenLimit: e.target.value })} /></FormField>}
    </div> : <p className={s.note}>No token meters apply, so no token ceilings are needed (published as 0).</p>}
    {(draft.workload === "generation" || draft.workload === "embeddings" || draft.workload === "batches") && <label className={styles.rowActions}><input type="checkbox" checked={!!draft.batchPrices} disabled={disabled} onChange={e => onChange({ ...draft, batchPrices: e.target.checked })} /><span>Batch prices</span><span className={s.note}>The provider's published batch rates, used for native batches.</span></label>}
    {canMarkFree && <div className={styles.rowActions}><Button size="sm" variant="secondary" disabled={disabled} onClick={() => onChange(markAllFree(draft))}>Mark all free</Button><span className={s.note}>Sets every applicable meter to $0.</span></div>}
    {draft.shown.some(m => draft.meters[m].mode === "unknown") && <p className={s.note}>Unknown meters publish no line: their usage is recorded with unknown cost, and budgeted requests are refused while they could apply.</p>}
    {draft.shown.map(meter => <MeterEditor key={meter} meter={meter} m={draft.meters[meter]} errors={errors} disabled={disabled} fid={fid} batchPrices={!!draft.batchPrices} onChange={next => setMeter(meter, next)} />)}
    {hidden.length > 0 && <p className={s.note}>Not applicable to {workloadLabels[draft.workload].toLowerCase()} models and published as not applicable: {hidden.map(m => METER_SPECS[m].title.toLowerCase()).join(", ")}.</p>}
    <PricePreview draft={draft} />
  </div>;
}

const selectionText = { evidence: "matches the provider-reported cost of recent requests", cheapest: "cheapest current endpoint", catalog: "catalog top-provider price (endpoint list unavailable)" } as const;
/** What the import used (endpoint, provider, ceilings) and its warnings, in plain language. */
function ImportNotice({ info, disabled, onUseImported }: { info: ImportInfo; disabled?: boolean; onUseImported: () => void }) {
  const e = info.endpoint, c = info.ceilings, kept = info.keptCeilings;
  return <Alert tone={info.needsReview ? "warning" : "info"} title="Draft from OpenRouter's public catalog">
    <p>Prefilled from <code className={s.mono}>{info.model}</code>. Nothing is published until you choose Publish price.{info.needsReview ? " Highlighted lines need review: their unit or price could not be confirmed from the catalog." : ""}</p>
    {e && <p>Price source: {e.provider ? <strong>{e.provider}</strong> : "OpenRouter"}{e.input_rate ? ` · ${e.input_rate}` : ""} · {selectionText[e.selection]}{e.selection === "evidence" && e.matched_attempts ? ` (${e.matched_attempts} request${e.matched_attempts === 1 ? "" : "s"})` : ""}{e.endpoint_count ? ` · ${e.endpoint_count} endpoint${e.endpoint_count === 1 ? "" : "s"} listed` : ""}.</p>}
    {c && !kept && (c.input_token_limit > 0 || c.output_token_limit > 0) && <p>Token ceilings default to {formatCount(c.input_token_limit)} input / {formatCount(c.output_token_limit)} output{c.context_length ? ` (the model allows up to ${formatCount(c.context_length)} context${c.max_completion_tokens ? ` and ${formatCount(c.max_completion_tokens)} output` : ""})` : ""}. They fit default tokens-per-minute limits; edit them below if needed.</p>}
    {kept && <div><p>Your token ceilings ({formatCount(kept.input || "0")} input / {formatCount(kept.output || "0")} output) were kept. Only rates were imported. The import suggests {formatCount(kept.importedInput || "0")} input / {formatCount(kept.importedOutput || "0")} output.</p><Button size="sm" variant="secondary" disabled={disabled} onClick={onUseImported}>Use imported ceilings</Button></div>}
    {info.warnings.length > 0 && <ul>{info.warnings.map((w, i) => <li key={i}>{w}</li>)}</ul>}
  </Alert>;
}

function MeterEditor({ meter, m, errors, disabled, fid, batchPrices, onChange }: { meter: Meter; m: MeterDraft; errors: Record<string, string>; disabled?: boolean; fid: (key: string) => string; batchPrices: boolean; onChange: (next: Partial<MeterDraft>) => void }) {
  const spec = METER_SPECS[meter], unit = unitFor(meter, m.batch)!, priced = m.mode === "priced";
  // Priced meters are reviewed row by row; other modes carry the meter-level flag until the mode is confirmed.
  const review = priced ? m.rows.some(r => r.review) : !!m.review;
  const setRow = (row: RateRow, next: Partial<RateRow>) => onChange({ rows: m.rows.map(r => r.id === row.id ? { ...r, ...next, review: false } : r) });
  // Re-express every row in the larger unit; a row that still isn't exact keeps its text and its error.
  const convert = (batch: number) => onChange({ batch, rows: m.rows.map(r => ({ ...r, usd: convertUsd(r.usd, m.batch, batch) ?? r.usd, review: false })) });
  return <fieldset className={styles.meter} data-review={review ? "true" : undefined} aria-label={spec.title}>
    <legend className={styles.meterTitle}>{spec.title}{review && <Badge tone="warning">Needs review</Badge>}</legend>
    <p className={s.note}>{spec.help}</p>
    {!priced && m.review && m.note && <p className={styles.reviewNote}>{m.note}</p>}
    <div className={styles.grid}>
      <FormField name={`${meter}.mode`} label={`${spec.title} price`}><NativeSelect id={fid(`${meter}.mode`)} disabled={disabled} value={m.mode} onChange={e => onChange({ mode: e.target.value as MeterMode, review: false })}>{meterModes.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect></FormField>
      {priced && spec.units.length > 1 && <FormField name={`${meter}.unit`} label="Billing unit" description="Prices below are read in this unit."><NativeSelect id={fid(`${meter}.unit`)} disabled={disabled} value={String(m.batch)} onChange={e => onChange({ batch: Number(e.target.value) })}>{spec.units.map(u => <option key={u.batch} value={u.batch}>{u.name[0].toUpperCase() + u.name.slice(1)}</option>)}</NativeSelect></FormField>}
    </div>
    {m.mode === "free" && <p className={s.note}>Free: publishes an explicit {formatUsd(0n)}{unit.display} line.</p>}
    {m.mode === "not_applicable" && <p className={s.note}>Any usage is flagged, not valued at zero.</p>}
    {priced && <ul className={styles.rows}>{m.rows.map(row => {
      const q = tierLabel(row), tier = row.variant !== undefined || row.minPromptTokens !== undefined, usdKey = rowKey(meter, row, "usd"), money = usdToMicroUsd(row.usd);
      const better = !money.ok && money.reason === "precision" ? exactAlternatives(meter, row.usd, m.batch).filter(a => a.batch > m.batch)[0] : undefined;
      const hint = row.review && row.note ? row.note : money.ok ? `Shown as ${formatUsd(money.microusd)}${unit.display}${q ? ` (${q})` : ""}` : undefined;
      return <li key={row.id} className={styles.row} data-review={row.review ? "true" : undefined}>
        <FormField name={usdKey} label={`${unit.input}${q ? ` (${q})` : tier ? " (tier)" : ""}`} description={hint} error={errors[usdKey]}><Input id={fid(usdKey)} inputMode="decimal" autoComplete="off" spellCheck={false} placeholder="0.00" disabled={disabled} value={row.usd} onChange={e => setRow(row, { usd: e.target.value })} /></FormField>
        {batchPrices && <FormField name={rowKey(meter, row, "batch")} label="Batch" error={errors[rowKey(meter, row, "batch")]}><Input id={fid(rowKey(meter, row, "batch"))} inputMode="decimal" autoComplete="off" spellCheck={false} placeholder="0.00" disabled={disabled} value={row.batchUsd ?? ""} onChange={e => setRow(row, { batchUsd: e.target.value })} /></FormField>}
        {row.variant !== undefined && <FormField name={rowKey(meter, row, "variant")} label="Variant" description="Resolution or size the provider reports, e.g. 768, 1K or 1024x1024." error={errors[rowKey(meter, row, "variant")]}><Input id={fid(rowKey(meter, row, "variant"))} autoComplete="off" spellCheck={false} disabled={disabled} value={row.variant} onChange={e => setRow(row, { variant: e.target.value })} /></FormField>}
        {row.minPromptTokens !== undefined && <FormField name={rowKey(meter, row, "tier")} label="Applies when prompt exceeds (tokens)" description="Strictly greater than: 272000 means “> 272K tokens”." error={errors[rowKey(meter, row, "tier")]}><Input id={fid(rowKey(meter, row, "tier"))} inputMode="numeric" autoComplete="off" disabled={disabled} value={row.minPromptTokens} onChange={e => setRow(row, { minPromptTokens: e.target.value })} /></FormField>}
        {(better || tier) && <div className={styles.rowActions}>
          {better && <Button size="sm" variant="secondary" disabled={disabled} onClick={() => convert(better.batch)}>Use {better.unit.name} (${better.usd}{better.unit.display})</Button>}
          {tier && <Button size="sm" variant="ghost" disabled={disabled} aria-label={`Remove ${spec.title} tier ${q || "row"}`} onClick={() => onChange({ rows: m.rows.filter(r => r.id !== row.id) })}><X aria-hidden />Remove</Button>}
        </div>}
      </li>;
    })}</ul>}
    {priced && errors[`${meter}.rows`] && <p id={fid(`${meter}.rows`)} tabIndex={-1} role="alert" className={styles.error}>{errors[`${meter}.rows`]}</p>}
    {priced && (spec.tiers || spec.variants) && <div className={styles.rowActions}>
      {spec.tiers && <Button size="sm" variant="ghost" disabled={disabled} onClick={() => onChange({ rows: [...m.rows, newRow(meter, { minPromptTokens: "" })] })}><Plus aria-hidden />Add prompt-size tier</Button>}
      {spec.variants && <Button size="sm" variant="ghost" disabled={disabled} onClick={() => onChange({ rows: [...m.rows, newRow(meter, { variant: "" })] })}><Plus aria-hidden />Add resolution tier</Button>}
    </div>}
    {priced && spec.maxUnits && <FormField name={`${meter}.max`} label={spec.maxUnits.label} labelHint="Needed for budgets" description={`${spec.maxUnits.help} Without it, budgeted requests are refused because the hold cannot be bounded.`} error={errors[`${meter}.max`]}><Input id={fid(`${meter}.max`)} inputMode="numeric" autoComplete="off" disabled={disabled} value={m.maxUnits} onChange={e => onChange({ maxUnits: e.target.value })} /></FormField>}
  </fieldset>;
}

const reasonText = { unknown: "unknown price", no_base: "tiers without a base price", no_ceiling: "no per-request ceiling" } as const;
function PricePreview({ draft }: { draft: PriceDraft }) {
  let bound: ReturnType<typeof priceBound> | undefined;
  const errors = validateDraft(draft);
  if (!Object.keys(errors).length) bound = priceBound(draftBody(draft));
  return <div className={styles.preview} aria-live="polite"><span className={s.primary}>Preview</span><span>{draftSummary(draft).join(" · ") || "No applicable meters."}</span>
    <span className={s.note}>{!bound ? "Complete the form to see the maximum hold per request." : bound.microusd !== null ? `Maximum budget hold per request: ${formatUsd(bound.microusd)} (the most a request can hold against budgets, not a typical charge).` : `Budgeted requests will be refused: ${bound.unbounded.map(u => `${METER_SPECS[u.meter].title.toLowerCase()} (${reasonText[u.reason]})`).join(", ")}.`}</span></div>;
}

type DialogProps = { deployment: Deployment; workload: WorkloadKind; profile?: string; current?: Price; importOnOpen?: boolean; onClose: () => void };
/** Publish an immutable v3 price for one route, optionally starting from OpenRouter's public catalog. */
export function PriceEditorDialog({ deployment, workload, profile, current, importOnOpen = false, onClose }: DialogProps) {
  const client = useQueryClient(), id = useId(), returnFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : undefined);
  const [draft, setDraft] = useState<PriceDraft>(() => current ? draftFromPrice(current, workload) : emptyDraft(workload)), initial = useRef(draft), dirty = draft !== initial.current;
  const [errors, setErrors] = useState<Record<string, string>>({}), [error, setError] = useState<unknown>(), [busy, setBusy] = useState(false), [importing, setImporting] = useState(false), [importError, setImportError] = useState<unknown>(), [discarding, setDiscarding] = useState(false);
  const mounted = useRef(true), inFlight = useRef(false), completed = useRef(false), controller = useRef<AbortController | undefined>(undefined);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; controller.current?.abort(); }; }, []);
  useEffect(() => { if (!dirty) return; const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; }; window.addEventListener("beforeunload", warn); return () => window.removeEventListener("beforeunload", warn); }, [dirty]);
  const openrouter = profile === "openrouter";
  async function runImport() {
    if (inFlight.current) return;
    const request = new AbortController(); controller.current = request; inFlight.current = true; setImporting(true); setImportError(undefined);
    try {
      const suggestion = await api<PriceSuggestion>(`${platformPath}/deployments/${encodeURIComponent(deployment.id)}/price-suggestion`, { signal: request.signal });
      if (!mounted.current || request.signal.aborted) return;
      // A draft only: the operator reviews and publishes with the normal immutable POST.
      // Re-imports merge rates only: token ceilings the admin entered are kept until they confirm.
      const imported = draftFromSuggestion(suggestion, workload);
      setDraft(previous => mergeImport(previous, imported)); setErrors({});
    } catch (caught) { if (mounted.current && !request.signal.aborted) setImportError(caught); }
    finally { inFlight.current = false; if (mounted.current) setImporting(false); }
  }
  useEffect(() => { if (importOnOpen && openrouter) void runImport(); }, []);
  const change = (next: PriceDraft) => { if (busy) return; setDraft(next); setErrors({}); };
  const discard = () => { controller.current?.abort(); onClose(); };
  const close = () => { if (busy) return; if (dirty) setDiscarding(true); else discard(); };
  async function submit(event: React.FormEvent) {
    event.preventDefault(); if (inFlight.current) return;
    const validation = validateDraft(draft); setErrors(validation);
    if (Object.keys(validation).length) { focusFirstPriceError(id, validation); return; }
    const request = new AbortController(); controller.current = request; inFlight.current = true; setBusy(true); setError(undefined);
    try {
      await api(`${platformPath}/deployments/${encodeURIComponent(deployment.id)}/prices`, { method: "POST", body: draftBody(draft), signal: request.signal });
      if (!mounted.current || request.signal.aborted) return;
      completed.current = true;
      void client.invalidateQueries({ queryKey: ["api"] }); void client.invalidateQueries({ queryKey: ["session"] });
      toast.success("Price published."); onClose();
    } catch (caught) { if (mounted.current && !request.signal.aborted) { setError(caught); if (caught instanceof ApiError && caught.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); } }
    finally { inFlight.current = false; if (mounted.current) setBusy(false); }
  }
  return <><Dialog open size="xl" title={`Publish price · ${deployment.upstream_model}`} description={`${workloadLabels[workload]} route. New prices apply to new requests; past usage keeps its price. Enter US dollars per unit. Unknown is not free, and these are estimates, not provider invoices.`} hideClose={busy} onOpenChange={open => { if (!open) close(); }} finalFocus={() => returnFocus.current?.isConnected ? returnFocus.current : true}
    footer={<><Button variant="secondary" disabled={busy} onClick={close}>Cancel</Button><Button type="submit" form={id} loading={busy} disabled={importing}>Publish price</Button></>}>
    <form id={id} noValidate onSubmit={event => void submit(event)} aria-busy={busy || importing} aria-label="Price lines" data-dirty={dirty ? "true" : undefined}>
      {openrouter && <div className={`${styles.rowActions} ${styles.importBar}`}><Button variant="secondary" loading={importing} disabled={busy} onClick={() => void runImport()}><Download aria-hidden />Import current OpenRouter price</Button><span className={s.note}>Reads OpenRouter's public catalog through the gateway and fills this form as a draft. Nothing is published automatically.</span></div>}
      {importError !== undefined && <ErrorNotice error={importError} retry={() => void runImport()} />}
      {importing && <p role="status" className={s.note}>Importing OpenRouter price…</p>}
      <PriceLinesEditor draft={draft} onChange={change} errors={errors} disabled={busy || importing} idPrefix={id} />
      {error !== undefined && <ErrorNotice error={error} />}
    </form>
  </Dialog><NavigationGuard dirty={dirty} allow={() => completed.current} /><DiscardChangesDialog open={discarding} onOpenChange={setDiscarding} onDiscard={discard} description="Your price draft hasn't been published." /></>;
}

const maxUnitText = (meter: Meter, value: string) => meter.endsWith("_audio_seconds_ms") ? `${formatAudio(value)} ${METER_SPECS[meter].noun}` : countNoun(meter, value);
/** Token ceilings (only those whose meters apply) plus per-request unit maxima. */
function ceilingSummary(price: Price, max: [Meter, string][]): string {
  const c = priceTokenCeilings(price), tokens = [c.input ? `${formatCount(price.input_token_limit)} input` : "", c.output ? `${formatCount(price.output_token_limit)} output` : ""].filter(Boolean);
  const parts = [tokens.length ? `${tokens.join(" / ")} tokens` : "", max.length ? `per request: ${max.map(([m, v]) => maxUnitText(m, v)).join(", ")}` : ""].filter(Boolean);
  return parts.join(" · ") || "None";
}
/** OpenRouter-style price card: one labelled line per SKU, from the gateway's exact display strings when present. */
export function PriceLinesView({ price }: { price: Price }) {
  const lines = priceDisplayLines(price), unpriced = unpricedMeters(price), max = Object.entries(price.max_units ?? {}) as [Meter, string][];
  return <dl className={styles.priceLines} aria-label={`Pricing v${price.pricing_version}`}>
    {lines.filter(l => !l.notApplicable).map((l, i) => <Fragment key={i}><dt>{l.label}</dt><dd>{l.text}</dd></Fragment>)}
    {lines.some(l => l.notApplicable) && <><dt>Not applicable</dt><dd>{lines.filter(l => l.notApplicable).map(l => l.label).join(", ")}</dd></>}
    {unpriced.length > 0 && <><dt>Not priced</dt><dd>{unpriced.map(m => METER_SPECS[m].title).join(", ")} · cost unknown, never free</dd></>}
    <dt>Ceilings</dt><dd>{ceilingSummary(price, max)}</dd>
    {price.batch_price_lines?.length ? <><dt>Batch prices</dt><dd>{price.batch_display_summary ?? "Published"}</dd></> : null}
  </dl>;
}

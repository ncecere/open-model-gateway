import { createContext, useContext, useEffect, useRef, useState, type RefObject } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, platformPath, type Provider, type Session } from "../../lib/api";
import { importsPath, mappingsPath, readCostPages, readImportSummary, readLatestMapping, validateReportFile, dimensionLabel, coversPeriod, writeBinding, type Binding, type CostImport, type ImportSummary, type MappingRevision } from "../../lib/provider-cost-writes";
import { formatSignedMicroUsd, reconciliationPath } from "../../lib/provider-costs";
import { Button, FormField, Input, NativeSelect, Stack, useApiScope, useChoices } from "../../components/ui";
import { Checkbox } from "../../components/ui/checkbox/checkbox";
import { Tabs, TabsList, Tab, TabsPanel } from "../../components/ui/tabs/tabs";
import { DataTable } from "../../components/ui/data-table/data-table";
import { InfoBanner } from "../../components/templates/notices";
import { DiscardChangesDialog, NavigationGuard } from "../../components/navigation-guard";
import { AlertDialog } from "../../components/ui/dialog/dialog";
import s from "./provider-cost-manager.module.css";

type Workflow = { busy: boolean; active: RefObject<AbortController | null>; setBusy: (busy: boolean) => void; dirty: boolean; setDirty: (dirty: boolean) => void };
const WorkflowContext = createContext<Workflow>(null!);

/** Tasks live above tab panels, but still abort on demotion/sign-out unmount.
 * No evidence Files or attestation drafts enter the Query mutation cache. */
function useWriteTask() {
  const client = useQueryClient(), request = useRef<AbortController | null>(null), workflow = useContext(WorkflowContext);
  const { active, setBusy: setWorkflowBusy } = workflow;
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  useEffect(() => () => {
    const controller = request.current;
    request.current = null;
    controller?.abort();
    // Release immediately, even if the transport never settles after abort.
    if (controller && active.current === controller) { active.current = null; setWorkflowBusy(false); }
  }, [active, setWorkflowBusy]);
  async function run<T>(work: (signal: AbortSignal) => Promise<T>, done: (value: T) => void) {
    if (active.current) return;
    const controller = new AbortController(); active.current = controller; request.current = controller; setBusy(true); setWorkflowBusy(true); setError(undefined);
    try { const value = await work(controller.signal); if (!controller.signal.aborted) done(value); }
    catch (e) { if (!controller.signal.aborted) { setError(e); if (e instanceof ApiError && e.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); } }
    finally {
      // A cancelled owner's late settlement must not unlock a newer write.
      if (active.current === controller) { active.current = null; setWorkflowBusy(false); }
      if (request.current === controller) { request.current = null; setBusy(false); }
    }
  }
  return { run, busy, error, clear: () => setError(undefined) };
}
function Refusal({ error }: { error: unknown }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => { if (error) ref.current?.focus({ preventScroll: true }); }, [error]);
  return error ? <div ref={ref} role="alert" tabIndex={-1} className={s.error}>{error instanceof Error ? error.message : "Unable to complete request."}</div> : null;
}
function identifier(value: { id: string }) {
  if (typeof value?.id !== "string" || !value.id) throw new Error("No revision identifier returned. Reload to check the result before retrying.");
  return value.id;
}
type ManagerProps = { onOpen: (id: string) => void; onClose: () => void };
export function ProviderCostManager({ session, onOpen, onClose }: ManagerProps & { session: Session }) {
  // UI is not an authorization boundary. Rust checks live browser Admin + CSRF.
  if (session.user.platform_role !== "admin" || !session.capabilities.platform_write) return null;
  return <Manager onOpen={onOpen} onClose={onClose} />;
}
function Manager(props: ManagerProps) {
  const [busy, setBusy] = useState(false), [dirty, setDirty] = useState(false), active = useRef<AbortController | null>(null);
  return <WorkflowContext.Provider value={{ busy, setBusy, dirty, setDirty, active }}><ManagerView {...props} /></WorkflowContext.Provider>;
}
function ManagerView({ onOpen, onClose }: ManagerProps) {
  const workflow = useContext(WorkflowContext);
  const [leaving, setLeaving] = useState<(() => void)>(), [pendingTab, setPendingTab] = useState<string>();
  function leave(action: () => void) {
    if (workflow.active.current) return;
    if (workflow.dirty) setLeaving(() => action); else action();
  }
  const scope = useApiScope(), client = useQueryClient(), task = useWriteTask();
  const [file, setFile] = useState<File>(), [selected, setSelected] = useState<CostImport>(), [resolved, setResolved] = useState<string>();
  const [mappingId, setMappingId] = useState<string>(), [tab, setTab] = useState("upload");
  const heading = useRef<HTMLHeadingElement>(null), fileInput = useRef<HTMLInputElement>(null);
  useEffect(() => { heading.current?.focus({ preventScroll: true }); }, []);
  const key = ["api", scope, importsPath, "admin-metadata"];
  const reports = useQuery({ queryKey: key, queryFn: ({ signal }) => readCostPages<CostImport>(importsPath, signal), retry: false });
  function select(report?: CostImport) { setSelected(report); setMappingId(undefined); setResolved(undefined); task.clear(); }
  function upload() {
    setResolved(undefined); setSelected(undefined); setMappingId(undefined);
    void task.run(async signal => {
      validateReportFile(file);
      const id = identifier(await api<{ id: string }>(importsPath, { method: "POST", rawJson: file, signal }));
      const rows = await readCostPages<CostImport>(importsPath, signal);
      const report = rows.find(r => r.id === id);
      if (!report) throw new Error("The gateway returned an import ID but its metadata is unavailable. Reload before continuing.");
      client.setQueryData(key, rows); return report;
    }, report => { setSelected(report); setResolved(report.id); setFile(undefined); if (fileInput.current) fileInput.current.value = ""; });
  }
  return <Stack gap={4} className={s.view} data-dirty={workflow.dirty ? "true" : undefined}>
    <div><Button variant="secondary" disabled={workflow.busy} onClick={() => leave(onClose)}>Back to comparisons</Button></div>
    <NavigationGuard dirty={workflow.dirty} pending={workflow.busy} />
    <DiscardChangesDialog open={!!leaving} onOpenChange={open => { if (!open) setLeaving(undefined); }} onDiscard={() => { if (!workflow.active.current) { leaving?.(); setLeaving(undefined); } }} description="Your mapping changes haven't been saved." />
    <AlertDialog open={pendingTab !== undefined} onOpenChange={open => { if (!open) setPendingTab(undefined); }} title="Write still in progress" description="Switching views will not cancel this write. Stay on this page until its result is known." confirmLabel="Switch view" cancelLabel="Stay here" tone="primary" onConfirm={() => { if (pendingTab) setTab(pendingTab); setPendingTab(undefined); }} />
    {workflow.busy && <p role="status">Write in progress. Navigation is blocked until the gateway responds.</p>}
    <h2 ref={heading} tabIndex={-1}>Upload &amp; manage provider costs</h2>
    <InfoBanner title="Permanent financial evidence">Anthropic Cost Report envelope JSON only: account_id, evidence_reference, exported_at, optional supersedes, request and ordered pages. Upload the complete closed UTC period (1–31 days), grouped by workspace_id and description. Include supersedes in the file for changed exact-coverage revisions. No credentials, headers, prompts or responses. Retained append-only; not a verified invoice.</InfoBanner>
    <Tabs value={tab} onValueChange={value => { if (workflow.active.current) setPendingTab(String(value)); else setTab(String(value)); }}><Stack gap={4}>
      <TabsList variant="pills" overflow="scroll" aria-label="Provider cost administration"><Tab value="upload">Upload report</Tab><Tab value="mappings" disabled={!selected}>Mappings</Tab><Tab value="freeze" disabled={!selected}>Freeze snapshot</Tab></TabsList>
      <TabsPanel value="upload"><Stack gap={4}>
        <form onSubmit={e => { e.preventDefault(); leave(upload); }} className={s.form}>
          <FormField label="Anthropic report JSON" description="Maximum 5 MiB (5,242,880 bytes), 50,000 lines. Raw bytes are sent unmodified; server validation is authoritative. Duplicate JSON keys, incomplete pagination and invalid coverage are refused."><Input ref={fileInput} type="file" accept="application/json,application/*+json" disabled={workflow.busy} onChange={e => { setFile(e.target.files?.[0]); task.clear(); }} /></FormField>
          <div><Button type="submit" disabled={workflow.busy} loading={task.busy}>Upload report</Button></div>
        </form>
        <Refusal error={task.error} />
        <p>Identical reuploads return the retained import, not a new revision. Changed reports never supersede automatically.</p>
        <FormField label="Existing import" description="Resume mapping or freezing a retained import without downloading its raw evidence."><NativeSelect disabled={reports.isPending || !!reports.error || workflow.busy} value={selected?.id ?? ""} onChange={e => { const report = reports.data?.find(r => r.id === e.target.value); leave(() => select(report)); }}><option value="">Choose an import…</option>{!reports.error && reports.data?.map(r => <option key={r.id} value={r.id}>{r.account_id} · {r.period_start} – {r.period_end} · {r.id}</option>)}</NativeSelect></FormField>
        <Refusal error={reports.error} />{reports.error && <Button onClick={() => void reports.refetch()}>Reload imports</Button>}
      </Stack></TabsPanel>
      {selected && <ImportFlow key={selected.id} report={selected} resolved={resolved} mappingId={mappingId} onMapping={setMappingId} onOpen={id => leave(() => onOpen(id))} />}
    </Stack></Tabs>
  </Stack>;
}
function ImportFlow({ report, resolved, mappingId, onMapping, onOpen }: { report: CostImport; resolved?: string; mappingId?: string; onMapping: (id: string) => void; onOpen: (id: string) => void }) {
  const scope = useApiScope();
  const summary = useQuery({ queryKey: ["api", scope, importsPath, report.id, "summary"], queryFn: ({ signal }) => readImportSummary(report, signal), retry: false });
  return <>
    <section aria-label="Selected import result" aria-live="polite" className={s.facts}>
      <h3>{resolved ? `Import resolved: ${resolved}` : `Selected import: ${report.id}`}</h3>
      <p>Provider account: {report.account_id}</p><p>UTC coverage [{report.period_start}, {report.period_end})</p>
      <p>{report.supersedes ? `Supersedes import: ${report.supersedes}` : "No superseded import recorded."}</p>
      {summary.error && <><Refusal error={summary.error} /><Button onClick={() => void summary.refetch()}>Reload import lines</Button></>}
      {summary.data ? <p>Imported total (signed): <strong>{formatSignedMicroUsd(summary.data.total)}</strong> · {report.line_count} lines</p> : !summary.error && <p role="status">Loading normalized import lines…</p>}
    </section>
    {summary.data && <>
      <MappingStep report={report} summary={summary.data} onSaved={onMapping} />
      <FreezeStep report={report} mappingId={mappingId} onOpen={onOpen} />
    </>}
  </>;
}
function MappingStep({ report, summary, onSaved }: { report: CostImport; summary: ImportSummary; onSaved: (id: string) => void }) {
  const scope = useApiScope(), client = useQueryClient(), [generation, setGeneration] = useState(0), [saved, setSaved] = useState<string>();
  const key = ["api", scope, mappingsPath, report.account_id, "latest"];
  // Never discard an attestation draft through focus/reconnect auto-refetch.
  // Only the explicit reload action replaces it after a revision conflict.
  const current = useQuery({ queryKey: key, queryFn: ({ signal }) => readLatestMapping(report.account_id, signal), retry: false, staleTime: Infinity });
  const connections = useChoices<Provider>(`${platformPath}/providers`);
  const error = current.error ?? connections.error;
  function retry() { void current.refetch(); void connections.refetch(); }
  const refusal = error ? <><Refusal error={error} /><Button onClick={retry}>Reload mapping data</Button></> : null;
  // An error with retained data is a failed background read, not a reason to
  // destroy drafts, conflict locks or pending write controllers. Null is a
  // successfully loaded first mapping revision; undefined means no data yet.
  if (current.data === undefined || connections.data === undefined) return <TabsPanel value="mappings"><Stack gap={3}>{refusal ?? <p role="status">Loading current mapping revision…</p>}</Stack></TabsPanel>;
  return <MappingEditor readError={error} onRetry={retry} saved={saved} key={`${current.data?.id ?? "none"}/${generation}`} report={report} summary={summary} latest={current.data ?? null} connections={connections.data?.filter(c => c.provider === "anthropic") ?? []}
    onSaved={revision => { setSaved(revision.id); onSaved(revision.id); client.setQueryData(key, revision); }} onReload={async () => { const result = await current.refetch(); if (!result.error) { setSaved(undefined); setGeneration(n => n + 1); } }} />;
}
function bindingFields(report: CostImport, dimensions: (string | null)[], initial?: Binding): Binding {
  return initial ? { ...initial, attested: false } : { provider_workspace_id: dimensions[0] ?? null, connection_id: "", deployment_id: null, workspace_id: null, effective_start: report.period_start, effective_end: report.period_end, exclusive: false, non_omg_traffic: false, attested: false, evidence_reference: "", model_mappings: [] };
}
// This controller is outside TabsPanel: drafts, refusals and task outcomes outlive
// the panel's presentation. Only explicit reload/import change replaces a draft.
function MappingEditor({ report, summary, latest, connections, onSaved, onReload, saved, readError, onRetry }: { report: CostImport; summary: ImportSummary; latest: MappingRevision | null; connections: Provider[]; onSaved: (revision: MappingRevision) => void; onReload: () => Promise<void>; saved?: string; readError: unknown; onRetry: () => void }) {
  const { busy, setDirty } = useContext(WorkflowContext);
  const [draft, setDraft] = useState<Binding[]>(latest?.bindings ?? []), [base, setBase] = useState(latest?.id ?? null);
  const [evidence, setEvidence] = useState(""), [confirmed, setConfirmed] = useState(false), [edit, setEdit] = useState<number>(), [formKey, setFormKey] = useState(0);
  // Unapplied fields belong to the workflow controller, not the tab's form.
  const [fields, setFields] = useState(() => bindingFields(report, summary.dimensions));
  const task = useWriteTask(), conflict = task.error instanceof ApiError && [409, 412].includes(task.error.status);
  const locked = busy || conflict;
  const fieldsDirty = JSON.stringify(fields) !== JSON.stringify(bindingFields(report, summary.dimensions, edit === undefined ? undefined : draft[edit]));
  useEffect(() => { setDirty(JSON.stringify(draft) !== JSON.stringify(latest?.bindings ?? []) || !!evidence || fieldsDirty); }, [draft, evidence, latest, fieldsDirty, setDirty]);
  useEffect(() => () => setDirty(false), [setDirty]);
  function stage(binding: Binding) { setDraft(rows => edit === undefined ? [...rows, binding] : rows.map((r, i) => i === edit ? binding : r)); setEdit(undefined); setFields(bindingFields(report, summary.dimensions)); setFormKey(n => n + 1); setConfirmed(false); }
  return <TabsPanel value="mappings"><Stack gap={4}>
    <Refusal error={readError} />{!!readError && <Button onClick={onRetry}>Reload mapping data</Button>}
    {saved && <p role="status">Saved mapping revision: {saved}</p>}
    <InfoBanner title="Whole replacement revision">All existing bindings are retained in this draft unless explicitly removed. One provider workspace maps to one connection per effective interval. If multiple OMG connections contribute, do not attest exclusive coverage of only one. Shared actual charges are never prorated. Default/missing provider identifiers are distinct from OMG Unallocated.</InfoBanner>
    <p>Base mapping revision: {base ?? "None (first revision)"}</p>
    <DataTable caption="Provider dimensions" stack columns={[
      { id: "dimension", header: "Provider workspace dimension", rowHeader: true, cell: (id: string | null) => dimensionLabel(id) },
      { id: "coverage", header: "Draft period coverage", cell: (id: string | null) => coversPeriod(draft, id, report) ? "Mapped" : "Unmapped" },
    ]} data={summary.dimensions} getRowId={id => JSON.stringify(id)} />
    <DataTable caption="Whole mapping draft" stack columns={[
      { id: "dimension", header: "Provider workspace", rowHeader: true, cell: (b: Binding) => dimensionLabel(b.provider_workspace_id) },
      { id: "scope", header: "Gateway scope", cell: (b: Binding) => <>{b.connection_id} · {b.deployment_id ?? "All deployments"} · {b.workspace_id ?? "Shared / unallocated"}</> },
      { id: "period", header: "Effective UTC interval", cell: (b: Binding) => `${b.effective_start} – ${b.effective_end}` },
      { id: "models", header: "Explicit model mappings (read-only)", cell: (b: Binding) => b.model_mappings.length ? <ul>{b.model_mappings.map((m, i) => <li key={i}>{m.provider_model} → {m.upstream_model}</li>)}</ul> : "No explicit model mappings" },
      { id: "actions", header: "Actions", cell: (binding: Binding) => { const index = draft.indexOf(binding); return <><Button size="sm" variant="ghost" disabled={locked} onClick={() => { setEdit(index); setFields(bindingFields(report, summary.dimensions, binding)); setFormKey(n => n + 1); }}>Edit binding {index + 1}</Button><Button size="sm" variant="ghost" disabled={locked} onClick={() => { setDraft(rows => rows.filter((_, i) => i !== index)); setEdit(undefined); setFields(bindingFields(report, summary.dimensions)); setFormKey(n => n + 1); setConfirmed(false); }}>Remove binding {index + 1}</Button></>; } },
    ]} data={draft} getRowId={(_, i) => String(i)} pageSize={10} />
    <p>Editing explicit model mappings is API-only for now. Existing pairs are preserved.</p>
    <BindingForm key={formKey} dimensions={summary.dimensions} connections={connections} initial={edit === undefined ? undefined : draft[edit]} fields={fields} onChange={setFields} locked={locked} onStage={stage} />
    <form className={s.form} onSubmit={e => { e.preventDefault(); if (!confirmed || locked || !evidence.trim()) return; void task.run(async signal => identifier(await api<{ id: string }>(mappingsPath, { method: "POST", signal, body: { account_id: report.account_id, supersedes: base, evidence_reference: evidence.trim(), bindings: draft.map(writeBinding) } })), id => { setBase(id); setConfirmed(false); setEvidence(""); onSaved({ id, account_id: report.account_id, supersedes: base, evidence_reference: evidence.trim(), bindings: draft.map(writeBinding) }); }); }}>
      <FormField label="Mapping revision evidence reference" description="Opaque reference, retained permanently. Not a fetched URL." ><Input value={evidence} maxLength={256} disabled={locked} onChange={e => { setEvidence(e.target.value); setConfirmed(false); }} /></FormField>
      <Checkbox label="I confirm the whole replacement mapping revision" description="Saving creates immutable history, including removals. Check every binding and its evidence before confirming." checked={confirmed} disabled={locked} onCheckedChange={setConfirmed} />
      <div><Button type="submit" disabled={!confirmed || locked || !evidence.trim()} loading={task.busy}>Save mapping revision</Button></div>
    </form>
    <Refusal error={task.error} />
    {conflict && <p>The mapping changed or the revision is no longer current. Reload and discard this draft, review the latest bindings, then attest and retry. Nothing is overwritten automatically.</p>}
    <div><Button variant="secondary" disabled={busy} onClick={() => void onReload()}>Reload latest mapping and discard draft</Button></div>
  </Stack></TabsPanel>;
}
function BindingForm({ dimensions, connections, initial, fields, onChange, locked, onStage }: { dimensions: (string | null)[]; connections: Provider[]; initial?: Binding; fields: Binding; onChange: (fields: Binding) => void; locked: boolean; onStage: (binding: Binding) => void }) {
  const options = [...new Set([...dimensions, ...(initial ? [initial.provider_workspace_id] : [])])];
  const { provider_workspace_id: dimension, connection_id: connection, deployment_id, workspace_id, effective_start: start, effective_end: end, exclusive, non_omg_traffic: external, attested, evidence_reference: evidence } = fields;
  const deployment = deployment_id ?? "", workspace = workspace_id ?? "";
  const [error, setError] = useState<unknown>(), heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => { heading.current?.focus({ preventScroll: true }); }, []);
  function changed<K extends keyof Binding>(key: K, value: Binding[K]) { onChange({ ...fields, [key]: value, attested: false }); }
  function submit() {
    if (locked || !attested) return;
    if (!connection || !evidence.trim() || !Number.isFinite(Date.parse(start)) || !Number.isFinite(Date.parse(end)) || Date.parse(end) <= Date.parse(start) || (exclusive && (!workspace || external))) { setError(new Error("Choose a connection, evidence reference and valid effective UTC interval. Exclusive use needs an OMG workspace and no non-OMG traffic.")); return; }
    onStage({ ...fields, deployment_id: deployment.trim() || null, workspace_id: workspace.trim() || null, evidence_reference: evidence.trim() });
  }
  return <form className={s.form} onSubmit={e => { e.preventDefault(); submit(); }}>
    <h3 ref={heading} tabIndex={-1}>{initial ? "Edit attested binding" : "Add attested binding"}</h3>
    <div className={s.grid}>
      <FormField label="Provider workspace dimension"><NativeSelect value={String(options.indexOf(dimension))} disabled={locked || !options.length} onChange={e => changed("provider_workspace_id", options[Number(e.target.value)] ?? null)}>{options.map((id, i) => <option key={JSON.stringify(id)} value={String(i)}>{dimensionLabel(id)}</option>)}</NativeSelect></FormField>
      <FormField label="Gateway connection"><NativeSelect value={connection} disabled={locked} onChange={e => onChange({ ...fields, connection_id: e.target.value, deployment_id: null, attested: false })}><option value="">Choose an Anthropic connection…</option>{initial && !connections.some(c => c.id === initial.connection_id) && <option value={initial.connection_id}>{initial.connection_id} (retained)</option>}{connections.map(c => <option key={c.id} value={c.id}>{c.name}</option>)}</NativeSelect></FormField>
      <FormField label="Deployment ID (optional)" description="UUID; must belong to the selected connection. Empty includes all its deployments."><Input value={deployment} disabled={locked} onChange={e => changed("deployment_id", e.target.value || null)} /></FormField>
      <FormField label="OMG workspace ID (optional)" description="UUID; required for exclusive attribution. No personal workspace directory is fetched."><Input value={workspace} disabled={locked} onChange={e => changed("workspace_id", e.target.value || null)} /></FormField>
      <FormField label="Effective start (UTC)" description="ISO 8601 timestamp, inclusive."><Input value={start} disabled={locked} onChange={e => changed("effective_start", e.target.value)} /></FormField>
      <FormField label="Effective end (UTC)" description="ISO 8601 timestamp, exclusive; no proration of partial days."><Input value={end} disabled={locked} onChange={e => changed("effective_end", e.target.value)} /></FormField>
    </div>
    <FormField label="Binding evidence reference"><Input value={evidence} maxLength={256} disabled={locked} onChange={e => changed("evidence_reference", e.target.value)} /></FormField>
    <Checkbox label="Exclusive OMG workspace use for this interval" checked={exclusive} disabled={locked} onCheckedChange={value => changed("exclusive", value)} />
    <Checkbox label="Non-OMG traffic exists" checked={external} disabled={locked} onCheckedChange={value => changed("non_omg_traffic", value)} />
    <Checkbox label="I attest this binding and its effective coverage" description="Current credentials do not establish historical ownership. Shared or incomplete scope remains unallocated. Existing explicit model mappings are preserved; unknown models remain unmatched." checked={attested} disabled={locked} onCheckedChange={value => onChange({ ...fields, attested: value })} />
    <div><Button type="submit" variant="secondary" disabled={locked || !attested || !options.length}>Apply binding to draft</Button></div>
    <Refusal error={error} />
  </form>;
}
function FreezeStep({ report, mappingId, onOpen }: { report: CostImport; mappingId?: string; onOpen: (id: string) => void }) {
  const scope = useApiScope(), client = useQueryClient(), task = useWriteTask(), [frozen, setFrozen] = useState<string>();
  const current = useQuery({ queryKey: ["api", scope, mappingsPath, report.account_id, "latest"], queryFn: ({ signal }) => readLatestMapping(report.account_id, signal), retry: false, staleTime: Infinity });
  const { busy } = useContext(WorkflowContext);
  const explicitMapping = mappingId ?? current.data?.id;
  return <TabsPanel value="freeze"><Stack gap={4}>
    <h3>Freeze reconciliation for this period</h3>
    <p>Import revision: {report.id} · Mapping revision: {explicitMapping ?? "None selected"}</p>
    <p>UTC coverage [{report.period_start}, {report.period_end}). Pins these exact immutable revisions and an observation timestamp. No estimates, ledger entries or holds are changed. This is not a verified invoice; unknown comparisons remain unknown.</p>
    <Refusal error={current.error} />{current.error && <Button onClick={() => void current.refetch()}>Reload freeze mapping</Button>}
    {!current.isPending && !current.error && !explicitMapping && <p>Save a mapping revision first, including an explicitly empty replacement set for unmapped coverage.</p>}
    <div><Button disabled={busy || current.isPending || !!current.error || !explicitMapping || !!frozen} loading={task.busy} onClick={() => { if (!explicitMapping) return; void task.run(async signal => identifier(await api<{ id: string }>(reconciliationPath, { method: "POST", signal, body: { import_id: report.id, mapping_id: explicitMapping } })), id => { setFrozen(id); void client.invalidateQueries({ queryKey: ["api", scope, reconciliationPath] }); }); }}>Freeze reconciliation</Button></div>
    <Refusal error={task.error} />
    {frozen && <><p role="status">Snapshot frozen: {frozen}</p><div><Button disabled={busy} onClick={() => onOpen(frozen)}>Open snapshot {frozen}</Button></div></>}
  </Stack></TabsPanel>;
}

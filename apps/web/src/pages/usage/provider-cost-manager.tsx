import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, platformPath, type Provider, type Session } from "../../lib/api";
import { importsPath, mappingsPath, readCostPages, readImportSummary, readLatestMapping, validateReportFile, dimensionLabel, coversPeriod, writeBinding, type Binding, type CostImport, type ImportSummary, type MappingRevision } from "../../lib/provider-cost-writes";
import { formatSignedMicroUsd, reconciliationPath } from "../../lib/provider-costs";
import { Button, FormField, Input, NativeSelect, Stack, useApiScope, useChoices } from "../../components/ui";
import { Checkbox } from "../../components/ui/checkbox/checkbox";
import { Tabs, TabsList, Tab, TabsPanel } from "../../components/ui/tabs/tabs";
import { DataTable } from "../../components/ui/data-table/data-table";
import { InfoBanner } from "../../components/templates/notices";
import s from "./provider-cost-manager.module.css";

/** No evidence Files or attestation drafts enter the Query mutation cache. */
function useWriteTask() {
  const client = useQueryClient(), request = useRef<AbortController | null>(null), active = useRef(false);
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  useEffect(() => () => { request.current?.abort(); }, []);
  async function run<T>(work: (signal: AbortSignal) => Promise<T>, done: (value: T) => void) {
    if (active.current) return;
    active.current = true; const controller = new AbortController(); request.current = controller; setBusy(true); setError(undefined);
    try { const value = await work(controller.signal); if (!controller.signal.aborted) done(value); }
    catch (e) { if (!controller.signal.aborted) { setError(e); if (e instanceof ApiError && e.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); } }
    finally { active.current = false; if (!controller.signal.aborted) setBusy(false); }
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
export function ProviderCostManager({ session, onOpen }: { session: Session; onOpen: (id: string) => void }) {
  // UI is not an authorization boundary. Rust checks live browser Admin + CSRF.
  if (session.user.platform_role !== "admin" || !session.capabilities.platform_write) return null;
  return <Manager onOpen={onOpen} />;
}
function Manager({ onOpen }: { onOpen: (id: string) => void }) {
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
  return <Stack gap={4} className={s.view}>
    <h2 ref={heading} tabIndex={-1}>Upload &amp; manage provider costs</h2>
    <InfoBanner title="Permanent financial evidence">Anthropic Cost Report envelope JSON only: account_id, evidence_reference, exported_at, optional supersedes, request and ordered pages. Upload the complete closed UTC period (1–31 days), grouped by workspace_id and description. Include supersedes in the file for changed exact-coverage revisions. No credentials, headers, prompts or responses. Retained append-only; not a verified invoice.</InfoBanner>
    <Tabs value={tab} onValueChange={value => setTab(String(value))}><Stack gap={4}>
      <TabsList variant="pills" overflow="scroll" aria-label="Provider cost administration"><Tab value="upload">Upload report</Tab><Tab value="mappings" disabled={!selected}>Mappings</Tab><Tab value="freeze" disabled={!selected}>Freeze snapshot</Tab></TabsList>
      <TabsPanel value="upload"><Stack gap={4}>
        <form onSubmit={e => { e.preventDefault(); upload(); }} className={s.form}>
          <FormField label="Anthropic report JSON" description="Maximum 5 MiB (5,242,880 bytes), 50,000 lines. Raw bytes are sent unmodified; server validation is authoritative. Duplicate JSON keys, incomplete pagination and invalid coverage are refused."><Input ref={fileInput} type="file" accept="application/json,application/*+json" disabled={task.busy} onChange={e => { setFile(e.target.files?.[0]); task.clear(); }} /></FormField>
          <div><Button type="submit" loading={task.busy}>Upload report</Button></div>
        </form>
        <Refusal error={task.error} />
        <p>Identical reuploads return the retained import, not a new revision. Changed reports never supersede automatically.</p>
        <FormField label="Existing import" description="Resume mapping or freezing a retained import without downloading its raw evidence."><NativeSelect disabled={reports.isPending || !!reports.error || task.busy} value={selected?.id ?? ""} onChange={e => select(reports.data?.find(r => r.id === e.target.value))}><option value="">Choose an import…</option>{!reports.error && reports.data?.map(r => <option key={r.id} value={r.id}>{r.account_id} · {r.period_start} – {r.period_end} · {r.id}</option>)}</NativeSelect></FormField>
        <Refusal error={reports.error} />{reports.error && <Button onClick={() => void reports.refetch()}>Reload imports</Button>}
      </Stack></TabsPanel>
      {selected && <ImportFlow key={selected.id} report={selected} resolved={resolved} mappingId={mappingId} onMapping={setMappingId} onOpen={onOpen} tab={tab} />}
    </Stack></Tabs>
  </Stack>;
}
function ImportFlow({ report, resolved, mappingId, onMapping, onOpen, tab }: { report: CostImport; resolved?: string; mappingId?: string; onMapping: (id: string) => void; onOpen: (id: string) => void; tab: string }) {
  const scope = useApiScope();
  const summary = useQuery({ queryKey: ["api", scope, importsPath, report.id, "summary"], queryFn: ({ signal }) => readImportSummary(report, signal), retry: false });
  return <>
    <section aria-label="Selected import result" aria-live="polite" className={s.facts}>
      <h3>{resolved ? `Import resolved: ${resolved}` : `Selected import: ${report.id}`}</h3>
      <p>Provider account: {report.account_id}</p><p>UTC coverage [{report.period_start}, {report.period_end})</p>
      <p>{report.supersedes ? `Supersedes import: ${report.supersedes}` : "No superseded import recorded."}</p>
      {summary.error ? <><Refusal error={summary.error} /><Button onClick={() => void summary.refetch()}>Reload import lines</Button></> : !summary.data ? <p role="status">Loading normalized import lines…</p> : <p>Imported total (signed): <strong>{formatSignedMicroUsd(summary.data.total)}</strong> · {report.line_count} lines</p>}
    </section>
    {summary.data && !summary.error && <>
      <TabsPanel value="mappings">{tab === "mappings" && <MappingStep report={report} summary={summary.data} onSaved={onMapping} />}</TabsPanel>
      <TabsPanel value="freeze">{tab === "freeze" && <FreezeStep report={report} mappingId={mappingId} onOpen={onOpen} />}</TabsPanel>
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
  if (current.error || connections.error) return <Stack gap={3}><Refusal error={current.error ?? connections.error} /><Button onClick={() => { void current.refetch(); void connections.refetch(); }}>Reload mapping data</Button></Stack>;
  if (current.isPending || connections.isPending) return <p role="status">Loading current mapping revision…</p>;
  return <Stack gap={4}>
    {saved && <p role="status">Saved mapping revision: {saved}</p>}
    <MappingEditor key={`${current.data?.id ?? "none"}/${generation}`} report={report} summary={summary} latest={current.data ?? null} connections={connections.data?.filter(c => c.provider === "anthropic") ?? []}
      onSaved={revision => { setSaved(revision.id); onSaved(revision.id); client.setQueryData(key, revision); }} onReload={async () => { const result = await current.refetch(); if (!result.error) { setSaved(undefined); setGeneration(n => n + 1); } }} />
  </Stack>;
}
function MappingEditor({ report, summary, latest, connections, onSaved, onReload }: { report: CostImport; summary: ImportSummary; latest: MappingRevision | null; connections: Provider[]; onSaved: (revision: MappingRevision) => void; onReload: () => Promise<void> }) {
  const [draft, setDraft] = useState<Binding[]>(latest?.bindings ?? []), [base, setBase] = useState(latest?.id ?? null);
  const [evidence, setEvidence] = useState(""), [confirmed, setConfirmed] = useState(false), [edit, setEdit] = useState<number>(), [formKey, setFormKey] = useState(0);
  const task = useWriteTask(), conflict = task.error instanceof ApiError && [409, 412].includes(task.error.status);
  const locked = task.busy || conflict;
  function stage(binding: Binding) { setDraft(rows => edit === undefined ? [...rows, binding] : rows.map((r, i) => i === edit ? binding : r)); setEdit(undefined); setFormKey(n => n + 1); setConfirmed(false); }
  return <Stack gap={4}>
    <InfoBanner title="Whole replacement revision">All existing bindings are retained in this draft unless explicitly removed. One provider workspace maps to one connection per effective interval. If multiple OMG connections contribute, do not attest exclusive coverage of only one. Shared actual charges are never prorated. Default/missing provider identifiers are distinct from OMG Unallocated.</InfoBanner>
    <p>Base mapping revision: {base ?? "None (first revision)"}</p>
    <DataTable caption="Provider dimensions" stack columns={[
      { id: "dimension", header: "Provider workspace dimension", rowHeader: true, cell: (id: string | null) => dimensionLabel(id) },
      { id: "coverage", header: "Draft period coverage", cell: (id: string | null) => draft.some(b => coversPeriod(b, id, report)) ? "Mapped" : "Unmapped" },
    ]} data={summary.dimensions} getRowId={id => JSON.stringify(id)} />
    <DataTable caption="Whole mapping draft" stack columns={[
      { id: "dimension", header: "Provider workspace", rowHeader: true, cell: (b: Binding) => dimensionLabel(b.provider_workspace_id) },
      { id: "scope", header: "Gateway scope", cell: (b: Binding) => <>{b.connection_id} · {b.deployment_id ?? "All deployments"} · {b.workspace_id ?? "Shared / unallocated"}</> },
      { id: "period", header: "Effective UTC interval", cell: (b: Binding) => `${b.effective_start} – ${b.effective_end}` },
      { id: "actions", header: "Actions", cell: (binding: Binding) => { const index = draft.indexOf(binding); return <><Button size="sm" variant="ghost" disabled={locked} onClick={() => { setEdit(index); setFormKey(n => n + 1); }}>Edit binding {index + 1}</Button><Button size="sm" variant="ghost" disabled={locked} onClick={() => { setDraft(rows => rows.filter((_, i) => i !== index)); setEdit(undefined); setFormKey(n => n + 1); setConfirmed(false); }}>Remove binding {index + 1}</Button></>; } },
    ]} data={draft} getRowId={(_, i) => String(i)} pageSize={10} />
    <BindingForm key={formKey} report={report} dimensions={summary.dimensions} connections={connections} initial={edit === undefined ? undefined : draft[edit]} locked={locked} onStage={stage} />
    <form className={s.form} onSubmit={e => { e.preventDefault(); if (!confirmed || locked || !evidence.trim()) return; void task.run(async signal => identifier(await api<{ id: string }>(mappingsPath, { method: "POST", signal, body: { account_id: report.account_id, supersedes: base, evidence_reference: evidence.trim(), bindings: draft.map(writeBinding) } })), id => { setBase(id); setConfirmed(false); setEvidence(""); onSaved({ id, account_id: report.account_id, supersedes: base, evidence_reference: evidence.trim(), bindings: draft.map(writeBinding) }); }); }}>
      <FormField label="Mapping revision evidence reference" description="Opaque reference, retained permanently. Not a fetched URL." ><Input value={evidence} maxLength={256} disabled={locked} onChange={e => { setEvidence(e.target.value); setConfirmed(false); }} /></FormField>
      <Checkbox label="I confirm the whole replacement mapping revision" description="Saving creates immutable history, including removals. Check every binding and its evidence before confirming." checked={confirmed} disabled={locked} onCheckedChange={setConfirmed} />
      <div><Button type="submit" disabled={!confirmed || locked || !evidence.trim()} loading={task.busy}>Save mapping revision</Button></div>
    </form>
    <Refusal error={task.error} />
    {conflict && <p>The mapping changed or the revision is no longer current. Reload and discard this draft, review the latest bindings, then attest and retry. Nothing is overwritten automatically.</p>}
    <div><Button variant="secondary" disabled={task.busy} onClick={() => void onReload()}>Reload latest mapping and discard draft</Button></div>
  </Stack>;
}
function BindingForm({ report, dimensions, connections, initial, locked, onStage }: { report: CostImport; dimensions: (string | null)[]; connections: Provider[]; initial?: Binding; locked: boolean; onStage: (binding: Binding) => void }) {
  const options = [...new Set([...dimensions, ...(initial ? [initial.provider_workspace_id] : [])])];
  const [dimension, setDimension] = useState(String(initial ? options.indexOf(initial.provider_workspace_id) : 0)), [connection, setConnection] = useState(initial?.connection_id ?? "");
  const [deployment, setDeployment] = useState(initial?.deployment_id ?? ""), [workspace, setWorkspace] = useState(initial?.workspace_id ?? "");
  const [start, setStart] = useState(initial?.effective_start ?? report.period_start), [end, setEnd] = useState(initial?.effective_end ?? report.period_end);
  const [exclusive, setExclusive] = useState(initial?.exclusive ?? false), [external, setExternal] = useState(initial?.non_omg_traffic ?? false), [attested, setAttested] = useState(false), [evidence, setEvidence] = useState(initial?.evidence_reference ?? "");
  const [error, setError] = useState<unknown>(), heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => { heading.current?.focus({ preventScroll: true }); }, []);
  function changed<T>(setter: (value: T) => void, value: T) { setter(value); setAttested(false); }
  function submit() {
    if (locked || !attested) return;
    if (!connection || !evidence.trim() || !Number.isFinite(Date.parse(start)) || !Number.isFinite(Date.parse(end)) || Date.parse(end) <= Date.parse(start) || (exclusive && (!workspace || external))) { setError(new Error("Choose a connection, evidence reference and valid effective UTC interval. Exclusive use needs an OMG workspace and no non-OMG traffic.")); return; }
    onStage({ provider_workspace_id: options[Number(dimension)] ?? null, connection_id: connection, deployment_id: deployment.trim() || null, workspace_id: workspace.trim() || null, effective_start: start, effective_end: end, exclusive, non_omg_traffic: external, attested, evidence_reference: evidence.trim(), model_mappings: initial?.model_mappings ?? [] });
  }
  return <form className={s.form} onSubmit={e => { e.preventDefault(); submit(); }}>
    <h3 ref={heading} tabIndex={-1}>{initial ? "Edit attested binding" : "Add attested binding"}</h3>
    <div className={s.grid}>
      <FormField label="Provider workspace dimension"><NativeSelect value={dimension} disabled={locked || !options.length} onChange={e => changed(setDimension, e.target.value)}>{options.map((id, i) => <option key={JSON.stringify(id)} value={String(i)}>{dimensionLabel(id)}</option>)}</NativeSelect></FormField>
      <FormField label="Gateway connection"><NativeSelect value={connection} disabled={locked} onChange={e => { changed(setConnection, e.target.value); setDeployment(""); }}><option value="">Choose an Anthropic connection…</option>{initial && !connections.some(c => c.id === initial.connection_id) && <option value={initial.connection_id}>{initial.connection_id} (retained)</option>}{connections.map(c => <option key={c.id} value={c.id}>{c.name}</option>)}</NativeSelect></FormField>
      <FormField label="Deployment ID (optional)" description="UUID; must belong to the selected connection. Empty includes all its deployments."><Input value={deployment} disabled={locked} onChange={e => changed(setDeployment, e.target.value)} /></FormField>
      <FormField label="OMG workspace ID (optional)" description="UUID; required for exclusive attribution. No personal workspace directory is fetched."><Input value={workspace} disabled={locked} onChange={e => changed(setWorkspace, e.target.value)} /></FormField>
      <FormField label="Effective start (UTC)" description="ISO 8601 timestamp, inclusive."><Input value={start} disabled={locked} onChange={e => changed(setStart, e.target.value)} /></FormField>
      <FormField label="Effective end (UTC)" description="ISO 8601 timestamp, exclusive; no proration of partial days."><Input value={end} disabled={locked} onChange={e => changed(setEnd, e.target.value)} /></FormField>
    </div>
    <FormField label="Binding evidence reference"><Input value={evidence} maxLength={256} disabled={locked} onChange={e => changed(setEvidence, e.target.value)} /></FormField>
    <Checkbox label="Exclusive OMG workspace use for this interval" checked={exclusive} disabled={locked} onCheckedChange={value => changed(setExclusive, value)} />
    <Checkbox label="Non-OMG traffic exists" checked={external} disabled={locked} onCheckedChange={value => changed(setExternal, value)} />
    <Checkbox label="I attest this binding and its effective coverage" description="Current credentials do not establish historical ownership. Shared or incomplete scope remains unallocated. Existing explicit model mappings are preserved; unknown models remain unmatched." checked={attested} disabled={locked} onCheckedChange={setAttested} />
    <div><Button type="submit" variant="secondary" disabled={locked || !attested || !options.length}>Apply binding to draft</Button></div>
    <Refusal error={error} />
  </form>;
}
function FreezeStep({ report, mappingId, onOpen }: { report: CostImport; mappingId?: string; onOpen: (id: string) => void }) {
  const scope = useApiScope(), client = useQueryClient(), task = useWriteTask(), [frozen, setFrozen] = useState<string>();
  const current = useQuery({ queryKey: ["api", scope, mappingsPath, report.account_id, "latest"], queryFn: ({ signal }) => readLatestMapping(report.account_id, signal), retry: false });
  const explicitMapping = mappingId ?? current.data?.id;
  return <Stack gap={4}>
    <h3>Freeze reconciliation for this period</h3>
    <p>Import revision: {report.id} · Mapping revision: {explicitMapping ?? "None selected"}</p>
    <p>UTC coverage [{report.period_start}, {report.period_end}). Pins these exact immutable revisions and an observation timestamp. No estimates, ledger entries or holds are changed. This is not a verified invoice; unknown comparisons remain unknown.</p>
    <Refusal error={current.error} />{current.error && <Button onClick={() => void current.refetch()}>Reload freeze mapping</Button>}
    {!current.isPending && !current.error && !explicitMapping && <p>Save a mapping revision first, including an explicitly empty replacement set for unmapped coverage.</p>}
    <div><Button disabled={current.isPending || !!current.error || !explicitMapping || !!frozen} loading={task.busy} onClick={() => { if (!explicitMapping) return; void task.run(async signal => identifier(await api<{ id: string }>(reconciliationPath, { method: "POST", signal, body: { import_id: report.id, mapping_id: explicitMapping } })), id => { setFrozen(id); void client.invalidateQueries({ queryKey: ["api", scope, reconciliationPath] }); }); }}>Freeze reconciliation</Button></div>
    <Refusal error={task.error} />
    {frozen && <><p role="status">Snapshot frozen: {frozen}</p><div><Button onClick={() => onOpen(frozen)}>Open snapshot {frozen}</Button></div></>}
  </Stack>;
}

/*
 * Workspace › Files: files kept in the gateway's encrypted file store for this
 * workspace (the same files as `GET /v1/files`): batch inputs and outputs, and
 * user files. A compact list with one toolbar row (search, purpose), Upload
 * in the header, Download and Delete per row (Delete confirms). No drawers.
 *
 * Privacy is the server's: workspace admins (and a personal workspace's
 * owner) see every file; other members see files they uploaded themselves.
 */
import { useId, useRef, useState } from "react";
import { FileText, Upload } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { api, type Session, type Workspace } from "../lib/api";
import { fileContentPath, filePath, filePurposes, filesQuery, isFilePurpose, purposeLabel, uploadErrorText, uploadFile, uploadPurposes, type FilePurpose, type FilesResponse, type GatewayFile } from "../lib/files";
import type { DashboardSearch } from "../lib/permissions";
import { Alert, Badge, Button, ErrorNotice, Heading, NativeSelect, Stack, useAction, useApi } from "../components/ui";
import { useDashboardNavigation } from "../components/navigation-link";
import { FilterToolbar } from "../components/templates/filter-toolbar";
import { ActionMenu } from "../components/templates/action-menu";
import { CopyId } from "../components/templates/copy-id";
import { StorageBar, bytesText, bytesTitle } from "../components/templates/storage-bar";
import { DataTable, type DataTableColumn } from "../components/ui/data-table/data-table";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Dialog } from "../components/ui/dialog/dialog";
import { Field } from "../components/ui/field/field";
import { Time } from "../components/ui/time/time";
import { toast } from "../components/ui/toast/toast";
import s from "./shared.module.css";
import fc from "./files.module.css";

type Scope = { session: Session; workspace: Workspace };

export function FilesPage({ workspace }: Scope) {
  const nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? { page: "files", ws: workspace.id }, ask = useAction();
  const go = (patch: Partial<DashboardSearch>) => { setCursors([]); nav?.navigate({ ...search, ...patch }); };
  const [cursors, setCursors] = useState<string[]>([]), after = cursors.at(-1);
  const query = useApi<FilesResponse>(filesQuery(workspace.id, { q: search.q, purpose: search.purpose, after }));
  const data = query.data, rows = data?.data ?? [], allowed = uploadPurposes(data?.store);
  const [uploading, setUploading] = useState(false), trigger = useRef<HTMLButtonElement>(null);
  const filtered = !!search.q || !!search.purpose;
  const columns: DataTableColumn<GatewayFile>[] = [
    { id: "name", header: "Name", rowHeader: true, hideable: false, cell: x => <span className={fc.name}><span className={s.primary}>{x.filename}</span><CopyId value={x.id} label="file ID" /></span> },
    { id: "purpose", header: "Purpose", cell: f => <Badge>{purposeLabel(f.purpose)}</Badge> },
    { id: "size", header: "Size", numeric: true, cell: f => <span title={bytesTitle(f.bytes)}>{bytesText(f.bytes)}</span> },
    { id: "created", header: "Created", cell: f => <Time value={f.created_at} format="relative" /> },
    { id: "expires", header: "Expires", defaultHiddenNarrow: true, cell: f => f.expires_at ? <Time value={f.expires_at} format="date" /> : <span className={s.muted}>Never</span> },
    { id: "status", header: "Status", defaultHiddenNarrow: true, cell: () => <Badge tone="good">Ready</Badge> },
  ];
  const remove = (f: GatewayFile) => ask({ title: `Delete ${f.filename}?`, description: "The file is deleted for everyone in this workspace, including batches that still read it. This can't be undone.", danger: true, submitLabel: "Delete file", successNotice: "File deleted.",
    run: (_, signal) => api(filePath(workspace.id, f.id), { method: "DELETE", signal }) });
  const canDelete = (f: GatewayFile) => f.mine || workspace.capabilities.view_all_activity;
  return <Stack gap={6} className={s.page}>
    <Heading title="Files" description={data?.scope === "own" ? "Files you uploaded to this workspace." : "Files kept for batches and model requests."}
      actions={<Button ref={trigger} disabled={!allowed.length} title={data && !allowed.length ? data.store.configured ? "Uploads are turned off in Settings" : "File storage isn't set up" : undefined} onClick={() => setUploading(true)}><Upload aria-hidden /> Upload</Button>} />
    {data && !data.store.configured && <Alert tone="info" title="File storage is off">A platform admin can set it up in Admin › Settings › Data &amp; privacy.</Alert>}
    {data && data.storage.used_bytes !== null && <StorageBar used={data.storage.used_bytes} quota={data.storage.quota_bytes} size="sm" />}
    {query.error && !data ? <ErrorNotice error={query.error} retry={() => void query.refetch()} /> : <div className={s.list}>
      <FilterToolbar search={{ label: "Search files", placeholder: "File name", value: search.q ?? "", onChange: next => go({ q: next || undefined }), debounceMs: 250 }}
        facets={[{ id: "purpose", label: "Purpose", type: "select", placeholder: "All purposes", options: filePurposes.map(p => ({ value: p.value, label: p.label })) }]}
        values={{ purpose: search.purpose ? [search.purpose] : [] }} onChange={next => { const picked = Array.isArray(next.purpose) ? next.purpose[0] : undefined; go({ purpose: isFilePurpose(picked) ? picked : undefined }); }} />
      <DataTable<GatewayFile> caption="Files" stack columns={columns} data={rows} getRowId={f => f.id} rowLabel={f => f.filename} manual loading={query.isFetching} error={data ? query.error : undefined} onRetry={() => void query.refetch()}
        rowActions={f => <ActionMenu label={`Actions for ${f.filename}`} actions={[
          { label: "Download", render: <a href={fileContentPath(workspace.id, f.id)} download={f.filename} /> },
          { label: "Delete…", danger: true, hidden: !canDelete(f), onSelect: () => remove(f) },
        ]} />}
        cursor={{ hasPrevious: cursors.length > 0, hasNext: !!data?.has_more, onPrevious: () => setCursors(cursors.slice(0, -1)), onNext: () => { const last = rows.at(-1); if (last) setCursors([...cursors, last.id]); } }}
        empty={<EmptyState size="compact" icon={<FileText />} title={filtered ? "No files match these filters" : "No files yet"} description={filtered ? undefined : "Upload a file here or with POST /v1/files."}
          action={filtered ? <Button size="sm" variant="ghost" onClick={() => go({ q: undefined, purpose: undefined })}>Clear filters</Button> : undefined} />} />
    </div>}
    {uploading && data && <UploadDialog workspace={workspace} purposes={allowed} maxBytes={data.max_bytes} onClose={() => { setUploading(false); requestAnimationFrame(() => trigger.current?.focus()); }} />}
  </Stack>;
}

/** Purpose and file; the upload streams to the gateway, which stores it encrypted. */
function UploadDialog({ workspace, purposes, maxBytes, onClose }: { workspace: Workspace; purposes: typeof filePurposes; maxBytes: number; onClose: () => void }) {
  const client = useQueryClient(), id = useId(), controller = useRef<AbortController | undefined>(undefined);
  const [purpose, setPurpose] = useState<FilePurpose>(purposes[0]?.value ?? "batch"), [file, setFile] = useState<File>();
  const [busy, setBusy] = useState(false), [error, setError] = useState<string>(), [submitted, setSubmitted] = useState(false);
  const tooLarge = !!file && file.size > maxBytes, fileError = submitted && !file ? "Choose a file." : tooLarge ? `Larger than the ${bytesText(maxBytes)} limit.` : undefined;
  async function submit(event: React.FormEvent) {
    event.preventDefault(); setSubmitted(true);
    if (!file || tooLarge || busy) return;
    const request = new AbortController(); controller.current = request; setBusy(true); setError(undefined);
    try { await uploadFile(workspace.id, purpose, file, request.signal); await client.invalidateQueries({ queryKey: ["api"] }); toast.success("File uploaded", file.name); onClose(); }
    catch (caught) { if (!request.signal.aborted) setError(uploadErrorText(caught)); }
    finally { setBusy(false); }
  }
  const close = () => { controller.current?.abort(); onClose(); };
  return <Dialog open title="Upload file" onOpenChange={open => { if (!open) close(); }}
    footer={<><Button variant="secondary" onClick={close}>Cancel</Button><Button type="submit" form={id} loading={busy}>Upload</Button></>}>
    <form id={id} noValidate onSubmit={event => void submit(event)} aria-busy={busy}><Stack gap={4}>
      <Field label="Purpose" description={purpose === "batch" ? "JSONL, one request per line." : purpose === "batch_output" ? undefined : "Stored now; using files in requests is coming."}>
        <NativeSelect value={purpose} disabled={busy} onChange={event => setPurpose(event.target.value as FilePurpose)}>{purposes.map(p => <option key={p.value} value={p.value}>{p.label}</option>)}</NativeSelect>
      </Field>
      <Field label="File" error={fileError} description={`Up to ${bytesText(maxBytes)}.`}>
        <input type="file" className={fc.fileInput} disabled={busy} accept={purpose === "batch" ? ".jsonl,application/jsonl" : purpose === "vision" ? "image/png,image/jpeg,image/gif,image/webp" : undefined} onChange={event => setFile(event.target.files?.[0])} />
      </Field>
      {error && <ErrorNotice error={new Error(error)} />}
    </Stack></form>
  </Dialog>;
}

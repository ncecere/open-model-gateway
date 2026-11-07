/*
 * Admin › Workspace templates (#84): reviewed onboarding templates for Teams
 * and Projects. Platform Admins create a template (version 1 waits for review),
 * another Platform Admin reviews it, then it is published and can create
 * workspaces; Platform Auditors read. The server enforces every rule (the
 * reviewer is never the author; only the published version is applied; the
 * whole workspace is created or nothing is); the dashboard only offers the
 * steps the viewer can take.
 */
import { LayoutTemplate, Plus } from "lucide-react";
import { api, platformPath, type Session } from "../lib/api";
import { applyPath, contentError, describeContent, exampleContent, summarizeContent, parseContent, templatePath, templatesPath, versionStateLabels, versionStateTone, versionStepPath, versionsPath, type TemplateApplication, type TemplateVersion, type WorkspaceTemplate, type WorkspaceTemplateSummary } from "../lib/workspace-templates";
import { nameField, type Field } from "../lib/forms";
import type { DirectoryUser } from "../lib/people";
import { Button, ErrorNotice, Heading, Inline, Stack, useAction, useApi } from "../components/ui";
import { DirectoryTable, LocalTable } from "../components/people";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { StatusBadge } from "../components/ui/badge/badge";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import s from "./shared.module.css";

const contentField = (value: string): Field => ({
  name: "content", label: "Content (JSON)", type: "textarea", required: true, value, validate: contentError,
  help: "References only: kind (team or project), catalogs (inherit, or replace with catalog ids), model ids, limits (a workspace policy body with a budgets list) or null, the owner slot and admin/member slots. No emails, secrets or platform roles.",
});
/** One searched user per slot field (people picker), or several for a multiple slot. */
const slotField = (name: string, label: string, required: boolean, multiple: boolean): Field => ({
  name: `slot:${name}`, label, type: "search", required, placeholder: "name@example.com",
  search: { path: `${platformPath}/users?status=active&role=any`, multiple, emptyText: "No one matches.", option: ((u: DirectoryUser) => u.platform_role && u.email ? { value: u.id, label: u.email } : undefined) as never },
});
function parseIds(value: string | undefined, multiple: boolean): string[] {
  if (!value) return [];
  if (!multiple) return [value];
  try { const ids = JSON.parse(value); return Array.isArray(ids) ? ids.filter((v): v is string => typeof v === "string") : []; } catch { return []; }
}

/** Admin › Workspace templates: the list, and creating a template. */
export function WorkspaceTemplatesPage({ session }: { session: Session }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const create = () => ask({
    title: "Create a workspace template", description: "Version 1 waits for review by another Platform Admin before it can be published.",
    fields: [nameField, contentField(JSON.stringify(exampleContent, null, 2))], submitLabel: "Create template", successNotice: "Template created.",
    run: (v, signal) => api<WorkspaceTemplate>(templatesPath, { method: "POST", body: { name: v.name, content: parseContent(v.content) }, signal }),
  });
  const columns: DataTableColumn<WorkspaceTemplateSummary>[] = [
    { id: "name", header: "Template", accessor: t => t.name, rowHeader: true, hideable: false, cell: t => <ResourceLink search={{ page: "workspace-template-detail", record: t.id }}>{t.name}</ResourceLink> },
    { id: "published", header: "Published version", accessor: t => t.published_version ?? 0, cell: t => t.published_version ? `Version ${t.published_version}` : <span className={s.muted}>None</span> },
    { id: "latest", header: "Latest version", accessor: t => t.latest_version, cell: t => `Version ${t.latest_version}` },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Workspace templates" description="Reviewed starting points for new Teams and Projects: catalogs, models, limits and member slots. Another Platform Admin reviews each version before it can be used." actions={writable && <Button onClick={create}><Plus aria-hidden />Create template</Button>} />
    <DirectoryTable<WorkspaceTemplateSummary> path={templatesPath} label="Workspace templates" storageKey="workspace-templates" columns={columns} rowKey={t => t.id} rowLabel={t => t.name}
      empty={{ icon: <LayoutTemplate />, title: "No templates yet.", description: writable ? "Create one to set up new Teams and Projects the same way every time." : "Platform Admins create templates." }} />
  </Stack>;
}

/** One template: its versions with the steps the viewer can take, and creating a workspace. */
export function WorkspaceTemplateDetail({ session, id }: { session: Session; id: string }) {
  const ask = useAction(), writable = session.capabilities.platform_write, nav = useDashboardNavigation();
  const q = useApi<WorkspaceTemplate>(templatePath(id), session.capabilities.platform_read);
  useResourceName(q.data?.name);
  if (q.isPending) return <p role="status">Loading template…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The returned template does not match the requested record.")} />;
  const t = q.data, published = t.versions.find(v => v.state === "published");
  const step = (v: TemplateVersion, which: "review" | "publish" | "retire") => ask({
    title: which === "review" ? `Approve version ${v.version}?` : which === "publish" ? `Publish version ${v.version}?` : `Retire version ${v.version}?`,
    danger: which === "retire",
    description: which === "review" ? "You confirm this version's content is right. You can't review a version you wrote." : which === "publish" ? "New workspaces can be created from it. It replaces the published version, if there is one (that version is retired)." : v.state === "published" ? "It can no longer be used, and no version of this template will be published. Workspaces already created from it are not changed." : "It can no longer be published or used.",
    submitLabel: which === "review" ? "Approve" : which === "publish" ? "Publish" : "Retire",
    successNotice: which === "review" ? "Version approved." : which === "publish" ? "Version published." : "Version retired.",
    run: (_, signal) => api<WorkspaceTemplate>(versionStepPath(id, v.version, which), { method: "POST", signal }),
  });
  const newVersion = () => ask({
    title: "Add a version", description: "The new version waits for review by another Platform Admin.",
    fields: [contentField(JSON.stringify((t.versions[0] ?? { content: exampleContent }).content, null, 2))], submitLabel: "Add version", successNotice: "Version added.",
    run: (v, signal) => api<WorkspaceTemplate>(versionsPath(id), { method: "POST", body: { content: parseContent(v.content) }, signal }),
  });
  const createWorkspace = (v: TemplateVersion) => {
    const key = crypto.randomUUID(), c = v.content;
    const slots = [{ name: c.owner.name, label: `${c.owner.name} (owner)`, required: true, multiple: false }, ...c.slots.map(sl => ({ name: sl.name, label: `${sl.name} (${sl.role})`, required: sl.required, multiple: sl.multiple }))];
    ask({
      title: `Create a ${c.kind === "project" ? "Project" : "Team"} from ${t.name}`,
      description: (({ catalogs, models, limits }) => `Version ${v.version}: ${catalogs} · ${models} · ${limits}. Everything is created together, or nothing is.`)(summarizeContent(c)),
      fields: [nameField, ...slots.map(sl => slotField(sl.name, sl.label, sl.required, sl.multiple))],
      submitLabel: "Create workspace", successNotice: "Workspace created.",
      run: async (values, signal) => {
        const assignments = slots.map(sl => ({ slot: sl.name, user_ids: parseIds(values[`slot:${sl.name}`], sl.multiple) })).filter(a => a.user_ids.length);
        // One slot per person (the server refuses a repeat with a generic 400).
        const seen = new Map<string, string>();
        for (const a of assignments) for (const user of a.user_ids) {
          const other = seen.get(user);
          if (other) throw new Error(`The same person is chosen for ${other} and ${a.slot}. Choose each person for one slot only.`);
          seen.set(user, a.slot);
        }
        return api<TemplateApplication>(applyPath(id), { method: "POST", body: { version: v.version, name: values.name, assignments, idempotency_key: key }, signal });
      },
      // After the dialog completes (its navigation guard allows leaving then): open the new workspace.
      after: result => nav?.navigate({ page: "workspace-detail", record: (result as TemplateApplication).workspace_id, kind: c.kind }),
    });
  };
  const isAuthor = (v: TemplateVersion) => v.author === session.user.id;
  const columns: DataTableColumn<TemplateVersion>[] = [
    { id: "version", header: "Version", accessor: v => v.version, rowHeader: true, hideable: false, cell: v => <CellText primary={`Version ${v.version}`} secondary={describeContent(v.content)} /> },
    { id: "state", header: "State", accessor: v => v.state, cell: v => <StatusBadge tone={versionStateTone[v.state]}>{versionStateLabels[v.state]}</StatusBadge> },
    { id: "author", header: "Author", accessor: v => isAuthor(v) ? "You" : "Another admin", cell: v => isAuthor(v) ? "You" : <span className={s.secondary}>Another admin</span> },
    { id: "hash", header: "Content hash", accessor: v => v.content_hash, cell: v => <><code title={v.content_hash} aria-hidden>{v.content_hash.slice(0, 12)}</code><span className="sr-only">{v.content_hash}</span></> },
    { id: "created", header: "Created", accessor: v => v.created_at, cell: v => <span className={s.secondary}>{new Date(v.created_at).toLocaleString()}</span> },
  ];
  const actions = (v: TemplateVersion) => !writable ? [] : [
    { label: "Approve…", hidden: v.state !== "pending_review", disabled: isAuthor(v), disabledReason: isAuthor(v) ? "Another Platform Admin reviews the versions you write." : undefined, onSelect: () => step(v, "review") },
    { label: "Publish…", hidden: v.state !== "reviewed", onSelect: () => step(v, "publish") },
    { label: "Create workspace…", hidden: v.state !== "published", onSelect: () => createWorkspace(v) },
    { label: "Retire…", danger: true, hidden: v.state === "retired", onSelect: () => step(v, "retire") },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title={t.name} description={published ? `Version ${published.version} is published. New Teams and Projects can be created from it.` : "No version is published yet."}
      actions={writable && <Inline gap={2}>{published && <Button onClick={() => createWorkspace(published)}>Create workspace</Button>}<Button variant="secondary" onClick={newVersion}>Add version</Button></Inline>} />
    <LocalTable<TemplateVersion> rows={t.versions} label="Versions" storageKey="workspace-template-versions" columns={columns} rowKey={v => String(v.version)} rowLabel={v => `Version ${v.version}`} rowActions={actions} empty={{ icon: <LayoutTemplate />, title: "No versions.", description: "" }} />
  </Stack>;
}

import { useState } from "react";
import { Boxes, Library, Plus, Settings2, SlidersHorizontal, UsersRound } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { api, platformPath, platformWorkspacePath, wsPath, type Session, type Workspace, type Catalog, type Model, type Grant, type CatalogAvailability, type WorkspaceKind } from "../lib/api";
import { nameField, parseCheckboxValues, type Field } from "../lib/forms";
import { kindLabels } from "../lib/people";
import { protocolLabel } from "../lib/model-setup";
import { Badge, Button, Empty, ErrorNotice, Heading, Panel, Stack, Status, Alert, useAction, useApi, useChoices } from "../components/ui";
import { Badge as BitopBadge } from "../components/ui/badge/badge";
import { FactsLine } from "../components/ui/description-list/description-list";
import { FilterToolbar } from "../components/templates/filter-toolbar";
import { IconCell, LabIcon } from "../components/provider-icon";
import { WorkspaceStatusBadge } from "../components/people";
import { NavigationGuard } from "../components/navigation-guard";
import { Card } from "../components/ui/card/card";
import { Checkbox, CheckboxGroup } from "../components/ui/checkbox/checkbox";
import { RadioGroup } from "../components/ui/radio-group/radio-group";
import { StickySaveBar } from "../components/templates/sticky-save-bar";
import { Table as BitopTable, Td, Tr } from "../components/ui/table/table";
import { toast } from "../components/ui/toast/toast";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { ResourcePage } from "../components/resource-page";
import { SettingsForm } from "../components/templates/settings-form";
import { Tabs, TabsList, Tab, TabsPanel } from "../components/ui/tabs/tabs";
import { ActionMenu } from "../components/templates/action-menu";
import s from "./shared.module.css";
import t from "../components/resource-page.module.css";
const enc = encodeURIComponent;
export const catalogFields = (catalog?: Catalog): Field[] => [{ ...nameField, value: catalog?.name }, { name: "description", label: "Description", type: "textarea", value: catalog?.description ?? "", maxLength: 2000 }];
export function catalogOverrideBody(values: Record<string, string>) { return { mode: "replace" as const, catalog_ids: parseCheckboxValues(values.catalog_ids) }; }
/*
 * Admin › Catalogs. A catalog is a set of approved models. One list shows each
 * catalog with its models and who gets it; one Defaults matrix sets which
 * catalogs each workspace type gets (live: workspaces following the defaults
 * change immediately). A workspace can instead have its own catalog choice
 * (set by a Platform Admin on the workspace's Models tab), which replaces the
 * type defaults rather than adding to them.
 */
export type CatalogModelPreview = { id: string; public_name: string; display_name: string; enabled: boolean };
/** GET /platform/catalogs rows and /catalogs/{id}: who gets it is absent from older gateways (shown as unknown). */
export type CatalogSummary = Catalog & { model_count?: number; models?: CatalogModelPreview[]; default_for?: WorkspaceKind[]; own_choice_count?: number; own_choice?: { workspaces: { id: string; name: string; kind: "team" | "project"; disabled: boolean }[]; personal_count: number } };
export type CatalogDefaults = Record<WorkspaceKind, string[]>;
const workspaceKinds = ["personal", "team", "project"] as const;
export const catalogDefaultsPath = `${platformPath}/catalog-defaults`;
export const catalogsExplainer = "A catalog is a set of approved models; each workspace type gets its defaults.";
export const defaultsLine = "Workspaces get the checked catalogs unless they choose their own.";
const retireNote = "Models added only through them are retired from those workspaces and aren't restored if you check them again.";
const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Who gets a catalog: chips for the type defaults that include it, plus workspaces whose own choice includes it. */
export function WhoGetsIt({ catalog }: { catalog: CatalogSummary }) {
  if (catalog.default_for === undefined) return <span className={s.muted}>Unknown</span>;
  const own = catalog.own_choice_count ?? 0;
  if (!catalog.default_for.length && !own) return <span className={s.muted}>No one yet</span>;
  return <span className={s.badges}>{catalog.default_for.map(k => <BitopBadge key={k} size="sm" tone="info">{kindLabels[k]} default</BitopBadge>)}{own > 0 && <BitopBadge size="sm" variant="outline">{plural(own, "workspace")} by own choice</BitopBadge>}</span>;
}
/** Model count with the first few models' icons. */
export function CatalogModelIcons({ catalog }: { catalog: CatalogSummary }) {
  const models = catalog.models ?? [], count = catalog.model_count;
  if (count === undefined) return <span className={s.muted}>Unknown</span>;
  if (!count) return <span className={s.muted}>No models yet</span>;
  return <span className={s.badges} title={models.map(m => m.display_name).join(", ")}>{models.slice(0, 5).map(m => <LabIcon key={m.id} model={[m.public_name, m.display_name]} size="sm" />)}<span>{plural(count, "model")}</span></span>;
}
export function Catalogs({ session }: { session: Session }) {
  const ask = useAction(), nav = useDashboardNavigation(), [localTab, setLocalTab] = useState("overview");
  // The former Personal/Team/Project defaults tabs are one Defaults matrix; old links land there.
  const raw = nav?.search.tab ?? localTab, tab = raw === "personal" || raw === "team" || raw === "project" ? "defaults" : raw;
  const setTab = (value: string) => nav ? nav.navigate({ ...nav.search, tab: value === "overview" ? undefined : value, q: undefined, offset: undefined }) : setLocalTab(value);
  return <Stack gap={6} className={s.page}>
    <Heading title="Catalogs" description={catalogsExplainer} actions={session.capabilities.platform_write && <Button onClick={() => ask({ title: "Create catalog", description: "Add models to it, then choose who gets it under Defaults.", fields: catalogFields(), submitLabel: "Create catalog", run: (v, signal) => api(`${platformPath}/catalogs`, { method: "POST", body: { name: v.name, description: v.description || null }, signal }) })}><Plus aria-hidden />Create catalog</Button>} />
    <Tabs value={tab} onValueChange={v => setTab(String(v))}>
      <TabsList variant="pills" className={t.list} aria-label="Catalog sections"><Tab value="overview"><Library aria-hidden />Catalogs</Tab><Tab value="defaults"><SlidersHorizontal aria-hidden />Defaults</Tab></TabsList>
      <TabsPanel value="overview"><CatalogList session={session} /></TabsPanel>
      <TabsPanel value="defaults"><DefaultsMatrix session={session} /></TabsPanel>
    </Tabs>
  </Stack>;
}
function CatalogList({ session }: { session: Session }) {
  const q = useChoices<CatalogSummary>(`${platformPath}/catalogs`, session.capabilities.platform_read), [filter, setFilter] = useState("");
  const needle = filter.trim().toLowerCase(), shown = (q.data ?? []).filter(c => !needle || `${c.name} ${c.description ?? ""}`.toLowerCase().includes(needle));
  return <Stack gap={4}>
    <FilterToolbar search={{ label: "Search catalogs", placeholder: "Name or description", value: filter, onChange: setFilter }} facets={[]} values={{}} onChange={() => undefined} />
    {q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : !q.data ? <p role="status">Loading catalogs…</p> :
      <Card flush><BitopTable caption="Catalogs" stack columns={["Catalog", "Models", "Who gets it"]} empty={!q.data.length ? <Empty title="No catalogs yet">Create a catalog, add approved models, then check who gets it under Defaults.</Empty> : !shown.length ? <Empty title="No catalogs match">Try another name.</Empty> : undefined}>
        {shown.map(c => <Tr key={c.id}>
          <Td><ResourceLink search={{ page: "catalog-detail", record: c.id }}>{c.name}</ResourceLink><span className={s.secondary}>{c.description || "No description"}</span></Td>
          <Td><CatalogModelIcons catalog={c} /></Td>
          <Td><WhoGetsIt catalog={c} /></Td>
        </Tr>)}
      </BitopTable></Card>}
  </Stack>;
}
/** One matrix for every type's live default catalogs (rows = catalogs, columns = types) with a single atomic Save. */
export function DefaultsMatrix({ session }: { session: Session }) {
  const read = session.capabilities.platform_read, writable = session.capabilities.platform_write;
  const q = useApi<CatalogDefaults>(catalogDefaultsPath, read), catalogs = useChoices<CatalogSummary>(`${platformPath}/catalogs`, read), client = useQueryClient();
  const [draft, setDraft] = useState<CatalogDefaults>(), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  if (q.isError || catalogs.isError) return <ErrorNotice error={q.error ?? catalogs.error} retry={() => { void q.refetch(); void catalogs.refetch(); }} />;
  if (!q.data || !catalogs.data) return <p role="status">Loading defaults…</p>;
  const saved = q.data, current = draft ?? saved, dirty = !!draft && workspaceKinds.some(k => !sameSet(draft[k], saved[k]));
  const names = new Map(catalogs.data.map(c => [c.id, c.name]));
  const removed = workspaceKinds.flatMap(k => saved[k].filter(id => !current[k].includes(id)).map(id => `${kindLabels[k]} › ${names.get(id) ?? "Unknown catalog"}`));
  const toggle = (kind: WorkspaceKind, id: string, on: boolean) => setDraft({ ...current, [kind]: on ? [...current[kind].filter(x => x !== id), id] : current[kind].filter(x => x !== id) });
  const reset = () => { setDraft(undefined); setError(undefined); };
  function save() {
    if (busy || !dirty) return;
    setBusy(true); setError(undefined);
    api(catalogDefaultsPath, { method: "PUT", body: current })
      .then(async () => { await client.invalidateQueries({ queryKey: ["api"] }); reset(); toast.success("Defaults saved"); })
      .catch(caught => setError(caught)).finally(() => setBusy(false));
  }
  return <Card title="Defaults" description={defaultsLine} flush>
    <BitopTable caption="Default catalogs by workspace type" columns={["Catalog", "Personal", "Team", "Project"]} empty={catalogs.data.length ? undefined : <Empty title="No catalogs yet">Create a catalog first.</Empty>}>
      {catalogs.data.map(c => <Tr key={c.id}>
        <Td><ResourceLink search={{ page: "catalog-detail", record: c.id }}>{c.name}</ResourceLink>{c.model_count !== undefined && <span className={s.secondary}>{plural(c.model_count, "model")}</span>}</Td>
        {workspaceKinds.map(k => <Td key={k}><Checkbox label={<span className="sr-only">{`${kindLabels[k]} default: ${c.name}`}</span>} checked={current[k].includes(c.id)} disabled={!writable || busy} onCheckedChange={on => toggle(k, c.id, on === true)} /></Td>)}
      </Tr>)}
    </BitopTable>
    <div className={s.pad}><Stack gap={3}>
      {dirty && removed.length > 0 && <Alert tone="warning" title="Unchecked catalogs">{removed.join(" · ")}. Workspaces using these defaults lose {removed.length === 1 ? "it" : "them"} right away. {retireNote}</Alert>}
      {error !== undefined && <ErrorNotice error={error} />}
    </Stack></div>
    {writable && <><StickySaveBar open={dirty} message="Unsaved default changes"><Button variant="secondary" disabled={busy} onClick={reset}>Discard</Button><Button loading={busy} onClick={save}>Save defaults</Button></StickySaveBar><NavigationGuard dirty={dirty && !busy} /></>}
  </Card>;
}
export function CatalogDetail({ session, id, tab, onTabChange }: { session: Session; id: string; tab?: string; onTabChange: (tab: string) => void }) {
  const q = useApi<CatalogSummary>(`${platformPath}/catalogs/${enc(id)}`, session.capabilities.platform_read), ask = useAction();
  if (q.isPending) return <p role="status">Loading catalog…</p>; if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The returned catalog does not match the requested record.")} />;
  const c = q.data;
  return <ResourcePage title={c.name} description={c.description || "A set of approved models."} facts={<FactsLine items={[{ label: "Who gets it", value: <WhoGetsIt catalog={c} /> }]} />}
    actions={session.capabilities.platform_write && <ActionMenu label="Catalog actions" actions={[{ label: "Delete catalog…", danger: true, onSelect: () => ask({ title: `Delete ${c.name}?`, description: `Removes it from the defaults and from every workspace's own catalog choice. ${retireNote.replace("them", "it")} Direct assignments stay.`, danger: true, submitLabel: "Delete catalog", run: (_, signal) => api(`${platformPath}/catalogs/${enc(id)}`, { method: "DELETE", signal }), after: () => window.history.back() }) }]} />}
    tab={tab === "models" ? "overview" : tab} onTabChange={onTabChange} tabs={[
      { value: "overview", label: "Models", icon: <Boxes aria-hidden />, count: c.model_count, content: <CatalogModels session={session} catalog={c} /> },
      { value: "workspaces", label: "Who gets it", icon: <UsersRound aria-hidden />, content: <CatalogAudience session={session} catalog={c} /> },
      { value: "settings", label: "Settings", icon: <Settings2 aria-hidden />, content: <Panel title="Catalog details"><SettingsForm fields={catalogFields(c)} writable={session.capabilities.platform_write} onSave={(v, signal) => api(`${platformPath}/catalogs/${enc(id)}`, { method: "PATCH", body: { name: v.name, description: v.description || null }, signal })} /></Panel> },
    ]} />;
}
/** The catalog's models: add with a picker, remove one at a time (with the retirement warning). */
function CatalogModels({ session, catalog }: { session: Session; catalog: CatalogSummary }) {
  const writable = session.capabilities.platform_write, path = `${platformPath}/catalogs/${enc(catalog.id)}/models`;
  const all = useChoices<Model>(`${platformPath}/models`, writable), linked = useChoices<Model>(path, session.capabilities.platform_read), ask = useAction();
  const inCatalog = (linked.data ?? []).map(m => m.id), addable = (all.data ?? []).filter(m => !inCatalog.includes(m.id));
  const put = (model_ids: string[], signal: AbortSignal) => api(path, { method: "PUT", body: { model_ids }, signal });
  const add = () => ask({ title: `Add models to ${catalog.name}`, description: "Workspaces that get this catalog can add these models right away.", fields: [{ name: "model_ids", label: "Models", type: "checkboxes", value: "[]", required: true, maxSelections: 200, options: addable.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}${m.enabled ? "" : " · disabled"}` })) }], submitLabel: "Add models", successNotice: "Models added.", run: (v, signal) => put([...inCatalog, ...parseCheckboxValues(v.model_ids)], signal) });
  const remove = (m: Model) => ask({ title: `Remove ${m.display_name} from ${catalog.name}?`, description: "Workspaces that had it only through this catalog lose it, and keys that list it stop using it. Adding it back doesn't restore those choices. Other catalogs and direct assignments still count.", danger: true, submitLabel: "Remove model", successNotice: "Model removed.", run: (_, signal) => put(inCatalog.filter(x => x !== m.id), signal) });
  return <Card title="Models" description="Workspaces that get this catalog choose which to add." actions={writable && <Button disabled={!all.data || !linked.data || addable.length === 0} onClick={add}><Plus aria-hidden />Add models</Button>} flush>
    {linked.isError ? <ErrorNotice error={linked.error} retry={() => void linked.refetch()} /> : all.isError ? <ErrorNotice error={all.error} retry={() => void all.refetch()} /> : !linked.data ? <p role="status" className={s.pad}>Loading models…</p> :
      <BitopTable caption={`Models in ${catalog.name}`} stack columns={["Model", "Protocols", "Status", ""]} empty={linked.data.length ? undefined : <Empty title="No models yet">{writable ? "Add approved models to offer them to the workspaces that get this catalog." : "No models are in this catalog."}</Empty>}>
        {linked.data.map(m => <Tr key={m.id}>
          <Td><IconCell icon={<LabIcon model={[m.public_name, m.display_name]} />}><ResourceLink search={{ page: "model-detail", record: m.id }}>{m.display_name}</ResourceLink><span className={s.secondary}>{m.public_name}</span></IconCell></Td>
          <Td>{m.supported_protocols?.length ? m.supported_protocols.map(protocolLabel).join(", ") : <span className={s.muted}>—</span>}</Td>
          <Td><Status enabled={m.enabled} /></Td>
          <Td>{writable ? <Button size="sm" variant="secondary" onClick={() => remove(m)}>Remove</Button> : null}</Td>
        </Tr>)}
      </BitopTable>}
  </Card>;
}
/** Who gets it: the type defaults (editable here, saved through the same atomic matrix) and the workspaces whose own choice includes it. */
function CatalogAudience({ session, catalog }: { session: Session; catalog: CatalogSummary }) {
  const read = session.capabilities.platform_read, writable = session.capabilities.platform_write;
  const defaults = useApi<CatalogDefaults>(catalogDefaultsPath, read), client = useQueryClient();
  const [draft, setDraft] = useState<string[]>(), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const own = catalog.own_choice;
  const serverKinds: string[] = defaults.data ? workspaceKinds.filter(k => defaults.data[k].includes(catalog.id)) : [];
  const picked = draft ?? serverKinds, dirty = !!draft && !sameSet(draft, serverKinds), removed = serverKinds.filter(k => !picked.includes(k));
  const reset = () => { setDraft(undefined); setError(undefined); };
  function save() {
    if (busy || !dirty || !defaults.data) return;
    const d = defaults.data, body = Object.fromEntries(workspaceKinds.map(k => [k, picked.includes(k) ? [...d[k].filter(x => x !== catalog.id), catalog.id] : d[k].filter(x => x !== catalog.id)]));
    setBusy(true); setError(undefined);
    api(catalogDefaultsPath, { method: "PUT", body })
      .then(async () => { await client.invalidateQueries({ queryKey: ["api"] }); reset(); toast.success("Defaults saved", catalog.name); })
      .catch(caught => setError(caught)).finally(() => setBusy(false));
  }
  return <Stack gap={6}>
    <Card title="Workspace type defaults" description={defaultsLine}>
      {defaults.isError ? <ErrorNotice error={defaults.error} retry={() => void defaults.refetch()} /> : !defaults.data ? <p role="status">Loading defaults…</p> : <Stack gap={4}>
        <CheckboxGroup legend={`Who gets ${catalog.name} by default`} value={picked} disabled={!writable || busy} onValueChange={next => setDraft(next as string[])}>{workspaceKinds.map(k => <Checkbox key={k} value={k} label={`${kindLabels[k]} workspaces`} />)}</CheckboxGroup>
        {dirty && removed.length > 0 && <Alert tone="warning">{removed.map(k => kindLabels[k as WorkspaceKind]).join(" and ")} workspaces using the defaults lose this catalog right away. Models added only through it are retired and aren't restored if you check it again.</Alert>}
        {error !== undefined && <ErrorNotice error={error} />}
        <p className={s.note}>The same setting as the <ResourceLink search={{ page: "catalogs", tab: "defaults" }}>Defaults</ResourceLink> matrix.</p>
      </Stack>}
      {writable && <><StickySaveBar open={dirty} message="Unsaved default changes"><Button variant="secondary" disabled={busy} onClick={reset}>Discard</Button><Button loading={busy} onClick={save}>Save defaults</Button></StickySaveBar><NavigationGuard dirty={dirty && !busy} /></>}
    </Card>
    <Card title="Workspaces with their own catalog choice" description="They use their own catalogs instead of the type defaults; change it on the workspace's Models tab." flush>
      {!own ? <p className={`${s.pad} ${s.muted}`}>Unknown on this gateway.</p> :
        <BitopTable caption={`Workspaces whose own catalog choice includes ${catalog.name}`} stack columns={["Workspace", "Type"]} empty={own.workspaces.length ? undefined : <Empty title="None">{own.personal_count ? "Only personal workspaces, listed below." : "Every workspace that gets this catalog gets it from the defaults."}</Empty>}>
          {own.workspaces.map(w => <Tr key={w.id}>
            <Td><ResourceLink search={{ page: "workspace-detail", record: w.id, kind: w.kind, tab: "model-access" }}>{w.name}</ResourceLink>{w.disabled && <> <WorkspaceStatusBadge disabled /></>}</Td>
            <Td>{kindLabels[w.kind]}</Td>
          </Tr>)}
        </BitopTable>}
      {own && own.personal_count > 0 && <p className={`${s.pad} ${s.note}`}>Plus {plural(own.personal_count, "personal workspace")}. Personal workspaces are private, so they aren't listed.</p>}
    </Card>
  </Stack>;
}
/*
 * Admin › Team/Project › Model access: one tab for both authorization sources.
 * Catalogs follow the live type defaults unless this workspace chooses its own
 * (a replacement, never a union). Specific models are direct platform
 * assignments, independent of catalogs. Shared by Teams and Projects.
 */
export function WorkspaceModelAccess({ session, workspace }: { session: Session; workspace: Workspace }) {
  // A disabled workspace is shown read-only: its configuration stays visible, nothing can change until it's enabled.
  return <Stack gap={6}><WorkspaceCatalogs session={session} workspaceId={workspace.id} kind={workspace.kind} readOnly={!!workspace.disabled_at} /><PlatformAssignment session={session} workspace={workspace} /></Stack>;
}
const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every(id => b.includes(id));
/** Catalog availability: live type defaults, or this workspace's own catalog choice (prefilled with what is available now). */
export function WorkspaceCatalogs({ session, workspaceId, kind = "team", readOnly = false }: { session: Session; workspaceId: string; kind?: WorkspaceKind; readOnly?: boolean }) {
  const path = `${platformWorkspacePath(workspaceId)}/catalogs`, read = session.capabilities.platform_read, writable = session.capabilities.platform_write && !readOnly;
  const q = useApi<CatalogAvailability>(path, read), defaults = useApi<{ kind: WorkspaceKind; catalog_ids: string[] }>(`${platformPath}/workspace-types/${kind}/catalogs`, read), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`, read);
  const client = useQueryClient(), ask = useAction();
  const [choice, setChoice] = useState<"defaults" | "choose">(), [selection, setSelection] = useState<string[]>(), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  if (q.isError || catalogs.isError) return <ErrorNotice error={q.error ?? catalogs.error} retry={() => { void q.refetch(); void catalogs.refetch(); }} />;
  if (!q.data || !catalogs.data) return <p role="status">Loading catalog availability…</p>;
  const noun = kindLabels[kind], lower = noun.toLowerCase(), names = new Map(catalogs.data.map(c => [c.id, c.name])), list = (ids: string[]) => ids.map(id => names.get(id) ?? "Unknown catalog").join(" · ");
  const replaced = q.data.mode === "replace", serverChoice = replaced ? "choose" : "defaults", picked = choice ?? serverChoice;
  // Live defaults by name; while inheriting, the effective list is exactly the defaults.
  const defaultIds = defaults.data?.catalog_ids ?? (replaced ? undefined : q.data.effective_catalog_ids);
  // Switching to "Choose" starts from what is available now, never from an empty list.
  const selected = selection ?? (replaced ? q.data.catalog_ids : q.data.effective_catalog_ids);
  const dirty = picked !== serverChoice || picked === "choose" && !sameSet(selected, q.data.catalog_ids);
  const removed = q.data.effective_catalog_ids.filter(id => picked === "choose" ? !selected.includes(id) : !(defaultIds ?? []).includes(id));
  const reset = () => { setChoice(undefined); setSelection(undefined); setError(undefined); };
  function save() {
    if (busy || !dirty) return;
    if (picked === "defaults") {
      ask({ title: `Use ${noun} defaults again?`, description: `Removes this ${lower}'s own catalog choice. Current and future ${noun} defaults apply automatically.${removed.length ? " Models selected only through catalogs that are no longer available are retired; they are not restored if a catalog returns." : ""}`, submitLabel: `Use ${noun} defaults`, run: async (_, signal) => { await api(path, { method: "DELETE", signal }); }, after: () => { reset(); toast.success("Catalogs saved", `${noun} defaults apply`); } });
      return;
    }
    setBusy(true); setError(undefined);
    api(path, { method: "PUT", body: { mode: "replace", catalog_ids: selected } })
      .then(async () => { await client.invalidateQueries({ queryKey: ["api"] }); reset(); toast.success("Catalogs saved", selected.length ? list(selected) : "No catalogs"); })
      .catch(caught => setError(caught)).finally(() => setBusy(false));
  }
  return <Card title="Catalogs" description="Members add models from these catalogs.">
    <Stack gap={5}>
      <RadioGroup legend="Catalog source" variant="card" orientation="horizontal" value={picked} disabled={!writable || busy} onValueChange={value => setChoice(value as "defaults" | "choose")} options={[
        { value: "defaults", label: `Use ${noun} defaults`, description: defaultIds ? defaultIds.length ? `Defaults: ${list(defaultIds)}` : `The ${noun} defaults currently include no catalogs.` : `Follows the live ${noun} defaults.` },
        { value: "choose", label: "Choose catalogs", description: "Only the catalogs picked here." },
      ]} />
      {picked === "choose" && <CheckboxGroup legend={`Catalogs for this ${lower}`} value={selected} disabled={!writable || busy} onValueChange={next => setSelection(next as string[])}>{catalogs.data.length ? catalogs.data.map(c => <Checkbox key={c.id} value={c.id} label={c.name} />) : <p className={s.muted}>No catalogs exist yet.</p>}</CheckboxGroup>}
      {picked === "choose" && selected.length === 0 && <Alert tone="warning" title="No catalogs">This {lower} will have no catalog models. Members can use only specifically assigned models below.</Alert>}
      {dirty && removed.length > 0 && <Alert tone="warning">No longer available: {list(removed)}. Models selected only through {removed.length === 1 ? "it" : "them"} are retired and are not restored if the catalog becomes available again.</Alert>}
      {error !== undefined && <ErrorNotice error={error} />}
    </Stack>
    {writable && <><StickySaveBar open={dirty} message={picked === "defaults" ? `Unsaved: use the ${noun} defaults` : "Unsaved catalog changes"}><Button variant="secondary" disabled={busy} onClick={reset}>Discard</Button><Button loading={busy} onClick={save}>Save catalogs</Button></StickySaveBar><NavigationGuard dirty={dirty && !busy} /></>}
  </Card>;
}
/** Direct platform assignments next to catalog-sourced authorizations, with each model's sources. */
export function PlatformAssignment({ session, workspace }: { session: Session; workspace: Workspace }) {
  const visible = session.capabilities.platform_write || workspace.role !== null, writable = session.capabilities.platform_write && !workspace.disabled_at;
  const models = useChoices<Model>(`${platformPath}/models`, writable), grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`, visible), ask = useAction();
  const lower = kindLabels[workspace.kind].toLowerCase(), direct = new Set(grants.data?.filter(g => g.direct_granted).map(g => g.model_id));
  const assignable = models.data?.filter(m => !direct.has(m.id)) ?? [];
  const assign = () => ask({ title: `Assign models to this ${lower}`, description: "Direct assignments are independent of catalogs. Members still choose which assigned models to use.", fields: [{ name: "model_ids", label: "Models", type: "checkboxes", value: "[]", maxSelections: 200, required: true, options: assignable.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}${m.enabled ? "" : " · disabled"}` })) }], submitLabel: "Assign models", run: async (v, signal) => { for (const id of parseCheckboxValues(v.model_ids)) await api(`${platformWorkspacePath(workspace.id)}/models`, { method: "POST", body: { model_id: id }, signal }); } });
  const remove = (g: Grant) => ask({ title: `Remove ${g.display_name || g.public_name}?`, description: g.catalog_granted ? "Removes only the direct assignment. The model stays available through this workspace's catalogs." : `Removes the direct assignment. Keys restricted to this model lose it, and it is not restored if assigned again.`, danger: true, submitLabel: "Remove assignment", run: (_, signal) => api(`${platformWorkspacePath(workspace.id)}/models/${enc(g.model_id)}`, { method: "DELETE", signal }) });
  return <Card title="Models" description="Models members added from catalogs, and models assigned directly." actions={writable && <Button disabled={!models.data || assignable.length === 0} onClick={assign}>Assign models</Button>} flush>
    {!visible ? <p className={s.pad}>Model authorizations are visible to Platform Admins and members of this {lower}.</p> : grants.isError ? <ErrorNotice error={grants.error} retry={() => void grants.refetch()} /> : models.isError ? <ErrorNotice error={models.error} /> : !grants.data ? <p role="status" className={s.pad}>Loading models…</p> :
      <BitopTable caption={`Models authorized for this ${lower}`} stack columns={["Model", "Protocols", "Source", ""]} empty={grants.data.length === 0 ? <Empty title="No models yet">Members add them from the catalogs, or assign them here.</Empty> : undefined}>
        {grants.data.map(g => <Tr key={g.model_id}>
          <Td><span className={s.primary}>{g.display_name || g.public_name}</span><span className={s.secondary}>{g.public_name}</span></Td>
          <Td>{g.supported_protocols?.length ? g.supported_protocols.map(protocolLabel).join(", ") : <span className={s.muted}>—</span>}</Td>
          <Td><span className={s.badges}>{g.catalog_granted && <Badge>Catalog</Badge>}{g.direct_granted && <Badge tone="good">Direct</Badge>}</span></Td>
          <Td>{writable && g.direct_granted ? <Button size="sm" variant="secondary" onClick={() => remove(g)}>Remove</Button> : null}</Td>
        </Tr>)}
      </BitopTable>}
  </Card>;
}
export function PlatformModelAccess({ session }: { session: Session }) { return <Catalogs session={session} />; }
export const recipientGrantPath = (workspace: string) => `${platformWorkspacePath(workspace)}/models`;

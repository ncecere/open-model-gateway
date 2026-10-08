import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, platformPath, platformWorkspacePath, wsPath, type Session, type Workspace, type Catalog, type Model, type Grant, type CatalogAvailability, type WorkspaceKind } from "../lib/api";
import { nameField, parseCheckboxValues, type Field } from "../lib/forms";
import { kindLabels } from "../lib/people";
import { protocolLabel } from "../lib/model-setup";
import { Badge, Button, CollectionTable, Empty, ErrorNotice, Heading, Panel, Stack, Alert, useAction, useApi, useChoices } from "../components/ui";
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
export function Catalogs({ session }: { session: Session }) {
  const ask = useAction(), nav = useDashboardNavigation(), [localTab, setLocalTab] = useState("overview"), tab = nav?.search.tab ?? localTab;
  const setTab = (value: string) => nav ? nav.navigate({ ...nav.search, tab: value === "overview" ? undefined : value, q: undefined, offset: undefined }) : setLocalTab(value);
  return <Stack gap={6} className={s.page}><Heading title="Catalogs" description="Groups of approved models. Choose which catalogs each workspace type gets by default." actions={session.capabilities.platform_write && <Button onClick={() => ask({ title: "Create catalog", fields: catalogFields(), submitLabel: "Create catalog", run: (v, signal) => api(`${platformPath}/catalogs`, { method: "POST", body: { name: v.name, description: v.description || null }, signal }) })}>Create catalog</Button>} /><Tabs value={tab} onValueChange={v => setTab(String(v))}><TabsList variant="pills" className={t.list} aria-label="Catalog sections"><Tab value="overview">Catalogs</Tab><Tab value="personal">Personal defaults</Tab><Tab value="team">Team defaults</Tab><Tab value="project">Project defaults</Tab></TabsList><TabsPanel value="overview"><CollectionTable<Catalog> searchable path={`${platformPath}/catalogs`} label="Catalogs" empty="Create approved model collections, then configure their live availability." rowKey={c => c.id} columns={[{ title: "Catalog", render: c => <ResourceLink search={{ page: "catalog-detail", record: c.id }}>{c.name}</ResourceLink> }, { title: "Description", render: c => c.description ?? "—" }]} /></TabsPanel>{(["personal", "team", "project"] as const).map(kind => <TabsPanel key={kind} value={kind}><TypeCatalogs kind={kind} session={session} /></TabsPanel>)}</Tabs></Stack>;
}
export function CatalogDetail({ session, id, tab, onTabChange }: { session: Session; id: string; tab?: string; onTabChange: (tab: string) => void }) {
  const q = useApi<Catalog>(`${platformPath}/catalogs/${enc(id)}`, session.capabilities.platform_read), ask = useAction();
  if (q.isPending) return <p role="status">Loading catalog…</p>; if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The returned catalog does not match the requested record.")} />;
  return <ResourcePage title={q.data.name} description={q.data.description ?? "Approved model catalog"} actions={session.capabilities.platform_write && <ActionMenu label="Catalog actions" actions={[{ label: "Delete catalog…", danger: true, onSelect: () => ask({ title: `Delete ${q.data.name}?`, description: "Removes availability from defaults and overrides. Catalog-sourced selections losing all eligibility are retired; independent direct assignments survive.", danger: true, submitLabel: "Delete catalog", run: (_, signal) => api(`${platformPath}/catalogs/${enc(id)}`, { method: "DELETE", signal }), after: () => window.history.back() }) }]} />} tab={tab} onTabChange={onTabChange} tabs={[{ value: "overview", label: "Models", content: <CatalogModels session={session} catalog={q.data} /> }, { value: "settings", label: "Settings", content: <Panel title="Catalog details"><SettingsForm fields={catalogFields(q.data)} writable={session.capabilities.platform_write} onSave={(v, signal) => api(`${platformPath}/catalogs/${enc(id)}`, { method: "PATCH", body: { name: v.name, description: v.description || null }, signal })} /></Panel> }]} />;
}
function CatalogModels({ session, catalog }: { session: Session; catalog: Catalog }) {
  const all = useChoices<Model>(`${platformPath}/models`, session.capabilities.platform_read), linked = useChoices<Model>(`${platformPath}/catalogs/${enc(catalog.id)}/models`, session.capabilities.platform_read);
  if (all.isError || linked.isError) return <ErrorNotice error={all.error ?? linked.error} retry={() => { void all.refetch(); void linked.refetch(); }} />;
  if (!all.data || !linked.data) return <p role="status">Loading catalog membership…</p>;
  return <Panel title="Linked models"><SettingsForm fields={[{ name: "model_ids", label: "Models in this catalog", type: "checkboxes", value: JSON.stringify(linked.data.map(m => m.id)), maxSelections: 200, options: all.data.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}${m.enabled ? "" : " · disabled"}` })) }]} writable={session.capabilities.platform_write} description="Workspaces see these models and choose which to add. If you remove a model from its last catalog, workspaces lose it and keys that list it stop using it. Adding it back doesn't restore those choices." onSave={(v, signal) => api(`${platformPath}/catalogs/${enc(catalog.id)}/models`, { method: "PUT", body: { model_ids: parseCheckboxValues(v.model_ids) }, signal })} /></Panel>;
}
export function TypeCatalogs({ session, kind }: { session: Session; kind: WorkspaceKind }) {
  const q = useApi<{ kind: WorkspaceKind; catalog_ids: string[] }>(`${platformPath}/workspace-types/${kind}/catalogs`, session.capabilities.platform_read), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`, session.capabilities.platform_read);
  if (q.isError || catalogs.isError) return <ErrorNotice error={q.error ?? catalogs.error} />; if (!q.data || !catalogs.data) return <p role="status">Loading defaults…</p>;
  return <Panel title={`${kind[0].toUpperCase()}${kind.slice(1)} catalog defaults`}><SettingsForm fields={[{ name: "catalog_ids", label: "Available catalogs", type: "checkboxes", value: JSON.stringify(q.data.catalog_ids), maxSelections: 200, options: catalogs.data.map(c => ({ value: c.id, label: c.name })) }]} writable={session.capabilities.platform_write} description="Changes apply right away to every workspace of this type that uses the defaults. Workspaces with their own catalog choice aren't affected." onSave={(v, signal) => api(`${platformPath}/workspace-types/${kind}/catalogs`, { method: "PUT", body: { catalog_ids: parseCheckboxValues(v.catalog_ids) }, signal })} /></Panel>;
}
/*
 * Admin › Team/Project › Model access: one tab for both authorization sources.
 * Catalogs follow the live type defaults unless this workspace chooses its own
 * (a replacement, never a union). Specific models are direct platform
 * assignments, independent of catalogs. Shared by Teams and Projects.
 */
export function WorkspaceModelAccess({ session, workspace }: { session: Session; workspace: Workspace }) {
  return <Stack gap={6}><WorkspaceCatalogs session={session} workspaceId={workspace.id} kind={workspace.kind} /><PlatformAssignment session={session} workspace={workspace} /></Stack>;
}
const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every(id => b.includes(id));
/** Catalog availability: live type defaults, or this workspace's own catalogs (prefilled with what is available now). */
export function WorkspaceCatalogs({ session, workspaceId, kind = "team" }: { session: Session; workspaceId: string; kind?: WorkspaceKind }) {
  const path = `${platformWorkspacePath(workspaceId)}/catalogs`, read = session.capabilities.platform_read, writable = session.capabilities.platform_write;
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
      ask({ title: `Use ${noun} defaults again?`, description: `Removes this ${lower}'s own catalog list. Current and future ${noun} defaults apply automatically.${removed.length ? " Models selected only through catalogs that are no longer available are retired; they are not restored if a catalog returns." : ""}`, submitLabel: `Use ${noun} defaults`, run: async (_, signal) => { await api(path, { method: "DELETE", signal }); }, after: () => { reset(); toast.success("Catalogs saved", `${noun} defaults apply`); } });
      return;
    }
    setBusy(true); setError(undefined);
    api(path, { method: "PUT", body: { mode: "replace", catalog_ids: selected } })
      .then(async () => { await client.invalidateQueries({ queryKey: ["api"] }); reset(); toast.success("Catalogs saved", selected.length ? list(selected) : "No catalogs"); })
      .catch(caught => setError(caught)).finally(() => setBusy(false));
  }
  return <Card title="Catalogs" description={`Catalogs offer approved models; members choose which to use. Available now: ${q.data.effective_catalog_ids.length ? list(q.data.effective_catalog_ids) : "none"}.`}>
    <Stack gap={5}>
      <RadioGroup legend="Catalog source" variant="card" orientation="horizontal" value={picked} disabled={!writable || busy} onValueChange={value => setChoice(value as "defaults" | "choose")} options={[
        { value: "defaults", label: `Use ${noun} defaults`, description: defaultIds ? defaultIds.length ? `Defaults: ${list(defaultIds)}` : `The ${noun} defaults currently include no catalogs.` : `Follows the live ${noun} defaults.` },
        { value: "choose", label: "Choose catalogs", description: `Only the catalogs picked here, for this ${lower}. Not added to the defaults.` },
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
  const writable = session.capabilities.platform_write, visible = writable || workspace.role !== null;
  const models = useChoices<Model>(`${platformPath}/models`, writable), grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`, visible), ask = useAction();
  const lower = kindLabels[workspace.kind].toLowerCase(), direct = new Set(grants.data?.filter(g => g.direct_granted).map(g => g.model_id));
  const assignable = models.data?.filter(m => !direct.has(m.id)) ?? [];
  const assign = () => ask({ title: `Assign models to this ${lower}`, description: "Direct assignments are independent of catalogs. Members still choose which assigned models to use.", fields: [{ name: "model_ids", label: "Models", type: "checkboxes", value: "[]", maxSelections: 200, required: true, options: assignable.map(m => ({ value: m.id, label: `${m.display_name} · ${m.public_name}${m.enabled ? "" : " · disabled"}` })) }], submitLabel: "Assign models", run: async (v, signal) => { for (const id of parseCheckboxValues(v.model_ids)) await api(`${platformWorkspacePath(workspace.id)}/models`, { method: "POST", body: { model_id: id }, signal }); } });
  const remove = (g: Grant) => ask({ title: `Remove ${g.display_name || g.public_name}?`, description: g.catalog_granted ? "Removes only the direct assignment. The model stays available through this workspace's catalogs." : `Removes the direct assignment. Keys restricted to this model lose it, and it is not restored if assigned again.`, danger: true, submitLabel: "Remove assignment", run: (_, signal) => api(`${platformWorkspacePath(workspace.id)}/models/${enc(g.model_id)}`, { method: "DELETE", signal }) });
  return <Card title="Specific models" description="Catalog models come from the catalogs above; direct models are assigned here by a Platform Admin and stay until removed, whatever the catalogs." actions={writable && <Button disabled={!models.data || assignable.length === 0} onClick={assign}>Assign models</Button>} flush>
    {!visible ? <p className={s.pad}>Model authorizations are visible to Platform Admins and members of this {lower}.</p> : grants.isError ? <ErrorNotice error={grants.error} retry={() => void grants.refetch()} /> : models.isError ? <ErrorNotice error={models.error} /> : !grants.data ? <p role="status" className={s.pad}>Loading models…</p> :
      <BitopTable caption={`Models authorized for this ${lower}`} stack columns={["Model", "Protocols", "Source", ""]} empty={grants.data.length === 0 ? <Empty title="No models yet">Choose catalogs above or assign specific models.</Empty> : undefined}>
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

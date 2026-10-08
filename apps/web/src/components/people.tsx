/*
 * People and record building blocks: avatar cells, status/role/grant badges,
 * "Copy ID" row actions and Grounded-style list tables (search, segmented
 * filters, Columns, "Rows x–y", rich empty state, row "…" menu).
 *
 * Provenance: adapted from Grounded web/src/components/person-cell.tsx,
 * web/src/pages/admin/people/common.tsx and web/src/components/templates/list-page.tsx
 * (read-only reference). OMG's lists page by offset against the gateway's
 * collection API. Search, facets and offset live in the dashboard URL (local
 * state outside the dashboard, e.g. tests); the filters are one FilterToolbar
 * row above the table with "Columns" at its end, as in Grounded.
 */
import { cloneElement, isValidElement, useEffect, useId, useState, type ReactNode } from "react";
import { Copy, X } from "lucide-react";
import type { PlatformUser, Role } from "../lib/api";
import { displayRole, grantLabel, groupGrantHint, platformRoleLabels, removableGrant, removeGrantLabel, userState, workspaceRoleLabels } from "../lib/people";
import { useChoices, useCollection } from "./ui";
import { ResourceLink, useDashboardNavigation } from "./navigation-link";
import type { DashboardSearch } from "../lib/permissions";
import { ActionMenu, type ActionItem } from "./templates/action-menu";
import { Avatar } from "./ui/avatar/avatar";
import { Badge, StatusBadge } from "./ui/badge/badge";
import { CellText, DataTable, type DataTableColumn } from "./ui/data-table/data-table";
import { EmptyState } from "./ui/empty-state/empty-state";
import { filterRows, type Facet, type FilterValues } from "./ui/filter-bar/filter-bar";
import { FilterToolbar, toolbarValuesFromSearch, toolbarValuesToSearch } from "./templates/filter-toolbar";
import { useStoredColumns } from "./templates/table-view";
import { toast } from "./ui/toast/toast";
import { Tooltip } from "./ui/tooltip/tooltip";
import s from "../pages/shared.module.css";
import p from "./people.module.css";

/** An avatar beside a name and secondary line (people circle, workspaces square). */
export function PersonCell({ name, shape, children }: { name: string; shape?: "circle" | "square"; children: ReactNode }) {
  return <span className={s.person}><Avatar name={name} shape={shape} size="sm" decorative /><span className={s.personText}>{children}</span></span>;
}
/**
 * A person as Grounded shows them (review #33, rule 15): display name over email, "(you)" for the viewer. Without a
 * display name from the identity provider, the email is the primary line. `link` makes the name open their page.
 */
export function PersonIdentity({ person, self, link }: { person: { display_name?: string | null; email: string | null }; self?: boolean; link?: DashboardSearch }) {
  const name = person.display_name?.trim() || person.email || "Retained identity", secondary = person.display_name?.trim() && person.email ? person.email : undefined;
  const primary = <>{link ? <ResourceLink search={link}>{name}</ResourceLink> : name}{self && <span className={s.muted}> (you)</span>}</>;
  return <PersonCell name={name}><CellText primary={primary} secondary={secondary} wrap="anywhere" /></PersonCell>;
}
export function UserStatusBadge({ user }: { user: Pick<PlatformUser, "disabled_at" | "cleaned_at"> }) {
  const state = userState(user);
  return state === "active" ? <StatusBadge tone="success">Active</StatusBadge> : state === "suspended" ? <StatusBadge tone="danger">Suspended</StatusBadge> : <StatusBadge tone="neutral">Cleaned</StatusBadge>;
}
export function WorkspaceStatusBadge({ disabled }: { disabled: boolean }) { return disabled ? <StatusBadge tone="warning">Disabled</StatusBadge> : <StatusBadge tone="success">Active</StatusBadge>; }
export function PlatformRoleBadge({ role }: { role?: PlatformUser["platform_role"] }) { return role ? <Badge tone="info">{platformRoleLabels[role]}</Badge> : <span className={s.muted}>—</span>; }
export function RoleBadge({ role }: { role: Role | null | undefined }) { return role ? <Badge tone={role === "owner" ? "info" : "neutral"}>{workspaceRoleLabels[role]}</Badge> : <span className={s.muted}>Inactive</span>; }
/**
 * Compact provenance badges: "Admin · group", "Owner · manual". With `onRemove`
 * (Platform Admin only), manual/bootstrap grants get an inline "×"; group grants
 * stay read-only and explain that they follow SSO.
 */
export function GrantBadges<G extends GrantLike>({ grants, empty = "—", onRemove, groupHint = groupGrantHint }: { grants?: G[] | null; empty?: ReactNode; onRemove?: (grant: G) => void; groupHint?: string }) {
  if (!grants?.length) return <span className={s.muted}>{empty}</span>;
  return <span className={s.badges}>{grants.map((g, i) => {
    const key = `${g.role}-${g.source}-${i}`;
    if (g.source === "group") return <GroupGrantBadge key={key} hint={groupHint}>{grantLabel(g)}</GroupGrantBadge>;
    return <Badge key={key} size="sm" variant="outline" tone="neutral">{grantLabel(g)}{onRemove && removableGrant(g) && <button type="button" className={p.remove} aria-label={removeGrantLabel(g)} onClick={() => onRemove(g)}><X aria-hidden /></button>}</Badge>;
  })}</span>;
}
/**
 * One role for a person: the effective role, or the highest retained grant in a
 * neutral tone while suspended/cleaned, or a neutral "No role". Status shows suspension.
 */
export function PlatformRoleCell({ user }: { user: Pick<PlatformUser, "platform_role" | "role_grants" | "disabled_at" | "cleaned_at"> }) {
  const { role, inactive } = displayRole(user);
  return role ? <Badge tone={inactive ? "neutral" : "info"}>{platformRoleLabels[role]}</Badge> : <Badge tone="neutral">No role</Badge>;
}
type GrantLike = { role: string; source: string; revoked_at?: string | null };
/** A read-only group grant with a tooltip and an always-present description. */
function GroupGrantBadge({ children, hint }: { children: ReactNode; hint: string }) {
  const id = useId();
  return <><Tooltip content={hint}><Badge size="sm" variant="outline" tone="info" tabIndex={0} aria-describedby={id} className={p.hinted}>{children}</Badge></Tooltip><span id={id} hidden>{hint}</span></>;
}

export async function copyText(value: string, what = "ID") {
  try { await navigator.clipboard.writeText(value); toast.success(`${what} copied`); }
  catch { toast.error("Couldn't copy", "The clipboard isn't available in this browser."); }
}
/** A row/header menu item that copies an identifier instead of showing it. */
export function copyIdAction(id: string | null | undefined, label = "Copy ID"): ActionItem { return { label, icon: <Copy aria-hidden />, hidden: !id, onSelect: () => void copyText(id!, label.replace(/^Copy /, "")) }; }

export type ListEmpty = { icon?: ReactNode; title: ReactNode; description?: ReactNode; action?: ReactNode; filtered?: ReactNode };
/**
 * A segmented (toggle) filter. Its id is the URL key and the query parameter sent to the server; `allLabel`
 * (default "All") is the unfiltered choice.
 */
export type ListFacet = { id: string; label: string; options: { value: string; label: string }[]; allLabel?: string };
/** Facet values the caller keeps itself (e.g. a default other than "All"), instead of the URL. */
export type FacetState = { values: FilterValues; onChange: (next: FilterValues) => void };
/** Audit lists: sign-in side effects (group sync, identity rebinds) shown or hidden; the server's `hide_sign_ins`. */
export const signInFacet: ListFacet = { id: "hide_sign_ins", label: "Sign-in events", allLabel: "Show", options: [{ value: "true", label: "Hide" }] };
type ListProps<T> = { label: string; storageKey: string; columns: DataTableColumn<T>[]; rowKey: (row: T) => string; rowLabel: (row: T) => string; rowActions?: (row: T) => ActionItem[]; empty: ListEmpty; search?: { placeholder: string } };
const toggles = <T,>(facets: (ListFacet & { accessor?: (row: T) => string | null | undefined })[]): Facet<T>[] => facets.map(f => ({ id: f.id, label: f.label, type: "toggle", allLabel: f.allLabel ?? "All", options: f.options, accessor: f.accessor }));
const one = (values: FilterValues, id: string) => { const v = values[id]; return Array.isArray(v) ? v[0] : undefined; };
const actionsFor = <T,>(rowActions: ListProps<T>["rowActions"], rowLabel: ListProps<T>["rowLabel"]) => rowActions ? (row: T) => <ActionMenu actions={rowActions(row)} label={`Actions for ${rowLabel(row)}`} /> : undefined;
/** The page header already holds the create action as its one primary button: the empty state repeats it as secondary. */
const secondary = (action: ReactNode) => isValidElement<{ variant?: string }>(action) && action.props.variant === undefined ? cloneElement(action, { variant: "secondary" }) : action;
const emptyState = (empty: ListEmpty, filtered: boolean) => <EmptyState size="compact" icon={empty.icon} title={filtered ? empty.filtered ?? "No results match these filters" : empty.title} description={filtered ? "Change the search or filters." : empty.description} action={filtered ? undefined : secondary(empty.action)} />;
type ListState = { offset?: number; q?: string } & Record<string, unknown>;
/** Search text, facet values and offset: in the dashboard URL, or local state outside the dashboard. */
function useListState<T>(facets: Facet<T>[], facetState?: FacetState) {
  const navigation = useDashboardNavigation();
  const [local, setLocal] = useState<ListState>({});
  const state: ListState = navigation?.search ?? local;
  const change = (patch: ListState) => navigation ? navigation.navigate({ ...navigation.search, ...patch }) : setLocal(prev => ({ ...prev, ...patch }));
  const values = facetState?.values ?? toolbarValuesFromSearch(state, facets);
  const setValues = (next: FilterValues) => {
    if (!facetState) return change({ ...toolbarValuesToSearch(facets, next), offset: undefined });
    facetState.onChange(next);
    if (state.offset) change({ offset: undefined });
  };
  return { state, change, values, setValues };
}

/** A server-filtered, offset-paged directory list (search, facets and offset in the URL). */
export function DirectoryTable<T>({ path, label, storageKey, columns, rowKey, rowLabel, rowActions, empty, search, facets = [], facetState, pageSize = 50, toolbar }: ListProps<T> & { path: string; facets?: ListFacet[]; facetState?: FacetState; pageSize?: number; toolbar?: ReactNode }) {
  const toolbarFacets = toggles<T>(facets), { state, change, values, setValues } = useListState(toolbarFacets, facetState);
  const offset = state.offset ?? 0, q = search ? state.q ?? "" : "";
  const params = new URLSearchParams({ limit: String(pageSize), offset: String(offset) }); if (q) params.set("q", q);
  for (const f of facets) { const v = one(values, f.id); if (v) params.set(f.id, v); }
  const query = useCollection<T>(`${path}${path.includes("?") ? "&" : "?"}${params}`);
  const rows = query.data?.data ?? [], hasMore = !!query.data && (query.data.has_more ?? rows.length === pageSize) && offset + pageSize <= 100000;
  const filtered = !!q || facets.some(f => one(values, f.id));
  const storage = `omg.enterprise.columns.${storageKey}`, cols = useStoredColumns(columns, storage), filters = !!search || facets.length > 0;
  return <div className={s.list}>
    {filters && <FilterToolbar<T> search={search ? { label: `Search ${label.toLowerCase()}`, placeholder: search.placeholder, value: q, onChange: next => change({ q: next || undefined, offset: undefined }) } : undefined}
      facets={toolbarFacets} values={values} onChange={setValues} end={(toolbar || cols.menu) && <>{toolbar}{cols.menu}</>} />}
    <DataTable<T> caption={label} stack columns={columns} data={rows} getRowId={rowKey} rowLabel={rowLabel} manual {...filters ? { hiddenColumns: cols.hidden, onHiddenColumnsChange: cols.setHidden } : { columnsMenu: true, columnsStorageKey: storage, toolbar }}
      rowActions={actionsFor(rowActions, rowLabel)} loading={query.isFetching} error={query.error} onRetry={() => void query.refetch()} empty={emptyState(empty, filtered)}
      cursor={{ hasPrevious: offset > 0, hasNext: hasMore, onPrevious: () => change({ offset: Math.max(0, offset - pageSize) }), onNext: () => change({ offset: offset + pageSize }), label: rows.length ? `Rows ${offset + 1}\u2013${offset + rows.length}${hasMore ? "" : ` of ${offset + rows.length}`}` : undefined }} />
  </div>;
}

/** A small list loaded whole (bounded), then searched, filtered and paged in memory (search and facets in the URL). */
export function LocalTable<T>({ path, rows: given, loading, error, retry, label, storageKey, columns, rowKey, rowLabel, rowActions, empty, search, facets = [], pageSize = 25 }: ListProps<T> & { path?: string; rows?: T[]; loading?: boolean; error?: unknown; retry?: () => void; facets?: (ListFacet & { accessor: (row: T) => string | null | undefined })[]; pageSize?: number }) {
  const choices = useChoices<T>(path ?? "", !!path && !given);
  const toolbarFacets = toggles<T>(facets), { state, change, values, setValues } = useListState(toolbarFacets);
  const q = search ? state.q ?? "" : "", [page, setPage] = useState(1), filterKey = JSON.stringify([q, values]);
  useEffect(() => setPage(1), [filterKey]);
  const all = given ?? choices.data ?? [], rows = facets.length ? filterRows(all, toolbarFacets, values) : all;
  const filtered = !!q || facets.some(f => one(values, f.id));
  const storage = `omg.enterprise.columns.${storageKey}`, cols = useStoredColumns(columns, storage), filters = !!search || facets.length > 0;
  return <div className={s.list}>
    {filters && <FilterToolbar<T> search={search ? { label: `Search ${label.toLowerCase()}`, placeholder: search.placeholder, value: q, onChange: next => change({ q: next || undefined }), debounceMs: 150 } : undefined}
      facets={toolbarFacets} values={values} onChange={setValues} end={cols.menu} />}
    <DataTable<T> caption={label} stack columns={columns} data={rows} getRowId={rowKey} rowLabel={rowLabel} {...filters ? { hiddenColumns: cols.hidden, onHiddenColumnsChange: cols.setHidden } : { columnsMenu: true, columnsStorageKey: storage }}
      filter={q} pageSize={pageSize} page={page} onPageChange={setPage}
      rowActions={actionsFor(rowActions, rowLabel)} loading={loading ?? (!!path && choices.isFetching)} error={error ?? choices.error} onRetry={retry ?? (() => void choices.refetch())}
      empty={emptyState(empty, filtered && all.length > 0)} noResults={filtered ? emptyState(empty, true) : undefined} />
  </div>;
}

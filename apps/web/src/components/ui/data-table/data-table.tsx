"use client";

import { ArrowDown, ArrowUp, ChevronsUpDown, Columns3, Search, X } from "lucide-react";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentPropsWithRef,
  type FocusEvent,
  type MouseEvent,
  type ReactNode,
} from "react";
import { ErrorAlert } from "@/components/ui/alert/alert";
import { Button } from "@/components/ui/button/button";
import { Checkbox } from "@/components/ui/checkbox/checkbox";
import { Field } from "@/components/ui/field/field";
import {
  activeFilterCount,
  type Facet,
  type FacetCounts,
  facetCounts as countFacets,
  FilterBar,
  type FilterBarProps,
  filterRows,
  type FilterValues,
} from "@/components/ui/filter-bar/filter-bar";
import { Input } from "@/components/ui/input/input";
import { Menu, MenuCheckboxItem, MenuGroup } from "@/components/ui/menu/menu";
import {
  Pagination,
  PaginationContent,
  PaginationItem,
  PaginationNext,
  PaginationPrevious,
  Paginator,
} from "@/components/ui/pagination/pagination";
import { Skeleton } from "@/components/ui/skeleton/skeleton";
import { Table, TableActions, Td, Th, Tr, type TableColumn, type TableProps } from "@/components/ui/table/table";
import { cx, dataFlag, NARROW_QUERY } from "@/lib/bitop-utils";
import styles from "./data-table.module.css";

/*
 * DataTable: a typed, dependency-free data grid on top of Table. Sorting,
 * row selection, a text filter and pagination are all optional; each piece
 * of state can be controlled or left to the table.
 *
 *   const columns: DataTableColumn<Invoice>[] = [
 *     { id: "number", header: "Invoice", accessor: "number", sortable: true, rowHeader: true },
 *     { id: "amount", header: "Amount", accessor: "amount", numeric: true, sortable: true,
 *       cell: (r) => formatMoney(r.amount) },
 *   ];
 *   <DataTable caption="Invoices" columns={columns} data={invoices} getRowId={(r) => r.id}
 *     selectable rowLabel={(r) => r.number} filterable pageSize={10} />
 *
 * Accessibility: sortable headers are real buttons inside <th>, and the
 * sorted column carries aria-sort. Selection checkboxes are named after the
 * row ("Select INV-001"); the header checkbox selects every row that passes
 * the filter and shows a mixed state. The filter is a labelled search input
 * and the number of matching rows is announced politely. Pagination is a
 * named navigation landmark with aria-current on the current page.
 *
 * All processing happens in memory. For server-side data, control `sort`,
 * `filter` and `page`, pass the current page's rows as `data` and set
 * `manual` plus `rowCount`.
 *
 * Async data: `loading`, `error` + `onRetry`, and either `cursor` (Previous /
 * Next for APIs that return cursors) or `loadMore` (a button after the
 * table). What the body shows:
 *
 *   rows to show?  yes → the rows. `error` adds an ErrorAlert above the
 *                        table; `loading` sets aria-busy and dims them
 *                        (stale rows never flash away).
 *                  no  → `error` (ErrorAlert in the body, Retry stays put
 *                        while `loading`) > `loading` (skeleton rows and a
 *                        polite "Loading…" status) > `empty` (no data) >
 *                        `noResults` (the filter matched nothing).
 *
 * The toolbar is always rendered. `cursor` replaces numbered pagination:
 * with it, `pageSize`/`page` are ignored. When a control that had focus
 * disappears or becomes disabled (Load more at the end, Next on the last
 * page, Retry after recovery, a deleted row's action), focus moves to the
 * current page, the remaining step button or the table, instead of <body>.
 *
 * Extras for list pages:
 *   - `columnsMenu`: a "Columns" menu of checkbox items to show and hide
 *     columns (`hideable: false` keeps one fixed; `defaultHidden` starts it
 *     hidden, `defaultHiddenNarrow` only below 600px). Control it with
 *     `hiddenColumns` / `onHiddenColumnsChange`, or let `columnsStorageKey`
 *     persist the choice in localStorage. `columnsMenuMin` leaves the menu
 *     out of small tables: it shows once that many columns can be hidden,
 *     or once one is hidden (so a column hidden on a phone can come back).
 *   - `showFilterLabel`: the search box's label is shown above it, like a
 *     facet's, instead of only to assistive technology.
 *   - `facets`: a FilterBar (toggle / select / date-range facets with
 *     counts, active-filter chips and "Clear all") under the toolbar. Rows
 *     are filtered in memory with each facet's accessor unless `manual`.
 *     Control it with `facetValues` / `onFacetValuesChange` to sync the URL
 *     (filterValuesToSearchParams / filterValuesFromSearchParams).
 *   - `bulkActions`: a bar with "N selected", your actions and "Clear
 *     selection", shown while rows are selected. It counts and passes only
 *     selected rows that pass the facets and text filter (never rows the
 *     user can't see); selections hidden by a filter are kept and come back
 *     when it's cleared. "Clear selection" clears all of them. In `manual`
 *     mode every selected id is passed.
 *   - `onRowClick`: the row opens the record. The row header cell's content
 *     (the first `rowHeader` column, else the first column) becomes a
 *     button: one tab stop per row, announced as a button named after the
 *     record, opened with Enter or Space. A click anywhere else on the row
 *     opens it too, except on links, buttons, menus, checkboxes and other
 *     controls in the row, which keep their own behaviour.
 *   - Table's `stickyHeader` (with `maxHeight`) and `density` pass through.
 */

export type SortDirection = "ascending" | "descending";
export type DataTableSort = { columnId: string; direction: SortDirection };

type Primitive = string | number | boolean | Date | null | undefined;

export type DataTableColumn<T> = {
  /** Unique column id (used for sorting). */
  id: string;
  /** Header text. Keep it plain text for sortable columns (it names the sort button). */
  header: ReactNode;
  /** The value used for sorting, filtering and (without `cell`) display. */
  accessor?: keyof T | ((row: T) => Primitive);
  /** Custom cell content. */
  cell?: (row: T) => ReactNode;
  sortable?: boolean;
  /** Custom comparator (ascending). Defaults to comparing accessor values. */
  sortFn?: (a: T, b: T) => number;
  /** Include in the text filter (default: true when there is an accessor). */
  filterable?: boolean;
  /** Right-aligned tabular numbers. */
  numeric?: boolean;
  /** Render cells as row headers (<th scope="row">), usually the name column. */
  rowHeader?: boolean;
  /** Visually hide the header text (still announced). */
  hideHeader?: boolean;
  width?: string;
  /** Muted secondary text. */
  muted?: boolean;
  /** Can be hidden from the Columns menu (default: true, except row-header columns). */
  hideable?: boolean;
  /** Start hidden (uncontrolled column visibility). */
  defaultHidden?: boolean;
  /** Start hidden on a narrow window (below 600px, NARROW_QUERY): a low-priority column; the Columns menu still offers it. */
  defaultHiddenNarrow?: boolean;
  /** Name in the Columns menu when `header` isn't plain text (default: header text, else id). */
  label?: string;
};

/** Server-driven Previous / Next paging (APIs that return next/previous cursors). */
export type DataTableCursor = {
  hasPrevious: boolean;
  hasNext: boolean;
  onPrevious: () => void;
  onNext: () => void;
  /** Range text next to the controls, e.g. "Showing 26–50". Announced politely when it changes. */
  label?: ReactNode;
};

/** A "Load more" button after the table (infinite / appended lists). */
export type DataTableLoadMore = {
  hasMore: boolean;
  /** The next batch is being fetched: the button shows a spinner and ignores clicks, but keeps focus. */
  loading?: boolean;
  onLoadMore: () => void;
  /** Button text (default "Load more"). */
  label?: string;
};

export type DataTableProps<T> = Omit<TableProps, "columns" | "children" | "empty"> & {
  columns: DataTableColumn<T>[];
  data: T[];
  /** Stable row id (default: the row index). Needed for selection across sorts and pages. */
  getRowId?: (row: T, index: number) => string;

  sort?: DataTableSort | null;
  defaultSort?: DataTableSort | null;
  onSortChange?: (sort: DataTableSort | null) => void;

  /** Adds a checkbox column. */
  selectable?: boolean;
  selectedIds?: string[];
  defaultSelectedIds?: string[];
  onSelectionChange?: (ids: string[]) => void;
  /** Names a row for its checkbox ("Select {rowLabel}"). Defaults to the row id. */
  rowLabel?: (row: T) => string;

  /** Shows a text filter above the table. */
  filterable?: boolean;
  filter?: string;
  defaultFilter?: string;
  onFilterChange?: (filter: string) => void;
  /** Label of the filter input (default "Filter rows"). */
  filterLabel?: string;
  /** Show the filter's label above it (default: only to assistive technology). */
  showFilterLabel?: boolean;
  filterPlaceholder?: string;

  /** Rows per page; omit to show every row. */
  pageSize?: number;
  /** Current page (1-based). */
  page?: number;
  defaultPage?: number;
  onPageChange?: (page: number) => void;

  /** Data is already sorted, filtered and paged (server-side); only render it. */
  manual?: boolean;
  /** Total rows when `manual` (for the page count and summary). */
  rowCount?: number;

  /** Shown when there are no rows at all. */
  empty?: ReactNode;
  /** Shown when the filter matches nothing (default: "No results for …"). */
  noResults?: ReactNode;
  /** Extra controls next to the filter, e.g. an "Export" button. */
  toolbar?: ReactNode;
  /** Name of the pagination landmark (default: "{caption} pages" if caption is a string). */
  paginationLabel?: string;

  /** Data is being fetched: aria-busy, skeleton rows when there is nothing to show, dimmed rows otherwise. */
  loading?: boolean;
  /** Number of skeleton rows while loading with no rows (default: pageSize up to 10, else 5). */
  loadingRows?: number;
  /** Announced while loading with no rows (default "Loading rows…"). */
  loadingLabel?: string;
  /** A failed fetch: shown as an ErrorAlert (in place of the body when there are no rows, else above the table). */
  error?: unknown;
  /** Adds a Retry button to the error alert. */
  onRetry?: () => void;
  /** Title of the error alert (default "Couldn't load rows"). */
  errorTitle?: ReactNode;
  /** Server-driven Previous / Next paging. Replaces numbered pagination (`pageSize` and `page` are ignored). */
  cursor?: DataTableCursor;
  /** A "Load more" button after the table; the number of new rows is announced. */
  loadMore?: DataTableLoadMore;
  /** Per-row actions in a trailing, right-aligned, unsortable column, pinned to the end edge while the table scrolls sideways. */
  rowActions?: (row: T) => ReactNode;
  /**
   * Makes each row open something (usually the record's detail sheet). The
   * row header cell's content (first `rowHeader` column, else the first
   * column) is rendered inside a button that calls this: the row's single
   * tab stop, opened with Enter or Space. A click anywhere on the row that
   * isn't on a control also calls it. Keep that cell free of links and
   * buttons (put them in other cells or `rowActions`).
   */
  onRowClick?: (row: T) => void;
  /** Optional accessible name of the row's open button, e.g. (r) => `Open ${r.name}` (default: the cell's content). */
  rowClickLabel?: (row: T) => string;
  /** Accessible (visually hidden) header of the actions column (default "Actions"). */
  rowActionsLabel?: string;

  /** Adds a "Columns" menu to show and hide columns. */
  columnsMenu?: boolean;
  /** Ids of hidden columns (controlled). */
  hiddenColumns?: string[];
  /** Initially hidden columns (default: columns with `defaultHidden`). */
  defaultHiddenColumns?: string[];
  onHiddenColumnsChange?: (hidden: string[]) => void;
  /** Persist hidden columns in localStorage under this key (uncontrolled only; read after mount, so SSR-safe). */
  columnsStorageKey?: string;
  /** Text of the Columns menu button (default "Columns"). */
  columnsMenuLabel?: string;
  /**
   * With `columnsMenu`, show the menu only when at least this many columns can be hidden, or once one
   * is hidden (default 0: always).
   */
  columnsMenuMin?: number;

  /** Faceted filters in a FilterBar under the toolbar (in-memory with accessors, or `manual`). */
  facets?: Facet<T>[];
  /** Facet values (controlled), e.g. read from the URL. */
  facetValues?: FilterValues;
  defaultFacetValues?: FilterValues;
  onFacetValuesChange?: (values: FilterValues) => void;
  /** Option counts. Default: computed from `data` (not in `manual` mode); pass your own for server data, or `false` to hide. */
  facetCounts?: FacetCounts | false;
  /** Labels of the filter bar (group name, chips, Clear all). */
  facetLabels?: FilterBarProps<T>["labels"];

  /**
   * Shown in a bar above the table while rows are selected: `(ids, clear) => <Button …>Delete</Button>`.
   * `ids` are the selected rows that pass the current facets and text filter (all selected ids in
   * `manual` mode); `clear` clears the whole selection.
   */
  bulkActions?: (selectedIds: string[], clearSelection: () => void) => ReactNode;
  /** "3 selected" text of the bulk bar. */
  selectedLabel?: (count: number) => string;
};

export type CellTextProps = Omit<ComponentPropsWithRef<"span">, "children"> & {
  /** The main line, e.g. a name. */
  primary: ReactNode;
  /** A smaller, muted second line, e.g. an email or id. */
  secondary?: ReactNode;
  /**
   * How long words break: `word` (default) wraps at spaces and breaks a word
   * only if it can't fit; `anywhere` also lets long unbroken values (ids,
   * URLs, hashes) break so they never set the column's width.
   */
  wrap?: "word" | "anywhere";
};

/**
 * The common two-line cell: a name over a muted subtitle.
 *
 *   cell: (u) => <CellText primary={u.name} secondary={u.email} />
 *
 * A space separates the lines for assistive technology and copy/paste.
 */
export function CellText({ primary, secondary, wrap = "word", className, ...props }: CellTextProps) {
  return (
    <span {...props} className={cx(styles.cellText, className)} data-wrap={wrap}>
      <span className={styles.cellPrimary}>{primary}</span>
      {secondary !== undefined && secondary !== null && secondary !== "" && (
        <>
          {" "}
          <span className={styles.cellSecondary}>{secondary}</span>
        </>
      )}
    </span>
  );
}

function useControllable<V>(value: V | undefined, defaultValue: V, onChange?: (v: V) => void): [V, (v: V) => void] {
  const [inner, setInner] = useState(defaultValue);
  const controlled = value !== undefined;
  return [
    controlled ? value : inner,
    (v: V) => {
      if (!controlled) setInner(v);
      onChange?.(v);
    },
  ];
}

function valueOf<T>(row: T, column: DataTableColumn<T>): Primitive {
  const { accessor } = column;
  if (accessor === undefined) return undefined;
  if (typeof accessor === "function") return accessor(row);
  return row[accessor] as Primitive;
}

const collator = typeof Intl !== "undefined" ? new Intl.Collator(undefined, { numeric: true, sensitivity: "base" }) : null;

/** Ascending comparison of accessor values; empty values sort last. */
export function compareValues(a: Primitive, b: Primitive): number {
  const emptyA = a === null || a === undefined || a === "";
  const emptyB = b === null || b === undefined || b === "";
  if (emptyA || emptyB) return emptyA === emptyB ? 0 : emptyA ? 1 : -1;
  if (a instanceof Date && b instanceof Date) return a.getTime() - b.getTime();
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (typeof a === "boolean" && typeof b === "boolean") return Number(a) - Number(b);
  const sa = String(a);
  const sb = String(b);
  return collator ? collator.compare(sa, sb) : sa < sb ? -1 : sa > sb ? 1 : 0;
}

const EMPTY_FILTERS: FilterValues = {};

const SKELETON_WIDTHS = ["72%", "48%", "86%", "60%", "40%"];

const CONTROL = [
  "a[href]", "button", "input", "select", "textarea", "label", "summary", "[tabindex]", '[contenteditable="true"]',
  ...["button", "link", "checkbox", "switch", "radio", "menuitem", "menuitemcheckbox", "menuitemradio", "combobox", "option", "tab", "slider", "textbox"]
    .map((role) => `[role="${role}"]`),
].join(", ");

/** Whether a click started on (or inside) a control of its own: those keep their own behaviour. */
function fromControl(target: EventTarget | null, row: HTMLElement) {
  let el = target instanceof Element ? target : null;
  while (el && el !== row) {
    if (el.matches(CONTROL)) return true;
    el = el.parentElement;
  }
  // Clicks inside portals (menus opened from the row) bubble through React, not the DOM tree.
  return target instanceof Node && !row.contains(target);
}

function textOf(v: Primitive): string {
  if (v === null || v === undefined) return "";
  if (v instanceof Date) return v.toLocaleDateString();
  return String(v);
}

export function DataTable<T>({
  columns,
  data,
  getRowId = (_row, index) => String(index),
  sort: sortProp,
  defaultSort = null,
  onSortChange,
  selectable = false,
  selectedIds: selectedProp,
  defaultSelectedIds = [],
  onSelectionChange,
  rowLabel,
  filterable = false,
  filter: filterProp,
  defaultFilter = "",
  onFilterChange,
  filterLabel = "Filter rows",
  showFilterLabel = false,
  filterPlaceholder = "Filter…",
  pageSize: pageSizeProp,
  page: pageProp,
  defaultPage = 1,
  onPageChange,
  manual = false,
  rowCount,
  empty,
  noResults,
  toolbar,
  paginationLabel,
  loading = false,
  loadingRows,
  loadingLabel = "Loading rows\u2026",
  error,
  onRetry,
  errorTitle = "Couldn't load rows",
  cursor,
  loadMore,
  rowActions,
  rowActionsLabel = "Actions",
  onRowClick,
  rowClickLabel,
  columnsMenu = false,
  hiddenColumns: hiddenProp,
  defaultHiddenColumns,
  onHiddenColumnsChange,
  columnsStorageKey,
  columnsMenuLabel = "Columns",
  columnsMenuMin = 0,
  facets,
  facetValues: facetValuesProp,
  defaultFacetValues = EMPTY_FILTERS,
  onFacetValuesChange,
  facetCounts: facetCountsProp,
  facetLabels,
  bulkActions,
  selectedLabel = (n) => `${n.toLocaleString()} selected`,
  caption,
  className,
  ...tableProps
}: DataTableProps<T>) {
  const [sort, setSort] = useControllable<DataTableSort | null>(sortProp, defaultSort, onSortChange);
  const [selected, setSelected] = useControllable<string[]>(selectedProp, defaultSelectedIds, onSelectionChange);
  const [filter, setFilterValue] = useControllable<string>(filterProp, defaultFilter, onFilterChange);
  const [page, setPage] = useControllable<number>(pageProp, defaultPage, onPageChange);
  const [facetValues, setFacetValuesState] = useControllable<FilterValues>(facetValuesProp, defaultFacetValues, onFacetValuesChange);
  // Uncontrolled visibility starts from defaultHiddenColumns, then `defaultHidden`
  // columns (plus `defaultHiddenNarrow` ones on a narrow window).
  const [innerHidden, setInnerHidden] = useState<string[]>(() => {
    if (defaultHiddenColumns) return defaultHiddenColumns;
    const narrow = typeof window !== "undefined" && (window.matchMedia?.(NARROW_QUERY).matches ?? false);
    return columns.filter((c) => c.defaultHidden || (narrow && c.defaultHiddenNarrow)).map((c) => c.id);
  });
  // The saved choice is read after mount, so the server render and the first
  // client render match (no hydration mismatch); a layout effect applies it
  // before the browser paints.
  useLayoutEffect(() => {
    if (!columnsStorageKey) return;
    try {
      const raw = window.localStorage.getItem(columnsStorageKey);
      const parsed: unknown = raw ? JSON.parse(raw) : null;
      if (Array.isArray(parsed) && parsed.every((x) => typeof x === "string")) setInnerHidden(parsed as string[]);
    } catch {
      /* storage unavailable or invalid: keep the defaults */
    }
  }, [columnsStorageKey]);
  const hidden = hiddenProp ?? innerHidden;
  function setHidden(next: string[]) {
    if (hiddenProp === undefined) {
      setInnerHidden(next);
      if (columnsStorageKey && typeof window !== "undefined") {
        try {
          window.localStorage.setItem(columnsStorageKey, JSON.stringify(next));
        } catch {
          /* ignore quota / privacy-mode errors */
        }
      }
    }
    onHiddenColumnsChange?.(next);
  }
  const hiddenSet = new Set(hidden);
  const isHideable = (c: DataTableColumn<T>) => c.hideable ?? !c.rowHeader;
  const shownColumns = columns.filter((c) => !(isHideable(c) && hiddenSet.has(c.id)));
  const hideableColumns = columns.filter(isHideable);
  // Small tables leave the menu out, unless a column is hidden (it must be possible to show it again).
  // Once shown it stays, so showing the last hidden column doesn't pull the menu from under the pointer.
  const [keepColumnsMenu, setKeepColumnsMenu] = useState(false);
  const someHidden = shownColumns.length < columns.length;
  if (someHidden && !keepColumnsMenu) setKeepColumnsMenu(true);
  const showColumnsMenu = columnsMenu && hideableColumns.length > 0 && (hideableColumns.length >= columnsMenuMin || someHidden || keepColumnsMenu);
  // With onRowClick, this column's cell holds the row's open button.
  const openColumnId = (shownColumns.find((c) => c.rowHeader) ?? shownColumns[0])?.id;
  // Cursor paging and numbered paging are mutually exclusive: the cursor wins.
  const pageSize = cursor ? undefined : pageSizeProp;

  const rootRef = useRef<HTMLDivElement>(null);
  const toolbarRef = useRef<HTMLDivElement>(null);
  const footerRef = useRef<HTMLDivElement>(null);

  /* ----- "Load more": announce how many rows arrived ----- */
  const [announcement, setAnnouncement] = useState("");
  const pendingMore = useRef<{ from: number; sawLoading: boolean } | null>(null);
  const moreLoading = Boolean(loadMore?.loading);
  const hasMore = Boolean(loadMore?.hasMore);

  function requestMore() {
    if (!loadMore || loadMore.loading) return;
    pendingMore.current = { from: data.length, sawLoading: false };
    setAnnouncement("");
    loadMore.onLoadMore();
  }

  useEffect(() => {
    const pending = pendingMore.current;
    if (!pending) return;
    if (moreLoading) {
      pending.sawLoading = true;
      return;
    }
    const added = data.length - pending.from;
    if (added > 0) {
      pendingMore.current = null;
      setAnnouncement(`${added} more ${added === 1 ? "row" : "rows"} loaded${hasMore ? "" : ". All rows loaded"}.`);
    } else if (pending.sawLoading) {
      // Finished without new rows (an error, or nothing left).
      pendingMore.current = null;
    }
  }, [data.length, moreLoading, hasMore]);

  /* ----- Keep focus when the focused control disappears or is disabled ----- */
  const lastFocused = useRef<HTMLElement | null>(null);

  function trackFocus(event: FocusEvent<HTMLDivElement>) {
    const target = event.target as HTMLElement;
    // The toolbar belongs to the consumer; leave its focus alone.
    lastFocused.current = toolbarRef.current?.contains(target) ? null : target;
  }

  function trackBlur(event: FocusEvent<HTMLDivElement>) {
    const target = event.target as HTMLElement & { disabled?: boolean };
    // Focus moved to nothing while the control is still usable (a click on
    // the page background, switching windows): not ours to restore.
    if (!event.relatedTarget && target.isConnected && !target.disabled && target === lastFocused.current) {
      lastFocused.current = null;
    }
  }

  useLayoutEffect(() => {
    const el = lastFocused.current as (HTMLElement & { disabled?: boolean }) | null;
    if (!el) return;
    const gone = !el.isConnected || el.disabled === true;
    if (!gone) return;
    const active = typeof document === "undefined" ? null : document.activeElement;
    lastFocused.current = null;
    if (active && active !== document.body && active !== el) return;
    const footer = footerRef.current;
    const inFooter = el.isConnected && footer?.contains(el);
    const target =
      (inFooter &&
        (footer!.querySelector<HTMLElement>('[aria-current="page"]') ??
          footer!.querySelector<HTMLElement>("button:not(:disabled):not([aria-disabled='true'])"))) ||
      rootRef.current?.querySelector<HTMLElement>("table");
    if (!target) return;
    if (target.tagName === "TABLE" && !target.hasAttribute("tabindex")) target.tabIndex = -1;
    target.focus();
  });

  const rows = useMemo(() => data.map((row, index) => ({ row, id: getRowId(row, index) })), [data, getRowId]);

  const faceted = useMemo(() => {
    if (manual || !facets?.length) return rows;
    const keep = new Set(filterRows(data, facets, facetValues));
    return rows.filter(({ row }) => keep.has(row));
  }, [rows, data, facets, facetValues, manual]);

  const counts = useMemo<FacetCounts | undefined>(() => {
    if (!facets?.length || facetCountsProp === false) return undefined;
    if (facetCountsProp) return facetCountsProp;
    return manual ? undefined : countFacets(data, facets, facetValues);
  }, [facets, facetCountsProp, manual, data, facetValues]);

  const filtered = useMemo(() => {
    const q = filter.trim().toLocaleLowerCase();
    if (manual || !q) return faceted;
    const searchable = columns.filter((c) => c.filterable ?? c.accessor !== undefined);
    return faceted.filter(({ row }) => searchable.some((c) => textOf(valueOf(row, c)).toLocaleLowerCase().includes(q)));
  }, [faceted, filter, columns, manual]);

  const sorted = useMemo(() => {
    if (manual || !sort) return filtered;
    const column = columns.find((c) => c.id === sort.columnId);
    if (!column) return filtered;
    const cmp = column.sortFn ?? ((a: T, b: T) => compareValues(valueOf(a, column), valueOf(b, column)));
    const dir = sort.direction === "ascending" ? 1 : -1;
    // Stable: ties keep their original order.
    return filtered
      .map((r, i) => ({ r, i }))
      .sort((x, y) => cmp(x.r.row, y.r.row) * dir || x.i - y.i)
      .map(({ r }) => r);
  }, [filtered, sort, columns, manual]);

  const total = manual ? (rowCount ?? data.length) : sorted.length;
  const pageCount = pageSize ? Math.max(1, Math.ceil(total / pageSize)) : 1;
  const currentPage = Math.min(Math.max(1, page), pageCount);
  const visible = !pageSize || manual ? sorted : sorted.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  const selectedSet = new Set(selected);
  const filteredIds = filtered.map((r) => r.id);
  // Bulk actions act only on selected rows that pass the facets and the text
  // filter; selections hidden by a filter stay selected but aren't counted or
  // passed. In `manual` mode the filtering is the server's, so every selected
  // id is passed (it may include rows on other pages).
  const filteredIdSet = new Set(filteredIds);
  const actionableIds = manual ? selected : selected.filter((id) => filteredIdSet.has(id));
  const selectedInView = filteredIds.filter((id) => selectedSet.has(id)).length;
  const allSelected = filteredIds.length > 0 && selectedInView === filteredIds.length;
  const someSelected = selectedInView > 0 && !allSelected;

  function toggleSort(columnId: string) {
    const next: DataTableSort | null =
      sort?.columnId !== columnId
        ? { columnId, direction: "ascending" }
        : sort.direction === "ascending"
          ? { columnId, direction: "descending" }
          : null;
    setSort(next);
    if (pageSize && currentPage !== 1) setPage(1);
  }

  function setFacetValues(values: FilterValues) {
    setFacetValuesState(values);
    setAnnouncement("");
    if (pageSize && currentPage !== 1) setPage(1);
  }

  function setFilter(value: string) {
    setFilterValue(value);
    setAnnouncement("");
    if (pageSize && currentPage !== 1) setPage(1);
  }

  function toggleRow(id: string, checked: boolean) {
    setSelected(checked ? [...selected.filter((s) => s !== id), id] : selected.filter((s) => s !== id));
  }

  function toggleAll(checked: boolean) {
    const inView = new Set(filteredIds);
    setSelected(checked ? [...selected.filter((s) => !inView.has(s)), ...filteredIds] : selected.filter((s) => !inView.has(s)));
  }

  const headerColumns: TableColumn[] = [
    ...(selectable
      ? [
          {
            label: (
              <Checkbox
                label={<span className="sr-only">Select all rows</span>}
                checked={allSelected}
                indeterminate={someSelected}
                disabled={filteredIds.length === 0}
                onCheckedChange={(checked) => toggleAll(checked)}
                className={styles.check}
              />
            ),
            width: "2.5rem",
          },
        ]
      : []),
    ...shownColumns.map((c) => {
      const active = sort?.columnId === c.id ? sort.direction : undefined;
      const label = c.sortable ? (
        <button type="button" className={styles.sort} data-active={active ? "" : undefined} onClick={() => toggleSort(c.id)}>
          <span className={c.hideHeader ? "sr-only" : undefined}>{c.header}</span>
          {active === "ascending" ? (
            <ArrowUp aria-hidden className={styles.sortIcon} />
          ) : active === "descending" ? (
            <ArrowDown aria-hidden className={styles.sortIcon} />
          ) : (
            <ChevronsUpDown aria-hidden className={styles.sortIcon} />
          )}
        </button>
      ) : (
        c.header
      );
      return {
        label,
        numeric: c.numeric,
        hideLabel: !c.sortable && c.hideHeader,
        width: c.width,
        sort: active,
      };
    }),
    ...(rowActions ? [{ label: rowActionsLabel, hideLabel: true, width: "1%", stickyEnd: true }] : []),
  ];

  const facetsActive = Boolean(facets?.length) && activeFilterCount(facetValues) > 0;
  const textFiltering = filter.trim() !== "";
  const filtering = textFiltering || facetsActive;
  const noRows = data.length === 0 && !filtering;
  const hasRows = visible.length > 0;
  const hasError = Boolean(error);
  // Precedence without rows: error > loading (skeletons) > empty > noResults.
  const showSkeleton = !hasRows && loading && !hasError;
  const errorAlert = hasError ? (
    <ErrorAlert
      error={error}
      title={errorTitle}
      actions={
        onRetry && (
          // Rendered as a non-native button so it keeps focus while `loading`.
          <Button size="sm" variant="secondary" loading={loading} render={<button type="button" />} onClick={onRetry}>
            Retry
          </Button>
        )
      }
    />
  ) : null;
  const emptyContent = hasRows ? undefined : hasError ? (
    <div className={styles.errorCell}>{errorAlert}</div>
  ) : showSkeleton ? undefined : noRows ? (
    (empty ?? <p className={styles.message}>No rows.</p>)
  ) : (
    (noResults ?? (
      <p className={styles.message}>
        {textFiltering ? `No results for “${filter.trim()}”${facetsActive ? " with these filters" : ""}.` : "No rows match these filters."}
      </p>
    ))
  );
  const skeletonCount = Math.max(1, loadingRows ?? (pageSize ? Math.min(pageSize, 10) : 5));
  const statusMessage = showSkeleton
    ? loadingLabel
    : announcement || (filtering ? `${total} ${total === 1 ? "row matches" : "rows match"} ${textFiltering ? "the filter" : "the filters"}` : "");

  const selectedCount = selected.length;
  const firstRow = total === 0 ? 0 : pageSize ? (currentPage - 1) * pageSize + 1 : 1;
  const lastRow = pageSize ? Math.min(total, currentPage * pageSize) : total;
  const navLabel = paginationLabel ?? (typeof caption === "string" ? `${caption} pages` : "Table pages");

  const showCursorNav = Boolean(cursor && (cursor.hasPrevious || cursor.hasNext));
  const summaryText = [
    selectable && `${selectedCount} of ${manual ? total : data.length} selected`,
    // No "Rows 0–0 of 0" under an empty state.
    pageSize && total > 0 && `Rows ${firstRow}–${lastRow} of ${total}`,
  ]
    .filter(Boolean)
    .join(" · ");
  const showLoadMore = Boolean(loadMore?.hasMore) && !(loading && data.length === 0);

  const searchBox = (
    <div className={styles.filter}>
      <Field label={filterLabel} hideLabel={!showFilterLabel}>
        <Input type="search" size="sm" value={filter} placeholder={filterPlaceholder} startIcon={<Search />} onValueChange={setFilter} />
      </Field>
    </div>
  );
  const tableActions = (
    <div className={styles.actions}>
      {toolbar}
      {showColumnsMenu && (
        <Menu
          align="end"
          trigger={
            <Button size="sm" variant="secondary">
              <Columns3 aria-hidden /> {columnsMenuLabel}
            </Button>
          }
        >
          <MenuGroup label="Show columns">
            {hideableColumns.map((c) => {
              const visible = !hiddenSet.has(c.id);
              // Keep at least one column on screen.
              const onlyOne = visible && shownColumns.length === 1;
              return (
                <MenuCheckboxItem
                  key={c.id}
                  checked={visible}
                  disabled={onlyOne}
                  onCheckedChange={(checked) => setHidden(checked ? hidden.filter((id) => id !== c.id) : [...hidden.filter((id) => id !== c.id), c.id])}
                >
                  {c.label ?? (typeof c.header === "string" ? c.header : c.id)}
                </MenuCheckboxItem>
              );
            })}
          </MenuGroup>
        </Menu>
      )}
    </div>
  );

  return (
    <div
      ref={rootRef}
      className={cx(styles.root, className)}
      data-loading={dataFlag(loading)}
      data-stale={dataFlag(loading && hasRows)}
      onFocus={trackFocus}
      onBlur={trackBlur}
    >
      {facets && facets.length > 0 ? (
        // With filters: one row, as a standalone FilterBar lays it out: the
        // search first, the filters, then the table's own actions (Columns) at the end.
        <div ref={toolbarRef}>
          <FilterBar
            facets={facets}
            value={facetValues}
            onValueChange={setFacetValues}
            counts={counts}
            labels={facetLabels}
            start={filterable ? searchBox : undefined}
            // The search text is a filter too: a chip, and "Clear all" clears it.
            search={filterable ? { value: filter, onClear: () => setFilter("") } : undefined}
            end={toolbar || showColumnsMenu ? tableActions : undefined}
          />
        </div>
      ) : (
        (filterable || toolbar || showColumnsMenu) && (
          <div ref={toolbarRef} className={styles.toolbar}>
            {filterable && searchBox}
            {(toolbar || showColumnsMenu) && tableActions}
          </div>
        )
      )}
      {bulkActions && actionableIds.length > 0 && (
        <div className={styles.bulk} role="group" aria-label="Bulk actions">
          <span className={styles.bulkCount}>{selectedLabel(actionableIds.length)}</span>
          <div className={styles.bulkActions}>{bulkActions(actionableIds, () => setSelected([]))}</div>
          <Button size="sm" variant="ghost" onClick={() => setSelected([])} className={styles.bulkClear}>
            <X aria-hidden /> Clear selection
          </Button>
        </div>
      )}
      {hasError && hasRows && errorAlert}
      <Table
        {...tableProps}
        aria-busy={loading || tableProps["aria-busy"] || undefined}
        className={styles.table}
        caption={caption}
        columns={headerColumns}
        empty={emptyContent}
      >
        {showSkeleton &&
          Array.from({ length: skeletonCount }, (_, i) => (
            <Tr key={`skeleton-${i}`} aria-hidden className={styles.skeletonRow}>
              {headerColumns.map((_c, j) => (
                <Td key={j}>
                  <Skeleton shape="text" width={SKELETON_WIDTHS[(i + j * 2) % SKELETON_WIDTHS.length]} />
                </Td>
              ))}
            </Tr>
          ))}
        {visible.map(({ row, id }) => {
          const isSelected = selectedSet.has(id);
          return (
            <Tr
              key={id}
              selected={isSelected}
              {...(onRowClick && {
                "data-clickable": "",
                // A pointer convenience; keyboard and screen-reader users use the row header's button.
                onClick: (event: MouseEvent<HTMLTableRowElement>) => {
                  if (fromControl(event.target, event.currentTarget)) return;
                  // Selecting text in a cell isn't a click on the row.
                  if (typeof window !== "undefined" && window.getSelection()?.toString()) return;
                  onRowClick(row);
                },
              })}
            >
              {selectable && (
                <Td>
                  <Checkbox
                    label={<span className="sr-only">Select {rowLabel ? rowLabel(row) : id}</span>}
                    checked={isSelected}
                    onCheckedChange={(checked) => toggleRow(id, checked)}
                    className={styles.check}
                  />
                </Td>
              )}
              {shownColumns.map((c) => {
                const value = c.cell ? c.cell(row) : textOf(valueOf(row, c));
                const content =
                  onRowClick && c.id === openColumnId ? (
                    <button type="button" className={styles.rowOpen} aria-label={rowClickLabel?.(row)} onClick={() => onRowClick(row)}>
                      {value}
                    </button>
                  ) : (
                    value
                  );
                return c.rowHeader ? (
                  <Th key={c.id}>{content}</Th>
                ) : (
                  <Td key={c.id} numeric={c.numeric} muted={c.muted}>
                    {content}
                  </Td>
                );
              })}
              {rowActions && (
                <Td nowrap stickyEnd>
                  <TableActions>{rowActions(row)}</TableActions>
                </Td>
              )}
            </Tr>
          );
        })}
      </Table>
      {showLoadMore && loadMore && (
        <div className={styles.more}>
          {/* Non-native so the button keeps focus while the next batch loads. */}
          <Button variant="secondary" size="sm" loading={moreLoading} render={<button type="button" />} onClick={requestMore}>
            {loadMore.label ?? "Load more"}
          </Button>
        </div>
      )}
      {(summaryText || (cursor && (cursor.label || showCursorNav)) || (pageSize !== undefined && pageCount > 1)) && (
        <div ref={footerRef} className={styles.footer}>
          <p className={styles.summary}>
            {summaryText}
            {cursor && (
              <>
                {summaryText && cursor.label ? " · " : null}
                <span aria-live="polite">{cursor.label}</span>
              </>
            )}
          </p>
          {pageSize !== undefined && pageCount > 1 && (
            <Paginator page={currentPage} pageCount={pageCount} onPageChange={setPage} size="sm" label={navLabel} compact />
          )}
          {cursor && showCursorNav && (
            <Pagination label={navLabel} data-compact="">
              <PaginationContent>
                <PaginationItem>
                  <PaginationPrevious
                    size="sm"
                    compact
                    disabled={!cursor.hasPrevious}
                    render={<button type="button" />}
                    onClick={() => {
                      if (!loading) cursor.onPrevious();
                    }}
                  />
                </PaginationItem>
                <PaginationItem>
                  <PaginationNext
                    size="sm"
                    compact
                    disabled={!cursor.hasNext}
                    render={<button type="button" />}
                    onClick={() => {
                      if (!loading) cursor.onNext();
                    }}
                  />
                </PaginationItem>
              </PaginationContent>
            </Pagination>
          )}
        </div>
      )}
      <p role="status" className="sr-only">
        {statusMessage}
      </p>
    </div>
  );
}

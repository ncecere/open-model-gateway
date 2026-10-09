/*
 * Table view controls: ColumnChooser + Density.
 *
 * - `ViewDataTable` wraps Bitop's DataTable (not edited) and turns on its
 *   built-in "Columns" menu plus a Density toggle in the toolbar. The view
 *   (hidden column ids + density) is controlled or uncontrolled and
 *   URL-friendly (`tableViewToSearch` / `tableViewFromSearch`).
 * - `ColumnChooser` is the same show/hide menu for hand-built Bitop `Table`s
 *   (e.g. LayerTable); it always keeps one column visible.
 * - `DensityToggle` is a labelled segmented control (comfortable/compact).
 *
 *   <ViewDataTable caption="Requests" columns={cols} data={rows} view={view} onViewChange={setView} />
 */
import { Columns3, Rows2, Rows4 } from "lucide-react";
import { type ReactNode, useLayoutEffect, useState } from "react";
import { Button } from "../ui/button/button";
import { DataTable, type DataTableColumn, type DataTableProps } from "../ui/data-table/data-table";
import { NARROW_QUERY } from "../../lib/bitop-utils";
import { Menu, MenuCheckboxItem, MenuGroup, MenuItem, MenuSeparator } from "../ui/menu/menu";
import { ToggleGroup, ToggleGroupItem } from "../ui/toggle-group/toggle-group";
import styles from "./table-view.module.css";

export type Density = "comfortable" | "compact";
export type TableView = { /** Ids of hidden columns. */ hidden: string[]; density: Density };

export type DensityToggleProps = { value: Density; onChange: (density: Density) => void; label?: string; disabled?: boolean };

/** Comfortable / compact rows as a joined two-item toggle group (icon buttons with accessible names). */
export function DensityToggle({ value, onChange, label = "Row density", disabled }: DensityToggleProps) {
  return (
    <ToggleGroup aria-label={label} joined variant="outline" size="sm" value={[value]} disabled={disabled} onValueChange={next => { const picked = next[0] as Density | undefined; if (picked && picked !== value) onChange(picked); }}>
      <ToggleGroupItem value="comfortable" iconOnly aria-label="Comfortable rows"><Rows2 aria-hidden /></ToggleGroupItem>
      <ToggleGroupItem value="compact" iconOnly aria-label="Compact rows"><Rows4 aria-hidden /></ToggleGroupItem>
    </ToggleGroup>
  );
}

export type ChooserColumn = { id: string; label: string; /** false keeps it always visible (e.g. the name column). */ hideable?: boolean };

export type ColumnChooserProps = {
  columns: ChooserColumn[];
  hidden: string[];
  onHiddenChange: (hidden: string[]) => void;
  /** Column ids hidden by default; adds a "Reset columns" item. */
  defaultHidden?: string[];
  label?: string;
  /** Adds a "Compact rows" item, so a toolbar needs no separate density toggle. */
  density?: { value: Density; onChange: (density: Density) => void };
};

/** A "Columns" menu of checkbox items to show and hide columns of a hand-built table. */
export function ColumnChooser({ columns, hidden, onHiddenChange, defaultHidden, label = "Columns", density }: ColumnChooserProps) {
  const hideable = columns.filter(c => c.hideable !== false);
  const visibleCount = columns.filter(c => !hidden.includes(c.id) || c.hideable === false).length;
  return (
    <Menu align="end" trigger={<Button size="sm" variant="secondary"><Columns3 aria-hidden /> {label}</Button>}>
      <MenuGroup label="Show columns">
        {hideable.map(c => {
          const shown = !hidden.includes(c.id);
          return <MenuCheckboxItem key={c.id} checked={shown} disabled={shown && visibleCount <= 1} onCheckedChange={checked => onHiddenChange(checked ? hidden.filter(id => id !== c.id) : [...hidden.filter(id => id !== c.id), c.id])}>{c.label}</MenuCheckboxItem>;
        })}
      </MenuGroup>
      {density && <><MenuSeparator /><MenuCheckboxItem checked={density.value === "compact"} onCheckedChange={checked => density.onChange(checked ? "compact" : "comfortable")}>Compact rows</MenuCheckboxItem></>}
      {defaultHidden && <><MenuSeparator /><MenuItem onClick={() => onHiddenChange(defaultHidden)}>Reset columns</MenuItem></>}
    </Menu>
  );
}

export type ViewDataTableProps<T> = Omit<DataTableProps<T>, "columnsMenu" | "hiddenColumns" | "onHiddenColumnsChange" | "density"> & {
  /** Controlled view. */
  view?: TableView;
  defaultView?: Partial<TableView>;
  onViewChange?: (view: TableView) => void;
  /** Hide the density toggle (keep only the Columns menu). */
  hideDensity?: boolean;
  /** Hide both the Columns menu and the density toggle (e.g. while there are no rows to arrange). */
  hideViewControls?: boolean;
};

/** Bitop DataTable with its Columns menu and a Density toggle; nothing in the vendored table is changed. */
export function ViewDataTable<T>({ view: viewProp, defaultView, onViewChange, hideDensity: hideDensityProp, hideViewControls = false, toolbar, columns, ...props }: ViewDataTableProps<T>) {
  const hideDensity = hideDensityProp || hideViewControls;
  const [inner, setInner] = useState<TableView>(() => ({ hidden: defaultView?.hidden ?? columns.filter(c => c.defaultHidden).map(c => c.id), density: defaultView?.density ?? "comfortable" }));
  const view = viewProp ?? inner;
  const update = (patch: Partial<TableView>) => { const next = { ...view, ...patch }; if (!viewProp) setInner(next); onViewChange?.(next); };
  return (
    <DataTable<T>
      {...props}
      columns={columns}
      columnsMenu={!hideViewControls}
      hiddenColumns={view.hidden}
      onHiddenColumnsChange={hidden => update({ hidden })}
      density={view.density}
      toolbar={toolbar || !hideDensity ? <span className={styles.toolbar}>{toolbar}{!hideDensity && <DensityToggle value={view.density} onChange={density => update({ density })} />}</span> : undefined}
    />
  );
}

/** Chooser entries for DataTable columns (same labels and hideability as DataTable's own Columns menu). */
export function chooserColumns<T>(columns: DataTableColumn<T>[]): ChooserColumn[] {
  return columns.map(c => ({ id: c.id, label: c.label ?? (typeof c.header === "string" ? c.header : c.id), hideable: c.hideable ?? !c.rowHeader }));
}

/**
 * DataTable's remembered column choice, lifted out so the "Columns" menu can sit at the end of a
 * FilterToolbar (Grounded's layout) instead of in its own row. Same defaults (`defaultHidden`, plus
 * `defaultHiddenNarrow` on a phone) and the same localStorage format as DataTable's `columnsStorageKey`.
 * `min` mirrors `columnsMenuMin`: the menu shows when that many columns can be hidden, or once one is.
 *
 *   const cols = useStoredColumns(columns, "omg.enterprise.columns.keys");
 *   <FilterToolbar … end={cols.menu} /> <DataTable … hiddenColumns={cols.hidden} onHiddenColumnsChange={cols.setHidden} />
 */
export function useStoredColumns<T>(columns: DataTableColumn<T>[], storageKey?: string, min = 0): { hidden: string[]; setHidden: (next: string[]) => void; menu: ReactNode } {
  const [hidden, setInner] = useState<string[]>(() => {
    const narrow = typeof window !== "undefined" && (window.matchMedia?.(NARROW_QUERY).matches ?? false);
    return columns.filter(c => c.defaultHidden || (narrow && c.defaultHiddenNarrow)).map(c => c.id);
  });
  const [kept, setKept] = useState(false);
  useLayoutEffect(() => {
    if (!storageKey) return;
    try { const parsed: unknown = JSON.parse(window.localStorage.getItem(storageKey) ?? "null"); if (Array.isArray(parsed) && parsed.every(x => typeof x === "string")) setInner(parsed as string[]); } catch { /* keep defaults */ }
  }, [storageKey]);
  const setHidden = (next: string[]) => { setInner(next); if (storageKey) try { window.localStorage.setItem(storageKey, JSON.stringify(next)); } catch { /* ignore */ } };
  const list = chooserColumns(columns), hideable = list.filter(c => c.hideable !== false), someHidden = hideable.some(c => hidden.includes(c.id));
  if (someHidden && !kept) setKept(true);
  const show = hideable.length > 0 && (hideable.length >= min || someHidden || kept);
  return { hidden, setHidden, menu: show ? <ColumnChooser columns={list} hidden={hidden} onHiddenChange={setHidden} /> : null };
}

/** URL search entries for a table view: `cols` (comma-separated hidden ids) and `density` (only when compact). */
export function tableViewToSearch(view: TableView): Record<string, string | undefined> {
  return { cols: view.hidden.length ? [...view.hidden].sort().join(",") : undefined, density: view.density === "compact" ? "compact" : undefined };
}

/** Reads a table view from URL search values; unknown column ids are dropped. */
export function tableViewFromSearch(search: Record<string, unknown>, columnIds: string[], defaults: TableView = { hidden: [], density: "comfortable" }): TableView {
  const hidden = typeof search.cols === "string" ? search.cols.split(",").filter(id => columnIds.includes(id)) : defaults.hidden;
  const density = search.density === "compact" || search.density === "comfortable" ? search.density : defaults.density;
  return { hidden, density };
}

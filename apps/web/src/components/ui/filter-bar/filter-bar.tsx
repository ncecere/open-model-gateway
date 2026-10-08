"use client";

import { X } from "lucide-react";
import { type ReactNode, useEffect, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button/button";
import { startOfDay } from "@/components/ui/calendar/calendar";
import { Combobox } from "@/components/ui/combobox/combobox";
import {
  dateRangePresets,
  DateRangePresets,
  type DateRangePreset,
  type DateRangePresetsProps,
  type DateRangeSelection,
  parseDateRangeSelection,
  serializeDateRangeSelection,
} from "@/components/ui/date-picker/date-picker";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group/toggle-group";
import { cx } from "@/lib/bitop-utils";
import styles from "./filter-bar.module.css";

/*
 * FilterBar: faceted filters above a list or table, with a chip per active
 * filter and "Clear all". Facets are data, so the same definitions drive the
 * controls, in-memory filtering (filterRows), counts (facetCounts) and the URL
 * (filterValuesToSearchParams / filterValuesFromSearchParams).
 *
 *   const facets: Facet<Doc>[] = [
 *     { id: "status", label: "Status", type: "toggle", allLabel: "All", accessor: (d) => d.status,
 *       options: [{ value: "ready", label: "Ready" }, { value: "failed", label: "Failed" }] },
 *     { id: "kind", label: "Kind", type: "select", accessor: (d) => d.kind, options: kinds },
 *     { id: "updated", label: "Updated", type: "date-range", accessor: (d) => d.updatedAt },
 *   ];
 *   <FilterBar facets={facets} value={filters} onValueChange={setFilters} counts={facetCounts(docs, facets, filters)} />
 *
 * Facet types:
 *   toggle      a ToggleGroup (single, or `multiple`); `allLabel` adds an "All" item
 *   select      a Combobox (single, or `multiple`) for long option lists (people, agents)
 *   date-range  DateRangePresets: Today / 7 / 30 / 90 days / Custom. Pressing
 *               Custom opens a range picker; the facet filters (and gets a
 *               chip) once a range is picked. Ranges are whole local days.
 *
 * State is controlled (`value` / `onValueChange`) or not (`defaultValue`).
 * A value is `{ [facetId]: string[] | DateRangeSelection }`; a missing or
 * empty entry means "no filter". To keep filters in the URL, write them with
 * filterValuesToSearchParams on change and read them back with
 * filterValuesFromSearchParams (see the DataTable docs).
 *
 * Chips: every active facet gets one, except a single-choice toggle, whose
 * pressed item already shows the choice ("Status: Active ×" next to a
 * pressed Active said it twice); its "All" item (or pressing it again)
 * clears it. A facet's `chip` overrides either way. "Clear all" shows
 * with the chips. A search box in `start` passes `search`: its text gets a
 * chip too ("Search: wifi"), and "Clear all" clears it with the facets.
 *
 * Accessibility: the bar is a group named "Filters". Each facet is labelled
 * by its visible label; counts are read after the option ("Failed, 3").
 * Active filters are a list of buttons named "Remove filter Status: Failed";
 * after removing one, focus moves to the next chip, then "Clear all", then
 * the first control. Filtering results are announced by the table/list.
 */

export type FacetOption = {
  value: string;
  label: string;
  /** Decorative icon before the label. */
  icon?: ReactNode;
  /** Group heading in select facets (options with the same group are listed together). */
  group?: string;
};

type FacetBase = {
  /** Stable id: the key in the value object and the URL parameter name. */
  id: string;
  /** Visible label and the control's accessible name. */
  label: string;
  /** A chip while active (default: true, except for single-choice toggles, whose pressed item shows the choice). */
  chip?: boolean;
};

export type ToggleFacet<T = unknown> = FacetBase & {
  type: "toggle";
  options: FacetOption[];
  /** Allow several options at once. */
  multiple?: boolean;
  /** Adds a first "All" item that clears this facet. */
  allLabel?: string;
  /** The row's value(s) for this facet (in-memory filtering and counts). */
  accessor?: (row: T) => string | string[] | null | undefined;
};

export type SelectFacet<T = unknown> = FacetBase & {
  type: "select";
  options: FacetOption[];
  multiple?: boolean;
  placeholder?: string;
  accessor?: (row: T) => string | string[] | null | undefined;
};

export type DateRangeFacet<T = unknown> = FacetBase & {
  type: "date-range";
  /** Default: Today, Last 7/30/90 days. */
  presets?: DateRangePreset[];
  allowCustom?: boolean;
  pickerProps?: DateRangePresetsProps["pickerProps"];
  /** The row's date (Date, ISO string or epoch ms). */
  accessor?: (row: T) => Date | string | number | null | undefined;
};

export type Facet<T = unknown> = ToggleFacet<T> | SelectFacet<T> | DateRangeFacet<T>;

export type FilterValue = string[] | DateRangeSelection;
export type FilterValues = Record<string, FilterValue | undefined>;
/** Per facet id, per option value: how many rows match. */
export type FacetCounts = Record<string, Record<string, number>>;

export type FilterBarLabels = {
  /** Name of the whole bar. */
  group: string;
  activeFilters: string;
  clearAll: string;
  /** "Remove filter Status: Failed" */
  remove: (facet: string, value: string) => string;
};

const defaultLabels: FilterBarLabels = {
  group: "Filters",
  activeFilters: "Active filters",
  clearAll: "Clear all",
  remove: (facet, value) => `Remove filter ${facet}: ${value}`,
};

export type FilterBarProps<T = unknown> = {
  facets: Facet<T>[];
  value?: FilterValues;
  defaultValue?: FilterValues;
  onValueChange?: (value: FilterValues) => void;
  /** Counts shown next to toggle options and as hints in select options. */
  counts?: FacetCounts;
  /** Content before the facets, e.g. a search input. */
  start?: ReactNode;
  /** Content after the facets, e.g. an Export button. */
  end?: ReactNode;
  /** Show the active-filter chips and "Clear all" (default true). */
  chips?: boolean;
  /** The text of a search box in `start`: a chip while it's not empty, cleared by its chip and by "Clear all". */
  search?: { value: string; onClear: () => void; label?: string };
  /** Control size (default "sm"). */
  size?: "sm" | "md";
  labels?: Partial<FilterBarLabels>;
  className?: string;
};

/* ---------------- Pure helpers ---------------- */

function isDateValue(v: FilterValue | undefined): v is DateRangeSelection {
  return Boolean(v) && !Array.isArray(v);
}

/** Is this facet's value an active filter? */
export function isFacetActive(value: FilterValue | undefined): boolean {
  if (!value) return false;
  return Array.isArray(value) ? value.length > 0 : Boolean(value.range);
}

/** Number of facets with an active filter. */
export function activeFilterCount(values: FilterValues): number {
  return Object.values(values).filter(isFacetActive).length;
}

function toDate(v: Date | string | number | null | undefined): Date | null {
  if (v === null || v === undefined || v === "") return null;
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? null : d;
}

function matchesFacet<T>(row: T, facet: Facet<T>, value: FilterValue | undefined): boolean {
  if (!isFacetActive(value) || !facet.accessor) return true;
  if (facet.type === "date-range") {
    const range = (value as DateRangeSelection).range!;
    const d = toDate(facet.accessor(row));
    if (!d) return false;
    const from = startOfDay(range.from).getTime();
    // The start of the next calendar day (not +24h: DST days are 23 or 25 hours long).
    const last = range.to ?? range.from;
    const to = new Date(last.getFullYear(), last.getMonth(), last.getDate() + 1).getTime();
    return d.getTime() >= from && d.getTime() < to;
  }
  const raw = facet.accessor(row);
  const rowValues = raw === null || raw === undefined ? [] : Array.isArray(raw) ? raw : [raw];
  return (value as string[]).some((v) => rowValues.includes(v));
}

/** Rows that pass every active facet (facets without an accessor are ignored). */
export function filterRows<T>(rows: T[], facets: Facet<T>[], values: FilterValues): T[] {
  const active = facets.filter((f) => isFacetActive(values[f.id]) && f.accessor);
  if (!active.length) return rows;
  return rows.filter((row) => active.every((f) => matchesFacet(row, f, values[f.id])));
}

/**
 * Option counts for toggle and select facets: for each facet, rows that
 * pass all the *other* facets, counted per option (standard faceted search,
 * so an option's count is what you'd get by picking it).
 */
export function facetCounts<T>(rows: T[], facets: Facet<T>[], values: FilterValues): FacetCounts {
  const out: FacetCounts = {};
  for (const facet of facets) {
    if (facet.type === "date-range" || !facet.accessor) continue;
    const others = facets.filter((f) => f.id !== facet.id);
    const pool = filterRows(rows, others, values);
    const counts: Record<string, number> = {};
    for (const o of facet.options) counts[o.value] = 0;
    for (const row of pool) {
      const raw = facet.accessor(row);
      const list = raw === null || raw === undefined ? [] : Array.isArray(raw) ? raw : [raw];
      for (const v of new Set(list)) if (v in counts) counts[v]! += 1;
    }
    out[facet.id] = counts;
  }
  return out;
}

/**
 * Writes the filters into URL search params (a copy of `params`): one
 * repeated parameter per selected option (`?status=failed&status=skipped`),
 * a date range as its preset id or `YYYY-MM-DD/YYYY-MM-DD`. Inactive facets
 * are removed; other parameters are kept.
 */
export function filterValuesToSearchParams<T>(facets: Facet<T>[], values: FilterValues, params?: URLSearchParams | string): URLSearchParams {
  const out = new URLSearchParams(params);
  for (const facet of facets) {
    out.delete(facet.id);
    const v = values[facet.id];
    if (!isFacetActive(v)) continue;
    if (facet.type === "date-range") out.set(facet.id, serializeDateRangeSelection(v as DateRangeSelection));
    else for (const item of v as string[]) out.append(facet.id, item);
  }
  return out;
}

/** Reads filters from URL search params; unknown option values and invalid dates are dropped. */
export function filterValuesFromSearchParams<T>(facets: Facet<T>[], params: URLSearchParams | string, today = new Date()): FilterValues {
  const input = new URLSearchParams(params);
  const out: FilterValues = {};
  for (const facet of facets) {
    if (facet.type === "date-range") {
      const sel = parseDateRangeSelection(input.get(facet.id), facet.presets ?? dateRangePresets, today);
      if (sel) out[facet.id] = sel;
      continue;
    }
    const known = new Set(facet.options.map((o) => o.value));
    const picked = input.getAll(facet.id).filter((v) => known.has(v));
    const list = facet.multiple ? [...new Set(picked)] : picked.slice(0, 1);
    if (list.length) out[facet.id] = list;
  }
  return out;
}

/* ---------------- Component ---------------- */

/** Whether an active facet gets a chip: its `chip`, else every facet but a single-choice toggle. */
function hasChip<T>(facet: Facet<T>): boolean {
  return facet.chip ?? !(facet.type === "toggle" && !facet.multiple);
}

type Chip = { key: string; facetId: string; facetLabel: string; text: string; remove: () => FilterValues };

function describeRange(sel: DateRangeSelection, presets: DateRangePreset[]): string {
  const preset = presets.find((p) => p.id === sel.preset);
  if (preset) return preset.label;
  if (!sel.range) return "";
  const f = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric" });
  const to = sel.range.to ?? sel.range.from;
  return `${f.format(sel.range.from)} – ${f.format(to)}`;
}

export function FilterBar<T>({
  facets,
  value: valueProp,
  defaultValue = {},
  onValueChange,
  counts,
  start,
  end,
  chips = true,
  search,
  size = "sm",
  labels: labelsProp,
  className,
}: FilterBarProps<T>) {
  const labels = { ...defaultLabels, ...labelsProp };
  const [inner, setInner] = useState<FilterValues>(defaultValue);
  const value = valueProp ?? inner;
  const set = (next: FilterValues) => {
    if (valueProp === undefined) setInner(next);
    onValueChange?.(next);
  };
  const setFacet = (id: string, v: FilterValue | undefined) => {
    const next = { ...value };
    if (isFacetActive(v)) next[id] = v;
    else delete next[id];
    set(next);
  };

  const baseId = useId();
  const rootRef = useRef<HTMLDivElement>(null);
  const chipsRef = useRef<HTMLUListElement>(null);
  const [focusChip, setFocusChip] = useState<number | null>(null);

  const chipList: Chip[] = [];
  const searchText = search?.value.trim() ?? "";
  if (search && searchText) {
    chipList.push({
      key: "search",
      facetId: "search",
      facetLabel: search.label ?? "Search",
      text: searchText,
      remove: () => {
        search.onClear();
        return value;
      },
    });
  }
  for (const facet of facets) {
    const v = value[facet.id];
    if (!isFacetActive(v) || !hasChip(facet)) continue;
    if (facet.type === "date-range") {
      chipList.push({
        key: facet.id,
        facetId: facet.id,
        facetLabel: facet.label,
        text: describeRange(v as DateRangeSelection, facet.presets ?? dateRangePresets),
        remove: () => {
          const next = { ...value };
          delete next[facet.id];
          return next;
        },
      });
      continue;
    }
    for (const item of v as string[]) {
      chipList.push({
        key: `${facet.id}:${item}`,
        facetId: facet.id,
        facetLabel: facet.label,
        text: facet.options.find((o) => o.value === item)?.label ?? item,
        remove: () => {
          const rest = (value[facet.id] as string[]).filter((x) => x !== item);
          const next = { ...value };
          if (rest.length) next[facet.id] = rest;
          else delete next[facet.id];
          return next;
        },
      });
    }
  }

  // After a chip is removed, move focus to the next chip, "Clear all", or the first control.
  useEffect(() => {
    if (focusChip === null) return;
    setFocusChip(null);
    const buttons = chipsRef.current?.querySelectorAll<HTMLElement>("button");
    const target =
      (buttons && buttons.length ? buttons[Math.min(focusChip, buttons.length - 1)] : null) ??
      rootRef.current?.querySelector<HTMLElement>("button, input, [tabindex='0']");
    target?.focus();
  }, [focusChip]);

  return (
    <div ref={rootRef} role="group" aria-label={labels.group} className={cx(styles.root, className)}>
      <div className={styles.controls}>
        {start}
        {facets.map((facet) => {
          const labelId = `${baseId}-${facet.id}`;
          const facetCounts = counts?.[facet.id];
          return (
            <div key={facet.id} className={styles.facet}>
              <span id={labelId} className={styles.label}>
                {facet.label}
              </span>
              {facet.type === "toggle" ? (
                <ToggleGroup
                  aria-labelledby={labelId}
                  size={size}
                  variant="outline"
                  joined
                  multiple={facet.multiple}
                  value={facet.allLabel && !isFacetActive(value[facet.id]) ? ["__all"] : ((value[facet.id] as string[] | undefined) ?? [])}
                  onValueChange={(next: string[]) => {
                    const picked = next.filter((x) => x !== "__all");
                    const showingAll = Boolean(facet.allLabel) && !isFacetActive(value[facet.id]);
                    // Pressing "All" (or unpressing the last option) clears the facet.
                    if (facet.allLabel && next.includes("__all") && !showingAll) setFacet(facet.id, undefined);
                    else setFacet(facet.id, facet.multiple ? picked : picked.slice(-1));
                  }}
                >
                  {facet.allLabel && <ToggleGroupItem value="__all">{facet.allLabel}</ToggleGroupItem>}
                  {facet.options.map((o) => {
                    const count = facetCounts?.[o.value];
                    return (
                      <ToggleGroupItem key={o.value} value={o.value} aria-label={count === undefined ? undefined : `${o.label}, ${count.toLocaleString()}`}>
                        {o.icon}
                        {o.label}
                        {count !== undefined && <span className={styles.count}>{count.toLocaleString()}</span>}
                      </ToggleGroupItem>
                    );
                  })}
                </ToggleGroup>
              ) : facet.type === "select" ? (
                <div className={styles.select}>
                  {facet.multiple ? (
                    <Combobox
                      multiple
                      aria-label={facet.label}
                      size={size}
                      placeholder={facet.placeholder ?? "Any"}
                      items={facet.options.map((o) => ({ ...o, hint: facetCounts?.[o.value]?.toLocaleString() }))}
                      value={(value[facet.id] as string[] | undefined) ?? []}
                      onValueChange={(v) => setFacet(facet.id, v)}
                      chipsLabel={`Selected ${facet.label.toLocaleLowerCase()}`}
                    />
                  ) : (
                    <Combobox
                      aria-label={facet.label}
                      size={size}
                      clearable
                      clearLabel={`Clear ${facet.label.toLocaleLowerCase()}`}
                      placeholder={facet.placeholder ?? "Any"}
                      items={facet.options.map((o) => ({ ...o, hint: facetCounts?.[o.value]?.toLocaleString() }))}
                      value={((value[facet.id] as string[] | undefined) ?? [])[0] ?? null}
                      onValueChange={(v) => setFacet(facet.id, v ? [v] : undefined)}
                    />
                  )}
                </div>
              ) : (
                <DateRangePresets
                  aria-label={facet.label}
                  size={size}
                  presets={facet.presets}
                  allowCustom={facet.allowCustom}
                  pickerProps={facet.pickerProps}
                  value={isDateValue(value[facet.id]) ? (value[facet.id] as DateRangeSelection) : null}
                  onValueChange={(sel) => setFacet(facet.id, sel ?? undefined)}
                />
              )}
            </div>
          );
        })}
        {end && <div className={styles.end}>{end}</div>}
      </div>
      {chips && chipList.length > 0 && (
        <div className={styles.chipsRow}>
          <ul ref={chipsRef} aria-label={labels.activeFilters} className={styles.chips}>
            {chipList.map((chip, i) => (
              <li key={chip.key}>
                <button
                  type="button"
                  className={styles.chip}
                  aria-label={labels.remove(chip.facetLabel, chip.text)}
                  onClick={() => {
                    set(chip.remove());
                    setFocusChip(i);
                  }}
                >
                  <span className={styles.chipFacet}>{chip.facetLabel}:</span> {chip.text}
                  <X aria-hidden className={styles.chipIcon} />
                </button>
              </li>
            ))}
            <li>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  const next = { ...value };
                  for (const f of facets) delete next[f.id];
                  search?.onClear();
                  set(next);
                  setFocusChip(0);
                }}
              >
                {labels.clearAll}
              </Button>
            </li>
          </ul>
        </div>
      )}
    </div>
  );
}

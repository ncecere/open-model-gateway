/*
 * FilterToolbar: the one filter row above every list, table, catalog and
 * report (Grounded's ListPage/DataTable layout). It wraps Bitop's FilterBar
 * (vendored, not edited): a search box, then facets with small labels above
 * the controls, all bottom-aligned; a flexible gap, then page actions (Sort,
 * View, Columns, Export) at the end of the same row WITHOUT labels of their
 * own (`SortSelect`, `ViewToggle`, `ColumnChooser`; accessible names only).
 * Result counts live next to the tabs or in the table footer, never on a line
 * of their own. Under the row, one chip per active filter and "Clear all".
 *
 * Facets are Bitop's (`toggle` = segmented control for a few options,
 * `select` = combobox, `multiple` for a multi-select popover with counts,
 * `date-range`) plus `range`: a min/max pair (e.g. price) with its own chip.
 * Custom leading controls (Period, Workspace) go in `start` wrapped in
 * `ToolbarField`, so their labels line up with the facets'.
 *
 * More than four facets: list the least-used ones in `more` (facet ids). On a
 * wide screen they move behind a "More filters (n)" popover button at the end
 * of the facets, so the row stays on one line; their chips still show under
 * the row. On a phone they are shown inline with the rest.
 *
 * Phone (≤640px): no drawer or sheet. The row collapses in place behind a
 * "Filters (n)" disclosure button (aria-expanded/aria-controls); on wider
 * screens the button is hidden and the controls are always shown. Every
 * control, including multi-selects, takes the full width.
 *
 * Controlled and URL-friendly: `values` / `onChange` use FilterBar's value
 * shape (`{ [facetId]: string[] }`, a range is `[min, max]`); pages map them to
 * their typed URL search (see `toolbarValuesFromSearch`/`toolbarValuesToSearch`
 * for comma-joined lists and `min..max` ranges). The search box keeps its own
 * text and commits after a pause (`debounceMs`, default 300).
 *
 *   <FilterToolbar search={{ label: "Search keys", placeholder: "Name or key ID", value: q, onChange: setQ }}
 *     facets={[{ id: "status", label: "Status", type: "toggle", allLabel: "All", options }]}
 *     values={values} onChange={setValues} end={<ColumnChooser … />} />
 */
import { ArrowUpDown, ChevronDown, LayoutList, Search, SlidersHorizontal, Table2, X } from "lucide-react";
import { type ReactNode, useEffect, useId, useRef, useState } from "react";
import { Button } from "../ui/button/button";
import { Field } from "../ui/field/field";
import { type Facet, type FacetCounts, FilterBar, type FilterValue, type FilterValues, isFacetActive } from "../ui/filter-bar/filter-bar";
import { Input, NativeSelect } from "../ui/input/input";
import { ToggleGroup, ToggleGroupItem } from "../ui/toggle-group/toggle-group";
import { Popover } from "../ui/popover/popover";
import { dateRangePresets, type DateRangeSelection } from "../ui/date-picker/date-picker";
import { cx, useMediaQuery } from "../../lib/bitop-utils";
import styles from "./filter-toolbar.module.css";

/** A min/max pair, e.g. a price band. Its value is `[min, max]` ("" = open end). */
export type RangeFacet = {
  id: string;
  label: string;
  type: "range";
  /** Shown before each bound in inputs and the chip, e.g. "$". */
  prefix?: string;
  inputMode?: "decimal" | "numeric";
  placeholders?: [string, string];
  /** An error for one bound, e.g. not a number; invalid text is never committed. */
  validate?: (value: string) => string | undefined;
  /** Help announced with both inputs (and shown as their tooltip). */
  description?: string;
};
export type ToolbarFacet<T = unknown> = Facet<T> | RangeFacet;
export type ToolbarValues = FilterValues;
export type ToolbarSearch = {
  /** Committed text (usually the URL's `q`). */
  value: string;
  onChange: (next: string) => void;
  /** Accessible name; visible above the box only with `showLabel` (Grounded hides it). */
  label: string;
  placeholder?: string;
  showLabel?: boolean;
  /** Commit delay while typing (default 300ms; 0 commits every keystroke). */
  debounceMs?: number;
};
/** A chip for a custom control in `start` (e.g. "Workspace: Research"). */
export type ToolbarChip = { key: string; label: string; text: string; onRemove: () => void };

export type FilterToolbarProps<T = unknown> = {
  facets?: ToolbarFacet<T>[];
  values?: ToolbarValues;
  onChange?: (next: ToolbarValues) => void;
  /** Option counts for toggle and select facets (FilterBar's shape). */
  counts?: FacetCounts;
  search?: ToolbarSearch;
  /** Custom controls after the search box (wrap each in ToolbarField). */
  start?: ReactNode;
  /** Page actions at the end of the row: Sort, View, Columns, Export. */
  end?: ReactNode;
  /** Chips for active `start` controls; "Clear all" removes them too. */
  extraChips?: ToolbarChip[];
  /** Active `start` controls without a chip (counted on the phone button), e.g. a non-default period. */
  extraActive?: number;
  /** A short note under the chips, e.g. what an active filter leaves out. */
  note?: ReactNode;
  /** Name of the group and the phone button (default "Filters"). */
  label?: string;
  /**
   * Ids of the least-used facets: on a wide screen they sit behind a "More filters (n)" popover
   * button so the row fits one line (use it when a toolbar has more than four facets).
   */
  more?: string[];
  /**
   * Custom controls (wrapped in ToolbarField) that belong with the `more` facets, e.g. an exact-ID text filter: inside
   * the "More filters" popover on a wide screen, inline after `start` on a phone. Count them in `extraActive`/`extraChips`.
   */
  moreStart?: ReactNode;
  /** How many `moreStart` controls are active (added to the "More filters (n)" count). */
  moreStartActive?: number;
  className?: string;
};

/** The toolbar's phone breakpoint (matches the CSS). */
export const TOOLBAR_NARROW_QUERY = "(max-width: 40rem)";

const isRange = <T,>(f: ToolbarFacet<T>): f is RangeFacet => f.type === "range";
const rangeActive = (v: FilterValue | undefined) => Array.isArray(v) && v.some(x => x.trim() !== "");

/** Active filters: one per selected option of a multi facet, one per other active facet, plus search text. */
export function activeToolbarCount<T>(facets: ToolbarFacet<T>[], values: ToolbarValues, search?: string): number {
  let n = search?.trim() ? 1 : 0;
  for (const f of facets) {
    const v = values[f.id];
    if (isRange(f)) n += rangeActive(v) ? 1 : 0;
    else if (isFacetActive(v)) n += Array.isArray(v) && f.type !== "date-range" && f.multiple ? v.length : 1;
  }
  return n;
}

/** "$0.10 – $5", "≥ $0.10", "≤ $5". */
export function rangeText(min: string, max: string, prefix = ""): string {
  const a = min.trim(), b = max.trim();
  if (a && b) return `${prefix}${a} – ${prefix}${b}`;
  return a ? `≥ ${prefix}${a}` : `≤ ${prefix}${b}`;
}

/** URL entries: lists "a,b", ranges "min..max", date ranges dropped (pages keep their own); inactive facets are undefined. */
export function toolbarValuesToSearch<T>(facets: ToolbarFacet<T>[], values: ToolbarValues): Record<string, string | undefined> {
  return Object.fromEntries(facets.filter(f => f.type !== "date-range").map(f => {
    const v = values[f.id];
    if (isRange(f)) return [f.id, rangeActive(v) ? `${(v as string[])[0] ?? ""}..${(v as string[])[1] ?? ""}` : undefined];
    return [f.id, Array.isArray(v) && v.length ? [...v].sort().join(",") : undefined];
  }));
}

/** Reads values from URL entries, dropping options a facet doesn't offer (and extra picks of a single-choice facet). */
export function toolbarValuesFromSearch<T>(search: Record<string, unknown>, facets: ToolbarFacet<T>[]): ToolbarValues {
  const out: ToolbarValues = {};
  for (const f of facets) {
    const raw = search[f.id];
    const text = typeof raw === "string" ? raw : typeof raw === "number" || typeof raw === "boolean" ? String(raw) : "";
    if (!text || f.type === "date-range") continue;
    if (isRange(f)) {
      const [min = "", max = ""] = text.split("..");
      if (min || max) out[f.id] = [min.slice(0, 32), max.slice(0, 32)];
      continue;
    }
    const picked = [...new Set(text.split(","))].filter(x => f.options.some(o => o.value === x));
    const list = f.multiple ? picked : picked.slice(0, 1);
    if (list.length) out[f.id] = list;
  }
  return out;
}

/**
 * A labelled custom control in the toolbar: a small label above, like the facets'. A form control
 * (input, select) is labelled through a Bitop Field; `group` labels a set of buttons (e.g. a ToggleGroup).
 */
export function ToolbarField({ label, children, hint, group = false, className }: { label: string; children: ReactNode; /** Announced help (screen readers and tooltip). */ hint?: string; group?: boolean; className?: string }) {
  const id = useId();
  if (group) return (
    <div role="group" aria-labelledby={`${id}-label`} className={cx(styles.field, className)} title={hint}>
      <span id={`${id}-label`} className={styles.label}>{label}</span>
      {children}
    </div>
  );
  return (
    <div className={cx(styles.field, className)} title={hint}>
      <span aria-hidden className={styles.label}>{label}</span>
      {/* The hint stays out of the Field (its description row would push the control out of line). */}
      <Field label={hint ? `${label}. ${hint}` : label} hideLabel>{children}</Field>
    </div>
  );
}

/** Sort for the end of the row: no visible label (its name is "Sort by"), an arrows icon and the current order. */
export function SortSelect<V extends string>({ value, options, onChange, label = "Sort by", className }: { value: V; options: readonly { value: V; label: string }[]; onChange: (next: V) => void; label?: string; className?: string }) {
  return <span className={cx(styles.sort, className)} title={label}>
    <ArrowUpDown aria-hidden className={styles.sortIcon} />
    <NativeSelect size="sm" aria-label={label} className={styles.sortSelect} value={value} onChange={event => onChange(event.target.value as V)}>{options.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}</NativeSelect>
  </span>;
}

export type ListLayout = "list" | "table";
/** List / Table as two icon buttons (names "List" and "Table", with tooltips), no visible label. */
export function ViewToggle({ value, onChange, label = "Layout" }: { value: ListLayout; onChange: (next: ListLayout) => void; label?: string }) {
  return <ToggleGroup aria-label={label} joined variant="outline" size="sm" value={[value]} onValueChange={next => { const v = next[0] as ListLayout | undefined; if (v && v !== value) onChange(v); }}>
    <ToggleGroupItem value="list" iconOnly aria-label="List" title="List"><LayoutList aria-hidden /></ToggleGroupItem>
    <ToggleGroupItem value="table" iconOnly aria-label="Table" title="Table"><Table2 aria-hidden /></ToggleGroupItem>
  </ToggleGroup>;
}

type Chip = { key: string; label: string; text: string; remove: () => void };

function chipsOf<T>(facets: ToolbarFacet<T>[], values: ToolbarValues, set: (next: ToolbarValues) => void): Chip[] {
  const without = (id: string, next?: FilterValue) => { const out = { ...values }; if (next && (Array.isArray(next) ? next.length : true)) out[id] = next; else delete out[id]; return out; };
  const chips: Chip[] = [];
  for (const f of facets) {
    const v = values[f.id];
    if (isRange(f)) {
      if (rangeActive(v)) { const [min = "", max = ""] = v as string[]; chips.push({ key: f.id, label: f.label, text: rangeText(min, max, f.prefix), remove: () => set(without(f.id)) }); }
      continue;
    }
    if (!isFacetActive(v)) continue;
    // Like FilterBar: a single-choice toggle's pressed item already shows the choice.
    if (!(f.chip ?? !(f.type === "toggle" && !f.multiple))) continue;
    if (f.type === "date-range") {
      const sel = v as DateRangeSelection, preset = (f.presets ?? dateRangePresets).find(p => p.id === sel.preset);
      const fmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric" });
      const text = preset?.label ?? (sel.range ? `${fmt.format(sel.range.from)} – ${fmt.format(sel.range.to ?? sel.range.from)}` : "");
      chips.push({ key: f.id, label: f.label, text, remove: () => set(without(f.id)) });
      continue;
    }
    for (const item of v as string[]) chips.push({ key: `${f.id}:${item}`, label: f.label, text: f.options.find(o => o.value === item)?.label ?? item, remove: () => set(without(f.id, (v as string[]).filter(x => x !== item))) });
  }
  return chips;
}

export function FilterToolbar<T>({ facets = [], values = {}, onChange, counts, search, start, end, extraChips = [], extraActive = 0, note, label = "Filters", more = [], moreStart, moreStartActive = 0, className }: FilterToolbarProps<T>) {
  const id = useId(), [open, setOpen] = useState(false);
  const narrow = useMediaQuery(TOOLBAR_NARROW_QUERY);
  const panelRef = useRef<HTMLDivElement>(null), chipsRef = useRef<HTMLUListElement>(null);
  const [focusChip, setFocusChip] = useState<number | null>(null);
  const set = (next: ToolbarValues) => onChange?.(next);
  const [text, setText] = useSearchDraft(search);

  const hidden = narrow ? [] : facets.filter(f => more.includes(f.id));
  const inline = facets.filter(f => !hidden.includes(f));
  const bitopFacets = inline.filter((f): f is Facet<T> => !isRange(f));
  const ranges = inline.filter(isRange);
  const valuesOf = (list: Facet<T>[]): FilterValues => Object.fromEntries(list.map(f => [f.id, values[f.id]]).filter(([, v]) => v !== undefined));
  const bitopValues = valuesOf(bitopFacets);
  /** FilterBar reports only its own facets: keep every other facet's value. */
  const merge = (own: Facet<T>[], next: FilterValues) => set({ ...Object.fromEntries(Object.entries(values).filter(([k]) => !own.some(f => f.id === k))), ...next });
  const setRange = (r: RangeFacet, next: string[]) => { const out = { ...values }; if (next.some(x => x.trim())) out[r.id] = next; else delete out[r.id]; set(out); };
  const active = activeToolbarCount(facets, values, search?.value) + extraChips.length + extraActive;

  const chips: Chip[] = [
    ...(search?.value.trim() ? [{ key: "search", label: "Search", text: search.value.trim(), remove: () => { setText(""); search.onChange(""); } }] : []),
    ...extraChips.map(c => ({ key: `extra:${c.key}`, label: c.label, text: c.text, remove: c.onRemove })),
    ...chipsOf(facets, values, set),
  ];
  const clearAll = () => {
    if (search?.value) { setText(""); search.onChange(""); }
    for (const c of extraChips) c.onRemove();
    const next = { ...values };
    for (const f of facets) delete next[f.id];
    set(next);
  };

  // After a chip is removed, focus the next chip, then "Clear all", then the first control.
  useEffect(() => {
    if (focusChip === null) return;
    setFocusChip(null);
    const buttons = chipsRef.current?.querySelectorAll<HTMLElement>("button");
    const target = (buttons?.length ? buttons[Math.min(focusChip, buttons.length - 1)] : null) ?? panelRef.current?.querySelector<HTMLElement>("input, button, [tabindex='0']");
    target?.focus();
  }, [focusChip]);

  const searchBox = search && (
    <div className={styles.search}>
      {search.showLabel && <span aria-hidden className={styles.label}>{search.label}</span>}
      <Field label={search.label} hideLabel>
        <Input type="search" size="sm" value={text} placeholder={search.placeholder} startIcon={<Search />} onValueChange={setText}
          onKeyDown={event => { if (event.key === "Enter") search.onChange(text.trim()); }} />
      </Field>
    </div>
  );
  const leading = (searchBox || start || ranges.length > 0 || narrow && moreStart) && <>
    {searchBox}
    {start}
    {narrow && moreStart}
    {ranges.map(r => <RangeControl key={r.id} facet={r} value={(values[r.id] as string[] | undefined) ?? []} onChange={next => setRange(r, next)} />)}
  </>;
  const moreFacets = hidden.filter((f): f is Facet<T> => !isRange(f)), moreRanges = hidden.filter(isRange);
  const moreActive = activeToolbarCount(hidden, values) + (narrow ? 0 : moreStartActive);
  const moreButton = (hidden.length > 0 || !narrow && !!moreStart) && (
    <Popover title="More filters" align="start" className={styles.morePopup}
      trigger={<Button size="sm" variant="secondary" className={styles.moreButton} aria-label={moreActive > 0 ? `More filters, ${moreActive} active` : undefined}><SlidersHorizontal aria-hidden />More filters{moreActive > 0 && ` (${moreActive})`}</Button>}>
      {!narrow && moreStart}
      {moreRanges.map(r => <RangeControl key={r.id} facet={r} value={(values[r.id] as string[] | undefined) ?? []} onChange={next => setRange(r, next)} />)}
      {moreFacets.length > 0 && <FilterBar<T> className={styles.moreBar} facets={moreFacets} value={valuesOf(moreFacets)} onValueChange={next => merge(moreFacets, next)} counts={counts} chips={false} labels={{ group: "More filters" }} />}
    </Popover>
  );
  const trailing = (moreButton || end) && <div className={styles.trailing}>{moreButton}{end && <div className={styles.end}>{end}</div>}</div>;

  return (
    <div className={cx(styles.root, className)} data-open={open ? "" : undefined}>
      <button type="button" className={styles.toggle} aria-expanded={open} aria-controls={`${id}-panel`} onClick={() => setOpen(!open)}>
        <SlidersHorizontal aria-hidden className={styles.toggleIcon} />
        <span>{label}{active > 0 && <> (<span>{active}<span className="sr-only"> active</span></span>)</>}</span>
        <ChevronDown aria-hidden className={styles.chevron} />
      </button>
      <div ref={panelRef} id={`${id}-panel`} className={styles.panel}>
        <FilterBar<T> className={styles.bar} facets={bitopFacets} value={bitopValues} onValueChange={next => merge(bitopFacets, next)}
          counts={counts} chips={false} labels={{ group: label }} start={leading || undefined} end={trailing || undefined} />
        {chips.length > 0 && (
          <ul ref={chipsRef} aria-label="Active filters" className={styles.chips}>
            {chips.map((chip, i) => (
              <li key={chip.key}>
                <button type="button" className={styles.chip} aria-label={`Remove filter ${chip.label}: ${chip.text}`} onClick={() => { chip.remove(); setFocusChip(i); }}>
                  <span className={styles.chipLabel}>{chip.label}:</span> <span className={styles.chipText}>{chip.text}</span>
                  <X aria-hidden className={styles.chipIcon} />
                </button>
              </li>
            ))}
            <li><Button size="sm" variant="ghost" onClick={() => { clearAll(); setFocusChip(0); }}>Clear all</Button></li>
          </ul>
        )}
        {note && <p className={styles.note}>{note}</p>}
      </div>
    </div>
  );
}

/** The search box's own text: follows the committed value, commits after a pause. */
function useSearchDraft(search: ToolbarSearch | undefined) {
  const committed = search?.value ?? "", delay = search?.debounceMs ?? 300;
  const [text, setText] = useState(committed);
  const last = useRef(committed);
  useEffect(() => { if (committed !== last.current) { last.current = committed; setText(committed); } }, [committed]);
  useEffect(() => {
    if (!search) return;
    const next = text.trim();
    if (next === committed.trim()) return;
    const commit = () => { last.current = next; search.onChange(next); };
    if (delay <= 0) { commit(); return; }
    const timer = setTimeout(commit, delay);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text]);
  return [text, setText] as const;
}

/** Min and max inputs under one label; a bound commits after a pause, on blur or Enter, and only when valid. */
function RangeControl({ facet, value, onChange }: { facet: RangeFacet; value: string[]; onChange: (next: string[]) => void }) {
  const id = useId(), [min = "", max = ""] = value;
  const [draft, setDraft] = useState<[string, string]>([min, max]);
  useEffect(() => setDraft([min, max]), [min, max]);
  const errors = draft.map(v => v.trim() ? facet.validate?.(v.trim()) : undefined);
  const commit = (next: [string, string]) => {
    if (next.some(v => v.trim() && facet.validate?.(v.trim()))) return;
    if (next[0].trim() === min.trim() && next[1].trim() === max.trim()) return;
    onChange([next[0].trim(), next[1].trim()]);
  };
  useEffect(() => { const timer = setTimeout(() => commit(draft), 500); return () => clearTimeout(timer); }, [draft[0], draft[1]]); // eslint-disable-line react-hooks/exhaustive-deps
  const error = errors.find(Boolean);
  const input = (i: 0 | 1, name: string) => (
    <Input size="sm" className={styles.rangeInput} inputMode={facet.inputMode ?? "decimal"} aria-label={`${facet.label}, ${name}`} aria-invalid={errors[i] ? true : undefined}
      aria-describedby={[facet.description ? `${id}-help` : "", errors[i] ? `${id}-error` : ""].filter(Boolean).join(" ") || undefined}
      value={draft[i]} placeholder={facet.placeholders?.[i] ?? name} startIcon={facet.prefix ? <span className={styles.prefix}>{facet.prefix}</span> : undefined}
      onValueChange={v => setDraft(i === 0 ? [v, draft[1]] : [draft[0], v])} onBlur={() => commit(draft)} onKeyDown={event => { if (event.key === "Enter") commit(draft); }} />
  );
  return (
    <div role="group" aria-labelledby={`${id}-label`} className={styles.field} title={facet.description}>
      <span id={`${id}-label`} className={styles.label}>{facet.label}</span>
      <div className={styles.range}>{input(0, "Min")}<span aria-hidden className={styles.dash}>–</span>{input(1, "Max")}</div>
      {facet.description && <span id={`${id}-help`} className="sr-only">{facet.description}</span>}
      {error && <span id={`${id}-error`} role="alert" className={styles.error}>{error}</span>}
    </div>
  );
}

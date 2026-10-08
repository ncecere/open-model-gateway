/*
 * PivotControls: the Explore builder's selects — metric × group by × then by ×
 * top N — as labelled Bitop NativeSelects in one responsive row (stacked at
 * 390px). Fully controlled with plain strings, so the state round-trips
 * through the URL (`pivotToSearch` / `pivotFromSearch`).
 *
 * "Then by" can't repeat "Group by": that option is disabled, and changing
 * Group by to the current Then by resets Then by to "none".
 *
 *   const pivot = pivotFromSearch(search, defaults, options);
 *   <PivotControls value={pivot} onChange={next => navigate({ search: { ...search, ...pivotToSearch(next) } })}
 *     metrics={[{ value: "spend", label: "Spend" }]} dimensions={[{ value: "model", label: "Model" }, …]} />
 */
import { useId } from "react";
import { Field } from "../ui/field/field";
import { NativeSelect } from "../ui/input/input";
import { cx } from "../../lib/bitop-utils";
import styles from "./pivot-controls.module.css";

export type PivotOption = { value: string; label: string; disabled?: boolean };
export type PivotValue = { metric: string; groupBy: string; /** "none" for no second dimension. */ thenBy: string; /** Decimal string, e.g. "10". */ topN: string };
export type PivotOptions = { metrics: PivotOption[]; dimensions: PivotOption[]; topN?: number[] };

export const PIVOT_NONE = "none";
export const defaultTopN = [5, 10, 25, 50];

export type PivotControlsProps = PivotOptions & {
  value: PivotValue;
  onChange: (next: PivotValue) => void;
  /** Hide the Then by select (single-dimension pivots). */
  hideThenBy?: boolean;
  disabled?: boolean;
  /** Labels (defaults: Metric, Group by, Then by, Top). */
  labels?: Partial<Record<keyof PivotValue, string>>;
  className?: string;
};

export function PivotControls({ value, onChange, metrics, dimensions, topN = defaultTopN, hideThenBy, disabled, labels, className }: PivotControlsProps) {
  const id = useId();
  const set = (patch: Partial<PivotValue>) => {
    const next = { ...value, ...patch };
    if (next.thenBy === next.groupBy) next.thenBy = PIVOT_NONE;
    onChange(next);
  };
  return (
    <div role="group" aria-label="Pivot" className={cx(styles.root, className)}>
      <Field label={labels?.metric ?? "Metric"} className={styles.field}>
        <NativeSelect id={`${id}-metric`} size="sm" value={value.metric} disabled={disabled} onChange={e => set({ metric: e.target.value })}>
          {metrics.map(o => <option key={o.value} value={o.value} disabled={o.disabled}>{o.label}</option>)}
        </NativeSelect>
      </Field>
      <Field label={labels?.groupBy ?? "Group by"} className={styles.field}>
        <NativeSelect id={`${id}-group`} size="sm" value={value.groupBy} disabled={disabled} onChange={e => set({ groupBy: e.target.value })}>
          {dimensions.map(o => <option key={o.value} value={o.value} disabled={o.disabled}>{o.label}</option>)}
        </NativeSelect>
      </Field>
      {!hideThenBy && <Field label={labels?.thenBy ?? "Then by"} className={styles.field}>
        <NativeSelect id={`${id}-then`} size="sm" value={value.thenBy} disabled={disabled} onChange={e => set({ thenBy: e.target.value })}>
          <option value={PIVOT_NONE}>None</option>
          {dimensions.map(o => <option key={o.value} value={o.value} disabled={o.disabled || o.value === value.groupBy}>{o.label}</option>)}
        </NativeSelect>
      </Field>}
      <Field label={labels?.topN ?? "Top"} className={styles.narrowField}>
        <NativeSelect id={`${id}-top`} size="sm" value={value.topN} disabled={disabled} onChange={e => set({ topN: e.target.value })}>
          {topN.map(n => <option key={n} value={String(n)}>{n}</option>)}
        </NativeSelect>
      </Field>
    </div>
  );
}

/** URL search entries for a pivot (`metric`, `group`, `then`, `top`); "none" Then by is left out. */
export function pivotToSearch(value: PivotValue): Record<string, string | undefined> {
  return { metric: value.metric, group: value.groupBy, then: value.thenBy === PIVOT_NONE ? undefined : value.thenBy, top: value.topN };
}

/** Reads a pivot from URL search values, falling back to `defaults` for anything missing or not offered. */
export function pivotFromSearch(search: Record<string, unknown>, defaults: PivotValue, options: PivotOptions): PivotValue {
  const pick = (raw: unknown, allowed: string[], fallback: string) => (typeof raw === "string" && allowed.includes(raw) ? raw : fallback);
  const dims = options.dimensions.filter(o => !o.disabled).map(o => o.value);
  const metric = pick(search.metric, options.metrics.filter(o => !o.disabled).map(o => o.value), defaults.metric);
  const groupBy = pick(search.group, dims, defaults.groupBy);
  let thenBy = pick(search.then, [PIVOT_NONE, ...dims], PIVOT_NONE);
  if (thenBy === groupBy) thenBy = PIVOT_NONE;
  const topN = pick(typeof search.top === "number" ? String(search.top) : search.top, (options.topN ?? defaultTopN).map(String), defaults.topN);
  return { metric, groupBy, thenBy, topN };
}

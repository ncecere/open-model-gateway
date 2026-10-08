"use client";

import type { ReactNode } from "react";
import { Disclosure } from "@/components/ui/disclosure/disclosure";
import { Table, Td, Th, Tr } from "@/components/ui/table/table";
import { cx } from "@/lib/bitop-utils";
import styles from "./chart.module.css";

/*
 * Chart primitives shared by bar-chart, line-chart and sparkline, so every
 * chart uses the same series colours, legend and "Show data" table:
 *
 *   - ChartTone and `chartToneClass`: series colours from the
 *     --color-chart-* tokens (>= 3:1 on surface and bg in every theme).
 *     Put `className={chartToneClass} data-tone={tone}` on an element and
 *     read `var(--chart-color)` in your CSS.
 *   - ChartLegend: a list of series with a swatch in the series' colour and
 *     line pattern (solid, dashed, dotted), so series differ by more than hue.
 *   - ChartData: a "Show data" disclosure with the chart's numbers as a real
 *     table, the accessible alternative to the plot for anyone who needs
 *     exact values.
 *
 *   <ChartLegend series={[{ key: "answers", label: "Answers" }, { key: "chats", label: "Conversations" }]} />
 *   <ChartData caption="Answers per day" series={series} data={points} />
 *
 * Values: NaN and ±Infinity are "no data" (see chartValue): charts leave them
 * out of the scale and draw a gap, and ChartData shows "No data". A missing
 * key counts as 0. Bar and line charts scale from 0, so negative values are
 * drawn at 0 (clamped); titles and the data table keep the real number.
 * Point labels needn't be unique (rows are keyed by position).
 */

/** Series colours; `accent` (magenta) is a seventh hue for a chart whose series would otherwise repeat one or put two blues side by side. */
export type ChartTone = "primary" | "info" | "success" | "warning" | "danger" | "neutral" | "accent";

/** Line pattern of a series; the second and third series get dashed and dotted lines by default. */
export type ChartPattern = "solid" | "dashed" | "dotted";

export type ChartSeries<K extends string = string> = {
  key: K;
  label: ReactNode;
  tone?: ChartTone;
  /** Line pattern (line charts and legend swatches). Default: by position. */
  pattern?: ChartPattern;
};

export type ChartPoint<K extends string = string> = {
  /** The point's label, e.g. a date; shown on the axis, in hover titles and as the table's row header. */
  label: string;
  values: Record<K, number>;
};

/** Series colours in order, for series without a tone. */
export const chartTones: ChartTone[] = ["primary", "info", "success", "warning", "danger", "neutral", "accent"];
const patterns: ChartPattern[] = ["solid", "dashed", "dotted"];

/** Put on an element with `data-tone` to get `--chart-color` for that tone. */
export const chartToneClass = styles.tone;

/** The tone of the series at `index` (its own tone, else the next in `chartTones`). */
export function seriesTone(series: { tone?: ChartTone }, index: number): ChartTone {
  return series.tone ?? chartTones[index % chartTones.length]!;
}

/** The line pattern of the series at `index`. */
export function seriesPattern(series: { pattern?: ChartPattern }, index: number): ChartPattern {
  return series.pattern ?? patterns[index % patterns.length]!;
}

/**
 * A point's value for a series: the number if it is finite, 0 for a missing
 * key, and null (no data) for NaN or ±Infinity.
 */
export function chartValue<K extends string>(point: ChartPoint<K>, key: K): number | null {
  const v = point.values[key] ?? 0;
  return Number.isFinite(v) ? v : null;
}

/** Runs of consecutive points with data, as [index, value] pairs: a line is drawn per run. */
export function chartRuns(values: (number | null)[]): [number, number][][] {
  const runs: [number, number][][] = [];
  let run: [number, number][] = [];
  values.forEach((v, i) => {
    if (v === null) {
      if (run.length) runs.push(run);
      run = [];
    } else run.push([i, v]);
  });
  if (run.length) runs.push(run);
  return runs;
}

/** Text of a series label for titles and summaries (non-string labels fall back to the key). */
export function seriesName(series: { key: string; label: ReactNode }): string {
  return typeof series.label === "string" || typeof series.label === "number" ? String(series.label) : series.key;
}

export type ChartLegendProps = {
  series: ChartSeries[];
  /** Swatch shape: a line (line/area charts) or a square (bar charts). */
  swatch?: "line" | "square";
  className?: string;
};

/** The series legend: a list of swatches and labels. */
export function ChartLegend({ series, swatch = "square", className }: ChartLegendProps) {
  if (series.length === 0) return null;
  return (
    <ul className={cx(styles.legend, className)}>
      {series.map((s, i) => (
        <li key={s.key} className={styles.legendItem}>
          <span
            aria-hidden
            className={cx(styles.tone, styles.swatch)}
            data-tone={seriesTone(s, i)}
            data-shape={swatch}
            data-pattern={swatch === "line" ? seriesPattern(s, i) : undefined}
          />
          {s.label}
        </li>
      ))}
    </ul>
  );
}

/** The `dataTable` option of BarChart and LineChart. */
export type ChartDataOptions = {
  /** Names the table, e.g. "Answers per day". */
  caption: string;
  /** Header of the label column (default "Date"). */
  labelHeader?: ReactNode;
  defaultOpen?: boolean;
};

export type ChartDataProps<K extends string = string> = {
  /** Names the table, e.g. "Answers per day". */
  caption: string;
  series: ChartSeries<K>[];
  data: ChartPoint<K>[];
  /** Header of the label column (default "Date"). */
  labelHeader?: ReactNode;
  formatValue?: (value: number) => string;
  /** The disclosure's text (default "Show data"). */
  toggleLabel?: ReactNode;
  /** Cell text for NaN / ±Infinity values (default "No data"). */
  missingLabel?: string;
  defaultOpen?: boolean;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
};

/** A "Show data" disclosure with the chart's numbers in a table. */
export function ChartData<K extends string>({
  caption,
  series,
  data,
  labelHeader = "Date",
  formatValue = (v) => v.toLocaleString(),
  toggleLabel = "Show data",
  missingLabel = "No data",
  defaultOpen,
  open,
  onOpenChange,
  className,
}: ChartDataProps<K>) {
  return (
    <Disclosure title={toggleLabel} open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange} className={cx(styles.data, className)}>
      <Table
        caption={caption}
        density="compact"
        maxHeight="16rem"
        stickyHeader
        columns={[{ label: labelHeader }, ...series.map((s) => ({ label: s.label, numeric: true }))]}
      >
        {data.map((p, i) => (
          <Tr key={i}>
            <Th>{p.label}</Th>
            {series.map((s) => {
              const v = chartValue(p, s.key);
              return (
                <Td key={s.key} numeric>
                  {v === null ? missingLabel : formatValue(v)}
                </Td>
              );
            })}
          </Tr>
        ))}
      </Table>
    </Disclosure>
  );
}

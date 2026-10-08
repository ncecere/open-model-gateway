import type { CSSProperties } from "react";
import {
  ChartData,
  ChartLegend,
  chartToneClass,
  chartValue,
  seriesName,
  seriesTone,
  type ChartDataOptions,
  type ChartPoint,
  type ChartSeries,
  type ChartTone,
} from "@/components/ui/chart/chart";
import { cx } from "@/lib/bitop-utils";
import styles from "./bar-chart.module.css";

/*
 * A small bar chart drawn with CSS: one slot per point, one bar per series,
 * either overlapping (drawn back to front, so put the largest series first)
 * or stacked. To assistive technology the plot is a single image named by
 * `summary`; `dataTable` adds a "Show data" disclosure with the numbers.
 * Colours, legend and data table are shared with LineChart (see chart).
 *
 * The scale runs from 0 to the largest finite value (or stack total).
 * Negative values are drawn as empty bars (clamped to 0); NaN and ±Infinity
 * are "no data": left out of the scale and drawn as an empty bar marked
 * `data-missing`. Titles and the data table show the real values.
 *
 * `domain={{ max: 100 }}` fixes the top of the scale (a percentage, say), so
 * a bar's height means the same whatever the data; values above it are drawn
 * full height and stacks are cut there (titles keep the values). Bars always start at 0.
 */

export type BarChartTone = ChartTone;
export type BarChartSeries<K extends string = string> = ChartSeries<K>;
export type BarChartPoint<K extends string = string> = ChartPoint<K>;

export type BarChartProps<K extends string = string> = {
  data: BarChartPoint<K>[];
  series: BarChartSeries<K>[];
  /** Text alternative of the whole chart, e.g. "Answers per day: 1,204 in total, most on 12 Sep (96)". */
  summary: string;
  /** overlap: bars share the slot, first series at the back; stack: bars add up. */
  layout?: "overlap" | "stack";
  size?: "sm" | "md" | "lg";
  /** Formats values for the peak label, hover titles and data table. */
  formatValue?: (value: number) => string;
  /** Show the legend (default true). */
  legend?: boolean;
  /** Show the first and last labels and the peak value (default true). */
  axis?: boolean;
  /** Fix the top of the scale, e.g. `{ max: 100 }` for a percentage (default: the largest value or stack). */
  domain?: { max?: number };
  /** Adds a "Show data" disclosure with the numbers in a table. */
  dataTable?: ChartDataOptions;
  className?: string;
};

/** A CSS bar chart with a legend; role="img" with a text summary. */
export function BarChart<K extends string>({
  data,
  series,
  summary,
  layout = "overlap",
  size = "md",
  formatValue = (v) => v.toLocaleString(),
  legend = true,
  axis = true,
  domain,
  dataTable,
  className,
}: BarChartProps<K>) {
  // Non-finite values (null here) are left out of the scale; negatives count as 0.
  const total = (p: BarChartPoint<K>) => series.reduce((sum, s) => sum + Math.max(0, chartValue(p, s.key) ?? 0), 0);
  const top = (p: BarChartPoint<K>) => Math.max(0, ...series.map((s) => chartValue(p, s.key) ?? 0));
  const peak = domain?.max ?? Math.max(0, ...data.map(layout === "stack" ? total : top));
  const pct = (v: number | null) => `${peak > 0 && v !== null ? (Math.min(peak, Math.max(0, v)) / peak) * 100 : 0}%`;
  const describe = (s: BarChartSeries<K>, v: number | null) => (v === null ? `${seriesName(s)}: no data` : `${formatValue(v)} ${seriesName(s)}`);

  return (
    <figure className={cx(styles.root, className)} data-size={size} data-layout={layout}>
      {legend && <ChartLegend series={series} className={styles.legend} />}
      <div className={styles.plot} role="img" aria-label={summary}>
        {axis && <span className={styles.peak}>{formatValue(peak)}</span>}
        <div className={styles.bars} data-layout={layout}>
          {data.map((p, j) => (
            <div key={j} className={styles.slot} title={`${p.label}: ${series.map((s) => describe(s, chartValue(p, s.key))).join(", ")}`}>
              {series.map((s, i) => {
                const v = chartValue(p, s.key);
                return (
                  <span
                    key={s.key}
                    className={cx(chartToneClass, styles.bar)}
                    data-tone={seriesTone(s, i)}
                    data-missing={v === null ? "" : undefined}
                    style={{ "--bar-size": pct(v) } as CSSProperties}
                  />
                );
              })}
            </div>
          ))}
        </div>
      </div>
      {axis && data.length > 0 && (
        <div aria-hidden className={styles.axis}>
          <span>{data[0]!.label}</span>
          {data.length > 1 && <span>{data[data.length - 1]!.label}</span>}
        </div>
      )}
      {dataTable && (
        <ChartData
          caption={dataTable.caption}
          labelHeader={dataTable.labelHeader}
          defaultOpen={dataTable.defaultOpen}
          series={series}
          data={data}
          formatValue={formatValue}
        />
      )}
    </figure>
  );
}

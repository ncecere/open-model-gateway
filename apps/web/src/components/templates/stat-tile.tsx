/*
 * StatTile: a KPI tile (Spend, Requests, Tokens, Cache hit %, $/M) on Bitop's
 * StatCard: label, big value, optional Sparkline from a number series, and a
 * Δ badge against the previous period whose colour meaning is configurable
 * (`increaseIs`: more spend is "bad", more requests "neutral", more cache hits
 * "good"). With `href`, `render` (router Link) or `onClick` the whole tile is
 * one stretched link/button named by the label.
 *
 * Unknown stays unknown: `value={null}` renders "Unknown", and a delta whose
 * current or previous value is unknown renders "No comparison" instead of a
 * fabricated percentage. Integer strings (micro-USD, token counts) are compared
 * exactly with BigInt.
 *
 *   <StatTile label="Spend" value={formatMicroUsd(total)} series={dailyMicro.map(Number)}
 *     delta={{ current: total, previous: prevTotal, increaseIs: "bad", label: "vs previous 30 days" }}
 *     render={<Link to="…" />} />
 */
import type { useRender } from "@base-ui/react/use-render";
import type { ReactNode } from "react";
import { StatCard } from "../ui/stat-card/stat-card";
import { Sparkline } from "../ui/sparkline/sparkline";
import { cx } from "../../lib/bitop-utils";
import { type Change, percentChange } from "./kit-format";
import styles from "./stat-tile.module.css";

export type DeltaSemantics = "good" | "bad" | "neutral";
type Comparable = string | number | bigint | null | undefined;

export type StatTileDelta = {
  /** What an increase means: "good" (green up), "bad" (red up, green down) or "neutral" (grey). Default "good". */
  increaseIs?: DeltaSemantics;
  /** Context after the badge, e.g. "vs previous 7 days". */
  label?: ReactNode;
} & ({ current: Comparable; previous: Comparable } | { change: Change | null });

export type StatTileProps = {
  /** Plain-text label; names the tile, its link and its sparkline. */
  label: string;
  /** The formatted headline value. `null`/`undefined` renders "Unknown" (never zero). */
  value: ReactNode | null | undefined;
  /** Muted footnote under the value, e.g. "3 unpriced requests". */
  hint?: ReactNode;
  /** A last line that may hold its own link or button (kept above a linked tile's stretched link), e.g. "View unresolved". */
  details?: ReactNode;
  /** Decorative icon. */
  icon?: ReactNode;
  /** Oldest-first series for a sparkline (null/NaN = no data gap). Two or more points are needed to draw it. */
  series?: (number | null)[];
  /** Text alternative for the sparkline (default: generated from label and first/last points). */
  seriesLabel?: string;
  /** Formats series values in the generated sparkline label. */
  formatSeriesValue?: (value: number) => string;
  delta?: StatTileDelta;
  /** Text when the comparison is unknown (default "No comparison"). */
  noComparisonLabel?: ReactNode;
  href?: string;
  render?: useRender.RenderProp;
  /** Makes the tile a button (e.g. expand a chart in place). Ignored with `href`/`render`. */
  onClick?: () => void;
  className?: string;
};

function sentimentOf(direction: Change["direction"], increaseIs: DeltaSemantics): "positive" | "negative" | "neutral" {
  if (direction === "flat" || increaseIs === "neutral") return "neutral";
  const good = increaseIs === "good" ? direction === "up" : direction === "down";
  return good ? "positive" : "negative";
}

export function StatTile({ label, value, hint, details, icon, series, seriesLabel, formatSeriesValue = v => v.toLocaleString(), delta, noComparisonLabel = "No comparison", href, render, onClick, className }: StatTileProps) {
  const change = delta ? ("change" in delta ? delta.change : percentChange(delta.current, delta.previous)) : null;
  const points = series?.map(v => (v === null || !Number.isFinite(v) ? Number.NaN : v)) ?? [];
  const finite = points.filter(Number.isFinite);
  // A single known point is a dot, not a trend (review #37): a sparkline needs two.
  const drawn = points.length > 1 && finite.length > 1;
  // The chart slot is a fixed-size block pinned to the bottom of the tile (stat-tile.module.css), so every tile in a
  // row draws its sparkline at the same height and width however many text lines sit above it. A tile given a series
  // it can't draw (fewer than two known points) keeps the empty slot, so it lines up with its neighbours too.
  const chart = series === undefined ? undefined : <span className={styles.chart} data-empty={drawn ? undefined : ""} aria-hidden={drawn ? undefined : true}>
    {drawn && <Sparkline values={points} variant="area" size="sm" label={seriesLabel ?? `${label} trend over ${points.length} points, from ${formatSeriesValue(finite[0]!)} to ${formatSeriesValue(finite[finite.length - 1]!)}`} />}
  </span>;
  const linkRender = href === undefined && render === undefined && onClick ? <button type="button" className={styles.button} onClick={onClick} /> : render;
  const unknown = value === null || value === undefined;
  return (
    <StatCard
      className={cx(styles.tile, className)}
      label={label}
      value={unknown ? <span className={styles.unknown}>Unknown</span> : value}
      icon={icon}
      chart={chart}
      href={href}
      render={linkRender}
      details={details}
      delta={change ? { value: change.text, trend: change.direction, sentiment: change.text === "New" ? "neutral" : sentimentOf(change.direction, delta?.increaseIs ?? "good"), label: delta?.label } : undefined}
      hint={delta && !change ? <>{noComparisonLabel}{hint ? <> · {hint}</> : null}</> : hint}
    />
  );
}

/** A responsive grid of StatTiles: 1 column at 390px, 2 on tablets, up to `columns` on wide screens. */
export function StatTileGrid({ children, columns = 4, label }: { children: ReactNode; columns?: 2 | 3 | 4 | 5; /** Optional accessible name for the group. */ label?: string }) {
  return <div className={styles.grid} data-columns={columns} role={label ? "group" : undefined} aria-label={label}>{children}</div>;
}

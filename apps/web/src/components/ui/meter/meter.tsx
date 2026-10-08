"use client";

import { Meter as BaseMeter } from "@base-ui/react/meter";
import { CircleAlert, TriangleAlert } from "lucide-react";
import { type CSSProperties, type ReactNode, useId } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./meter.module.css";

/*
 * Meter: how much of a limit is used (storage, documents, queries today),
 * on Base UI Meter (role="meter"). Unlike Progress, it measures an amount
 * within a range rather than a task completing.
 *
 *   <Meter label="Storage" value={8.6} max={10} formatValue={(v) => `${v} GB`} />
 *   <Meter label="Queries today" value={4120} max={5000} marker={{ value: 4000, label: "Team limit" }} />
 *
 * Tones: at `warningAt` (default 80% of the way from `min` to `max`) the bar
 * turns to the warning colour and "Near limit" is shown; at `criticalAt`
 * (default 100%) it turns
 * to the danger colour with "At limit" (or "Over limit" above max). The word
 * and an icon always accompany the colour, and are part of the meter's
 * aria-valuetext ("4,120 of 5,000, near limit"). Values above max keep their
 * real number in the text while the bar stops at full.
 *
 * `marker` draws a tick on the track, e.g. a soft limit below a hard ceiling;
 * it's described in text under the bar. `max={null}` shows the usage with
 * "No limit" and no bar.
 *
 * Base UI's Meter.Root appends a visually hidden "x" to the meter's content
 * (a workaround for meters with no text); this meter always contains its
 * label, so the root is rendered without it and screen readers don't read a
 * stray "x" after the value.
 */

export type MeterLevel = "normal" | "warning" | "critical" | "over";

export type MeterMarker = {
  value: number;
  /** Names the marker, e.g. "Team limit". Shown under the bar with its value. */
  label: string;
};

export type MeterLabels = {
  /** Default "of": "8 of 10". */
  of: string;
  warning: string;
  critical: string;
  over: string;
  /** Shown when `max` is null. */
  unlimited: string;
};

export type MeterProps = {
  /** Visible label; the meter's accessible name. */
  label: ReactNode;
  hideLabel?: boolean;
  value: number;
  /** The limit. `null` means unlimited: no bar, just the value and "No limit". */
  max: number | null;
  min?: number;
  /** Fraction of the range (min to max) where the warning tone starts (default 0.8). */
  warningAt?: number;
  /** Fraction of the range (min to max) where the critical tone starts (default 1). */
  criticalAt?: number;
  /** A tick on the track, e.g. a soft limit below the ceiling. */
  marker?: MeterMarker;
  /** Formats value, max and marker (default: toLocaleString). */
  formatValue?: (value: number) => string;
  /** Replaces the "X of Y" text (visible and announced). */
  valueText?: string;
  /** Show the status word and icon at warning and above (default true). */
  showStatus?: boolean;
  /** Muted text under the bar, e.g. "Resets at midnight UTC". */
  description?: ReactNode;
  size?: "sm" | "md";
  labels?: Partial<MeterLabels>;
  className?: string;
};

const defaultLabels: MeterLabels = { of: "of", warning: "Near limit", critical: "At limit", over: "Over limit", unlimited: "No limit" };

/**
 * The level of `value` against `max` with the given thresholds, as fractions
 * of the range: (value - min) / (max - min). `min` defaults to 0.
 */
export function meterLevel(value: number, max: number | null, warningAt = 0.8, criticalAt = 1, min = 0): MeterLevel {
  if (max === null || max <= min) return "normal";
  const ratio = (value - min) / (max - min);
  if (ratio > 1 && criticalAt <= 1) return "over";
  if (ratio >= criticalAt) return "critical";
  if (ratio >= warningAt) return "warning";
  return "normal";
}

export function Meter({
  label,
  hideLabel,
  value,
  max,
  min = 0,
  warningAt = 0.8,
  criticalAt = 1,
  marker,
  formatValue = (v) => v.toLocaleString(),
  valueText,
  showStatus = true,
  description,
  size = "md",
  labels: labelsProp,
  className,
}: MeterProps) {
  const labels = { ...defaultLabels, ...labelsProp };
  const descriptionId = useId();
  const level = meterLevel(value, max, warningAt, criticalAt, min);
  const status = level === "normal" ? null : labels[level];
  const StatusIcon = level === "warning" ? TriangleAlert : CircleAlert;

  if (max === null) {
    return (
      <div className={cx(styles.root, className)} data-size={size} data-level="normal">
        <div className={styles.header}>
          <span className={cx(styles.label, hideLabel && "sr-only")}>{label}</span>
          <span className={styles.value}>
            {valueText ?? formatValue(value)} · {labels.unlimited}
          </span>
        </div>
        {description && <p className={styles.description}>{description}</p>}
      </div>
    );
  }

  const text = valueText ?? `${formatValue(value)} ${labels.of} ${formatValue(max)}`;
  const span = max - min;
  const pos = (v: number) => `${span > 0 ? Math.min(100, Math.max(0, ((v - min) / span) * 100)) : 0}%`;
  const clamped = Math.min(max, Math.max(min, value));
  const markerText = marker ? `${marker.label}: ${formatValue(marker.value)}` : null;

  const body = (
    <>
      <div className={styles.header}>
        <BaseMeter.Label className={cx(styles.label, hideLabel && "sr-only")}>{label}</BaseMeter.Label>
        <span aria-hidden className={styles.value}>
          {showStatus && status && (
            <span className={styles.status}>
              <StatusIcon className={styles.statusIcon} />
              {status}
            </span>
          )}
          {text}
        </span>
      </div>
      <BaseMeter.Track className={styles.track}>
        <BaseMeter.Indicator className={styles.indicator} />
        {marker && <span aria-hidden className={styles.marker} style={{ "--meter-marker": pos(marker.value) } as CSSProperties} />}
      </BaseMeter.Track>
      {(description || markerText) && (
        <p id={descriptionId} className={styles.description}>
          {markerText && (
            <span className={styles.markerText}>
              <span aria-hidden className={styles.markerKey} />
              {markerText}
            </span>
          )}
          {markerText && description ? " · " : null}
          {description}
        </p>
      )}
    </>
  );

  return (
    <BaseMeter.Root
      // Our own children replace Base UI's (which add the hidden "x").
      render={(rootProps) => <div {...rootProps}>{body}</div>}
      value={clamped}
      min={min}
      max={max}
      aria-valuetext={status ? `${text}, ${status.toLocaleLowerCase()}` : text}
      aria-describedby={description || marker ? descriptionId : undefined}
      className={cx(styles.root, className)}
      data-size={size}
      data-level={level}
    />
  );
}

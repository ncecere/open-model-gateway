/*
 * UsageBar: spend against a limit — "$12.40 / $50.00 · Monthly" over a Bitop
 * Meter (role="meter", near/at/over-limit wording and colour). Without a limit
 * it reads "$12.40 / ∞" and draws no bar. Amounts are integer micro-USD
 * strings shown exactly with `formatMicroUsd` (a sub-cent spend is "$0.0081",
 * never "$0"); the bar position is computed with BigInt basis points.
 * Unknown usage reads "Unknown / $50.00" with no bar: unknown is not zero.
 *
 *   <UsageBar label="Key budget" used={key.used_microusd} limit={key.limit_microusd} period="month" />
 *   <UsageBar label="Workspace spend" used="12400000" limit={null} size="sm" />
 */
import type { ReactNode } from "react";
import { Meter } from "../ui/meter/meter";
import { cx } from "../../lib/bitop-utils";
import { formatMicroUsd, formatUsd, type LimitPeriod, limitPeriodLabel, parseInteger, shareBasisPoints } from "./kit-format";
import styles from "./usage-bar.module.css";

export type UsageBarProps = {
  /** Accessible name, e.g. "Key budget" (visually hidden unless `showLabel`). */
  label: string;
  showLabel?: boolean;
  /** Spent amount, integer micro-USD string; null = unknown. */
  used: string | null | undefined;
  /** Limit, integer micro-USD string; null = no limit (∞). */
  limit: string | null | undefined;
  /** Reset period shown after the amounts ("· Monthly"). */
  period?: LimitPeriod;
  /** Fraction where the warning tone starts (default 0.8). */
  warningAt?: number;
  /** Muted text under the bar, e.g. "Resets monthly on the 1st at 00:00 UTC". */
  description?: ReactNode;
  size?: "sm" | "md";
  className?: string;
};

/** "$12.40 / $50.00 · Monthly", "$12.40 / ∞", "Unknown / $50.00". Spend is rounded for reading (`formatUsd`); the limit is exact. */
export function usageText(used: string | null | undefined, limit: string | null | undefined, period?: LimitPeriod): string {
  const amount = parseInteger(used) === null ? "Unknown" : formatUsd(used);
  const cap = limit === null || limit === undefined ? "∞" : formatMicroUsd(limit);
  return `${amount} / ${cap}${period ? ` · ${limitPeriodLabel(period)}` : ""}`;
}

export function UsageBar({ label, showLabel = false, used, limit, period, warningAt = 0.8, description, size = "md", className }: UsageBarProps) {
  const text = usageText(used, limit, period);
  const knownUsed = parseInteger(used) !== null;
  const cap = parseInteger(limit);
  const unlimited = limit === null || limit === undefined;
  if (!knownUsed || unlimited || cap === null || cap === 0n) {
    // No bar: an unknown amount, no limit, or a limit that can't be read.
    const spoken = `${label}: ${text.replace(" / ∞", ", no limit").replace(" / ", " of ")}`;
    return (
      <div className={cx(styles.plain, className)} data-size={size}>
        <span className={cx(styles.label, !showLabel && "sr-only")}>{label}</span>
        <span className={styles.text} aria-label={spoken} role="img" data-unknown={knownUsed ? undefined : ""}>{text}</span>
        {description && <p className={styles.description}>{description}</p>}
      </div>
    );
  }
  const bp = shareBasisPoints(used, cap)!;
  // Number() only positions the bar; the visible and announced amounts stay exact strings.
  const value = Number(bp > 1_000_000n ? 1_000_000n : bp);
  return (
    <Meter
      className={className}
      label={label}
      hideLabel={!showLabel}
      size={size}
      value={value}
      max={10000}
      warningAt={warningAt}
      valueText={text}
      description={description}
    />
  );
}

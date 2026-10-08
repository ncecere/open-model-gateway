/*
 * BudgetRing: a small SVG ring of spend against a budget with the share in
 * the middle and the exact amounts beside it ("$0.0081 / $50.00 · Monthly").
 * It is a role="meter" with an aria-valuetext that says the same thing, so the
 * ring is never the only carrier of the number. A tiny real share shows "<1%"
 * (never 0%); no budget shows "∞" and an empty ring; unknown usage shows "?"
 * with a dashed ring and "Unknown". Warning at 80%, danger at/over 100%, with
 * the words "Near limit" / "At limit" / "Over limit" next to the colour.
 * The fill animates only without prefers-reduced-motion.
 *
 *   <BudgetRing label="Key limit" used={k.used_microusd} limit={k.limit_microusd} period="month" />
 */
import type { CSSProperties } from "react";
import { cx } from "../../lib/bitop-utils";
import { formatBasisPoints, type LimitPeriod, parseInteger, shareBasisPoints } from "./kit-format";
import { usageText } from "./usage-bar";
import styles from "./budget-ring.module.css";

export type BudgetRingProps = {
  /** Names the meter and is shown above the amounts, e.g. "Key limit". */
  label: string;
  used: string | null | undefined;
  /** null = no budget. */
  limit: string | null | undefined;
  period?: LimitPeriod;
  /** Fraction where the warning tone starts (default 0.8). */
  warningAt?: number;
  size?: "sm" | "md";
  /** Hide the amounts next to the ring (they remain in the accessible text). */
  ringOnly?: boolean;
  className?: string;
};

const R = 16;
const C = 2 * Math.PI * R;

export function BudgetRing({ label, used, limit, period, warningAt = 0.8, size = "md", ringOnly = false, className }: BudgetRingProps) {
  const text = usageText(used, limit, period);
  const usedValue = parseInteger(used), cap = parseInteger(limit);
  const unlimited = limit === null || limit === undefined;
  const bp = usedValue === null || cap === null || cap === 0n ? null : shareBasisPoints(usedValue, cap);
  const level = bp === null ? "none" : bp > 10000n ? "over" : bp === 10000n ? "critical" : bp >= BigInt(Math.round(warningAt * 10000)) ? "warning" : "normal";
  const status = level === "over" ? "Over limit" : level === "critical" ? "At limit" : level === "warning" ? "Near limit" : null;
  const center = usedValue === null ? "?" : unlimited ? "∞" : bp === null ? "—" : formatBasisPoints(bp, { whole: true, nonZero: usedValue > 0n });
  const fraction = bp === null ? 0 : Math.min(1, Number(bp) / 10000);
  const spoken = `${text.replace(" / ∞", ", no limit").replace(" / ", " of ")}${bp !== null ? `, ${center} used` : ""}${status ? `, ${status.toLowerCase()}` : ""}`;
  const meter = bp !== null ? { role: "meter", "aria-valuemin": 0, "aria-valuemax": 100, "aria-valuenow": Math.min(100, Number(bp) / 100) } : { role: "img" };
  return (
    <div className={cx(styles.root, className)} data-size={size} data-level={level} data-unknown={usedValue === null ? "" : undefined}>
      <span {...meter} aria-label={bp !== null ? label : `${label}: ${spoken}`} aria-valuetext={bp !== null ? spoken : undefined} className={styles.ring}>
        <svg viewBox="0 0 40 40" aria-hidden focusable="false" className={styles.svg}>
          <circle className={styles.track} cx="20" cy="20" r={R} />
          {fraction > 0 && <circle className={styles.fill} cx="20" cy="20" r={R} strokeDasharray={C.toFixed(3)} style={{ "--ring-offset": (C * (1 - fraction)).toFixed(3) } as CSSProperties} />}
        </svg>
        <span aria-hidden className={styles.center}>{center}</span>
      </span>
      {!ringOnly && <span className={styles.text} aria-hidden>
        <span className={styles.label}>{label}</span>
        <span className={styles.amounts}>{text}</span>
        {status && <span className={styles.status}>{status}</span>}
      </span>}
    </div>
  );
}

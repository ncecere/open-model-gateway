import { ArrowDownRight, ArrowRight, ArrowUpRight } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./stat-card.module.css";

export type StatTrend = "up" | "down" | "flat";

export type StatCardProps = {
  label: ReactNode;
  /** The headline number (rendered with tabular figures). */
  value: ReactNode;
  /** Change indicator, e.g. { value: "+12%", trend: "up" }. */
  delta?: {
    value: ReactNode;
    trend: StatTrend;
    /** Whether the change is good; defaults to up = positive. */
    sentiment?: "positive" | "negative" | "neutral";
    /** Context, e.g. "vs last week". */
    label?: ReactNode;
  };
  /** Decorative icon shown in a tinted square. */
  icon?: ReactNode;
  /** Muted footnote under the value. */
  hint?: ReactNode;
  className?: string;
};

const trendIcons = { up: ArrowUpRight, down: ArrowDownRight, flat: ArrowRight } as const;
const trendWords = { up: "Increased by", down: "Decreased by", flat: "Unchanged:" } as const;

/** A metric tile: label, big number, optional delta and icon. Uses <dl> semantics. */
export function StatCard({ label, value, delta, icon, hint, className }: StatCardProps) {
  const sentiment = delta?.sentiment ?? (delta?.trend === "up" ? "positive" : delta?.trend === "down" ? "negative" : "neutral");
  const TrendIcon = delta ? trendIcons[delta.trend] : null;
  return (
    <div className={cx(styles.card, className)}>
      {icon && (
        <span aria-hidden className={styles.icon}>
          {icon}
        </span>
      )}
      <dl className={styles.list}>
      <dt className={styles.label}>{label}</dt>
      <dd className={styles.value}>{value}</dd>
      {(delta || hint) && (
        <dd className={styles.meta}>
          {delta && TrendIcon && (
            <span className={styles.delta} data-sentiment={sentiment}>
              <TrendIcon aria-hidden />
              <span className="sr-only">{trendWords[delta.trend]} </span>
              {delta.value}
            </span>
          )}
          {delta?.label && <span className={styles.hint}>{delta.label}</span>}
          {hint && <span className={styles.hint}>{hint}</span>}
        </dd>
      )}
      </dl>
    </div>
  );
}

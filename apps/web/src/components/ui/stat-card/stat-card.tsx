"use client";

import { useRender } from "@base-ui/react/use-render";
import { ArrowDownRight, ArrowRight, ArrowUpRight } from "lucide-react";
import type { ReactNode } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./stat-card.module.css";

/*
 * A metric tile. With `href` or `render` the label becomes a link whose hit
 * area is stretched over the whole card (one tab stop, named by the label),
 * so the tile opens a page without nesting the <dl> inside an <a>.
 * `chart` puts a small trend (a Sparkline) under the value.
 */

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
  /** Block content under the value, e.g. a breakdown list. */
  details?: ReactNode;
  /** A small chart under the value, e.g. `<Sparkline values={daily} label="…" />`. */
  chart?: ReactNode;
  /** Makes the card a link to this URL. */
  href?: string;
  /** Makes the card a link rendered by another element, e.g. `render={<Link to="/sources" />}`. */
  render?: useRender.RenderProp;
  className?: string;
};

const trendIcons = { up: ArrowUpRight, down: ArrowDownRight, flat: ArrowRight } as const;
const trendWords = { up: "Increased by", down: "Decreased by", flat: "Unchanged:" } as const;

function StatLink({ href, render, children }: { href?: string; render?: useRender.RenderProp; children: ReactNode }) {
  return useRender({ render, defaultTagName: "a", props: { href, className: styles.link, children } });
}

/** A metric tile: label, big number, optional delta, icon and link. Uses <dl> semantics. */
export function StatCard({ label, value, delta, icon, hint, details, chart, href, render, className }: StatCardProps) {
  const sentiment = delta?.sentiment ?? (delta?.trend === "up" ? "positive" : delta?.trend === "down" ? "negative" : "neutral");
  const TrendIcon = delta ? trendIcons[delta.trend] : null;
  const linked = href !== undefined || render !== undefined;
  return (
    <div className={cx(styles.card, className)} data-linked={dataFlag(linked)}>
      {icon && (
        <span aria-hidden className={styles.icon}>
          {icon}
        </span>
      )}
      <dl className={styles.list}>
        <dt className={styles.label}>
          {linked ? (
            <StatLink href={href} render={render}>
              {label}
            </StatLink>
          ) : (
            label
          )}
        </dt>
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
        {chart && <dd className={styles.chart}>{chart}</dd>}
        {details && <dd className={styles.details}>{details}</dd>}
      </dl>
    </div>
  );
}

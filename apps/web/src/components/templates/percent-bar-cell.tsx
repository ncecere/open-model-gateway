/*
 * PercentBarCell: a table cell value with its share of the total as text and a
 * thin bar (Explore / top-N tables). The share is computed exactly with BigInt
 * from integer strings (micro-USD, tokens, requests): a tiny real share reads
 * "<0.1%", never "0%"; an unknown part or total reads "Unknown" with no bar; a
 * zero total reads "—". The bar is decorative (the percentage is text).
 *
 *   cell: r => <PercentBarCell value={formatMicroUsd(r.spend)} part={r.spend} total={totals.spend} />
 */
import type { CSSProperties, ReactNode } from "react";
import { cx } from "../../lib/bitop-utils";
import { cssPercent, formatShare, parseInteger, shareBasisPoints } from "./kit-format";
import styles from "./percent-bar-cell.module.css";

type Amount = string | number | bigint | null | undefined;

export type PercentBarCellProps = {
  /** The formatted value shown first, e.g. "$12.40" or "1,204". Omit to show only the share. */
  value?: ReactNode;
  /** This row's amount (integer string/bigint/safe integer). */
  part: Amount;
  /** The total the share is of. */
  total: Amount;
  /** Bar colour (default primary). */
  tone?: "primary" | "neutral" | "warning" | "danger";
  className?: string;
};

export function PercentBarCell({ value, part, total, tone = "primary", className }: PercentBarCellProps) {
  const known = parseInteger(part) !== null && parseInteger(total) !== null && parseInteger(total) !== 0n;
  const bp = known ? shareBasisPoints(part, total) : null;
  const share = formatShare(part, total);
  return (
    <span className={cx(styles.root, className)} data-tone={tone}>
      <span className={styles.text}>
        {value !== undefined && <span className={styles.value}>{value}</span>}
        <span className={styles.share} data-unknown={share === "Unknown" ? "" : undefined}>{share === "—" ? <><span aria-hidden>—</span><span className="sr-only">No total</span></> : share}</span>
      </span>
      {known && <span aria-hidden className={styles.track}><span className={styles.bar} style={{ "--percent": cssPercent(bp) } as CSSProperties} /></span>}
    </span>
  );
}

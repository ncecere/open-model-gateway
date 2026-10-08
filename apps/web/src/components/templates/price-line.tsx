/*
 * PriceLine: a price with its unit always next to it — "$0.50 /M tokens" —
 * from an exact integer micro-USD string (per `unit`), optional tier rows
 * ("≤128K  $0.50 /M tokens", ">128K  $2.50 /M tokens") and an optional struck
 * list price before a discounted one. A missing price reads "Unknown · not
 * free" (amber), never $0; an explicit not-applicable reads "Not applicable".
 * PriceLines lays several labelled lines out as a description list.
 *
 *   <PriceLine amount="500000" unit="M tokens" listAmount="600000" />
 *   <PriceLine unit="M tokens" tiers={[{ label: "≤128K", amount: "500000" }, { label: ">128K", amount: "2500000" }]} />
 *   <PriceLines items={[{ label: "Input", price: { amount: "100000", unit: "M tokens" } }, { label: "Web search", price: { amount: null, unit: "1K calls" } }]} />
 */
import type { ReactNode } from "react";
import { cx } from "../../lib/bitop-utils";
import { formatMicroUsd, parseInteger } from "./kit-format";
import styles from "./price-line.module.css";

export type PriceTier = { /** Threshold label, e.g. "≤128K" or "1024×1024". */ label: string; amount: string | null | undefined; unit?: string };

export type PriceLineProps = {
  /** Integer micro-USD per unit; null/undefined = unknown (never free). Ignored when `tiers` is given. */
  amount?: string | null;
  /** The unit after "/", e.g. "M tokens", "image", "1K calls". */
  unit: string;
  /** A higher list price, shown struck through before `amount`. */
  listAmount?: string | null;
  tiers?: PriceTier[];
  /** The meter does not apply to this route (e.g. output for embeddings). */
  notApplicable?: boolean;
  className?: string;
};

function Amount({ amount, unit, list }: { amount: string | null | undefined; unit: string; list?: string | null }) {
  if (parseInteger(amount) === null) return <span className={styles.unknown}>Unknown · not free</span>;
  const showList = list != null && parseInteger(list) !== null && BigInt(list) > BigInt(amount!);
  return (
    <span className={styles.price}>
      {showList && <><s className={styles.list}><span className="sr-only">List price </span>{formatMicroUsd(list)}</s>{" "}</>}
      <span className={styles.amount}>{showList && <span className="sr-only">now </span>}{formatMicroUsd(amount)}</span>{" "}
      {/* Review rule 6: "$0.10 / M tokens", a space on both sides of the slash. */}
      <span className={styles.unit}><span aria-hidden>/ </span><span className="sr-only">per </span>{unit}</span>
    </span>
  );
}

export function PriceLine({ amount, unit, listAmount, tiers, notApplicable, className }: PriceLineProps) {
  if (notApplicable) return <span className={cx(styles.na, className)}>Not applicable</span>;
  if (tiers && tiers.length > 0) {
    return (
      <span className={cx(styles.tiers, className)} role="list">
        {tiers.map(t => (
          <span key={t.label} role="listitem" className={styles.tier}>
            <span className={styles.tierLabel}>{t.label}</span>
            <Amount amount={t.amount} unit={t.unit ?? unit} />
          </span>
        ))}
      </span>
    );
  }
  return <span className={className}><Amount amount={amount} unit={unit} list={listAmount} /></span>;
}

export type PriceLinesProps = { items: { label: ReactNode; price: PriceLineProps }[]; className?: string };

/** Labelled price lines (Input, Output, Cache read…) as a two-column description list; stacks at 390px. */
export function PriceLines({ items, className }: PriceLinesProps) {
  return (
    <dl className={cx(styles.lines, className)}>
      {items.map((item, i) => (
        <div key={i} className={styles.lineRow}>
          <dt className={styles.lineLabel}>{item.label}</dt>
          <dd className={styles.lineValue}><PriceLine {...item.price} /></dd>
        </div>
      ))}
    </dl>
  );
}

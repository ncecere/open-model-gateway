/*
 * Money: a micro-USD amount for reading (tiles, cards, table cells). Shows the
 * rounded headline from `formatUsd` ("$8.27", "$0.0081") and, when rounding
 * changed it, the exact value (`formatMicroUsd`, "$8.271628") as its title. Unknown stays "Unknown". Prices, configured limits,
 * inputs and exports use the exact value directly.
 *
 *   <StatTile label="Spend" value={<Money value={total} />} />
 */
import { formatMicroUsd, formatUsd } from "../../lib/governance";

/** The rounded display text and, when it differs, the exact amount. */
export function moneyText(value: string | null | undefined): { text: string; exact: string | null } {
  const text = formatUsd(value);
  const exact = formatMicroUsd(value);
  return { text, exact: exact === text ? null : exact };
}

export function Money({ value, prefix = "", className }: { value: string | null | undefined; /** Text before the amount, e.g. "+" or "At least ". */ prefix?: string; className?: string }) {
  const { text, exact } = moneyText(value);
  if (exact === null) return <span className={className}>{prefix}{text}</span>;
  return <span className={className} title={`Exactly ${exact}`}>{prefix}{text}</span>;
}

/*
 * Shared display formatters (full UI review, app-wide rules 3–6). One place for
 * how the dashboard writes readiness, statuses, dates, tokens and prices, so a
 * value reads the same on every page. Money stays exact (micro-USD strings,
 * lib/governance formatMicroUsd); unknown is never zero.
 *
 *   Readiness   Ready / Needs setup / Needs attention / Not serving   (readinessText)
 *   Tokens      "145 in · 0 out", "Unknown", "Not applicable"          (tokensText)
 *   Prices      "$0.10 / M input tokens"                              (priceText)
 *   Table time  "Oct 8, 2:30 AM" (year only when not this year)       (tableTime)
 *   Detail time "Oct 8, 2026, 2:30 AM EDT"                            (detailTime)
 *   UTC period  "Oct 1 – Oct 8, 2026 (UTC)" (end shown inclusive)     (utcPeriod)
 */
import { formatMicroUsd } from "./governance";
export { readinessText } from "./model-setup";
export { countText, tokensText } from "./requests";

const valid = (d: Date) => Number.isFinite(d.getTime());
const yearOf = (d: Date, timeZone?: string) => new Intl.DateTimeFormat("en-US", { year: "numeric", timeZone }).format(d);

/** "Oct 8, 2:30 AM" for tables (12-hour, the viewer's zone; the year only when it isn't this year). Invalid input is returned as given. */
export function tableTime(iso: string, now = new Date(), timeZone?: string): string {
  const d = new Date(iso);
  if (!valid(d)) return iso;
  const sameYear = yearOf(d, timeZone) === yearOf(now, timeZone);
  return new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", ...(sameYear ? {} : { year: "numeric" as const }), hour: "numeric", minute: "2-digit", hour12: true, timeZone }).format(d);
}

/** "Oct 8, 2026, 2:30 AM EDT" for detail pages. */
export function detailTime(iso: string, timeZone?: string): string {
  const d = new Date(iso);
  if (!valid(d)) return iso;
  return new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit", hour12: true, timeZoneName: "short", timeZone }).format(d);
}

/** A UTC period "[start, end)" from YYYY-MM-DD dates, end shown inclusive: "Oct 1 – Oct 8, 2026 (UTC)". */
export function utcPeriod(start: string, endExclusive: string): string {
  const a = new Date(`${start}T00:00:00Z`), b = new Date(new Date(`${endExclusive}T00:00:00Z`).getTime() - 86_400_000);
  if (!valid(a) || !valid(b)) return `${start} – ${endExclusive} (UTC)`;
  const day = (d: Date, year: boolean) => new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", ...(year ? { year: "numeric" as const } : {}), timeZone: "UTC" }).format(d);
  const sameYear = a.getUTCFullYear() === b.getUTCFullYear();
  if (a.getTime() >= b.getTime()) return `${day(a, true)} (UTC)`;
  return `${day(a, !sameYear)} – ${day(b, true)} (UTC)`;
}

/** "Oct 1, 2026" for a YYYY-MM-DD UTC date in prose (never the raw ISO date). */
export function utcDay(date: string): string {
  const d = new Date(`${date}T00:00:00Z`);
  return valid(d) ? new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" }).format(d) : date;
}

/** "$0.10 / M input tokens" (a space on both sides of the slash); a missing rate is "Unknown". */
export function priceText(amountMicroUsd: string | null | undefined, unit: string): string {
  if (amountMicroUsd == null || !/^\d+$/.test(amountMicroUsd)) return "Unknown";
  return `${formatMicroUsd(amountMicroUsd)} / ${unit.replace(/^\//, "").trim()}`;
}

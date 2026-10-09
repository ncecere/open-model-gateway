/*
 * Exact formatting helpers shared by the UI kit templates (StatTile,
 * PercentBarCell, UsageBar, BudgetRing, PriceLine, Timeline).
 *
 * Money stays an integer micro-USD decimal string end to end: shares and
 * changes are computed with BigInt (basis points), never Number/parseFloat,
 * and every amount is displayed through lib/governance `formatMicroUsd`, so a
 * sub-cent value is "$0.0081", never "$0". `null`/invalid inputs are
 * "unknown", never zero.
 */
import { formatMicroUsd, formatUsd } from "../../lib/governance";

export { formatMicroUsd, formatUsd };

/** A non-negative integer decimal string (micro-USD, token counts) as BigInt; null when unknown or invalid. */
export function parseInteger(value: string | number | bigint | null | undefined): bigint | null {
  if (value === null || value === undefined) return null;
  if (typeof value === "bigint") return value >= 0n ? value : null;
  if (typeof value === "number") return Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : null;
  return value.length <= 128 && /^\d+$/.test(value) ? BigInt(value) : null;
}

/** `part / total` in basis points (1 bp = 0.01%), rounded down. null when either side is unknown or the total is zero. */
export function shareBasisPoints(part: string | number | bigint | null | undefined, total: string | number | bigint | null | undefined): bigint | null {
  const p = parseInteger(part), t = parseInteger(total);
  if (p === null || t === null || t === 0n) return null;
  return (p * 10000n) / t;
}

/**
 * Basis points as a percentage label: "12.5%", "100%", "0%". A non-zero share below 0.1% is "<0.1%" (and below
 * 1% with `whole`, "<1%"), so a tiny real amount never reads as 0%.
 */
export function formatBasisPoints(bp: bigint | null, { whole = false, nonZero = false }: { whole?: boolean; nonZero?: boolean } = {}): string {
  if (bp === null) return "Unknown";
  if (bp === 0n) return nonZero ? (whole ? "<1%" : "<0.1%") : "0%";
  if (whole) return bp < 100n ? "<1%" : `${(bp / 100n).toLocaleString()}%`;
  if (bp < 10n) return "<0.1%";
  const tenths = bp / 10n; // 0.1% steps
  const int = tenths / 10n, frac = tenths % 10n;
  return `${int.toLocaleString()}${frac === 0n ? "" : `.${frac}`}%`;
}

/** Share of `total` as a percentage label; "Unknown" when either side is unknown, "—" for a zero total. */
export function formatShare(part: string | number | bigint | null | undefined, total: string | number | bigint | null | undefined, whole = false): string {
  const t = parseInteger(total);
  if (parseInteger(part) === null || t === null) return "Unknown";
  if (t === 0n) return "—";
  const p = parseInteger(part)!;
  return formatBasisPoints(shareBasisPoints(p, t), { whole, nonZero: p > 0n });
}

/** Basis points as a CSS percentage for a bar width (clamped 0–100%). */
export function cssPercent(bp: bigint | null): string {
  if (bp === null || bp <= 0n) return "0%";
  if (bp >= 10000n) return "100%";
  return `${Number(bp) / 100}%`;
}

export type ChangeDirection = "up" | "down" | "flat";
export type Change = { direction: ChangeDirection; /** "+12.5%", "−3%", "0%", or "New" when the previous value was zero. */ text: string };

/**
 * Change from `previous` to `current`. Integer strings/bigints are compared exactly with BigInt; finite numbers
 * (ratios such as a cache hit rate) with Number. null when either side is unknown (no fabricated delta).
 */
export function percentChange(current: string | number | bigint | null | undefined, previous: string | number | bigint | null | undefined): Change | null {
  const exactIntegers = (v: unknown) => typeof v !== "number" || (Number.isSafeInteger(v) && v >= 0);
  if (typeof current === "number" && typeof previous === "number" && !(exactIntegers(current) && exactIntegers(previous))) {
    if (!Number.isFinite(current) || !Number.isFinite(previous)) return null;
    if (current === previous) return { direction: "flat", text: "0%" };
    if (previous === 0) return { direction: current > previous ? "up" : "down", text: "New" };
    const pct = ((current - previous) / Math.abs(previous)) * 100;
    const rounded = Math.round(pct * 10) / 10;
    if (rounded === 0) return { direction: pct > 0 ? "up" : "down", text: pct > 0 ? "<+0.1%" : "<−0.1%" };
    return { direction: pct > 0 ? "up" : "down", text: `${pct > 0 ? "+" : "−"}${Math.abs(rounded).toLocaleString()}%` };
  }
  const c = parseInteger(current), p = parseInteger(previous);
  if (c === null || p === null) return null;
  if (c === p) return { direction: "flat", text: "0%" };
  if (p === 0n) return { direction: "up", text: "New" };
  const up = c > p, diff = up ? c - p : p - c;
  const bp = (diff * 10000n) / p;
  const label = formatBasisPoints(bp, { nonZero: true }).replace("<", "");
  return { direction: up ? "up" : "down", text: bp < 10n ? `<${up ? "+" : "−"}${label}` : `${up ? "+" : "−"}${label}` };
}

/** Milliseconds as "41 ms", "1.2 s", "2 min 5 s"; "Unknown" for null/invalid. */
export function formatDurationMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return "Unknown";
  if (ms < 1000) return `${Math.round(ms).toLocaleString()} ms`;
  if (ms < 60_000) return `${(Math.round(ms / 100) / 10).toLocaleString()} s`;
  const minutes = Math.floor(ms / 60_000), seconds = Math.round((ms % 60_000) / 1000);
  return seconds ? `${minutes.toLocaleString()} min ${seconds} s` : `${minutes.toLocaleString()} min`;
}

/** Budget/limit reset periods used by PeriodPills, UsageBar and BudgetRing. */
export type LimitPeriod = "day" | "week" | "month" | "lifetime";
export const limitPeriods: { value: LimitPeriod; label: string; reset: string }[] = [
  { value: "day", label: "Daily", reset: "Resets daily at 00:00 UTC" },
  { value: "week", label: "Weekly", reset: "Resets weekly on Monday at 00:00 UTC" },
  { value: "month", label: "Monthly", reset: "Resets monthly on the 1st at 00:00 UTC" },
  { value: "lifetime", label: "Lifetime", reset: "Never resets" },
];
export const limitPeriodLabel = (period: LimitPeriod) => limitPeriods.find(p => p.value === period)?.label ?? period;

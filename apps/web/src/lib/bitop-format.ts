/*
 * Locale-aware formatting helpers built on Intl. Installed by the `format`
 * item at `@/lib/bitop-format`. Every function is pure, takes an explicit
 * `locale` (default: the runtime's locale) and never throws: invalid input
 * returns "" (or the `fallback` you pass).
 *
 *   formatDate("2026-09-26T12:00:00Z", { style: "datetime" })  // "Sep 26, 2026, 12:00 PM"
 *   formatNumber(1234567, { compact: true })                   // "1.2M"
 *   formatBytes(1_500_000)                                     // "1.5 MB"
 *   formatBytes(1536, { binary: true })                        // "1.5 KiB"
 *   formatRelativeTime(Date.now() - 3 * 3600_000)              // "3 hours ago"
 *   plural(3, { one: "file", other: "files" })                 // "3 files"
 */

/** Anything `new Date()` understands. */
export type DateInput = Date | string | number;

/** Parses a date input; returns undefined for missing or invalid values. */
export function toDate(value: DateInput | null | undefined): Date | undefined {
  if (value === null || value === undefined || value === "") return undefined;
  const date = value instanceof Date ? value : new Date(value);
  return Number.isNaN(date.getTime()) ? undefined : date;
}

export type DateStyle = "date" | "datetime" | "time" | "long";

export type FormatDateOptions = {
  /**
   * `date` "Sep 26, 2026" · `datetime` "Sep 26, 2026, 12:00 PM" ·
   * `time` "12:00 PM" · `long` "Saturday, September 26, 2026 at 12:00:00 PM UTC".
   * Default `datetime`.
   */
  style?: DateStyle;
  locale?: string;
  /** IANA time zone, e.g. "UTC" or "Europe/Berlin" (default: the runtime's). */
  timeZone?: string;
  /** Returned for missing or invalid dates (default ""). */
  fallback?: string;
};

const DATE_STYLES: Record<DateStyle, Intl.DateTimeFormatOptions> = {
  date: { dateStyle: "medium" },
  datetime: { dateStyle: "medium", timeStyle: "short" },
  time: { timeStyle: "short" },
  long: { dateStyle: "full", timeStyle: "long" },
};

/** Formats a date in the given style. Missing or invalid dates return `fallback` (""). */
export function formatDate(value: DateInput | null | undefined, options: FormatDateOptions = {}): string {
  const { style = "datetime", locale, timeZone, fallback = "" } = options;
  const date = toDate(value);
  if (!date) return fallback;
  try {
    return new Intl.DateTimeFormat(locale, { ...DATE_STYLES[style], timeZone }).format(date);
  } catch {
    // An unknown time zone or locale tag throws a RangeError.
    return fallback;
  }
}

export type FormatNumberOptions = {
  locale?: string;
  /** Default 3 (Intl's default), or 1 when `compact`. */
  maximumFractionDigits?: number;
  /** Short form: "1.2K", "3.4M". */
  compact?: boolean;
};

/** Formats a number with grouping ("1,234,567"). NaN returns "". */
export function formatNumber(n: number, options: FormatNumberOptions = {}): string {
  const { locale, compact = false, maximumFractionDigits = compact ? 1 : 3 } = options;
  if (typeof n !== "number" || Number.isNaN(n)) return "";
  try {
    return new Intl.NumberFormat(locale, { notation: compact ? "compact" : "standard", maximumFractionDigits }).format(n);
  } catch {
    return String(n);
  }
}

export type FormatBytesOptions = {
  locale?: string;
  /** Powers of 1024 with IEC units (KiB, MiB…) instead of powers of 1000 (kB, MB…). Default false. */
  binary?: boolean;
  /** Default: 1 below 10 of a unit ("1.5 MB"), 0 above ("42 MB"). Whole bytes never show decimals. */
  maximumFractionDigits?: number;
};

const DECIMAL_UNITS = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
const BINARY_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/** "999 B", "1.5 kB", "42 MB"; with `binary`, "1.5 KiB". Non-finite input returns "". */
export function formatBytes(bytes: number, options: FormatBytesOptions = {}): string {
  const { locale, binary = false } = options;
  if (typeof bytes !== "number" || !Number.isFinite(bytes)) return "";
  const base = binary ? 1024 : 1000;
  const units = binary ? BINARY_UNITS : DECIMAL_UNITS;
  let value = Math.abs(bytes);
  let i = 0;
  while (value >= base && i < units.length - 1) {
    value /= base;
    i++;
  }
  const digits = i === 0 ? 0 : (options.maximumFractionDigits ?? (value < 10 ? 1 : 0));
  // Rounding can reach the next unit ("1000 kB"): promote it.
  if (i < units.length - 1 && Number(value.toFixed(digits)) >= base) {
    value /= base;
    i++;
  }
  const n = formatNumber(Math.sign(bytes) * value, { locale, maximumFractionDigits: i === 0 ? 0 : digits });
  return `${n} ${units[i]}`;
}

export type FormatRelativeTimeOptions = {
  /** Reference time (default: now). */
  now?: DateInput;
  locale?: string;
  /** `auto` (default) gives "yesterday" / "now"; `always` gives "1 day ago" / "in 0 seconds". */
  numeric?: "auto" | "always";
  /** Returned for missing or invalid dates (default ""). */
  fallback?: string;
};

const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;
const MONTH = 30.436875 * DAY; // average Gregorian month
const YEAR = 365.2425 * DAY;

/**
 * Picks the unit for a distance in seconds. Each unit is used until the
 * rounded value would reach the next one, so it never says "60 minutes" or
 * "12 months".
 */
export function relativeTimeUnit(seconds: number): [value: number, unit: Intl.RelativeTimeFormatUnit] {
  const abs = Math.abs(seconds);
  const pick = (size: number, unit: Intl.RelativeTimeFormatUnit): [number, Intl.RelativeTimeFormatUnit] => {
    const v = Math.round(abs / size);
    return [seconds < 0 ? -v : v, unit];
  };
  if (abs < MINUTE - 0.5) return pick(1, "second");
  if (abs < HOUR - MINUTE / 2) return pick(MINUTE, "minute");
  if (abs < DAY - HOUR / 2) return pick(HOUR, "hour");
  if (abs < WEEK - DAY / 2) return pick(DAY, "day");
  if (abs < MONTH) return pick(WEEK, "week");
  if (abs < YEAR - MONTH / 2) return pick(MONTH, "month");
  return pick(YEAR, "year");
}

/** Whole calendar days from `now` to `date` in the local time zone (negative in the past). */
export function calendarDayDiff(date: Date, now: Date): number {
  const midnight = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  return Math.round((midnight(date) - midnight(now)) / (DAY * 1000));
}

/**
 * "3 hours ago", "yesterday", "in 2 weeks". Missing or invalid dates return
 * `fallback` (""). From half a day to a week away it counts calendar days,
 * so "yesterday" is always the day before today (10 PM two days ago is "2
 * days ago", like that morning); closer, hours and minutes; further, weeks,
 * months and years.
 */
export function formatRelativeTime(value: DateInput | null | undefined, options: FormatRelativeTimeOptions = {}): string {
  const { locale, numeric = "auto", fallback = "" } = options;
  const date = toDate(value);
  const now = options.now === undefined ? new Date() : toDate(options.now);
  if (!date || !now) return fallback;
  const seconds = (date.getTime() - now.getTime()) / 1000;
  const days = calendarDayDiff(date, now);
  const byDay = Math.abs(seconds) >= DAY / 2 && days !== 0 && Math.abs(days) < 7;
  const [n, unit] = byDay ? [days, "day" as const] : relativeTimeUnit(seconds);
  try {
    // `+ 0` turns -0 into 0 so "now" isn't rendered as "0 seconds ago".
    return new Intl.RelativeTimeFormat(locale, { numeric }).format(n + 0, unit);
  } catch {
    return fallback;
  }
}

export type PluralForms = {
  one: string;
  other: string;
  /** Used for a count of exactly 0, verbatim (e.g. "No files"); `{count}` is still replaced. */
  zero?: string;
  /** Extra CLDR categories for languages that have them (e.g. Polish "few", "many"). */
  two?: string;
  few?: string;
  many?: string;
};

/**
 * Picks the plural form for `count` with Intl.PluralRules and includes the
 * formatted count. A form containing `{count}` is used as a template;
 * otherwise the count is prefixed:
 *
 *   plural(1, { one: "file", other: "files" })                   // "1 file"
 *   plural(1200, { one: "file", other: "files" })                // "1,200 files"
 *   plural(3, { one: "{count} file left", other: "{count} files left" })
 *   plural(0, { one: "file", other: "files", zero: "No files" }) // "No files"
 */
export function plural(count: number, forms: PluralForms, locale?: string): string {
  const formatted = formatNumber(count, { locale });
  const fill = (form: string) => form.replaceAll("{count}", formatted);
  if (count === 0 && forms.zero !== undefined) return fill(forms.zero);
  let category: Intl.LDMLPluralRule = "other";
  try {
    category = new Intl.PluralRules(locale).select(count);
  } catch {
    category = count === 1 ? "one" : "other";
  }
  const form = (category !== "zero" ? forms[category] : undefined) ?? forms.other;
  return form.includes("{count}") ? fill(form) : `${formatted} ${form}`;
}

"use client";

import { type ComponentPropsWithRef, type ReactNode, useEffect, useReducer } from "react";
import { type DateInput, type DateStyle, formatDate, formatRelativeTime, toDate } from "@/lib/bitop-format";
import { cx } from "@/lib/bitop-utils";
import styles from "./time.module.css";

/*
 * Time: a <time dateTime="…ISO…"> with locale-formatted text.
 *
 *   <Time value={row.createdAt} />                    // "Sep 26, 2026, 12:00 PM"
 *   <Time value={row.createdAt} format="date" />      // "Sep 26, 2026"
 *   <Time value={row.updatedAt} format="relative" />  // "3 hours ago", full date in the title
 *
 * Relative times re-render while mounted, less often as the value ages
 * (every second under a minute, every 15 s under an hour, every 5 min under a
 * day, then hourly). The timer is cleared on unmount and never runs for the
 * absolute formats or when `now` is fixed.
 */

export type TimeFormat = DateStyle | "relative";

export type TimeProps = Omit<ComponentPropsWithRef<"time">, "dateTime" | "children"> & {
  /** A Date, ISO string or epoch milliseconds. */
  value: DateInput | null | undefined;
  /** Default "datetime". */
  format?: TimeFormat;
  locale?: string;
  /** IANA time zone for absolute formats and the relative title (default: the runtime's). */
  timeZone?: string;
  /** Fixed reference time for `relative` (disables the refresh timer; useful for SSR and tests). */
  now?: DateInput;
  /** Rendered instead of <time> when `value` is missing or invalid. Default: nothing. */
  fallback?: ReactNode;
};

/** How long to wait before refreshing a relative time that is `ageMs` away from now. */
export function relativeRefreshInterval(ageMs: number): number {
  const age = Math.abs(ageMs);
  if (age < 60_000) return 1_000;
  if (age < 3_600_000) return 15_000;
  if (age < 86_400_000) return 300_000;
  return 3_600_000;
}

/** A semantic <time> element; `format="relative"` keeps itself up to date. */
export function Time({ value, format = "datetime", locale, timeZone, now, fallback = null, title, className, ...props }: TimeProps) {
  const date = toDate(value);
  const time = date?.getTime();
  const ticking = format === "relative" && now === undefined && time !== undefined;
  const [, refresh] = useReducer((n: number) => n + 1, 0);

  useEffect(() => {
    if (!ticking || time === undefined) return;
    let timer: ReturnType<typeof setTimeout>;
    const schedule = () => {
      timer = setTimeout(() => {
        refresh();
        schedule();
      }, relativeRefreshInterval(Date.now() - time));
    };
    schedule();
    return () => clearTimeout(timer);
  }, [ticking, time]);

  if (!date) return <>{fallback}</>;

  const relative = format === "relative";
  const text = relative ? formatRelativeTime(date, { now, locale }) : formatDate(date, { style: format, locale, timeZone });
  return (
    <time
      {...props}
      dateTime={date.toISOString()}
      title={title ?? (relative ? formatDate(date, { style: "long", locale, timeZone }) : undefined)}
      suppressHydrationWarning
      className={cx(styles.time, className)}
    >
      {text}
    </time>
  );
}

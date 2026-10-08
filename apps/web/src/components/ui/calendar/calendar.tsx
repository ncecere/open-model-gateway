"use client";

import { useDirection } from "@base-ui/react/direction-provider";
import { useRender } from "@base-ui/react/use-render";
import { ChevronLeft, ChevronRight } from "lucide-react";
import {
  type ComponentPropsWithRef,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import { IconButton } from "@/components/ui/button/button";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./calendar.module.css";

/*
 * Calendar: a dependency-free month grid (WAI-ARIA APG "date grid").
 *
 *   <Calendar value={day} onValueChange={setDay} />
 *   <Calendar mode="range" numberOfMonths={2} value={range} onValueChange={setRange} />
 *   <Calendar mode="multiple" isDateDisabled={isWeekend} />
 *
 * Month and weekday names come from Intl.DateTimeFormat, so passing `locale`
 * is all it takes to localise it. Dates are plain local `Date`s at midnight;
 * helpers (toISODate, parseISODate, isSameDay…) are exported for callers.
 *
 * Keyboard (focus is on a day): arrows move by day / week, Home / End go to
 * the start / end of the week, PageUp / PageDown change the month and
 * Shift+PageUp / PageDown the year. Enter or Space selects. Only one day is
 * in the tab order (roving tabindex); the grid follows the focused day into
 * other months. Day buttons are built with Base UI useRender.
 */

/* ---------- date helpers ---------- */

export type Weekday = 0 | 1 | 2 | 3 | 4 | 5 | 6;
export type DateRange = { from: Date; to?: Date };

export function startOfDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}
export function addDays(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}
/** Adds months, clamping the day (Jan 31 + 1 month = Feb 28/29). */
export function addMonths(d: Date, n: number): Date {
  const first = new Date(d.getFullYear(), d.getMonth() + n, 1);
  const last = new Date(first.getFullYear(), first.getMonth() + 1, 0).getDate();
  return new Date(first.getFullYear(), first.getMonth(), Math.min(d.getDate(), last));
}
export function startOfMonth(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), 1);
}
function endOfMonth(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth() + 1, 0);
}
function startOfWeek(d: Date, weekStartsOn: Weekday): Date {
  return addDays(d, -((d.getDay() - weekStartsOn + 7) % 7));
}
export function isSameDay(a: Date | null | undefined, b: Date | null | undefined): boolean {
  return Boolean(a && b && a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate());
}
function isSameMonth(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth();
}
/** Day-level comparison: negative when a is before b. */
function compareDays(a: Date, b: Date): number {
  return startOfDay(a).getTime() - startOfDay(b).getTime();
}
/** `YYYY-MM-DD` in local time (no timezone shift). */
export function toISODate(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}
/** Parses `YYYY-MM-DD` as a local date; returns null when invalid. */
export function parseISODate(s: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s);
  if (!m) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
  return d.getMonth() === Number(m[2]) - 1 ? d : null;
}

/* ---------- props ---------- */

export type CalendarLabels = {
  previousMonth: string;
  nextMonth: string;
  month: string;
  year: string;
  /** Accessible name of a day button. */
  day: (date: Date, state: { selected: boolean; today: boolean; formatted: string }) => string;
};

const DEFAULT_LABELS: CalendarLabels = {
  previousMonth: "Previous month",
  nextMonth: "Next month",
  month: "Month",
  year: "Year",
  day: (_date, { selected, formatted }) => (selected ? `${formatted}, selected` : formatted),
};

type CalendarBaseProps = Omit<ComponentPropsWithRef<"div">, "defaultValue" | "onChange" | "children" | "dir"> & {
  /** BCP 47 locale for month and weekday names (defaults to the user's). */
  locale?: string;
  /** First day of the week: 0 = Sunday … 6 = Saturday. */
  weekStartsOn?: Weekday;
  /** Show the days of the previous / next month that fill the first and last week. */
  showOutsideDays?: boolean;
  /** How many months to show side by side. */
  numberOfMonths?: number;
  /** The first displayed month (controlled). */
  month?: Date;
  defaultMonth?: Date;
  onMonthChange?: (month: Date) => void;
  /** Earliest selectable date. */
  min?: Date;
  /** Latest selectable date. */
  max?: Date;
  /** Marks individual dates unavailable (still focusable, announced as disabled). */
  isDateDisabled?: (date: Date) => boolean;
  /** Disables the whole calendar. */
  disabled?: boolean;
  /** `dropdown` adds month and year selects for quick navigation. */
  captionLayout?: "label" | "dropdown";
  /** First and last year in the year select (defaults to min/max, else −100 / +10 years). */
  yearRange?: [number, number];
  /** Overrides "today" (useful for tests and time-zone aware apps). */
  today?: Date;
  /** Extra content inside each day button, e.g. a price or a dot. */
  renderDayContent?: (date: Date) => ReactNode;
  /** Replace the day button element (Base UI `useRender`). */
  renderDay?: useRender.RenderProp;
  labels?: Partial<CalendarLabels>;
  className?: string;
};

export type CalendarSingleProps = CalendarBaseProps & {
  mode?: "single";
  value?: Date | null;
  defaultValue?: Date | null;
  onValueChange?: (value: Date | null) => void;
  /** When true, clicking the selected day keeps it selected instead of clearing it. */
  required?: boolean;
};
export type CalendarMultipleProps = CalendarBaseProps & {
  mode: "multiple";
  value?: Date[];
  defaultValue?: Date[];
  onValueChange?: (value: Date[]) => void;
  /** Maximum number of selected days. */
  maxSelected?: number;
};
export type CalendarRangeProps = CalendarBaseProps & {
  mode: "range";
  value?: DateRange | null;
  defaultValue?: DateRange | null;
  onValueChange?: (value: DateRange | null) => void;
};
export type CalendarProps = CalendarSingleProps | CalendarMultipleProps | CalendarRangeProps;

type AnyValue = Date | Date[] | DateRange | null;

/* ---------- day button ---------- */

type DayButtonProps = ComponentPropsWithRef<"button"> & { render?: useRender.RenderProp };

function CalendarDayButton({ render, ...props }: DayButtonProps) {
  return useRender({ render, defaultTagName: "button", props: { type: "button", ...props } });
}

/* ---------- component ---------- */

export function Calendar(allProps: CalendarProps) {
  const {
    mode = "single",
    value: valueProp,
    defaultValue,
    onValueChange,
    locale,
    weekStartsOn = 0,
    showOutsideDays = true,
    numberOfMonths = 1,
    month: monthProp,
    defaultMonth,
    onMonthChange,
    min,
    max,
    isDateDisabled,
    disabled = false,
    captionLayout = "label",
    yearRange,
    today: todayProp,
    renderDayContent,
    renderDay,
    labels: labelsProp,
    className,
    ...rest
  } = allProps;
  // Strip mode-specific props so they don't reach the DOM.
  const { required, maxSelected, ...divProps } = rest as typeof rest & { required?: boolean; maxSelected?: number };

  const labels = { ...DEFAULT_LABELS, ...labelsProp };
  const direction = useDirection();
  const baseId = useId();
  const today = startOfDay(todayProp ?? new Date());
  const minDay = min ? startOfDay(min) : undefined;
  const maxDay = max ? startOfDay(max) : undefined;

  // ----- value (controlled / uncontrolled) -----
  const [innerValue, setInnerValue] = useState<AnyValue>(defaultValue ?? (mode === "multiple" ? [] : null));
  const value: AnyValue = valueProp !== undefined ? (valueProp as AnyValue) : innerValue;
  const setValue = (next: AnyValue) => {
    if (valueProp === undefined) setInnerValue(next);
    (onValueChange as ((v: AnyValue) => void) | undefined)?.(next);
  };

  const single = mode === "single" ? (value as Date | null) : null;
  const multiple = mode === "multiple" ? ((value as Date[] | null) ?? []) : [];
  const range = mode === "range" ? (value as DateRange | null) : null;

  const firstSelected: Date | undefined =
    single ?? range?.from ?? [...multiple].sort((a, b) => a.getTime() - b.getTime())[0] ?? undefined;

  // ----- displayed month (controlled / uncontrolled) -----
  const [innerMonth, setInnerMonth] = useState(() => startOfMonth(defaultMonth ?? firstSelected ?? todayProp ?? new Date()));
  const month = startOfMonth(monthProp ?? innerMonth);
  const setMonth = (next: Date) => {
    const m = startOfMonth(next);
    if (isSameMonth(m, month)) return;
    if (monthProp === undefined) setInnerMonth(m);
    onMonthChange?.(m);
  };
  const months = Array.from({ length: Math.max(1, numberOfMonths) }, (_, i) => addMonths(month, i));
  const viewStart = month;
  const viewEnd = endOfMonth(addMonths(month, months.length - 1));
  const inView = (d: Date) => compareDays(d, viewStart) >= 0 && compareDays(d, viewEnd) <= 0;

  // ----- focus (roving tabindex) -----
  const [focused, setFocused] = useState<Date | null>(null);
  const pendingFocus = useRef(false);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const userRef = allProps.ref;
  const setRootRef = useCallback(
    (node: HTMLDivElement | null) => {
      rootRef.current = node;
      if (typeof userRef === "function") userRef(node);
      else if (userRef) userRef.current = node;
    },
    [userRef],
  );

  const clamp = (d: Date) => (minDay && compareDays(d, minDay) < 0 ? minDay : maxDay && compareDays(d, maxDay) > 0 ? maxDay : d);

  const active: Date = (() => {
    if (focused && inView(focused)) return focused;
    const candidates = [single, range?.from, ...multiple, today].filter((d): d is Date => Boolean(d) && inView(d as Date));
    if (candidates[0]) return clamp(candidates[0]);
    const first = clamp(viewStart);
    return inView(first) ? first : viewStart;
  })();
  const activeKey = toISODate(active);

  useEffect(() => {
    if (!pendingFocus.current) return;
    pendingFocus.current = false;
    rootRef.current?.querySelector<HTMLElement>(`[data-day="${activeKey}"]`)?.focus();
  });

  function moveFocus(target: Date) {
    const d = clamp(startOfDay(target));
    setFocused(d);
    pendingFocus.current = true;
    if (!inView(d)) setMonth(compareDays(d, viewStart) < 0 ? d : addMonths(startOfMonth(d), -(months.length - 1)));
  }

  // ----- state helpers -----
  const isDisabled = (d: Date) =>
    disabled || Boolean(minDay && compareDays(d, minDay) < 0) || Boolean(maxDay && compareDays(d, maxDay) > 0) || Boolean(isDateDisabled?.(d));

  const rangeEnd = range?.to ?? range?.from;
  const spansDays = Boolean(range?.to && !isSameDay(range.from, range.to));
  function selectionOf(d: Date) {
    if (mode === "single") return { selected: isSameDay(d, single) };
    if (mode === "multiple") return { selected: multiple.some((m) => isSameDay(m, d)) };
    if (!range) return { selected: false };
    const start = isSameDay(d, range.from);
    const end = isSameDay(d, rangeEnd);
    const middle = !start && !end && compareDays(d, range.from) > 0 && Boolean(rangeEnd && compareDays(d, rangeEnd) < 0);
    return { selected: start || end || middle, start, end, middle };
  }

  function select(d: Date) {
    if (mode === "single") {
      setValue(isSameDay(d, single) && !required ? null : d);
    } else if (mode === "multiple") {
      if (multiple.some((m) => isSameDay(m, d))) setValue(multiple.filter((m) => !isSameDay(m, d)));
      else if (maxSelected === undefined || multiple.length < maxSelected) setValue([...multiple, d]);
    } else if (!range || range.to) {
      setValue({ from: d });
    } else if (compareDays(d, range.from) < 0) {
      setValue({ from: d, to: range.from });
    } else {
      setValue({ from: range.from, to: d });
    }
  }

  function onDayClick(e: MouseEvent<HTMLButtonElement>, d: Date) {
    if (isDisabled(d)) {
      e.preventDefault();
      return;
    }
    select(d);
    moveFocus(d);
  }

  function onGridKeyDown(e: KeyboardEvent<HTMLTableElement>) {
    const key = (e.target as HTMLElement).closest<HTMLElement>("[data-day]")?.dataset.day;
    const from = key ? parseISODate(key) : null;
    if (!from) return;
    const rtl = direction === "rtl";
    let next: Date | null = null;
    switch (e.key) {
      case "ArrowLeft":
        next = addDays(from, rtl ? 1 : -1);
        break;
      case "ArrowRight":
        next = addDays(from, rtl ? -1 : 1);
        break;
      case "ArrowUp":
        next = addDays(from, -7);
        break;
      case "ArrowDown":
        next = addDays(from, 7);
        break;
      case "Home":
        next = startOfWeek(from, weekStartsOn);
        break;
      case "End":
        next = addDays(startOfWeek(from, weekStartsOn), 6);
        break;
      case "PageUp":
        next = addMonths(from, e.shiftKey ? -12 : -1);
        break;
      case "PageDown":
        next = addMonths(from, e.shiftKey ? 12 : 1);
        break;
      default:
        return;
    }
    e.preventDefault();
    moveFocus(next);
  }

  // ----- formatting (Intl) -----
  const fmt = useMemo(
    () => ({
      caption: new Intl.DateTimeFormat(locale, { month: "long", year: "numeric" }),
      monthName: new Intl.DateTimeFormat(locale, { month: "long" }),
      full: new Intl.DateTimeFormat(locale, { dateStyle: "full" }),
      weekdayShort: new Intl.DateTimeFormat(locale, { weekday: "short" }),
      weekdayLong: new Intl.DateTimeFormat(locale, { weekday: "long" }),
      day: new Intl.DateTimeFormat(locale, { day: "numeric" }),
    }),
    [locale],
  );
  // 2023-01-01 was a Sunday.
  const weekdays = Array.from({ length: 7 }, (_, i) => new Date(2023, 0, 1 + ((weekStartsOn + i) % 7)));

  const prevDisabled = disabled || Boolean(minDay && compareDays(endOfMonth(addMonths(month, -1)), minDay) < 0);
  const nextDisabled = disabled || Boolean(maxDay && compareDays(addMonths(month, months.length), maxDay) > 0);

  const thisYear = today.getFullYear();
  const [fromYear, toYear] = yearRange ?? [minDay?.getFullYear() ?? thisYear - 100, maxDay?.getFullYear() ?? thisYear + 10];

  return (
    <div
      {...divProps}
      ref={setRootRef}
      className={cx(styles.root, className)}
      data-disabled={dataFlag(disabled)}
      data-mode={mode}
    >
      <div className={styles.nav}>
        <IconButton
          size="sm"
          variant="ghost"
          label={labels.previousMonth}
          icon={direction === "rtl" ? <ChevronRight aria-hidden /> : <ChevronLeft aria-hidden />}
          disabled={prevDisabled}
          onClick={() => setMonth(addMonths(month, -1))}
          className={styles.navButton}
        />
        <IconButton
          size="sm"
          variant="ghost"
          label={labels.nextMonth}
          icon={direction === "rtl" ? <ChevronLeft aria-hidden /> : <ChevronRight aria-hidden />}
          disabled={nextDisabled}
          onClick={() => setMonth(addMonths(month, 1))}
          className={styles.navButton}
        />
      </div>
      <div className={styles.months}>
        {months.map((m, index) => {
          const captionId = `${baseId}-caption-${index}`;
          const gridStart = startOfWeek(m, weekStartsOn);
          const weekCount = Math.ceil((Math.round(compareDays(endOfMonth(m), gridStart) / 86_400_000) + 1) / 7);
          const dropdown = captionLayout === "dropdown" && index === 0;
          return (
            <div key={toISODate(m)} className={styles.month}>
              <div className={styles.caption} data-layout={dropdown ? "dropdown" : "label"}>
                <span id={captionId} aria-live="polite" className={cx(styles.captionLabel, dropdown && "sr-only")}>
                  {fmt.caption.format(m)}
                </span>
                {dropdown && (
                  <>
                    <span className={styles.selectWrap}>
                      <select
                        aria-label={labels.month}
                        className={styles.select}
                        value={m.getMonth()}
                        disabled={disabled}
                        onChange={(e) => setMonth(new Date(m.getFullYear(), Number(e.target.value), 1))}
                      >
                        {Array.from({ length: 12 }, (_, i) => {
                          const option = new Date(m.getFullYear(), i, 1);
                          const out =
                            Boolean(minDay && compareDays(endOfMonth(option), minDay) < 0) || Boolean(maxDay && compareDays(option, maxDay) > 0);
                          return (
                            <option key={i} value={i} disabled={out}>
                              {fmt.monthName.format(option)}
                            </option>
                          );
                        })}
                      </select>
                    </span>
                    <span className={styles.selectWrap}>
                      <select
                        aria-label={labels.year}
                        className={styles.select}
                        value={m.getFullYear()}
                        disabled={disabled}
                        onChange={(e) => {
                          let next = new Date(Number(e.target.value), m.getMonth(), 1);
                          if (minDay && compareDays(endOfMonth(next), minDay) < 0) next = startOfMonth(minDay);
                          if (maxDay && compareDays(next, maxDay) > 0) next = startOfMonth(maxDay);
                          setMonth(next);
                        }}
                      >
                        {Array.from({ length: Math.max(0, toYear - fromYear) + 1 }, (_, i) => fromYear + i)
                          .concat(m.getFullYear() < fromYear || m.getFullYear() > toYear ? [m.getFullYear()] : [])
                          .sort((a, b) => a - b)
                          .map((y) => (
                            <option key={y} value={y}>
                              {y}
                            </option>
                          ))}
                      </select>
                    </span>
                  </>
                )}
              </div>
              <table
                role="grid"
                aria-labelledby={captionId}
                aria-multiselectable={mode === "single" ? undefined : true}
                aria-disabled={disabled || undefined}
                className={styles.grid}
                onKeyDown={onGridKeyDown}
              >
                <thead>
                  <tr>
                    {weekdays.map((w) => (
                      <th key={w.getDay()} scope="col" abbr={fmt.weekdayLong.format(w)} className={styles.weekday}>
                        {fmt.weekdayShort.format(w)}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {Array.from({ length: weekCount }, (_, w) => (
                    <tr key={w}>
                      {Array.from({ length: 7 }, (_, i) => {
                        const d = addDays(gridStart, w * 7 + i);
                        const outside = !isSameMonth(d, m);
                        if (outside && !showOutsideDays) return <td key={i} role="gridcell" className={styles.cell} />;
                        const sel = selectionOf(d);
                        const isToday = isSameDay(d, today);
                        const off = isDisabled(d);
                        const key = toISODate(d);
                        const formatted = fmt.full.format(d);
                        return (
                          <td
                            key={i}
                            role="gridcell"
                            aria-selected={sel.selected}
                            className={styles.cell}
                            data-outside={dataFlag(outside)}
                            data-selected={dataFlag(sel.selected)}
                            data-range-start={dataFlag(sel.start && spansDays)}
                            data-range-end={dataFlag(sel.end && spansDays)}
                            data-range-middle={dataFlag(sel.middle)}
                          >
                            <CalendarDayButton
                              render={renderDay}
                              className={styles.day}
                              data-day={outside ? undefined : key}
                              data-date={key}
                              tabIndex={!outside && key === activeKey ? 0 : -1}
                              aria-label={labels.day(d, { selected: sel.selected, today: isToday, formatted })}
                              aria-current={isToday ? "date" : undefined}
                              aria-disabled={off || undefined}
                              data-selected={dataFlag(sel.selected)}
                              data-today={dataFlag(isToday)}
                              data-outside={dataFlag(outside)}
                              data-disabled={dataFlag(off)}
                              data-range-middle={dataFlag(sel.middle)}
                              onClick={(e) => onDayClick(e, d)}
                            >
                              <span aria-hidden className={styles.dayNumber}>
                                {fmt.day.format(d)}
                              </span>
                              {renderDayContent && <span className={styles.dayContent}>{renderDayContent(d)}</span>}
                            </CalendarDayButton>
                          </td>
                        );
                      })}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          );
        })}
      </div>
    </div>
  );
}

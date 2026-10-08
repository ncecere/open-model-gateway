"use client";

import { Field as BaseField } from "@base-ui/react/field";
import { Popover as BasePopover } from "@base-ui/react/popover";
import { CalendarDays } from "lucide-react";
import { type ReactNode, type Ref, useId, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/button/button";
import {
  addDays,
  Calendar,
  type CalendarProps,
  type DateRange,
  isSameDay,
  parseISODate,
  startOfDay,
  toISODate,
} from "@/components/ui/calendar/calendar";
import popup from "@/components/ui/styles/popup.module.css";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group/toggle-group";
import { cx } from "@/lib/bitop-utils";
import styles from "./date-picker.module.css";

/*
 * DatePicker: a trigger button that opens a Calendar in a Base UI Popover.
 *
 *   <Field label="Start date" description="The first day of the rollout.">
 *     <DatePicker value={day} onValueChange={setDay} />
 *   </Field>
 *   <DatePicker mode="range" aria-label="Report period" numberOfMonths={2} />
 *
 * The trigger is a Base UI Field control, so inside a <Field> it is labelled
 * by the field label, described by its description / error and marked
 * invalid. Its accessible name also includes the current value. Opening moves
 * focus to the selected day (or today); choosing a date (or finishing a range)
 * closes the popup and focus returns to the trigger. With `name`, the value is
 * submitted as `YYYY-MM-DD` (a range as `YYYY-MM-DD/YYYY-MM-DD`).
 *
 * Range presets, two ways:
 *   - `presets` on a range DatePicker lists preset buttons (Today, Last 7
 *     days…) beside the calendar; the calendar itself is "custom".
 *
 *       <DatePicker mode="range" presets={dateRangePresets} aria-label="Period" />
 *
 *   - DateRangePresets is a toggle group of presets plus "Custom", which
 *     reveals a range DatePicker. Its value keeps the preset id, so a URL can
 *     say `?range=30d` and stay relative to today (see
 *     serializeDateRangeSelection / parseDateRangeSelection). "Custom" with
 *     no range yet is `{ preset: "custom", range: null }` (not a filter).
 *
 *       <DateRangePresets aria-label="Date range" value={sel} onValueChange={setSel} />
 */

/* ---------------- Range presets ---------------- */

export type DateRangePreset = {
  /** Stable id, e.g. "7d"; used in URLs. Don't use "custom". */
  id: string;
  label: string;
  /** The range for a given day ("today"). */
  range: (today: Date) => DateRange;
};

/** A preset for the last `days` days including today. */
export function lastDaysPreset(days: number, label = `Last ${days} days`, id = `${days}d`): DateRangePreset {
  return { id, label, range: (today) => ({ from: addDays(startOfDay(today), -(days - 1)), to: startOfDay(today) }) };
}

/** Today, Last 7 days, Last 30 days, Last 90 days. */
export const dateRangePresets: DateRangePreset[] = [
  { id: "today", label: "Today", range: (today) => ({ from: startOfDay(today), to: startOfDay(today) }) },
  lastDaysPreset(7),
  lastDaysPreset(30),
  lastDaysPreset(90),
];

/** The id of the preset whose range equals `range` today, if any. */
export function matchDateRangePreset(range: DateRange | null | undefined, presets: DateRangePreset[] = dateRangePresets, today = new Date()): string | null {
  if (!range?.to) return null;
  const hit = presets.find((p) => {
    const r = p.range(today);
    return isSameDay(r.from, range.from) && isSameDay(r.to, range.to);
  });
  return hit?.id ?? null;
}

/** A preset id (the range recomputed from today) or "custom" with an explicit range. */
export type DateRangeSelection = { preset: string; range: DateRange | null };

/** For URLs: the preset id, or "YYYY-MM-DD/YYYY-MM-DD" for a custom range. Null → "". */
export function serializeDateRangeSelection(value: DateRangeSelection | null | undefined): string {
  if (!value) return "";
  if (value.preset !== "custom") return value.preset;
  if (!value.range) return "";
  return value.range.to ? `${toISODate(value.range.from)}/${toISODate(value.range.to)}` : toISODate(value.range.from);
}

/** Reverses serializeDateRangeSelection; unknown or invalid text gives null. */
export function parseDateRangeSelection(text: string | null | undefined, presets: DateRangePreset[] = dateRangePresets, today = new Date()): DateRangeSelection | null {
  if (!text) return null;
  const preset = presets.find((p) => p.id === text);
  if (preset) return { preset: preset.id, range: preset.range(today) };
  const [a, b] = text.split("/");
  const from = a ? parseISODate(a) : null;
  const to = b ? parseISODate(b) : null;
  if (!from || (b !== undefined && !to)) return null;
  return { preset: "custom", range: { from, to: to ?? from } };
}

type CalendarPassThrough = Pick<
  CalendarProps,
  | "locale"
  | "weekStartsOn"
  | "showOutsideDays"
  | "numberOfMonths"
  | "min"
  | "max"
  | "isDateDisabled"
  | "captionLayout"
  | "yearRange"
  | "today"
  | "renderDayContent"
  | "labels"
>;

export type DatePickerLabels = {
  /** Accessible name of the popup (dialog). */
  dialog: string;
  /** Joins the two ends of a range in the trigger. */
  rangeSeparator: string;
};

type DatePickerBaseProps = CalendarPassThrough & {
  /** Shown when nothing is selected. */
  placeholder?: string;
  /** Intl.DateTimeFormat options for the trigger text. */
  formatOptions?: Intl.DateTimeFormatOptions;
  /** Name for form submission (a hidden input). Inside a Field, the Field's name is used. */
  name?: string;
  disabled?: boolean;
  /** Accessible name when the picker is not inside a labelled Field. */
  "aria-label"?: string;
  id?: string;
  size?: "sm" | "md";
  /** Stretch the trigger to the container width. */
  block?: boolean;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Close the popup once a date (or a full range) is picked. */
  closeOnSelect?: boolean;
  side?: "top" | "bottom";
  align?: "start" | "center" | "end";
  /** Extra content under the calendar, e.g. preset buttons. */
  footer?: ReactNode;
  labels?: CalendarProps["labels"] & Partial<DatePickerLabels>;
  className?: string;
  ref?: Ref<HTMLButtonElement>;
};

export type DatePickerSingleProps = DatePickerBaseProps & {
  mode?: "single";
  value?: Date | null;
  defaultValue?: Date | null;
  onValueChange?: (value: Date | null) => void;
};

export type DatePickerRangeProps = DatePickerBaseProps & {
  mode: "range";
  value?: DateRange | null;
  defaultValue?: DateRange | null;
  onValueChange?: (value: DateRange | null) => void;
  /** Preset buttons beside the calendar, e.g. `dateRangePresets`. Picking one sets the range and closes. */
  presets?: DateRangePreset[];
  /** "Today" for the presets (default: now). */
  presetsToday?: Date;
  /** Names the preset list (default "Presets"). */
  presetsLabel?: string;
};

export type DatePickerProps = DatePickerSingleProps | DatePickerRangeProps;

type Value = Date | DateRange | null;

export function DatePicker(props: DatePickerProps) {
  const {
    mode = "single",
    value: valueProp,
    defaultValue,
    onValueChange,
    placeholder = mode === "range" ? "Pick a date range" : "Pick a date",
    formatOptions,
    name,
    disabled,
    "aria-label": ariaLabel,
    id,
    size = "md",
    block,
    open: openProp,
    defaultOpen = false,
    onOpenChange,
    closeOnSelect = true,
    side = "bottom",
    align = "start",
    footer,
    labels,
    className,
    ref,
    locale,
    ...rest
  } = props;
  const { presets, presetsToday, presetsLabel = "Presets", ...calendarProps } = rest as typeof rest & {
    presets?: DateRangePreset[];
    presetsToday?: Date;
    presetsLabel?: string;
  };

  const [innerValue, setInnerValue] = useState<Value>(defaultValue ?? null);
  const value: Value = valueProp !== undefined ? valueProp : innerValue;
  const [innerOpen, setInnerOpen] = useState(defaultOpen);
  const open = openProp ?? innerOpen;
  const setOpen = (next: boolean) => {
    if (openProp === undefined) setInnerOpen(next);
    onOpenChange?.(next);
  };

  const valueId = useId();
  const ownLabelId = useId();
  const popupRef = useRef<HTMLDivElement | null>(null);

  const formatter = useMemo(() => new Intl.DateTimeFormat(locale, formatOptions ?? { dateStyle: "medium" }), [locale, formatOptions]);
  const separator = labels?.rangeSeparator ?? " – ";

  let text: string | null = null;
  let serialized = "";
  if (value instanceof Date) {
    text = formatter.format(value);
    serialized = toISODate(value);
  } else if (value) {
    text = value.to ? `${formatter.format(value.from)}${separator}${formatter.format(value.to)}` : `${formatter.format(value.from)}${separator}…`;
    serialized = value.to ? `${toISODate(value.from)}/${toISODate(value.to)}` : toISODate(value.from);
  }

  function handleChange(next: Value) {
    if (valueProp === undefined) setInnerValue(next);
    (onValueChange as ((v: Value) => void) | undefined)?.(next);
    if (!closeOnSelect || !next) return;
    if (next instanceof Date || next.to) setOpen(false);
  }

  const calendar =
    mode === "range" ? (
      <Calendar
        {...calendarProps}
        locale={locale}
        labels={labels}
        mode="range"
        value={value as DateRange | null}
        onValueChange={handleChange}
      />
    ) : (
      <Calendar
        {...calendarProps}
        locale={locale}
        labels={labels}
        mode="single"
        required
        value={value as Date | null}
        onValueChange={handleChange}
      />
    );

  return (
    <BasePopover.Root open={open} onOpenChange={(o) => setOpen(o)}>
      <BaseField.Control
        id={id}
        disabled={disabled}
        value={serialized}
        render={(controlProps) => {
          // Keep what a button understands from the Field control: id, label / description
          // wiring, validity and focus tracking. Drop input-only props.
          const {
            value: _value,
            defaultValue: _defaultValue,
            onChange: _onChange,
            name: fieldName,
            autoFocus: _autoFocus,
            type: _type,
            "aria-labelledby": fieldLabelId,
            ...buttonProps
          } = controlProps as typeof controlProps & {
            "aria-labelledby"?: string;
            type?: string;
            value?: unknown;
            name?: string;
          };
          const labelledBy = [fieldLabelId ?? (ariaLabel ? ownLabelId : undefined), valueId].filter(Boolean).join(" ");
          const submitName = name ?? fieldName;
          return (
            <>
              <BasePopover.Trigger
                {...buttonProps}
                ref={ref ? mergeRefs(buttonProps.ref, ref) : buttonProps.ref}
                aria-labelledby={labelledBy}
                className={cx(styles.trigger, className)}
                data-size={size}
                data-block={block ? "" : undefined}
                data-placeholder={text ? undefined : ""}
              >
                <CalendarDays aria-hidden className={styles.icon} />
                {ariaLabel && !fieldLabelId && (
                  <span id={ownLabelId} className="sr-only">
                    {ariaLabel}
                  </span>
                )}
                <span id={valueId} className={styles.value}>
                  {text ?? placeholder}
                </span>
              </BasePopover.Trigger>
              {submitName && <input type="hidden" name={submitName} value={serialized} disabled={disabled} />}
            </>
          );
        }}
      />
      <BasePopover.Portal>
        <BasePopover.Positioner className={popup.positioner} side={side} align={align} sideOffset={6}>
          <BasePopover.Popup
            ref={popupRef}
            aria-label={labels?.dialog ?? (mode === "range" ? "Choose dates" : "Choose date")}
            className={cx(popup.popup, styles.popup)}
            initialFocus={() => popupRef.current?.querySelector<HTMLElement>('[data-day][tabindex="0"]') ?? true}
          >
            {mode === "range" && presets?.length ? (
              <div className={styles.withPresets}>
                <div role="group" aria-label={presetsLabel} className={styles.presets}>
                  {presets.map((p) => {
                    const r = p.range(presetsToday ?? new Date());
                    const pressed = Boolean(value && !(value instanceof Date) && value.to && isSameDay(value.from, r.from) && isSameDay(value.to, r.to));
                    return (
                      <Button
                        key={p.id}
                        size="sm"
                        variant="ghost"
                        aria-pressed={pressed}
                        className={styles.preset}
                        onClick={() => {
                          if (valueProp === undefined) setInnerValue(r);
                          (onValueChange as ((v: Value) => void) | undefined)?.(r);
                          setOpen(false);
                        }}
                      >
                        {p.label}
                      </Button>
                    );
                  })}
                </div>
                {calendar}
              </div>
            ) : (
              calendar
            )}
            {footer && <div className={styles.footer}>{footer}</div>}
          </BasePopover.Popup>
        </BasePopover.Positioner>
      </BasePopover.Portal>
    </BasePopover.Root>
  );
}

function mergeRefs<T>(...refs: (Ref<T> | undefined)[]) {
  return (node: T | null) => {
    for (const r of refs) {
      if (typeof r === "function") r(node);
      else if (r) (r as { current: T | null }).current = node;
    }
  };
}

/* ---------------- DateRangePresets ---------------- */

export type DateRangePresetsProps = {
  /** Names the preset group, e.g. "Date range". */
  "aria-label": string;
  presets?: DateRangePreset[];
  value?: DateRangeSelection | null;
  defaultValue?: DateRangeSelection | null;
  /**
   * Called with the preset and its range, `{ preset: "custom", range }`, or null when cleared.
   * A custom range is reported once both ends are picked (the first click in the calendar sets the
   * start and keeps the picker open, the second sets the end); closing the picker in between keeps
   * the previous range. Pressing "Custom" before a range is picked calls it with `{ preset: "custom", range: null }`;
   * if the parent stores that as null (as FilterBar does: no range, no filter), Custom stays
   * pressed and its picker stays open until a range, another preset or clearing.
   */
  onValueChange?: (value: DateRangeSelection | null) => void;
  /** Offer "Custom" (a range DatePicker). Default true. */
  allowCustom?: boolean;
  customLabel?: string;
  /** Allow unpressing the current preset (value becomes null). Default true. */
  clearable?: boolean;
  /** "Today" for the presets (default: now). */
  today?: Date;
  /** Passed to the custom range DatePicker, e.g. { numberOfMonths: 2, max: new Date() }. */
  pickerProps?: Omit<DatePickerRangeProps, "mode" | "value" | "defaultValue" | "onValueChange">;
  size?: "sm" | "md";
  className?: string;
};

/**
 * A toggle group of range presets plus "Custom", which reveals a range
 * DatePicker. The value keeps the preset id so it can live in a URL.
 */
export function DateRangePresets({
  "aria-label": ariaLabel,
  presets = dateRangePresets,
  value: valueProp,
  defaultValue = null,
  onValueChange,
  allowCustom = true,
  customLabel = "Custom",
  clearable = true,
  today,
  pickerProps,
  size = "sm",
  className,
}: DateRangePresetsProps) {
  const [inner, setInner] = useState<DateRangeSelection | null>(defaultValue);
  // "Custom" pressed before a range is picked. Parents such as FilterBar
  // store `{ preset: "custom", range: null }` as "no filter" (null), so keep
  // the pending choice here; otherwise the picker would never appear.
  const [customPending, setCustomPending] = useState(false);
  const given = valueProp !== undefined ? valueProp : inner;
  // A real value from the parent (e.g. the URL changed) ends the pending state.
  if (given !== null && customPending) setCustomPending(false);
  const value: DateRangeSelection | null = given ?? (customPending ? { preset: "custom", range: null } : null);
  const set = (next: DateRangeSelection | null) => {
    setCustomPending(next?.preset === "custom" && !next.range);
    if (valueProp === undefined) setInner(next);
    onValueChange?.(next);
  };
  const [pickerOpen, setPickerOpen] = useState(false);
  // The start of a custom range while the end isn't picked yet. It isn't a selection (a URL can't
  // hold half a range), so it stays here until the second pick.
  const [draft, setDraft] = useState<DateRange | null>(null);

  return (
    <div className={cx(styles.presetsBar, className)}>
      <ToggleGroup
        aria-label={ariaLabel}
        size={size}
        variant="outline"
        joined
        value={value ? [value.preset] : []}
        onValueChange={(ids) => {
          const id = ids[0] as string | undefined;
          if (!id) {
            if (clearable) set(null);
            return;
          }
          if (id === "custom") {
            set({ preset: "custom", range: value?.range ?? null });
            setPickerOpen(true);
            return;
          }
          const preset = presets.find((p) => p.id === id);
          if (preset) set({ preset: id, range: preset.range(today ?? new Date()) });
        }}
      >
        {presets.map((p) => (
          <ToggleGroupItem key={p.id} value={p.id}>
            {p.label}
          </ToggleGroupItem>
        ))}
        {allowCustom && <ToggleGroupItem value="custom">{customLabel}</ToggleGroupItem>}
      </ToggleGroup>
      {allowCustom && value?.preset === "custom" && (
        <DatePicker
          numberOfMonths={2}
          {...pickerProps}
          mode="range"
          size={size}
          aria-label={pickerProps?.["aria-label"] ?? `${ariaLabel}: ${customLabel.toLocaleLowerCase()}`}
          open={pickerOpen}
          onOpenChange={(open) => {
            setPickerOpen(open);
            if (!open) setDraft(null);
          }}
          value={draft ?? value.range}
          onValueChange={(range) => {
            if (range && !range.to) {
              setDraft(range);
              return;
            }
            setDraft(null);
            set({ preset: "custom", range });
          }}
        />
      )}
    </div>
  );
}

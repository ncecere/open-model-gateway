"use client";

import { type ReactNode, useState } from "react";
import { cx } from "@/lib/bitop-utils";
import { Input, type InputProps } from "@/components/ui/input/input";
import styles from "./number-input.module.css";

/*
 * A text input for amounts that shows thousands separators ("50,000") while
 * it isn't being edited. On focus it shows the plain number so it is easy to
 * change; what the person types is parsed back (separators dropped, the
 * locale's decimal mark read as ".") and reported through onValueChange.
 *
 * The value stays a string so forms can keep blank ("no limit") and invalid
 * text and show their own validation. Text that isn't a number is shown as is.
 *
 * It is an <input type="text" inputMode="numeric|decimal">, not
 * type="number": number inputs can't show grouping and change on scroll.
 */

export type NumberInputProps = Omit<InputProps, "value" | "defaultValue" | "onChange" | "onValueChange" | "type"> & {
  /** The plain value: digits with an optional "." and leading "-", or "" for empty. */
  value: string;
  onValueChange: (value: string) => void;
  /** Locale for grouping and the decimal mark; defaults to the browser's. */
  locale?: string;
  /** Fraction digits shown while not editing; 0 also sets inputMode="numeric". Default 3. */
  maximumFractionDigits?: number;
  /** Unit shown after the input, e.g. "GiB" or "per day". Decorative: put the unit in the label or description too. */
  unit?: ReactNode;
};

function separators(locale?: string) {
  const parts = new Intl.NumberFormat(locale).formatToParts(12345.6);
  return {
    group: parts.find((p) => p.type === "group")?.value ?? ",",
    decimal: parts.find((p) => p.type === "decimal")?.value ?? ".",
  };
}

/**
 * "50000" → "50,000" (en). Returns the text unchanged if it isn't a plain
 * number or has more fraction digits than shown, so rounding never hides what
 * was typed ("1.5" with 0 fraction digits stays "1.5" for the form to reject).
 */
export function formatNumberText(value: string, locale?: string, maximumFractionDigits = 3): string {
  const t = value.trim();
  const m = /^-?\d+(?:\.(\d+))?$/.exec(t);
  if (!m || (m[1]?.length ?? 0) > maximumFractionDigits) return value;
  // Format the decimal string itself: Number() would round past 2^53 and show a
  // different amount than the one stored.
  return new Intl.NumberFormat(locale, { maximumFractionDigits, useGrouping: true }).format(t as Intl.StringNumericLiteral);
}

/** "50,000" → "50000", "1,5" → "1.5" (de). Text that isn't a number comes back without separators only. */
export function parseNumberText(text: string, locale?: string): string {
  const { group, decimal } = separators(locale);
  let out = text.replace(/[\s\u00a0\u202f]/g, "");
  if (group.trim() !== "") out = out.split(group).join("");
  if (decimal !== ".") out = out.split(decimal).join(".");
  return out;
}

export function NumberInput({
  value,
  onValueChange,
  locale,
  maximumFractionDigits = 3,
  unit,
  className,
  onFocus,
  onBlur,
  inputMode,
  ...props
}: NumberInputProps) {
  // While focused, the text as typed; otherwise the formatted value.
  const [draft, setDraft] = useState<string | null>(null);
  const toLocal = (v: string) => {
    const { decimal } = separators(locale);
    return decimal === "." ? v : v.replace(".", decimal);
  };
  const input = (
    <Input
      {...props}
      type="text"
      autoComplete={props.autoComplete ?? "off"}
      inputMode={inputMode ?? (maximumFractionDigits > 0 ? "decimal" : "numeric")}
      value={draft ?? formatNumberText(value, locale, maximumFractionDigits)}
      onFocus={(e) => {
        setDraft(toLocal(value));
        onFocus?.(e);
      }}
      onBlur={(e) => {
        setDraft(null);
        onBlur?.(e);
      }}
      onValueChange={(text) => {
        setDraft(text);
        onValueChange(parseNumberText(text, locale));
      }}
      className={cx(styles.input, !unit && className)}
    />
  );
  if (!unit) return input;
  return (
    <span className={cx(styles.root, className)}>
      {input}
      <span aria-hidden className={styles.unit}>
        {unit}
      </span>
    </span>
  );
}

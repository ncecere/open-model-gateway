"use client";

import { Field as BaseField } from "@base-ui/react/field";
import { Input as BaseInput } from "@base-ui/react/input";
import { ChevronDown } from "lucide-react";
import type { ComponentPropsWithRef, ReactNode, Ref } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./input.module.css";

/*
 * Text-like controls. All three are Base UI Field controls, so inside a
 * <Field> they are automatically labelled, described (aria-describedby) and
 * marked invalid (aria-invalid) — and they still work standalone.
 *
 * NativeSelect deliberately renders a real <select>: it is the most robust,
 * accessible option for forms and keeps native semantics for tests
 * (userEvent.selectOptions, role="combobox").
 */

export type ControlSize = "sm" | "md";

export type InputProps = Omit<ComponentPropsWithRef<"input">, "className" | "size"> & {
  className?: string;
  size?: ControlSize;
  /** A decorative icon shown inside the start of the input. */
  startIcon?: ReactNode;
  /** Called with the new string value (in addition to `onChange`). */
  onValueChange?: (value: string) => void;
};

export function Input({ className, size = "md", startIcon, onValueChange, ref, ...props }: InputProps) {
  const input = (
    <BaseInput
      {...(props as BaseInput.Props)}
      ref={ref}
      onValueChange={onValueChange ? (v) => onValueChange(v) : undefined}
      data-size={size}
      className={cx(styles.control, !startIcon && className)}
    />
  );
  if (!startIcon) return input;
  return (
    <span className={cx(styles.adorned, className)}>
      <span aria-hidden className={styles.adornment}>
        {startIcon}
      </span>
      {input}
    </span>
  );
}

export type TextareaProps = Omit<ComponentPropsWithRef<"textarea">, "className"> & {
  className?: string;
};

export function Textarea({ className, rows = 3, ref, ...props }: TextareaProps) {
  return (
    <BaseField.Control
      {...(props as unknown as BaseField.Control.Props)}
      ref={ref as unknown as Ref<HTMLInputElement>}
      render={<textarea rows={rows} />}
      className={cx(styles.control, styles.textarea, className)}
    />
  );
}

export type NativeSelectProps = Omit<ComponentPropsWithRef<"select">, "className" | "size"> & {
  className?: string;
  size?: ControlSize;
};

/** A styled native <select>. Pass <option>/<optgroup> children. */
export function NativeSelect({ className, size = "md", ref, children, ...props }: NativeSelectProps) {
  return (
    <span className={styles.selectWrap}>
      <BaseField.Control
        {...(props as unknown as BaseField.Control.Props)}
        ref={ref as unknown as Ref<HTMLInputElement>}
        data-size={size}
        render={<select>{children}</select>}
        className={cx(styles.control, styles.select, className)}
      />
      <ChevronDown aria-hidden className={styles.chevron} />
    </span>
  );
}

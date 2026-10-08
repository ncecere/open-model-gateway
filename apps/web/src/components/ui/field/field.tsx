"use client";

import { Field as BaseField } from "@base-ui/react/field";
import { Fieldset as BaseFieldset } from "@base-ui/react/fieldset";
import { Form as BaseForm } from "@base-ui/react/form";
import { CircleAlert } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./field.module.css";

/*
 * Field wraps Base UI's Field parts into one convenient component:
 *
 *   <Field label="Base URL" description="Include /v1" error={err}>
 *     <Input />
 *   </Field>
 *
 * Base UI links everything: the label's `for` points at the control, the
 * description and (when present) the error are added to the control's
 * aria-describedby, and `error` makes the control aria-invalid. Works with
 * Input, Textarea, NativeSelect and any Base UI form control.
 *
 * Without `error`, the field shows Base UI's own validation message once it
 * has been validated (on submit inside a Form): the browser's constraint
 * message (required, type="email", min, pattern…) or the string returned by
 * `validate`, so an invalid field never shows only a red border:
 *
 *   <Field label="Slug" validate={(v) => (/^[a-z0-9-]+$/.test(String(v)) ? null : "Use lowercase letters, digits and hyphens.")}>
 *
 * The parts are also exported (FieldRoot, FieldLabel, ...) for custom layouts.
 */

const hasText = (node: ReactNode) => node !== undefined && node !== null && node !== false && node !== "";

export type FieldProps = Omit<BaseField.Root.Props, "className" | "children"> & {
  className?: string;
  /** Visible label. Omit only when the child labels itself (e.g. Select with `label`). */
  label?: ReactNode;
  /** Keep the label for assistive technology but hide it visually. */
  hideLabel?: boolean;
  /** Short muted text after the label, e.g. "Optional". */
  labelHint?: ReactNode;
  /** Help text, announced as the control's description. */
  description?: ReactNode;
  /** Error message. When set the control is marked invalid and the message is announced. */
  error?: ReactNode;
  children: ReactNode;
};

export function Field({ label, hideLabel, labelHint, description, error, className, children, invalid, ...props }: FieldProps) {
  const hasError = error !== undefined && error !== null && error !== false && error !== "";
  return (
    <BaseField.Root {...props} invalid={hasError || invalid || undefined} className={cx(styles.field, className)}>
      {label !== undefined && (
        <div className={cx(styles.labelRow, hideLabel && "sr-only")}>
          <BaseField.Label className={styles.label}>{label}</BaseField.Label>
          {labelHint && <span className={styles.labelHint}>{labelHint}</span>}
        </div>
      )}
      {children}
      {description && <BaseField.Description className={styles.description}>{description}</BaseField.Description>}
      {hasError ? (
        <BaseField.Error match className={styles.error}>
          <CircleAlert aria-hidden className={styles.errorIcon} />
          <span>{error}</span>
        </BaseField.Error>
      ) : (
        // Without `error`: the constraint-validation or `validate` message, once the field is
        // validated (on submit inside a Form), so an invalid field never shows only a red border.
        <BaseField.Error
          className={styles.error}
          render={(errorProps) => (
            <div {...errorProps}>
              {hasText(errorProps.children) && <CircleAlert aria-hidden className={styles.errorIcon} />}
              <span>{errorProps.children}</span>
            </div>
          )}
        />
      )}
    </BaseField.Root>
  );
}

/* ---- composable parts (styled Base UI Field) ---- */

type WithClass<P> = Omit<P, "className"> & { className?: string };

export function FieldRoot({ className, ...props }: WithClass<BaseField.Root.Props>) {
  return <BaseField.Root {...props} className={cx(styles.field, className)} />;
}
export function FieldLabel({ className, ...props }: WithClass<BaseField.Label.Props>) {
  return <BaseField.Label {...props} className={cx(styles.label, className)} />;
}
export function FieldDescription({ className, ...props }: WithClass<BaseField.Description.Props>) {
  return <BaseField.Description {...props} className={cx(styles.description, className)} />;
}
export function FieldError({ className, ...props }: WithClass<BaseField.Error.Props>) {
  return <BaseField.Error {...props} className={cx(styles.error, className)} />;
}

/* ---- Fieldset ---- */

export type FieldsetProps = WithClass<BaseFieldset.Root.Props> & {
  legend: ReactNode;
  description?: ReactNode;
  /** Visually hide the legend (still announced). */
  hideLegend?: boolean;
};

/** Groups related controls under a legend. */
export function Fieldset({ legend, description, hideLegend, className, children, ...props }: FieldsetProps) {
  return (
    <BaseFieldset.Root {...props} className={cx(styles.fieldset, className)}>
      <BaseFieldset.Legend className={cx(styles.legend, hideLegend && "sr-only")}>{legend}</BaseFieldset.Legend>
      {description && <p className={styles.description}>{description}</p>}
      <div className={styles.fieldsetBody}>{children as ReactNode}</div>
    </BaseFieldset.Root>
  );
}

/* ---- Form ---- */

export type FormProps = WithClass<BaseForm.Props> & {
  /** Vertical gap between fields. */
  gap?: "sm" | "md" | "lg";
};

/**
 * Base UI Form: native submission plus `errors` for server-side validation
 * (keyed by Field `name`). Lays fields out in a vertical stack.
 */
export function Form({ className, gap = "md", ...props }: FormProps) {
  return <BaseForm {...props} data-gap={gap} className={cx(styles.form, className)} />;
}

export type FormActionsProps = { children: ReactNode; className?: string; align?: "start" | "end" | "between" };

/** A right-aligned row of form buttons. */
export function FormActions({ children, className, align = "end" }: FormActionsProps) {
  return (
    <div data-align={align} className={cx(styles.actions, className)}>
      {children}
    </div>
  );
}

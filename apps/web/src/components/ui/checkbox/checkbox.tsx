"use client";

import { Checkbox as BaseCheckbox } from "@base-ui/react/checkbox";
import { CheckboxGroup as BaseCheckboxGroup } from "@base-ui/react/checkbox-group";
import { Field as BaseField } from "@base-ui/react/field";
import { Fieldset as BaseFieldset } from "@base-ui/react/fieldset";
import { Check, CircleAlert, Minus } from "lucide-react";
import { createContext, useContext, type ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./checkbox.module.css";

/*
 * Checkbox and CheckboxGroup (Base UI). Each checkbox is labelled by an
 * enclosing <label> (Field.Label) and can carry a description, which Base UI
 * wires to aria-describedby. Groups use a fieldset + legend.
 */

const InGroup = createContext(false);

export type CheckboxProps = Omit<BaseCheckbox.Root.Props, "className" | "children"> & {
  /** Visible label (required: every checkbox needs an accessible name). */
  label: ReactNode;
  description?: ReactNode;
  className?: string;
};

export function Checkbox({ label, description, className, disabled, ...props }: CheckboxProps) {
  const inGroup = useContext(InGroup);
  const content = (
    <>
      <BaseField.Label className={styles.label}>
        <BaseCheckbox.Root {...props} disabled={disabled} className={styles.box}>
          <BaseCheckbox.Indicator
            className={styles.indicator}
            render={(indicatorProps, state) => (
              <span {...indicatorProps}>
                {state.indeterminate ? <Minus aria-hidden strokeWidth={3} /> : <Check aria-hidden strokeWidth={3} />}
              </span>
            )}
          />
        </BaseCheckbox.Root>
        <span className={styles.text}>{label}</span>
      </BaseField.Label>
      {description && <BaseField.Description className={styles.description}>{description}</BaseField.Description>}
    </>
  );
  return inGroup ? (
    <BaseField.Item disabled={disabled} className={cx(styles.item, className)}>
      {content}
    </BaseField.Item>
  ) : (
    <BaseField.Root disabled={disabled} className={cx(styles.item, className)}>
      {content}
    </BaseField.Root>
  );
}

export type CheckboxGroupProps = Omit<BaseCheckboxGroup.Props, "className"> & {
  legend: ReactNode;
  description?: ReactNode;
  error?: ReactNode;
  /** Name submitted with a Base UI Form. */
  name?: string;
  orientation?: "vertical" | "horizontal";
  className?: string;
  children: ReactNode;
};

export function CheckboxGroup({
  legend,
  description,
  error,
  name,
  orientation = "vertical",
  className,
  children,
  ...props
}: CheckboxGroupProps) {
  return (
    <BaseField.Root name={name} invalid={error ? true : undefined} className={cx(styles.group, className)}>
      <BaseFieldset.Root render={<BaseCheckboxGroup {...props} />} className={styles.fieldset}>
        <BaseFieldset.Legend className={styles.legend}>{legend}</BaseFieldset.Legend>
        {description && <p className={styles.groupDescription}>{description}</p>}
        <div className={styles.options} data-orientation={orientation}>
          <InGroup.Provider value={true}>{children}</InGroup.Provider>
        </div>
      </BaseFieldset.Root>
      {error && (
        <BaseField.Error match className={styles.error}>
          <CircleAlert aria-hidden className={styles.errorIcon} />
          <span>{error}</span>
        </BaseField.Error>
      )}
    </BaseField.Root>
  );
}

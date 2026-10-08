"use client";

import { Field as BaseField } from "@base-ui/react/field";
import { Fieldset as BaseFieldset } from "@base-ui/react/fieldset";
import { Radio as BaseRadio } from "@base-ui/react/radio";
import { RadioGroup as BaseRadioGroup } from "@base-ui/react/radio-group";
import { CircleAlert } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./radio-group.module.css";

export type RadioOption<V extends string = string> = {
  value: V;
  label: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
};

export type RadioGroupProps<V extends string = string> = Omit<
  BaseRadioGroup.Props<V>,
  "className" | "children" | "onValueChange"
> & {
  legend: ReactNode;
  description?: ReactNode;
  error?: ReactNode;
  options: RadioOption<V>[];
  onValueChange?: (value: V) => void;
  orientation?: "vertical" | "horizontal";
  /** `card` renders each option as a selectable tile. */
  variant?: "default" | "card";
  className?: string;
};

/** A labelled group of radio buttons (fieldset + legend). */
export function RadioGroup<V extends string = string>({
  legend,
  description,
  error,
  options,
  name,
  onValueChange,
  orientation = "vertical",
  variant = "default",
  className,
  ...props
}: RadioGroupProps<V>) {
  return (
    <BaseField.Root name={name} invalid={error ? true : undefined} className={cx(styles.group, className)}>
      <BaseFieldset.Root
        render={<BaseRadioGroup<V> {...props} onValueChange={onValueChange ? (v) => onValueChange(v as V) : undefined} />}
        className={styles.fieldset}
      >
        <BaseFieldset.Legend className={styles.legend}>{legend}</BaseFieldset.Legend>
        {description && <p className={styles.groupDescription}>{description}</p>}
        <div className={styles.options} data-orientation={orientation} data-variant={variant}>
          {options.map((o) => (
            <BaseField.Item key={o.value} disabled={o.disabled} className={styles.item} data-variant={variant}>
              <BaseField.Label className={styles.label}>
                <BaseRadio.Root value={o.value} disabled={o.disabled} className={styles.radio}>
                  <BaseRadio.Indicator className={styles.indicator} />
                </BaseRadio.Root>
                <span className={styles.text}>{o.label}</span>
              </BaseField.Label>
              {o.description && <BaseField.Description className={styles.description}>{o.description}</BaseField.Description>}
            </BaseField.Item>
          ))}
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

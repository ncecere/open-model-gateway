"use client";

import { Field as BaseField } from "@base-ui/react/field";
import { Switch as BaseSwitch } from "@base-ui/react/switch";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./switch.module.css";

export type SwitchProps = Omit<BaseSwitch.Root.Props, "className" | "children"> & {
  /** Visible label (required for an accessible name). */
  label: ReactNode;
  description?: ReactNode;
  /** Put the switch after the label (settings-row layout). */
  labelPosition?: "start" | "end";
  className?: string;
};

/** An on/off toggle for settings that apply immediately. */
export function Switch({ label, description, labelPosition = "end", className, disabled, ...props }: SwitchProps) {
  return (
    <BaseField.Root disabled={disabled} className={cx(styles.item, className)} data-label-position={labelPosition}>
      <BaseField.Label className={styles.label}>
        <BaseSwitch.Root {...props} disabled={disabled} className={styles.track}>
          <BaseSwitch.Thumb className={styles.thumb} />
        </BaseSwitch.Root>
        <span className={styles.text}>{label}</span>
      </BaseField.Label>
      {description && <BaseField.Description className={styles.description}>{description}</BaseField.Description>}
    </BaseField.Root>
  );
}

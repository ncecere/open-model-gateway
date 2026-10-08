"use client";

import { Select as BaseSelect } from "@base-ui/react/select";
import { Check, ChevronsUpDown } from "lucide-react";
import type { ReactNode } from "react";
import popup from "@/components/ui/styles/popup.module.css";
import { cx } from "@/lib/bitop-utils";
import styles from "./select.module.css";

/*
 * Select: Base UI Select for rich option lists (icons, descriptions, custom
 * rendering). For ordinary forms prefer NativeSelect, which keeps native
 * <select> semantics.
 */

export type SelectItem<V extends string = string> = {
  value: V;
  label: string;
  /** Optional decorative icon. */
  icon?: ReactNode;
  /** Muted secondary text shown in the list. */
  hint?: ReactNode;
  disabled?: boolean;
};

export type SelectProps<V extends string = string> = {
  /** Visible label (rendered with Select.Label so the trigger is named). */
  label: ReactNode;
  hideLabel?: boolean;
  items: SelectItem<V>[];
  value?: V | null;
  defaultValue?: V | null;
  onValueChange?: (value: V | null) => void;
  placeholder?: ReactNode;
  name?: string;
  disabled?: boolean;
  required?: boolean;
  size?: "sm" | "md";
  className?: string;
};

export function Select<V extends string = string>({
  label,
  hideLabel,
  items,
  value,
  defaultValue,
  onValueChange,
  placeholder = "Select…",
  name,
  disabled,
  required,
  size = "md",
  className,
}: SelectProps<V>) {
  return (
    <BaseSelect.Root<V>
      items={items}
      value={value}
      defaultValue={defaultValue}
      onValueChange={onValueChange ? (v) => onValueChange(v as V | null) : undefined}
      name={name}
      disabled={disabled}
      required={required}
    >
      <div className={cx(styles.root, className)}>
        <BaseSelect.Label className={cx(styles.label, hideLabel && "sr-only")}>{label}</BaseSelect.Label>
        <BaseSelect.Trigger className={styles.trigger} data-size={size}>
          <BaseSelect.Value className={styles.value} placeholder={placeholder} />
          <BaseSelect.Icon className={styles.icon}>
            <ChevronsUpDown aria-hidden />
          </BaseSelect.Icon>
        </BaseSelect.Trigger>
      </div>
      <BaseSelect.Portal>
        <BaseSelect.Positioner className={popup.positioner} sideOffset={6} alignItemWithTrigger={false}>
          <BaseSelect.Popup className={cx(popup.popup, styles.popup)}>
            <BaseSelect.List>
              {items.map((item) => (
                <BaseSelect.Item key={item.value} value={item.value} label={item.label} disabled={item.disabled} className={cx(popup.item, styles.item)}>
                  {item.icon && <span className={styles.itemIcon}>{item.icon}</span>}
                  <BaseSelect.ItemText className={styles.itemText}>{item.label}</BaseSelect.ItemText>
                  {item.hint && <span className={styles.hint}>{item.hint}</span>}
                  <BaseSelect.ItemIndicator className={styles.indicator}>
                    <Check aria-hidden />
                  </BaseSelect.ItemIndicator>
                </BaseSelect.Item>
              ))}
            </BaseSelect.List>
          </BaseSelect.Popup>
        </BaseSelect.Positioner>
      </BaseSelect.Portal>
    </BaseSelect.Root>
  );
}

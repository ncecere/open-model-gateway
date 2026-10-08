"use client";

import { Toggle as BaseToggle } from "@base-ui/react/toggle";
import type { ReactNode } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./toggle.module.css";

/*
 * Toggle: a two-state button (Base UI Toggle, aria-pressed). Sizes and focus
 * ring match Button; the pressed state uses the primary tint plus an inset
 * ring, so it is not signalled by fill colour alone.
 *
 *   <Toggle aria-label="Bold" iconOnly><Bold aria-hidden /></Toggle>
 *   <Toggle variant="outline" defaultPressed>Preview</Toggle>
 *
 * Inside a ToggleGroup, pass `value` to identify the toggle.
 */

export type ToggleVariant = "ghost" | "outline";
export type ToggleSize = "sm" | "md";

type ToggleBaseProps = Omit<BaseToggle.Props, "className" | "children"> & {
  /** "ghost" is transparent until pressed; "outline" has a control boundary. */
  variant?: ToggleVariant;
  size?: ToggleSize;
  className?: string;
  children?: ReactNode;
};

/**
 * Icon-only toggles have no visible text, so the type requires an
 * `aria-label` whenever `iconOnly` is set.
 */
export type ToggleProps = ToggleBaseProps &
  ({ iconOnly?: false } | { iconOnly: true; "aria-label": string; children: ReactNode });

export function Toggle(allProps: ToggleProps) {
  const { variant = "ghost", size = "md", iconOnly = false, className, ...props } = allProps;
  return (
    <BaseToggle
      {...props}
      data-variant={variant}
      data-size={size}
      data-icon-only={dataFlag(iconOnly)}
      className={cx(styles.toggle, className)}
    />
  );
}

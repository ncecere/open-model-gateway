"use client";

import { Tooltip as BaseTooltip } from "@base-ui/react/tooltip";
import type { ReactElement, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./tooltip.module.css";

/*
 * Tooltip (Base UI). Supplementary only: never put essential information in
 * a tooltip (touch and screen-reader users may never see it). For icon-only
 * buttons the accessible name still comes from `aria-label`.
 */

export type TooltipProps = {
  /** The tooltip text. */
  content: ReactNode;
  /** The trigger element (must accept a ref and spread props, e.g. Button). */
  children: ReactElement;
  side?: "top" | "bottom" | "left" | "right";
  align?: "start" | "center" | "end";
  /** Optional shortcut hint rendered after the text. */
  shortcut?: ReactNode;
  delay?: number;
  className?: string;
};

export function Tooltip({ content, children, side = "top", align = "center", shortcut, delay, className }: TooltipProps) {
  return (
    <BaseTooltip.Root>
      <BaseTooltip.Trigger render={children} delay={delay} />
      <BaseTooltip.Portal>
        <BaseTooltip.Positioner side={side} align={align} sideOffset={6} className={styles.positioner}>
          <BaseTooltip.Popup className={cx(styles.popup, className)}>
            {content}
            {shortcut && <span className={styles.shortcut}>{shortcut}</span>}
          </BaseTooltip.Popup>
        </BaseTooltip.Positioner>
      </BaseTooltip.Portal>
    </BaseTooltip.Root>
  );
}

/** Groups tooltips so moving between triggers shows them without a delay. */
export const TooltipProvider = BaseTooltip.Provider;

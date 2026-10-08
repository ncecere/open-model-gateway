"use client";

import { Tooltip as BaseTooltip } from "@base-ui/react/tooltip";
import { type ReactElement, type ReactNode, useId } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./tooltip.module.css";

/*
 * Tooltip (Base UI). Supplementary only: never put essential information in
 * a tooltip (touch and screen-reader users may never see it). For icon-only
 * buttons the accessible name still comes from `aria-label`.
 *
 * TooltipText puts a tooltip on plain text, such as a score in a table
 * cell, without dressing it up as a button:
 *
 *   <TooltipText content="Recall@5: the share of questions whose document was in the top 5.">82%</TooltipText>
 *
 * The text gets a dotted underline and a tab stop, so keyboard users can
 * open the tooltip too, and the tooltip's text is its accessible
 * description (read after the text by screen readers, which never see a
 * tooltip). Tooltips don't open on touch, so keep what it says
 * supplementary; use a Popover with `openOnHover` for anything essential.
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

export type TooltipTextProps = Omit<TooltipProps, "children" | "className"> & {
  /** The text the tooltip explains. */
  children: ReactNode;
  /** Class of the text (not the tooltip). */
  className?: string;
};

/** A tooltip on plain text: focusable, underlined with dots, and described by the tooltip's text. */
export function TooltipText({ content, children, className, ...props }: TooltipTextProps) {
  const descriptionId = useId();
  return (
    <>
      <Tooltip content={content} {...props}>
        <span tabIndex={0} aria-describedby={descriptionId} className={cx(styles.text, className)}>
          {children}
        </span>
      </Tooltip>
      {/* The description is always in the page, whether the tooltip is open or not. */}
      <span id={descriptionId} hidden>
        {content}
      </span>
    </>
  );
}

/** Groups tooltips so moving between triggers shows them without a delay. */
export const TooltipProvider = BaseTooltip.Provider;

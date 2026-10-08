"use client";

import { ScrollArea as BaseScrollArea } from "@base-ui/react/scroll-area";
import type { CSSProperties, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./scroll-area.module.css";

/*
 * ScrollArea (Base UI): a scroll container with slim custom scrollbars that
 * keeps native scrolling (wheel, touch, keyboard, find-in-page).
 *
 *   <ScrollArea label="Release notes" maxHeight="18rem">…long content…</ScrollArea>
 *
 * When the content overflows, Base UI makes the viewport focusable
 * (tabIndex 0) so keyboard users can scroll it with the arrow keys (WCAG
 * 2.1.1); at that point it also becomes a region named by `label`, so the
 * tab stop is announced. Content that fits is neither a tab stop nor a
 * landmark.
 */

export type ScrollAreaOrientation = "vertical" | "horizontal" | "both";

export type ScrollAreaProps = Omit<BaseScrollArea.Root.Props, "className" | "children"> & {
  /** Names the viewport when it is scrollable (required: it becomes a focusable region). */
  label: string;
  /** Which scrollbars to render (default "vertical"). */
  orientation?: ScrollAreaOrientation;
  /** Maximum height of the area, e.g. "20rem". Or size it with `className`. */
  maxHeight?: string;
  className?: string;
  viewportClassName?: string;
  contentClassName?: string;
  children?: ReactNode;
};

export function ScrollArea({
  label,
  orientation = "vertical",
  maxHeight,
  className,
  viewportClassName,
  contentClassName,
  style,
  children,
  ...props
}: ScrollAreaProps) {
  const sizeStyle = maxHeight ? ({ "--scroll-area-max-height": maxHeight } as CSSProperties) : undefined;
  return (
    <BaseScrollArea.Root
      {...props}
      style={typeof style === "function" ? style : { ...sizeStyle, ...style }}
      data-max-height={maxHeight ? "" : undefined}
      className={cx(styles.root, className)}
    >
      <BaseScrollArea.Viewport
        className={cx(styles.viewport, viewportClassName)}
        render={(viewportProps, state) => (
          <div {...viewportProps} {...(state.hasOverflowX || state.hasOverflowY ? { role: "region", "aria-label": label } : {})} />
        )}
      >
        {/*
          Base UI sizes the content to fit (inline min-width: fit-content) so it
          can scroll sideways. Without a horizontal scrollbar that only lets
          long lines widen the content past the viewport, where they're clipped
          and never ellipsised (e.g. sidebar labels), so keep it viewport-wide.
        */}
        <BaseScrollArea.Content
          className={cx(styles.content, contentClassName)}
          style={orientation === "vertical" ? { minWidth: 0 } : undefined}
        >
          {children}
        </BaseScrollArea.Content>
      </BaseScrollArea.Viewport>
      {orientation !== "horizontal" && <ScrollBar orientation="vertical" />}
      {orientation !== "vertical" && <ScrollBar orientation="horizontal" />}
      {orientation === "both" && <BaseScrollArea.Corner className={styles.corner} />}
    </BaseScrollArea.Root>
  );
}

export type ScrollBarProps = Omit<BaseScrollArea.Scrollbar.Props, "className"> & { className?: string };

/** One scrollbar with its thumb. ScrollArea renders these; export for custom compositions. */
export function ScrollBar({ className, orientation = "vertical", ...props }: ScrollBarProps) {
  return (
    <BaseScrollArea.Scrollbar {...props} orientation={orientation} className={cx(styles.scrollbar, className)}>
      <BaseScrollArea.Thumb className={styles.thumb} />
    </BaseScrollArea.Scrollbar>
  );
}

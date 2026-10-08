"use client";

import { Popover as BasePopover } from "@base-ui/react/popover";
import { type ReactElement, type ReactNode, useRef } from "react";
import popup from "@/components/ui/styles/popup.module.css";
import { cx } from "@/lib/bitop-utils";
import styles from "./popover.module.css";

export type PopoverProps = {
  trigger: ReactElement;
  /** Heading, used as the popup's accessible name. */
  title?: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  side?: "top" | "bottom" | "left" | "right";
  align?: "start" | "center" | "end";
  /** Also open on hover (for "infotips"; still keyboard and touch accessible). */
  openOnHover?: boolean;
  /**
   * Where focus goes when the popover opens with a mouse or pen: its first
   * control ("first", the default) or the popup itself ("popup"), so that in a
   * list of links nothing looks selected. Opened from the keyboard, focus
   * always goes to the first control; by touch, to the popup.
   */
  pointerFocus?: "first" | "popup";
  className?: string;
};

/** A non-modal floating panel anchored to its trigger. */
export function Popover({
  trigger,
  title,
  description,
  children,
  open,
  defaultOpen,
  onOpenChange,
  side = "bottom",
  align = "center",
  openOnHover,
  pointerFocus = "first",
  className,
}: PopoverProps) {
  const popupRef = useRef<HTMLDivElement>(null);
  const initialFocus =
    pointerFocus === "popup" ? (type: string) => (type === "mouse" || type === "pen" ? popupRef.current : true) : undefined;
  return (
    <BasePopover.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}>
      <BasePopover.Trigger render={trigger} openOnHover={openOnHover} />
      <BasePopover.Portal>
        <BasePopover.Positioner className={popup.positioner} side={side} align={align} sideOffset={8}>
          <BasePopover.Popup ref={popupRef} initialFocus={initialFocus} className={cx(popup.popup, styles.popup, className)}>
            {title && <BasePopover.Title className={styles.title}>{title}</BasePopover.Title>}
            {description && <BasePopover.Description className={styles.description}>{description}</BasePopover.Description>}
            {children}
          </BasePopover.Popup>
        </BasePopover.Positioner>
      </BasePopover.Portal>
    </BasePopover.Root>
  );
}

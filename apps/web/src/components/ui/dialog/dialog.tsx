"use client";

import { AlertDialog as BaseAlertDialog } from "@base-ui/react/alert-dialog";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { X } from "lucide-react";
import type { ReactElement, ReactNode } from "react";
import { ErrorAlert } from "@/components/ui/alert/alert";
import { Button, IconButton, type ButtonProps } from "@/components/ui/button/button";
import { cx } from "@/lib/bitop-utils";
import styles from "./dialog.module.css";

/*
 * Dialog and AlertDialog on Base UI. Base UI traps focus, restores it to the
 * trigger on close, closes on Escape, labels the popup with the title and
 * describes it with the description.
 */

export type DialogSize = "sm" | "md" | "lg" | "xl";

export type DialogProps = {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  /** Optional trigger element, e.g. `<Button>Open</Button>`. */
  trigger?: ReactElement;
  /** Sticky footer, usually buttons. Use <DialogClose> for Cancel. */
  footer?: ReactNode;
  size?: DialogSize;
  /** Hide the × close button (Escape still closes). */
  hideClose?: boolean;
  /** Element to focus when opened (default: first focusable). */
  initialFocus?: BaseDialog.Popup.Props["initialFocus"];
  /**
   * Element to focus when closed (default: the trigger, or the element that
   * had focus when it opened). Pass a ref when the dialog is opened from a
   * control that doesn't keep focus, e.g. a select's change.
   */
  finalFocus?: BaseDialog.Popup.Props["finalFocus"];
  className?: string;
  children?: ReactNode;
};

export function Dialog({
  open,
  defaultOpen,
  onOpenChange,
  title,
  description,
  trigger,
  footer,
  size = "md",
  hideClose,
  initialFocus,
  finalFocus,
  className,
  children,
}: DialogProps) {
  return (
    <BaseDialog.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}>
      {trigger && <BaseDialog.Trigger render={trigger} />}
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className={styles.backdrop} />
        <BaseDialog.Viewport className={styles.viewport}>
          <BaseDialog.Popup
            // Modal (focus is trapped, the page behind is inert to pointers): say so to screen readers too.
            aria-modal="true"
            className={cx(styles.popup, className)}
            data-size={size}
            data-closable={hideClose ? undefined : ""}
            initialFocus={initialFocus}
            finalFocus={finalFocus}
          >
            <div className={styles.header}>
              <div className={styles.heading}>
                <BaseDialog.Title className={styles.title}>{title}</BaseDialog.Title>
                {description && <BaseDialog.Description className={styles.description}>{description}</BaseDialog.Description>}
              </div>
            </div>
            {children !== undefined && <div className={styles.body}>{children}</div>}
            {footer && <div className={styles.footer}>{footer}</div>}
            {/* Last in DOM order so initial focus lands on the first field, not on ×. */}
            {!hideClose && (
              <BaseDialog.Close render={<IconButton size="sm" icon={<X aria-hidden />} label="Close" className={styles.close} />} />
            )}
          </BaseDialog.Popup>
        </BaseDialog.Viewport>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

/** A button that closes the surrounding Dialog. Defaults to the secondary variant. */
export function DialogClose({ variant = "secondary", ...props }: ButtonProps) {
  return <BaseDialog.Close render={<Button variant={variant} {...(props as ButtonProps)} />} />;
}

/* ------------------------------------------------------------------ */

export type AlertDialogProps = {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  title: ReactNode;
  description: ReactNode;
  /** Label of the confirming button, e.g. "Delete source". */
  confirmLabel: ReactNode;
  cancelLabel?: ReactNode;
  onConfirm: () => void;
  /** The action is running: confirm shows a spinner and is disabled. */
  busy?: boolean;
  /** An error from the action, shown as a danger alert. */
  error?: unknown;
  /** `danger` (default) for destructive actions. */
  tone?: "danger" | "primary";
  trigger?: ReactElement;
  /** Extra content between the description and the buttons. */
  children?: ReactNode;
  /**
   * Element to focus when closed (default: the trigger, or the element that
   * had focus when it opened), e.g. a ref to the select whose change asked
   * for confirmation, so Cancel returns there.
   */
  finalFocus?: BaseAlertDialog.Popup.Props["finalFocus"];
  className?: string;
};

/**
 * Confirmation for consequential actions (role="alertdialog"). Clicking
 * outside does not dismiss it; Escape and Cancel do. Initial focus goes to
 * Cancel so Enter never confirms a destructive action by accident.
 */
export function AlertDialog({
  open,
  defaultOpen,
  onOpenChange,
  title,
  description,
  confirmLabel,
  cancelLabel = "Cancel",
  onConfirm,
  busy = false,
  error,
  tone = "danger",
  trigger,
  children,
  finalFocus,
  className,
}: AlertDialogProps) {
  return (
    <BaseAlertDialog.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}>
      {trigger && <BaseAlertDialog.Trigger render={trigger} />}
      <BaseAlertDialog.Portal>
        <BaseAlertDialog.Backdrop className={styles.backdrop} />
        <BaseAlertDialog.Viewport className={styles.viewport}>
          <BaseAlertDialog.Popup aria-modal="true" className={cx(styles.popup, className)} data-size="sm" finalFocus={finalFocus}>
            <div className={styles.header}>
              <div className={styles.heading}>
                <BaseAlertDialog.Title className={styles.title}>{title}</BaseAlertDialog.Title>
                <BaseAlertDialog.Description className={styles.description}>{description}</BaseAlertDialog.Description>
              </div>
            </div>
            {(children || Boolean(error)) && (
              <div className={styles.body}>
                {children}
                <ErrorAlert error={error} />
              </div>
            )}
            <div className={styles.footer}>
              <BaseAlertDialog.Close render={<Button variant="secondary" />}>{cancelLabel}</BaseAlertDialog.Close>
              <Button variant={tone} loading={busy} onClick={onConfirm}>
                {confirmLabel}
              </Button>
            </div>
          </BaseAlertDialog.Popup>
        </BaseAlertDialog.Viewport>
      </BaseAlertDialog.Portal>
    </BaseAlertDialog.Root>
  );
}

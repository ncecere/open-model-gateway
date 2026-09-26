"use client";

import { useRender } from "@base-ui/react/use-render";
import type { ComponentPropsWithRef, MouseEvent, ReactNode } from "react";
import { Spinner } from "@/components/ui/spinner/spinner";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./button.module.css";

export type ButtonVariant = "primary" | "secondary" | "danger" | "ghost" | "link";
export type ButtonSize = "sm" | "md";

type ButtonBaseProps = Omit<ComponentPropsWithRef<"button">, "className"> & {
  variant?: ButtonVariant;
  size?: ButtonSize;
  /** Shows a spinner, sets `aria-busy` and disables the button. */
  loading?: boolean;
  /** Stretch to the container width. */
  block?: boolean;
  className?: string;
  /**
   * Render a different element, e.g. a router link:
   * `<Button render={<Link to="/x" />}>Go</Button>`.
   * (Base UI `useRender` semantics: props are merged onto the element.)
   */
  render?: useRender.RenderProp;
};

/**
 * Icon-only buttons have no visible text, so the type requires an
 * `aria-label` whenever `iconOnly` is set.
 */
export type ButtonProps = ButtonBaseProps &
  ({ iconOnly?: false } | { iconOnly: true; "aria-label": string; children: ReactNode });

function preventActivation(event: MouseEvent) {
  event.preventDefault();
  event.stopPropagation();
}

export function Button(allProps: ButtonProps) {
  const {
    variant = "primary",
    size = "md",
    loading = false,
    block = false,
    iconOnly = false,
    className,
    children,
    disabled,
    type,
    render,
    ref,
    ...props
  } = allProps;
  const native = render === undefined;
  const inactive = Boolean(disabled || loading);

  return useRender({
    render,
    defaultTagName: "button",
    ref,
    props: {
      ...props,
      className: cx(styles.button, className),
      "data-variant": variant,
      "data-size": size,
      "data-icon-only": dataFlag(iconOnly),
      "data-loading": dataFlag(loading),
      "data-block": dataFlag(block),
      "data-disabled": dataFlag(inactive),
      "aria-busy": loading || undefined,
      // Native buttons get real `disabled` and a safe default type. Other
      // elements (links) can't be disabled natively: mark them aria-disabled
      // (they stay focusable, so the state is discoverable) and cancel
      // activation in the capture phase, which stops link navigation and
      // every click handler, like a disabled <button>.
      ...(native
        ? { type: type ?? "button", disabled: inactive }
        : {
            "aria-disabled": inactive || undefined,
            ...(inactive && { onClickCapture: preventActivation }),
          }),
      children: (
        <>
          {loading && <Spinner size={size === "sm" ? "sm" : "md"} className={styles.spinner} />}
          {iconOnly && loading ? null : children}
        </>
      ),
    },
  });
}

export type IconButtonProps = Omit<ButtonBaseProps, "children"> & {
  /** The icon element (decorative; mark it aria-hidden). */
  icon: ReactNode;
  /** Accessible name — required, since there is no visible text. */
  label: string;
};

/** A square, icon-only button. Defaults to the ghost variant. */
export function IconButton({ icon, label, variant = "ghost", ...props }: IconButtonProps) {
  return (
    <Button {...props} variant={variant} iconOnly aria-label={label}>
      {icon}
    </Button>
  );
}

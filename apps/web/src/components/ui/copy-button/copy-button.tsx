"use client";

import { Check, Copy } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button, type ButtonProps } from "@/components/ui/button/button";
import { Tooltip } from "@/components/ui/tooltip/tooltip";
import { cx } from "@/lib/bitop-utils";
import styles from "./copy-button.module.css";

/*
 * CopyButton: an icon button that copies text, swaps to a check mark and
 * announces the result through a polite live region that is always mounted.
 * useCopyToClipboard() is the same logic as a hook, for custom triggers
 * (MessageAction, CodeBlock, Snippet use it).
 */

export type CopyState = "idle" | "copied" | "failed";

/** Writes text to the clipboard, falling back to execCommand in insecure contexts. */
export async function writeClipboard(text: string): Promise<void> {
  if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  const ok = document.execCommand("copy");
  ta.remove();
  if (!ok) throw new Error("copy failed");
}

export type UseCopyToClipboardOptions = {
  /** How long the "copied" state lasts, in ms. */
  timeout?: number;
  onCopy?: (text: string) => void;
  onError?: (error: unknown) => void;
};

/** Copy state machine: `copy(text)` → "copied" (or "failed") → back to "idle". */
export function useCopyToClipboard({ timeout = 2000, onCopy, onError }: UseCopyToClipboardOptions = {}) {
  const [state, setState] = useState<CopyState>("idle");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const copy = useCallback(
    async (text: string) => {
      clearTimeout(timer.current);
      try {
        await writeClipboard(text);
        setState("copied");
        onCopy?.(text);
      } catch (error) {
        setState("failed");
        onError?.(error);
      }
      timer.current = setTimeout(() => setState("idle"), timeout);
    },
    [timeout, onCopy, onError],
  );
  return { state, copy, copied: state === "copied" };
}

export type CopyButtonProps = Omit<ButtonProps, "children" | "onClick" | "iconOnly" | "aria-label" | "onCopy" | "onError" | "value"> & {
  /** The text to copy (or a function that returns it at click time). */
  value: string | (() => string);
  /** What is being copied, used in the accessible name: "Copy code", "Copied code". */
  label?: string;
  /** Show "Copy"/"Copied" text next to the icon. */
  showText?: boolean;
  /** Show a tooltip (icon-only buttons). */
  tooltip?: boolean;
  timeout?: number;
  onCopy?: (text: string) => void;
  onError?: (error: unknown) => void;
};

export function CopyButton({
  value,
  label = "text",
  showText = false,
  tooltip = true,
  timeout,
  onCopy,
  onError,
  variant = "ghost",
  size = "sm",
  className,
  ...props
}: CopyButtonProps) {
  const { state, copy } = useCopyToClipboard({ timeout, onCopy, onError });
  const copied = state === "copied";
  const name = copied ? `Copied ${label}` : `Copy ${label}`;
  const button = (
    <Button
      {...props}
      variant={variant}
      size={size}
      className={cx(styles.button, className)}
      data-state={state}
      onClick={() => copy(typeof value === "function" ? value() : value)}
      {...(showText ? { "aria-label": name } : { iconOnly: true as const, "aria-label": name })}
    >
      {copied ? <Check aria-hidden /> : <Copy aria-hidden />}
      {showText && <span aria-hidden>{copied ? "Copied" : "Copy"}</span>}
    </Button>
  );
  return (
    <>
      {tooltip && !showText ? <Tooltip content={copied ? "Copied" : "Copy"}>{button}</Tooltip> : button}
      <CopyStatus state={state} label={label} />
    </>
  );
}

/** The always-mounted polite live region used by copy controls. */
export function CopyStatus({ state, label = "text" }: { state: CopyState; label?: string }) {
  return (
    <span role="status" className="sr-only">
      {state === "copied" ? `Copied ${label} to clipboard` : state === "failed" ? "Couldn't copy. Select the text and copy it manually." : ""}
    </span>
  );
}

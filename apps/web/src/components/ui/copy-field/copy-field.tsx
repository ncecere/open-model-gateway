"use client";

import { Check, Copy } from "lucide-react";
import { type ReactNode, useEffect, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button/button";
import { cx } from "@/lib/bitop-utils";
import styles from "./copy-field.module.css";

/*
 * CopyField: a read-only value (e.g. an API key secret or a curl example)
 * with a copy button. The value is plain selectable text, so it can also be
 * copied manually. Copy results are announced through a polite live region
 * that is always mounted (so the change is reliably read out).
 */

export type CopyFieldProps = {
  label: ReactNode;
  /** Plain-text name used in the button's accessible name, e.g. "API key". Defaults to `label` when it's a string. */
  name?: string;
  value: string;
  description?: ReactNode;
  /** Wrap long values over multiple lines (code snippets). */
  multiline?: boolean;
  /** Monospace (default true). */
  mono?: boolean;
  className?: string;
};

async function writeClipboard(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  // Fallback for insecure contexts: a temporary textarea + execCommand.
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

export function CopyField({ label, name, value, description, multiline = false, mono = true, className }: CopyFieldProps) {
  const id = useId();
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const accessibleName = name ?? (typeof label === "string" ? label : "value");

  async function copy() {
    clearTimeout(timer.current);
    try {
      await writeClipboard(value);
      setState("copied");
    } catch {
      setState("failed");
    }
    timer.current = setTimeout(() => setState("idle"), 2500);
  }

  const Value = multiline ? "pre" : "code";
  return (
    <div role="group" aria-labelledby={`${id}-label`} aria-describedby={description ? `${id}-desc` : undefined} className={cx(styles.root, className)}>
      <span id={`${id}-label`} className={styles.label}>
        {label}
      </span>
      <div className={styles.box} data-multiline={multiline ? "" : undefined}>
        <Value className={styles.value} data-mono={mono ? "" : undefined}>
          {value}
        </Value>
        <Button
          variant="secondary"
          size="sm"
          onClick={copy}
          aria-label={state === "copied" ? `Copied ${accessibleName}` : `Copy ${accessibleName}`}
          className={styles.button}
        >
          {state === "copied" ? <Check aria-hidden /> : <Copy aria-hidden />}
          {state === "copied" ? "Copied" : "Copy"}
        </Button>
      </div>
      {description && (
        <p id={`${id}-desc`} className={styles.description}>
          {description}
        </p>
      )}
      <span role="status" className="sr-only">
        {state === "copied" ? `${accessibleName} copied to clipboard` : state === "failed" ? "Couldn't copy. Select the text and copy it manually." : ""}
      </span>
    </div>
  );
}

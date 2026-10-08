"use client";

import { Collapsible } from "@base-ui/react/collapsible";
import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./disclosure.module.css";

/*
 * Disclosure: a button that shows and hides a panel (Base UI Collapsible).
 * The trigger is a real <button> with aria-expanded and aria-controls, so
 * keyboard and screen-reader users get the same behaviour as a native
 * <details>. Use it for optional settings such as "Advanced options".
 *
 *   <Disclosure title="Advanced options" summary="Depth 2 · 200 pages">…fields…</Disclosure>
 */

export type DisclosureProps = {
  /** The trigger's text (its accessible name). */
  title: ReactNode;
  /** Muted text after the title, e.g. the current values while closed. */
  summary?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Keep the closed panel in the DOM (hidden) so find-in-page can reveal it. */
  keepMounted?: boolean;
  className?: string;
  children: ReactNode;
};

export function Disclosure({ title, summary, open, defaultOpen, onOpenChange, keepMounted, className, children }: DisclosureProps) {
  return (
    <Collapsible.Root
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}
      className={cx(styles.root, className)}
    >
      <Collapsible.Trigger className={styles.trigger}>
        <ChevronRight aria-hidden className={styles.icon} />
        <span className={styles.title}>{title}</span>
        {/* Cut with an ellipsis when long: a text summary shows in full on hover. */}
        {summary && (
          <span className={styles.summary} title={typeof summary === "string" ? summary : undefined}>
            {summary}
          </span>
        )}
      </Collapsible.Trigger>
      <Collapsible.Panel keepMounted={keepMounted} className={styles.panel}>
        <div className={styles.content}>{children}</div>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
}

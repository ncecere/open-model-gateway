import type { ComponentPropsWithRef } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./kbd.module.css";

export type KbdProps = ComponentPropsWithRef<"kbd"> & { size?: "sm" | "md" };

/** A single key cap. */
export function Kbd({ size = "md", className, ...props }: KbdProps) {
  return <kbd data-size={size} className={cx(styles.kbd, className)} {...props} />;
}

export function isMac(): boolean {
  if (typeof navigator === "undefined") return false;
  return /Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent);
}

const symbols: Record<string, { mac: string; other: string; spoken: string }> = {
  mod: { mac: "⌘", other: "Ctrl", spoken: "Command or Control" },
  shift: { mac: "⇧", other: "Shift", spoken: "Shift" },
  alt: { mac: "⌥", other: "Alt", spoken: "Option or Alt" },
  enter: { mac: "↵", other: "Enter", spoken: "Enter" },
  esc: { mac: "Esc", other: "Esc", spoken: "Escape" },
  up: { mac: "↑", other: "↑", spoken: "Up arrow" },
  down: { mac: "↓", other: "↓", spoken: "Down arrow" },
};

export type KbdShortcutProps = {
  /** Keys, e.g. ["mod", "K"]. `mod` renders ⌘ on Apple platforms and Ctrl elsewhere. */
  keys: string[];
  size?: "sm" | "md";
  className?: string;
};

/** A key combination. Symbols are hidden from screen readers and spelled out instead. */
export function KbdShortcut({ keys, size = "md", className }: KbdShortcutProps) {
  const mac = isMac();
  return (
    <span className={cx(styles.shortcut, className)}>
      {keys.map((k, i) => {
        const sym = symbols[k.toLowerCase()];
        const shown = sym ? (mac ? sym.mac : sym.other) : k;
        const spoken = sym ? sym.spoken : k;
        return (
          <Kbd key={i} size={size}>
            <span aria-hidden>{shown}</span>
            <span className="sr-only">{spoken}</span>
          </Kbd>
        );
      })}
    </span>
  );
}

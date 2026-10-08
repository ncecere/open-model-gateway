"use client";

import { Moon, Sun } from "lucide-react";
import { useCallback, useEffect, useSyncExternalStore } from "react";
import { IconButton, type IconButtonProps } from "@/components/ui/button/button";

/*
 * Light / dark mode for bitop-ui themes. The themes read `data-theme` on
 * <html> ("light" | "dark"); this module keeps it in sync with a stored
 * preference or, by default, with `prefers-color-scheme`.
 *
 *   <ColorModeToggle />                       // a Sun/Moon icon button
 *   const { mode, resolved, setMode } = useColorMode();
 *
 * To avoid a flash of the wrong theme on load, inline `colorModeScript` in a
 * <script> in your index.html <head> (see the docs).
 *
 * Changes follow the system preference (matchMedia) and other tabs: a mode
 * set in one tab reaches the others through the `storage` event.
 */

export type ColorMode = "light" | "dark" | "system";
export type ResolvedColorMode = "light" | "dark";

export const COLOR_MODE_STORAGE_KEY = "bitop-color-mode";

/** Inline this in <head> to set data-theme before first paint. */
export const colorModeScript = `(function(){try{var m=localStorage.getItem("${COLOR_MODE_STORAGE_KEY}");var d=m==="dark"||(m!=="light"&&window.matchMedia("(prefers-color-scheme: dark)").matches);document.documentElement.dataset.theme=d?"dark":"light"}catch(e){}})();`;

const listeners = new Set<() => void>();

function readMode(): ColorMode {
  if (typeof window === "undefined") return "system";
  try {
    const stored = window.localStorage.getItem(COLOR_MODE_STORAGE_KEY);
    return stored === "light" || stored === "dark" ? stored : "system";
  } catch {
    return "system";
  }
}

function prefersDark(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function resolve(mode: ColorMode): ResolvedColorMode {
  if (mode === "system") return prefersDark() ? "dark" : "light";
  return mode;
}

function apply(mode: ColorMode) {
  if (typeof document === "undefined") return;
  document.documentElement.dataset.theme = resolve(mode);
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  const media = typeof window !== "undefined" && typeof window.matchMedia === "function" ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  const onChange = () => {
    if (readMode() === "system") apply("system");
    listener();
  };
  // Another tab changed (or cleared) the stored mode: apply it here too.
  const onStorage = (event: StorageEvent) => {
    if (event.key !== null && event.key !== COLOR_MODE_STORAGE_KEY) return;
    apply(readMode());
    listener();
  };
  media?.addEventListener("change", onChange);
  if (typeof window !== "undefined") window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(listener);
    media?.removeEventListener("change", onChange);
    if (typeof window !== "undefined") window.removeEventListener("storage", onStorage);
  };
}

/** Sets the colour mode everywhere (persisted in localStorage). */
export function setColorMode(mode: ColorMode) {
  try {
    if (mode === "system") window.localStorage.removeItem(COLOR_MODE_STORAGE_KEY);
    else window.localStorage.setItem(COLOR_MODE_STORAGE_KEY, mode);
  } catch {
    /* storage unavailable: still apply for this page view */
  }
  apply(mode);
  listeners.forEach((l) => l());
}

export function useColorMode() {
  const mode = useSyncExternalStore(subscribe, readMode, () => "system" as ColorMode);
  const resolved = useSyncExternalStore(
    subscribe,
    () => resolve(readMode()),
    () => "light" as ResolvedColorMode,
  );
  useEffect(() => apply(mode), [mode]);
  const set = useCallback((m: ColorMode) => setColorMode(m), []);
  const toggle = useCallback(() => setColorMode(resolve(readMode()) === "dark" ? "light" : "dark"), []);
  return { mode, resolved, setMode: set, toggle };
}

export type ColorModeToggleProps = Omit<IconButtonProps, "icon" | "label" | "onClick"> & {
  /** Accessible name. The pressed state says whether dark mode is on. */
  label?: string;
};

/** An icon button that switches between light and dark (aria-pressed = dark). */
export function ColorModeToggle({ label = "Dark mode", variant = "ghost", ...props }: ColorModeToggleProps) {
  const { resolved, toggle } = useColorMode();
  const dark = resolved === "dark";
  return (
    <IconButton
      {...props}
      variant={variant}
      label={label}
      aria-pressed={dark}
      icon={dark ? <Moon aria-hidden /> : <Sun aria-hidden />}
      onClick={toggle}
    />
  );
}

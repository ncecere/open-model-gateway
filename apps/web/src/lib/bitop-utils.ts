/*
 * Shared helpers for bitop-ui components. Installed by the `core` item at
 * `@/lib/bitop-utils` (named so it never collides with shadcn's `lib/utils`).
 */
type ClassValue = string | number | false | null | undefined;

/** Joins class names, skipping falsy values. */
export function cx(...values: ClassValue[]): string {
  return values.filter(Boolean).join(" ");
}

/** Maps a boolean to a presence-only data attribute value (`data-foo=""`). */
export function dataFlag(on: boolean | undefined): "" | undefined {
  return on ? "" : undefined;
}

/** Tones shared by badges, alerts, status dots and toasts. */
export type Tone = "neutral" | "info" | "success" | "warning" | "danger";

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

/**
 * Does `file` match an `accept` string such as "image/*,.pdf,application/json"?
 * Same rules as `<input type="file" accept>`: ".ext" matches the file name
 * (case-insensitive), "type/*" matches a MIME family, anything else an exact
 * MIME type; "*" or "*\/*" match everything. An empty accept allows all files.
 * The picker's accept is only a hint to the browser, and drag and drop and
 * paste ignore it, so check files with this too.
 */
export function matchesAccept(file: File, accept: string | undefined): boolean {
  const rules = (accept ?? "")
    .split(",")
    .map((a) => a.trim().toLowerCase())
    .filter(Boolean);
  if (!rules.length) return true;
  const name = file.name.toLowerCase();
  const type = (file.type || "").toLowerCase();
  return rules.some((a) => {
    if (a === "*" || a === "*/*") return true;
    if (a.startsWith(".")) return name.endsWith(a);
    if (a.endsWith("/*")) return type.startsWith(a.slice(0, -1));
    return type === a;
  });
}

/** Tones shared by badges, alerts, status dots and toasts. */
export type Tone = "neutral" | "info" | "success" | "warning" | "danger";

"use client";

import { Avatar as BaseAvatar } from "@base-ui/react/avatar";
import { cx } from "@/lib/bitop-utils";
import styles from "./avatar.module.css";

export type AvatarProps = {
  /** Person or workspace name: used for initials and the accessible name. */
  name: string;
  src?: string;
  size?: "xs" | "sm" | "md" | "lg" | "xl";
  /** `square` suits workspaces/teams. */
  shape?: "circle" | "square";
  /** Hide from assistive technology when the name is already shown next to it. */
  decorative?: boolean;
  className?: string;
};

/** A letter or digit (any script), with any combining marks after it. */
const WORD_START = /[\p{L}\p{N}][\p{M}]*/u;

/**
 * Lowercase name particles and joining words passed over for the second
 * initial ("Ludwig van Beethoven" → "LB", "Office of the Registrar" → "OR").
 * Capitalised, they count ("Van Morrison" → "VM").
 */
const PARTICLES = new Set([
  "a", "al", "and", "bin", "bint", "da", "das", "de", "del", "della", "der", "des", "di", "do", "dos", "du", "el", "for",
  "ibn", "la", "le", "of", "the", "ten", "ter", "van", "von", "y", "zu",
]);

/** A word's first letter or digit, and whether the word is a lowercase particle. */
function wordStart(word: string): { start: string; particle: boolean } | null {
  const start = word.match(WORD_START)?.[0];
  if (!start) return null;
  return { start, particle: PARTICLES.has(word.replace(/[^\p{L}]/gu, "")) };
}

/**
 * Up to two initials: the first letter or digit of the first two words
 * ("IT Help Desk" → "IH", "Ada Lovelace" → "AL"). Punctuation and symbols
 * are skipped ("Go docs (signed-in)" → "GD", not "G("), and a word with no
 * letters or digits ("—", "(", "&") doesn't count. The second initial skips
 * lowercase particles ("Charles de Gaulle" → "CG") unless only particles
 * follow ("Maria de" → "MD").
 */
export function initials(name: string): string {
  const words = name
    .split(/\s+/)
    .map(wordStart)
    .filter((w): w is { start: string; particle: boolean } => w !== null);
  const [first, ...rest] = words;
  if (!first) return "?";
  const second = rest.find((w) => !w.particle) ?? rest[0];
  return (first.start + (second?.start ?? "")).toUpperCase();
}

/** Deterministic tint (1–6) so the same name always gets the same colour. */
function hue(name: string): number {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return (h % 6) + 1;
}

export function Avatar({ name, src, size = "md", shape = "circle", decorative = false, className }: AvatarProps) {
  return (
    <BaseAvatar.Root
      role={decorative ? undefined : "img"}
      aria-label={decorative ? undefined : name}
      aria-hidden={decorative || undefined}
      data-size={size}
      data-shape={shape}
      data-hue={hue(name)}
      className={cx(styles.avatar, className)}
    >
      {src && <BaseAvatar.Image src={src} alt="" className={styles.image} />}
      <BaseAvatar.Fallback aria-hidden className={styles.fallback}>
        {initials(name)}
      </BaseAvatar.Fallback>
    </BaseAvatar.Root>
  );
}

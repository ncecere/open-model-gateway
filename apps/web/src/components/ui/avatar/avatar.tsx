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

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  const first = parts[0]?.[0] ?? "";
  const last = parts.length > 1 ? (parts[parts.length - 1]?.[0] ?? "") : "";
  return (first + last).toUpperCase() || "?";
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

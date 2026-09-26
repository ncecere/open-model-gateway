import type { ComponentPropsWithRef } from "react";
import { cx, dataFlag, type Tone } from "@/lib/bitop-utils";
import styles from "./badge.module.css";

export type BadgeProps = ComponentPropsWithRef<"span"> & {
  tone?: Tone;
  /** `soft` = tinted fill (default); `outline` = hairline ring, no fill. */
  variant?: "soft" | "outline";
  size?: "sm" | "md";
  /** Show a leading status dot. */
  dot?: boolean;
  /** Pulse the dot (in-progress states). Disabled under reduced motion. */
  pulse?: boolean;
};

/** A small pill label. Text colours meet 4.5:1 on their tinted fills. */
export function Badge({ tone = "neutral", variant = "soft", size = "md", dot, pulse, className, children, ...props }: BadgeProps) {
  return (
    <span {...props} data-tone={tone} data-variant={variant} data-size={size} className={cx(styles.badge, className)}>
      {(dot || pulse) && <span aria-hidden className={styles.dot} data-pulse={dataFlag(pulse)} />}
      {children}
    </span>
  );
}

export type StatusBadgeProps = Omit<BadgeProps, "dot">;

/** A pill with a coloured status dot, e.g. "● Ready", "● Processing" (pulse). */
export function StatusBadge(props: StatusBadgeProps) {
  return <Badge {...props} dot />;
}

export type StatusDotProps = Omit<ComponentPropsWithRef<"span">, "children"> & {
  tone?: Tone;
  pulse?: boolean;
  /**
   * Accessible text. Without it the dot is decorative (aria-hidden) and the
   * status must be conveyed by adjacent text — colour alone is not enough.
   */
  label?: string;
};

export function StatusDot({ tone = "neutral", pulse, label, className, ...props }: StatusDotProps) {
  return (
    <span
      {...props}
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      data-tone={tone}
      className={cx(styles.standaloneDot, className)}
    >
      <span className={styles.dot} data-pulse={dataFlag(pulse)} />
    </span>
  );
}

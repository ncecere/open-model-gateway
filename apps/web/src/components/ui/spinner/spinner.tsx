import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./spinner.module.css";

export type SpinnerProps = Omit<ComponentPropsWithRef<"span">, "children"> & {
  size?: "sm" | "md" | "lg";
  /**
   * Accessible label. When set, the spinner is announced as a status;
   * otherwise it is decorative (aria-hidden) and the caller conveys state.
   */
  label?: string;
};

/** A rotating ring. Decorative unless given a `label`. */
export function Spinner({ size = "md", label, className, ...props }: SpinnerProps) {
  return (
    <span
      role={label ? "status" : undefined}
      aria-hidden={label ? undefined : true}
      data-size={size}
      className={cx(styles.spinner, className)}
      {...props}
    >
      <svg viewBox="0 0 24 24" fill="none" className={styles.svg}>
        <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="2.5" className={styles.track} />
        <path d="M21 12a9 9 0 0 0-9-9" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" />
      </svg>
      {label && <span className="sr-only">{label}</span>}
    </span>
  );
}

export type LoadingProps = Omit<ComponentPropsWithRef<"div">, "children"> & {
  /** Visible and announced text. */
  label?: ReactNode;
  /** Centre within a padded block (page/section loading). */
  block?: boolean;
};

/** A labelled loading indicator announced politely to assistive technology. */
export function Loading({ label = "Loading…", block = true, className, ...props }: LoadingProps) {
  return (
    <div role="status" data-block={block ? "" : undefined} className={cx(styles.loading, className)} {...props}>
      <Spinner size="sm" />
      <span>{label}</span>
    </div>
  );
}

import type { ComponentPropsWithRef, CSSProperties } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./skeleton.module.css";

export type SkeletonProps = Omit<ComponentPropsWithRef<"span">, "children"> & {
  width?: CSSProperties["width"];
  height?: CSSProperties["height"];
  shape?: "rect" | "text" | "circle";
};

/**
 * A shimmering placeholder. Always aria-hidden: pair a region of skeletons
 * with a single <Loading> or an sr-only role="status" message.
 */
export function Skeleton({ width, height, shape = "rect", className, style, ...props }: SkeletonProps) {
  return (
    <span
      aria-hidden
      data-shape={shape}
      className={cx(styles.skeleton, className)}
      style={{ width, height, ...style }}
      {...props}
    />
  );
}

export type SkeletonTextProps = { lines?: number; className?: string };

/** A paragraph of skeleton lines; the last one is shorter. */
export function SkeletonText({ lines = 3, className }: SkeletonTextProps) {
  return (
    <span aria-hidden className={cx(styles.text, className)}>
      {Array.from({ length: lines }, (_, i) => (
        <Skeleton key={i} shape="text" width={i === lines - 1 && lines > 1 ? "60%" : "100%"} />
      ))}
    </span>
  );
}

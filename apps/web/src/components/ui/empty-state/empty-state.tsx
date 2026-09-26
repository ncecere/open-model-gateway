import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./empty-state.module.css";

export type EmptyStateProps = Omit<ComponentPropsWithRef<"div">, "title"> & {
  /** Decorative icon, shown in a soft tinted circle. */
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  /** Primary (and optional secondary) action. */
  action?: ReactNode;
  /** Render the title as a heading at this level (default: a paragraph). */
  titleAs?: "h1" | "h2" | "h3" | "h4" | "p";
  /** `compact` for use inside tables and small cards. */
  size?: "md" | "compact";
};

export function EmptyState({ icon, title, description, action, titleAs: Title = "p", size = "md", className, ...props }: EmptyStateProps) {
  return (
    <div data-size={size} className={cx(styles.empty, className)} {...props}>
      {icon && (
        <span aria-hidden className={styles.icon}>
          {icon}
        </span>
      )}
      <Title className={styles.title}>{title}</Title>
      {description && <p className={styles.description}>{description}</p>}
      {action && <div className={styles.action}>{action}</div>}
    </div>
  );
}

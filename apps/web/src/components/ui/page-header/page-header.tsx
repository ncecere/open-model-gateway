import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./page-header.module.css";

export type PageHeaderProps = Omit<ComponentPropsWithRef<"div">, "title"> & {
  /** The page heading (an <h1> by default). */
  title: ReactNode;
  /** Heading level, e.g. "h2" when the header sits inside another page. */
  titleAs?: "h1" | "h2" | "h3";
  description?: ReactNode;
  /** Buttons on the right. */
  actions?: ReactNode;
  /** Shown above the title, e.g. <Breadcrumbs />. */
  breadcrumbs?: ReactNode;
  /** Inline meta next to the title, e.g. status badges. */
  meta?: ReactNode;
  /** A line of key facts under the title and description, e.g. a <FactsLine>. */
  facts?: ReactNode;
};

export function PageHeader({ title, titleAs: Title = "h1", description, actions, breadcrumbs, meta, facts, className, ...props }: PageHeaderProps) {
  return (
    <div className={cx(styles.header, className)} {...props}>
      {breadcrumbs && <div className={styles.breadcrumbs}>{breadcrumbs}</div>}
      <div className={styles.row}>
        <div className={styles.heading}>
          <div className={styles.titleRow}>
            <Title className={styles.title}>{title}</Title>
            {meta && <div className={styles.meta}>{meta}</div>}
          </div>
          {description && <p className={styles.description}>{description}</p>}
          {facts && <div className={styles.facts}>{facts}</div>}
        </div>
        {actions && <div className={styles.actions}>{actions}</div>}
      </div>
    </div>
  );
}

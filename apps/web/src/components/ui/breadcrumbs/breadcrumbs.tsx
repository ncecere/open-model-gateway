"use client";

import { useRender } from "@base-ui/react/use-render";
import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./breadcrumbs.module.css";

export type BreadcrumbItem = {
  label: ReactNode;
  href?: string;
  /** Router link, e.g. `<Link to="/teams/$team" params={{ team }} />`. */
  render?: useRender.RenderProp;
  icon?: ReactNode;
};

export type BreadcrumbsProps = {
  items: BreadcrumbItem[];
  /** Landmark name (default "Breadcrumb"). */
  label?: string;
  className?: string;
};

function Crumb({ item }: { item: BreadcrumbItem }) {
  return useRender({
    render: item.render,
    defaultTagName: "a",
    props: {
      href: item.href,
      className: styles.link,
      children: (
        <>
          {item.icon}
          {item.label}
        </>
      ),
    },
  });
}

/** A trail of links; the last item is the current page (aria-current="page"). */
export function Breadcrumbs({ items, label = "Breadcrumb", className }: BreadcrumbsProps) {
  return (
    <nav aria-label={label} className={cx(styles.nav, className)}>
      <ol className={styles.list}>
        {items.map((item, i) => {
          const last = i === items.length - 1;
          return (
            <li key={i} className={styles.item}>
              {last ? (
                <span aria-current="page" className={styles.current}>
                  {item.icon}
                  {item.label}
                </span>
              ) : (
                <Crumb item={item} />
              )}
              {!last && <ChevronRight aria-hidden className={styles.separator} />}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}

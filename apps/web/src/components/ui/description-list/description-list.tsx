import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./description-list.module.css";

/*
 * Key/value facts in two shapes:
 *
 * DescriptionList: a <dl> of label/value pairs for detail pages and sheets.
 *   "grid" puts labels in a column next to their values (stacking on narrow
 *   containers); "stacked" puts each label above its value, optionally in
 *   several columns.
 *
 *   <DescriptionList items={[
 *     { label: "Start URL", value: <TextLink href={url} external>{url}</TextLink> },
 *     { label: "Schedule", value: "Weekly, next Oct 3" },
 *   ]} />
 *   <DescriptionList layout="stacked" columns={3}>
 *     <DescriptionItem label="Status"><StatusBadge tone="success">Ready</StatusBadge></DescriptionItem>
 *   </DescriptionList>
 *
 * FactsLine: one wrapping line of short facts for a detail-page header,
 *   "Website · Nomic 768 · 58 documents · synced 2 h ago", with optional
 *   icons. It's a list (so screen readers announce the count); the "·"
 *   separators are CSS and never read out, and a separator that would start a
 *   wrapped line is clipped. Give each fact a `label` when the value alone
 *   doesn't say what it is: it's read before the value ("Embedding profile:
 *   Nomic 768") and hidden visually unless `showLabels` is set.
 *
 * Values can be rich: badges, links, <Time>, copy fields.
 */

export type DescriptionEntry = {
  label: ReactNode;
  value: ReactNode;
  /** Stable key (default: the label when it's a string, else the index). */
  id?: string;
};

export type DescriptionListProps = Omit<ComponentPropsWithRef<"dl">, "children"> & {
  items?: DescriptionEntry[];
  /** DescriptionItem elements (instead of, or after, `items`). */
  children?: ReactNode;
  /** grid: label column + value; stacked: label above value. */
  layout?: "grid" | "stacked";
  /** Columns of pairs in the stacked layout (they wrap to fewer on narrow containers). */
  columns?: 1 | 2 | 3 | 4;
  /** Text size. */
  size?: "sm" | "md";
  /** Hairlines between the rows of a grid. */
  dividers?: boolean;
};

export function DescriptionList({
  items,
  children,
  layout = "grid",
  columns = 1,
  size = "md",
  dividers = false,
  className,
  ...props
}: DescriptionListProps) {
  return (
    <dl
      {...props}
      data-layout={layout}
      data-columns={layout === "stacked" ? columns : undefined}
      data-size={size}
      data-dividers={dividers ? "" : undefined}
      className={cx(styles.list, className)}
    >
      {items?.map((item, i) => (
        <DescriptionItem key={item.id ?? (typeof item.label === "string" ? item.label : i)} label={item.label}>
          {item.value}
        </DescriptionItem>
      ))}
      {children}
    </dl>
  );
}

export type DescriptionItemProps = Omit<ComponentPropsWithRef<"div">, "children"> & {
  label: ReactNode;
  /** The value. Empty values show `empty`. */
  children?: ReactNode;
  /** Shown when there is no value (default "—", announced as "Not set"). */
  empty?: ReactNode;
};

/** One label/value pair (a <div> holding a <dt> and a <dd>). */
export function DescriptionItem({ label, children, empty, className, ...props }: DescriptionItemProps) {
  const blank = children === undefined || children === null || children === "" || children === false;
  return (
    <div {...props} className={cx(styles.item, className)}>
      <dt className={styles.term}>{label}</dt>
      <dd className={styles.detail} data-empty={blank ? "" : undefined}>
        {blank
          ? (empty ?? (
              <>
                <span aria-hidden>—</span>
                <span className="sr-only">Not set</span>
              </>
            ))
          : children}
      </dd>
    </div>
  );
}

/* ---------------- Facts line ---------------- */

export type Fact = {
  value: ReactNode;
  /** What the fact is, e.g. "Embedding profile". Read before the value; visible with `showLabels`. */
  label?: ReactNode;
  /** Decorative icon before the fact. */
  icon?: ReactNode;
  id?: string;
};

export type FactsLineProps = Omit<ComponentPropsWithRef<"ul">, "children"> & {
  items?: Fact[];
  /** FactsLineItem elements (instead of, or after, `items`). */
  children?: ReactNode;
  /** Show each fact's label ("Profile: Nomic 768") instead of only announcing it. */
  showLabels?: boolean;
  size?: "sm" | "md";
};

export function FactsLine({ items, children, showLabels = false, size = "md", className, ...props }: FactsLineProps) {
  return (
    <div className={cx(styles.factsClip, className)} data-size={size}>
      <ul {...props} className={styles.facts} data-show-labels={showLabels ? "" : undefined}>
        {items?.map((f, i) => (
          <FactsLineItem key={f.id ?? (typeof f.label === "string" ? f.label : i)} label={f.label} icon={f.icon}>
            {f.value}
          </FactsLineItem>
        ))}
        {children}
      </ul>
    </div>
  );
}

export type FactsLineItemProps = Omit<ComponentPropsWithRef<"li">, "children"> & {
  label?: ReactNode;
  icon?: ReactNode;
  children: ReactNode;
};

/** One fact in a FactsLine. */
export function FactsLineItem({ label, icon, children, className, ...props }: FactsLineItemProps) {
  return (
    <li {...props} className={cx(styles.fact, className)}>
      {icon && (
        <span aria-hidden className={styles.factIcon}>
          {icon}
        </span>
      )}
      {label !== undefined && label !== null && label !== "" && <span className={styles.factLabel}>{label}: </span>}
      <span className={styles.factValue}>{children}</span>
    </li>
  );
}

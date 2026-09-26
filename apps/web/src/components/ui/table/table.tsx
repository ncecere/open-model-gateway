"use client";

import { type ComponentPropsWithRef, type CSSProperties, type ReactNode, useEffect, useId, useRef, useState } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./table.module.css";

/*
 * Data table with real <th scope="col"> headers and a caption.
 *
 *   <Table caption="Documents" columns={["Name", { label: "Size", numeric: true }, ""]}>
 *     <Tr><Td>handbook.pdf</Td><Td numeric>1.2 MB</Td><Td>…actions…</Td></Tr>
 *   </Table>
 *
 * An empty column label ("") becomes a screen-reader-only "Actions" header.
 *
 * Whenever the table overflows its wrapper (too wide for a narrow screen, or
 * taller than `maxHeight`), the wrapper becomes a focusable region named by
 * the caption, so keyboard users can scroll it (WCAG 2.1.1). Overflow is
 * measured with a ResizeObserver; a table that fits is not a tab stop.
 */

export type TableColumn =
  | string
  | {
      label: ReactNode;
      numeric?: boolean;
      /** Visually hide the header text (still announced). */
      hideLabel?: boolean;
      width?: string;
      /** Sets aria-sort on the header cell (use on the one sorted column). */
      sort?: "ascending" | "descending" | "none" | "other";
    };

export type TableProps = Omit<ComponentPropsWithRef<"table">, "children"> & {
  /** Describes the table for assistive technology (required). */
  caption: ReactNode;
  /** Show the caption visually (default: screen-reader only). */
  showCaption?: boolean;
  columns: TableColumn[];
  children?: ReactNode;
  /** Header stays visible while the table body scrolls (set `maxHeight`). */
  stickyHeader?: boolean;
  /** Max height of the scroll area, e.g. "24rem". */
  maxHeight?: string;
  density?: "comfortable" | "compact";
  /** Rendered in a full-width row instead of `children` (e.g. an EmptyState). */
  empty?: ReactNode;
  /** Wrap in a card-like surface with ring and radius. */
  framed?: boolean;
};

export function Table({
  caption,
  showCaption,
  columns,
  children,
  stickyHeader,
  maxHeight,
  density = "comfortable",
  empty,
  framed = false,
  className,
  ...props
}: TableProps) {
  const captionId = useId();
  const wrap = useRef<HTMLDivElement>(null);
  const [overflowing, setOverflowing] = useState(false);
  const wrapStyle: CSSProperties | undefined = maxHeight ? { maxHeight } : undefined;

  // A scroll container must be keyboard-focusable (WCAG 2.1.1): track whether it scrolls.
  useEffect(() => {
    const el = wrap.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const check = () => setOverflowing(el.scrollWidth > el.clientWidth + 1 || el.scrollHeight > el.clientHeight + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    if (el.firstElementChild) ro.observe(el.firstElementChild);
    return () => ro.disconnect();
  }, []);

  return (
    <div
      ref={wrap}
      className={styles.wrap}
      data-framed={dataFlag(framed)}
      data-scroll={dataFlag(Boolean(maxHeight))}
      data-overflowing={dataFlag(overflowing)}
      style={wrapStyle}
      {...(overflowing ? { tabIndex: 0, role: "region", "aria-labelledby": captionId } : {})}
    >
      <table
        {...props}
        data-density={density}
        data-sticky={dataFlag(stickyHeader)}
        className={cx(styles.table, className)}
      >
        <caption id={captionId} className={showCaption ? styles.caption : "sr-only"}>
          {caption}
        </caption>
        <thead>
          <tr>
            {columns.map((c, i) => {
              const col = typeof c === "string" ? { label: c } : c;
              const blank = col.label === "" || col.label === undefined || col.label === null;
              return (
                <th
                  key={i}
                  scope="col"
                  aria-sort={"sort" in col ? col.sort : undefined}
                  data-numeric={dataFlag(col.numeric)}
                  style={col.width ? { width: col.width } : undefined}
                  className={styles.th}
                >
                  {blank ? <span className="sr-only">Actions</span> : col.hideLabel ? <span className="sr-only">{col.label}</span> : col.label}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {empty ? (
            <tr className={styles.emptyRow}>
              <td colSpan={columns.length} className={styles.emptyCell}>
                {empty}
              </td>
            </tr>
          ) : (
            children
          )}
        </tbody>
      </table>
    </div>
  );
}

export type TrProps = ComponentPropsWithRef<"tr"> & { selected?: boolean };

export function Tr({ selected, className, ...props }: TrProps) {
  return <tr data-selected={dataFlag(selected)} className={cx(styles.tr, className)} {...props} />;
}

export type TdProps = ComponentPropsWithRef<"td"> & {
  /** Right-align with tabular figures. */
  numeric?: boolean;
  /** Muted secondary text. */
  muted?: boolean;
  /** Keep content on one line. */
  nowrap?: boolean;
};

export function Td({ numeric, muted, nowrap, className, ...props }: TdProps) {
  return (
    <td
      data-numeric={dataFlag(numeric)}
      data-muted={dataFlag(muted)}
      data-nowrap={dataFlag(nowrap)}
      className={cx(styles.td, className)}
      {...props}
    />
  );
}

/** A row header cell (<th scope="row">), e.g. the name column. */
export function Th({ className, ...props }: ComponentPropsWithRef<"th">) {
  return <th scope="row" className={cx(styles.rowHeader, className)} {...props} />;
}

/** Right-aligned container for per-row action buttons. */
export function TableActions({ children }: { children: ReactNode }) {
  return <div className={styles.actions}>{children}</div>;
}

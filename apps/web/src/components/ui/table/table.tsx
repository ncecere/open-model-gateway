"use client";

import { type ComponentPropsWithRef, type CSSProperties, type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";
import { cx, dataFlag, mergeRefs, useScrollEdges } from "@/lib/bitop-utils";
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
 * While columns are hidden to the side, that edge shows a shadow so it's
 * clear the table scrolls (for example the actions column on a phone).
 *
 * - A column marked `stickyEnd` (with `<Td stickyEnd>` cells: row actions)
 *   stays pinned to the end edge while the rest scrolls under it, so row
 *   actions never sit off-screen.
 * - `empty` content is centred on the visible width of the scroll box, not
 *   on the full width of a table wider than it.
 * - `stack`: below 600px each row becomes a block, the first cell on its own
 *   line and the others under it, each with its column's name (for settings
 *   tables whose columns would otherwise squash on a phone).
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
      /** Pin the column to the end edge while the table scrolls sideways (row actions); mark its cells `<Td stickyEnd>`. */
      stickyEnd?: boolean;
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
  /** Below 600px, show each row as a block with its cells' column names (settings tables on a phone). */
  stack?: boolean;
};

/** The header texts, as names for a stacked table's cells ("" for the first column and visually hidden headers). */
function labelStackedCells(table: HTMLTableElement) {
  const heads = [...(table.tHead?.rows[0]?.cells ?? [])].map((th, i) => (i === 0 || th.querySelector(".sr-only") ? "" : (th.textContent ?? "")));
  for (const body of table.tBodies) {
    for (const row of body.rows) {
      [...row.cells].forEach((cell, i) => {
        const label = cell.colSpan > 1 ? "" : (heads[i] ?? "");
        if (label) cell.dataset.label = label;
        else delete cell.dataset.label;
      });
    }
  }
}

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
  stack = false,
  className,
  ...props
}: TableProps) {
  const captionId = useId();
  const wrap = useRef<HTMLDivElement>(null);
  const [edgesRef, edges] = useScrollEdges<HTMLDivElement>();
  const wrapRef = useMemo(() => mergeRefs(wrap, edgesRef), [edgesRef]);
  const [overflowing, setOverflowing] = useState(false);
  const tableRef = useRef<HTMLTableElement>(null);
  const ref = useMemo(() => (props.ref ? mergeRefs(tableRef, props.ref) : tableRef), [props.ref]);
  const stickyEnd = columns.some((c) => typeof c !== "string" && c.stickyEnd);
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

  // A stacked table names each cell after its column (shown above the value on a phone).
  useEffect(() => {
    const table = tableRef.current;
    if (!stack || !table) return;
    labelStackedCells(table);
    if (typeof MutationObserver === "undefined") return;
    const mo = new MutationObserver(() => labelStackedCells(table));
    mo.observe(table, { childList: true, subtree: true });
    return () => mo.disconnect();
  }, [stack, columns]);

  return (
    <div className={styles.frame} data-framed={dataFlag(framed)} data-sticky-end={dataFlag(stickyEnd)}>
      <div
        ref={wrapRef}
        className={styles.wrap}
        data-scroll={dataFlag(Boolean(maxHeight))}
        data-overflowing={dataFlag(overflowing)}
        style={wrapStyle}
        {...(overflowing ? { tabIndex: 0, role: "region", "aria-labelledby": captionId } : {})}
      >
        <table
          {...props}
          ref={ref}
          data-density={density}
          data-sticky={dataFlag(stickyHeader)}
          data-stack={dataFlag(stack)}
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
                    data-sticky-end={dataFlag("stickyEnd" in col && col.stickyEnd)}
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
                  <div className={styles.emptyContent}>{empty}</div>
                </td>
              </tr>
            ) : (
              children
            )}
          </tbody>
        </table>
      </div>
      {edges.start && <span aria-hidden className={styles.edge} data-side="start" />}
      {edges.end && <span aria-hidden className={styles.edge} data-side="end" />}
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
  /** A cell of a `stickyEnd` column: pinned to the end edge while the table scrolls sideways. */
  stickyEnd?: boolean;
};

export function Td({ numeric, muted, nowrap, stickyEnd, className, ...props }: TdProps) {
  return (
    <td
      data-numeric={dataFlag(numeric)}
      data-muted={dataFlag(muted)}
      data-nowrap={dataFlag(nowrap)}
      data-sticky-end={dataFlag(stickyEnd)}
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

/*
 * Rows for hand-built Bitop `Table`s (no drawers: details open in place).
 *
 * - ExpandableRow: a row whose first cell is a disclosure button
 *   (aria-expanded/aria-controls, named "Details for <row>") that shows a
 *   full-width detail row underneath. Add `expandColumn` as the table's first
 *   column. Controlled (`expanded`/`onExpandedChange`) or uncontrolled.
 * - TableDividerRow: a full-width labelled divider inside a table body, e.g.
 *   "Fallback only · not used by default" above pinned/fallback routes, with
 *   the reason as visible text (not tooltip-only).
 *
 *   <Table caption="Routes" columns={[expandColumn, "Route", { label: "Price", numeric: true }]}>
 *     <ExpandableRow label={r.name} colSpan={3} details={<RouteDetail r={r} />}><Th>{r.name}</Th><Td numeric>…</Td></ExpandableRow>
 *     <TableDividerRow colSpan={3} label="Fallback only · not used by default" reason="Used only when earlier routes fail." />
 *   </Table>
 */
import { ChevronRight } from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import { IconButton } from "../ui/button/button";
import { type TableColumn, Td, Tr } from "../ui/table/table";
import styles from "./table-rows.module.css";

/** The leading, visually hidden "Details" column for ExpandableRow tables. */
export const expandColumn: TableColumn = { label: "Details", hideLabel: true, width: "2.75rem" };

export type ExpandableRowProps = {
  /** Plain-text row name for the button: "Details for <label>". */
  label: string;
  /** Total number of table columns, including the expand column. */
  colSpan: number;
  /** The row's remaining cells (Th/Td). */
  children: ReactNode;
  /** Content of the detail row. */
  details: ReactNode;
  expanded?: boolean;
  defaultExpanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
  selected?: boolean;
  /** Muted (e.g. disabled/revoked records). */
  inactive?: boolean;
};

export function ExpandableRow({ label, colSpan, children, details, expanded, defaultExpanded = false, onExpandedChange, selected, inactive }: ExpandableRowProps) {
  const [inner, setInner] = useState(defaultExpanded);
  const open = expanded ?? inner;
  const id = useId();
  const toggle = () => { if (expanded === undefined) setInner(!open); onExpandedChange?.(!open); };
  return (
    <>
      <Tr selected={selected} className={styles.row} data-expanded={open ? "" : undefined} data-inactive={inactive ? "" : undefined}>
        <Td className={styles.toggleCell}>
          <IconButton size="sm" icon={<ChevronRight aria-hidden className={styles.chevron} />} label={`Details for ${label}`} aria-expanded={open} aria-controls={open ? id : undefined} onClick={toggle} />
        </Td>
        {children}
      </Tr>
      {open && (
        <tr id={id} className={styles.detailRow}>
          <td colSpan={colSpan} className={styles.detailCell}>{details}</td>
        </tr>
      )}
    </>
  );
}

export type TableDividerRowProps = {
  colSpan: number;
  label: ReactNode;
  /** Why rows below are separated; shown as muted text. */
  reason?: ReactNode;
};

export function TableDividerRow({ colSpan, label, reason }: TableDividerRowProps) {
  return (
    <tr className={styles.dividerRow}>
      <td colSpan={colSpan} className={styles.dividerCell}>
        <span className={styles.dividerLabel}>{label}</span>
        {reason && <span className={styles.dividerReason}>{reason}</span>}
      </td>
    </tr>
  );
}

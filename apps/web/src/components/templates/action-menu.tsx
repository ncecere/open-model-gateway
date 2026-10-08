/*
 * The "…" menu of a page header or a table row. Destructive actions always
 * come last, after a separator, whatever order they're passed in (D3, D5).
 * A disabled action can say why (`disabledReason`), shown after its label.
 */
import { MoreHorizontal } from "lucide-react";
import { Fragment, type ReactElement, type ReactNode } from "react";
import { IconButton } from "@/components/ui/button/button";
import { Menu, MenuItem, MenuLinkItem, MenuSeparator } from "@/components/ui/menu/menu";
import styles from "./templates.module.css";

export type ActionItem = {
  label: string;
  icon?: ReactNode;
  /** Runs the action (buttons). */
  onSelect?: () => void;
  /** Renders a link instead, e.g. <Link to="…" />. */
  render?: ReactElement;
  /** Deletes, revokes, suspends…: shown in red, last. */
  danger?: boolean;
  disabled?: boolean;
  /** Why it's disabled, shown under the label (P-04). */
  disabledReason?: string;
  /** Leave the action out. */
  hidden?: boolean;
};

/** Visible actions, destructive ones last (a stable partition). */
export function orderActions(actions: ActionItem[]): ActionItem[] {
  const shown = actions.filter((a) => !a.hidden);
  return [...shown.filter((a) => !a.danger), ...shown.filter((a) => a.danger)];
}

export type ActionMenuProps = {
  actions: ActionItem[];
  /** The trigger's accessible name, e.g. "More actions" or "Actions for Handbook". */
  label: string;
  size?: "sm" | "md";
};

/** A "…" icon button opening the actions; renders nothing without actions. */
export function ActionMenu({ actions, label, size = "sm" }: ActionMenuProps) {
  const ordered = orderActions(actions);
  if (ordered.length === 0) return null;
  const firstDanger = ordered.findIndex((a) => a.danger);
  return (
    <Menu align="end" trigger={<IconButton size={size} variant="ghost" icon={<MoreHorizontal aria-hidden />} label={label} />}>
      {ordered.map((a, i) => {
        const text = a.disabled && a.disabledReason ? <ItemText label={a.label} reason={a.disabledReason} /> : a.label;
        return (
          <Fragment key={a.label}>
            {i === firstDanger && i > 0 && <MenuSeparator />}
            {a.render && !a.disabled ? (
              <MenuLinkItem icon={a.icon} tone={a.danger ? "danger" : "default"} render={a.render}>
                {text}
              </MenuLinkItem>
            ) : (
              <MenuItem icon={a.icon} tone={a.danger ? "danger" : "default"} disabled={a.disabled} onClick={a.onSelect}>
                {text}
              </MenuItem>
            )}
          </Fragment>
        );
      })}
    </Menu>
  );
}

/** A menu item's label with the reason it's disabled under it (ActionMenu's, for other menus). */
export function ItemText({ label, reason }: { label: string; reason: string }) {
  return (
    <span className={styles.menuText}>
      <span>{label}</span>
      <span className={styles.menuReason}>{reason}</span>
    </span>
  );
}

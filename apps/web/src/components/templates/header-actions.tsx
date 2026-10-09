/*
 * HeaderActions: a page or record header's actions. On desktop they render as
 * given. On a phone (≤ 640px) two or more actions collapse to the primary
 * action plus one "⋯" menu holding the rest (Bitop Menu via ActionMenu), so a
 * header never wraps into a stack of buttons. Destructive items stay red and
 * last in the menu.
 *
 * Children are read, not rewrapped: Bitop <Button>s become menu items (label,
 * icon, onClick or link `render`, disabled + `title` as the reason, variant
 * "danger" → red), an existing <ActionMenu>'s items join the same menu, and
 * anything else (PrevNext, custom controls) stays inline. Fragments are
 * flattened. The primary is the last primary-variant button (else the first
 * button) and is rendered unchanged, so its ref and type="submit" still work.
 *
 * PageHeader (templates/page-header), Heading, ResourcePage, RecordPage use it.
 */
import { Children, Fragment, isValidElement, type ReactElement, type ReactNode } from "react";
import { useMediaQuery } from "../../lib/bitop-utils";
import { Button, type ButtonProps } from "../ui/button/button";
import { ActionMenu, type ActionItem, type ActionMenuProps } from "./action-menu";
import styles from "./templates.module.css";

/** Where header actions collapse (Bitop's narrow breakpoint for phones; tested at 390px). */
export const HEADER_COLLAPSE_QUERY = "(max-width: 640px)";

type Part = { kind: "button"; element: ReactElement<ButtonProps>; item: ActionItem } | { kind: "menu"; items: ActionItem[] } | { kind: "inline"; node: ReactNode };

/** Fragments and arrays flattened; empty slots (false, null, "") dropped. */
export function flattenActions(children: ReactNode): ReactNode[] {
  const out: ReactNode[] = [];
  Children.forEach(children, child => {
    if (child === null || child === undefined || typeof child === "boolean" || child === "") return;
    if (isValidElement<{ children?: ReactNode }>(child) && child.type === Fragment) out.push(...flattenActions(child.props.children));
    else out.push(child);
  });
  return out;
}

/** Visible text of a node (icons and other elements without text contribute nothing). */
function textOf(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(textOf).join("");
  if (isValidElement<{ children?: ReactNode }>(node)) return textOf(node.props.children);
  return "";
}

/** A Bitop Button as a menu item; null when it can't be one (no label, or a form submit button). */
function buttonItem(element: ReactElement<ButtonProps>): ActionItem | null {
  const p = element.props as ButtonProps & { title?: string; "aria-label"?: string };
  if (p.type === "submit") return null;
  const label = (textOf(p.children).replace(/\s+/g, " ").trim() || p["aria-label"]) ?? "";
  if (!label) return null;
  const icon = Children.toArray(p.children).find(c => isValidElement<{ "aria-hidden"?: unknown }>(c) && !!c.props["aria-hidden"]);
  const disabled = !!(p.disabled || p.loading);
  return {
    label, icon, danger: p.variant === "danger", disabled,
    disabledReason: disabled && p.title ? p.title : undefined,
    render: typeof p.render === "function" ? undefined : p.render,
    // MenuItem passes its click event through, like the button did.
    onSelect: p.onClick as (() => void) | undefined,
  };
}

function classify(node: ReactNode): Part {
  if (isValidElement(node) && node.type === Button) {
    const item = buttonItem(node as ReactElement<ButtonProps>);
    if (item) return { kind: "button", element: node as ReactElement<ButtonProps>, item };
  }
  if (isValidElement(node) && node.type === ActionMenu) return { kind: "menu", items: (node.props as ActionMenuProps).actions.filter(a => !a.hidden) };
  return { kind: "inline", node };
}

export type CollapsedActions = { inline: ReactNode[]; primary: ReactElement | null; more: ActionItem[] } | null;

/** The phone layout of `children`, or null when there are fewer than two actions to collapse. */
export function collapseActions(children: ReactNode): CollapsedActions {
  const parts = flattenActions(children).map(classify);
  const buttons = parts.filter((p): p is Extract<Part, { kind: "button" }> => p.kind === "button");
  const menuItems = parts.flatMap(p => p.kind === "menu" ? p.items : []);
  if (buttons.length + menuItems.length < 2) return null;
  const isPrimary = (b: (typeof buttons)[number]) => (b.element.props.variant ?? "primary") === "primary";
  const primary = [...buttons].reverse().find(isPrimary) ?? buttons[0] ?? null;
  return {
    inline: parts.flatMap(p => p.kind === "inline" ? [p.node] : []),
    primary: primary?.element ?? null,
    more: [...buttons.filter(b => b !== primary).map(b => b.item), ...menuItems],
  };
}

/** A header's actions: unchanged on desktop; primary + "⋯" on a phone when there are two or more. */
export function HeaderActions({ children }: { children: ReactNode }) {
  const narrow = useMediaQuery(HEADER_COLLAPSE_QUERY);
  const collapsed = narrow ? collapseActions(children) : null;
  if (!collapsed) return <div className={styles.headerActions}>{children}</div>;
  return <div className={styles.headerActions} data-collapsed="">
    {collapsed.inline.map((node, i) => <Fragment key={`inline-${i}`}>{node}</Fragment>)}
    {collapsed.primary}
    <ActionMenu label="More actions" size="md" actions={collapsed.more} />
  </div>;
}

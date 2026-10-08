"use client";

import { Menu as BaseMenu } from "@base-ui/react/menu";
import { Check, ChevronRight } from "lucide-react";
import { createContext, type ReactElement, type ReactNode, useContext, useRef } from "react";
import popup from "@/components/ui/styles/popup.module.css";
import { cx, useLandmarkContainer } from "@/lib/bitop-utils";
import styles from "./menu.module.css";

/*
 * Dropdown menu on Base UI Menu: roving focus, typeahead, Escape to close,
 * focus returns to the trigger.
 *
 *   <Menu trigger={<Button variant="secondary">Actions</Button>}>
 *     <MenuItem icon={<Pencil aria-hidden />} onClick={rename}>Rename</MenuItem>
 *     <MenuLinkItem render={<Link to="/settings" />}>Settings</MenuLinkItem>
 *     <MenuSeparator />
 *     <MenuItem tone="danger" onClick={remove}>Delete</MenuItem>
 *   </Menu>
 *
 * The item parts (MenuItem, MenuCheckboxItem, MenuRadioGroup, MenuSubmenu…)
 * also work inside ContextMenu and Menubar, which share Base UI's Menu parts.
 *
 * The popup is portalled into the outermost landmark around its trigger
 * (the page's <main>, the sidebar's <nav>…), not the end of <body>, so it
 * is inside the page's landmarks like the trigger (axe `region`); outside
 * any landmark, or from a dialog, it goes to <body>. `container` overrides
 * the choice.
 */

export type MenuProps = {
  /** The trigger element, usually a Button or IconButton. */
  trigger: ReactElement;
  children: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  side?: "top" | "bottom" | "left" | "right";
  align?: "start" | "center" | "end";
  sideOffset?: number;
  /** Popup width hint. */
  width?: "auto" | "trigger";
  /** Where the popup is portalled: an element, or null for <body> (default: the trigger's outermost landmark, else <body>). */
  container?: HTMLElement | null;
  className?: string;
};

/** Whether the menu around is portalled into a landmark (its submenus are positioned the same way). */
const InLandmark = createContext(false);

export function Menu({
  trigger,
  children,
  open,
  defaultOpen,
  onOpenChange,
  side = "bottom",
  align = "start",
  sideOffset = 6,
  width = "auto",
  container,
  className,
}: MenuProps) {
  const triggerRef = useRef<HTMLButtonElement>(null);
  const landmark = useLandmarkContainer(triggerRef);
  const into = container === undefined ? landmark : container;
  return (
    <BaseMenu.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}>
      <BaseMenu.Trigger ref={triggerRef} render={trigger} />
      <BaseMenu.Portal container={into ?? undefined} className={popup.portal}>
        <BaseMenu.Positioner
          className={popup.positioner}
          side={side}
          align={align}
          sideOffset={sideOffset}
          // Inside a landmark that scrolls or clips its content, a fixed popup isn't clipped.
          positionMethod={into ? "fixed" : "absolute"}
        >
          <BaseMenu.Popup className={cx(popup.popup, className)} data-width={width}>
            <InLandmark.Provider value={Boolean(into)}>{children}</InLandmark.Provider>
          </BaseMenu.Popup>
        </BaseMenu.Positioner>
      </BaseMenu.Portal>
    </BaseMenu.Root>
  );
}

type ItemExtras = {
  icon?: ReactNode;
  /** A keyboard shortcut hint shown on the right (visual only). */
  shortcut?: ReactNode;
  tone?: "default" | "danger";
  className?: string;
};

export type MenuItemProps = Omit<BaseMenu.Item.Props, "className"> & ItemExtras;

export function MenuItem({ icon, shortcut, tone = "default", className, children, ...props }: MenuItemProps) {
  return (
    <BaseMenu.Item {...props} className={cx(popup.item, tone === "danger" && popup.itemDanger, className)}>
      {icon}
      <span className={styles.label}>{children}</span>
      {shortcut && (
        <span aria-hidden className={popup.itemShortcut}>
          {shortcut}
        </span>
      )}
    </BaseMenu.Item>
  );
}

export type MenuLinkItemProps = Omit<BaseMenu.LinkItem.Props, "className"> & ItemExtras;

/**
 * A menu item that navigates. Pass `href`, or `render={<Link to=… />}` for
 * router links. It closes the menu when clicked (Base UI's default keeps it
 * open, which suits full page loads but leaves it open over a client-side
 * route); pass `closeOnClick={false}` to keep it.
 */
export function MenuLinkItem({ icon, shortcut, tone = "default", className, children, closeOnClick = true, ...props }: MenuLinkItemProps) {
  return (
    <BaseMenu.LinkItem {...props} closeOnClick={closeOnClick} className={cx(popup.item, tone === "danger" && popup.itemDanger, className)}>
      {icon}
      <span className={styles.label}>{children}</span>
      {shortcut && (
        <span aria-hidden className={popup.itemShortcut}>
          {shortcut}
        </span>
      )}
    </BaseMenu.LinkItem>
  );
}

export function MenuSeparator({ className }: { className?: string }) {
  return <BaseMenu.Separator className={cx(popup.separator, className)} />;
}

export type MenuGroupProps = { label?: ReactNode; children: ReactNode; className?: string };

export function MenuGroup({ label, children, className }: MenuGroupProps) {
  return (
    <BaseMenu.Group className={className}>
      {label && <BaseMenu.GroupLabel className={popup.groupLabel}>{label}</BaseMenu.GroupLabel>}
      {children}
    </BaseMenu.Group>
  );
}

export type MenuCheckboxItemProps = Omit<BaseMenu.CheckboxItem.Props, "className"> & {
  /** A keyboard shortcut hint shown on the right (visual only). */
  shortcut?: ReactNode;
  className?: string;
};

/**
 * A menu item that toggles a setting (role="menuitemcheckbox"). Controlled
 * with `checked` / `onCheckedChange`, or uncontrolled with `defaultChecked`.
 * The menu stays open on click so several options can be toggled.
 */
export function MenuCheckboxItem({ shortcut, className, children, ...props }: MenuCheckboxItemProps) {
  return (
    <BaseMenu.CheckboxItem {...props} className={cx(popup.item, styles.selectable, className)}>
      <span aria-hidden className={styles.indicator}>
        <BaseMenu.CheckboxItemIndicator className={styles.indicatorMark}>
          <Check />
        </BaseMenu.CheckboxItemIndicator>
      </span>
      <span className={styles.label}>{children}</span>
      {shortcut && (
        <span aria-hidden className={popup.itemShortcut}>
          {shortcut}
        </span>
      )}
    </BaseMenu.CheckboxItem>
  );
}

export type MenuRadioGroupProps = Omit<BaseMenu.RadioGroup.Props, "className"> & {
  /** Visible group label; also names the group for assistive technology. */
  label?: ReactNode;
  className?: string;
};

/** A set of mutually exclusive MenuRadioItems. Use `value` / `onValueChange` or `defaultValue`. */
export function MenuRadioGroup({ label, className, children, ...props }: MenuRadioGroupProps) {
  return (
    <BaseMenu.RadioGroup {...props} className={className}>
      {label && <BaseMenu.GroupLabel className={popup.groupLabel}>{label}</BaseMenu.GroupLabel>}
      {children}
    </BaseMenu.RadioGroup>
  );
}

export type MenuRadioItemProps = Omit<BaseMenu.RadioItem.Props, "className"> & {
  shortcut?: ReactNode;
  className?: string;
};

/** One option of a MenuRadioGroup (role="menuitemradio"). */
export function MenuRadioItem({ shortcut, className, children, ...props }: MenuRadioItemProps) {
  return (
    <BaseMenu.RadioItem {...props} className={cx(popup.item, styles.selectable, className)}>
      <span aria-hidden className={styles.indicator}>
        <BaseMenu.RadioItemIndicator className={cx(styles.indicatorMark, styles.radioDot)} />
      </span>
      <span className={styles.label}>{children}</span>
      {shortcut && (
        <span aria-hidden className={popup.itemShortcut}>
          {shortcut}
        </span>
      )}
    </BaseMenu.RadioItem>
  );
}

export type MenuSubmenuProps = {
  /** The submenu trigger's text. */
  label: ReactNode;
  icon?: ReactNode;
  /** Typeahead text when `label` isn't a plain string. */
  textValue?: string;
  disabled?: boolean;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Class for the submenu popup. */
  className?: string;
  children: ReactNode;
};

/**
 * A nested menu. Its trigger is an item that opens the submenu on hover,
 * Enter, Space or ArrowRight; ArrowLeft or Escape closes it again.
 */
export function MenuSubmenu({
  label,
  icon,
  textValue,
  disabled,
  open,
  defaultOpen,
  onOpenChange,
  className,
  children,
}: MenuSubmenuProps) {
  // Base UI portals a submenu into its parent menu's portal, so it is in the same landmark.
  const inLandmark = useContext(InLandmark);
  return (
    <BaseMenu.SubmenuRoot
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={onOpenChange ? (o) => onOpenChange(o) : undefined}
      disabled={disabled}
    >
      <BaseMenu.SubmenuTrigger
        className={cx(popup.item, styles.submenuTrigger)}
        disabled={disabled}
        label={textValue ?? (typeof label === "string" ? label : undefined)}
      >
        {icon}
        <span className={styles.label}>{label}</span>
        <ChevronRight aria-hidden className={styles.chevron} />
      </BaseMenu.SubmenuTrigger>
      <BaseMenu.Portal className={popup.portal}>
        <BaseMenu.Positioner className={popup.positioner} alignOffset={-4} positionMethod={inLandmark ? "fixed" : "absolute"}>
          <BaseMenu.Popup className={cx(popup.popup, className)}>{children}</BaseMenu.Popup>
        </BaseMenu.Positioner>
      </BaseMenu.Portal>
    </BaseMenu.SubmenuRoot>
  );
}

/** Non-interactive header content (e.g. the signed-in user's name and email). */
export function MenuHeader({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx(styles.header, className)}>{children}</div>;
}

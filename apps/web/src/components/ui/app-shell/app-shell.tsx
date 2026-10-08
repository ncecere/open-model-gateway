"use client";

import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { useRender } from "@base-ui/react/use-render";
import { ChevronRight, ChevronsUpDown, PanelLeft, X } from "lucide-react";
import { createContext, type MouseEvent, type ReactElement, type ReactNode, useCallback, useContext, useEffect, useId, useMemo, useRef, useState } from "react";
import { Avatar } from "@/components/ui/avatar/avatar";
import { IconButton } from "@/components/ui/button/button";
import { Container } from "@/components/ui/layout/layout";
import { Menu } from "@/components/ui/menu/menu";
import { ScrollArea } from "@/components/ui/scroll-area/scroll-area";
import { Tooltip } from "@/components/ui/tooltip/tooltip";
import { cx, dataFlag, NARROW_QUERY, useMediaQuery } from "@/lib/bitop-utils";
import styles from "./app-shell.module.css";

/*
 * Generic, unopinionated app-shell primitives:
 *
 *   <AppShell sidebar={<Sidebar>…</Sidebar>} topbar={<TopBar start={…} end={…} />}>
 *     <Main>…page…</Main>
 *   </AppShell>
 *
 * - AppShell renders a skip link to #main and owns the sidebar collapsed state.
 * - Sidebar: an <aside> (complementary landmark, named by `label`) with
 *   SidebarHeader (Brand, WorkspaceSwitcher), SidebarContent
 *   (SidebarNav > SidebarSection > SidebarItem) and SidebarFooter
 *   (SidebarUser). SidebarContent scrolls in a ScrollArea, so the header and
 *   the footer's account menu stay pinned at short window heights.
 * - SidebarItem marks the current page with aria-current="page" (router
 *   links set it automatically). When collapsed, labels become visually
 *   hidden (still announced) and show as tooltips.
 * - Main is the <main id="main"> skip-link target, with a max-width container.
 * - Narrow windows (below 600px, `drawerQuery`): no permanent rail. The
 *   sidebar is a modal drawer (Base UI Dialog) opened from the same toggle,
 *   with its full labels; Escape, the backdrop, its close button or
 *   following a link in it close it; after a link, focus moves to the main
 *   content (the skip-link target), since the page it came from is gone.
 */

/** Below this width the sidebar is a drawer (AppShell `drawerQuery`). */
export const SIDEBAR_DRAWER_QUERY = NARROW_QUERY;

type ShellState = {
  /** The rail is collapsed to icons (never while the sidebar is a drawer). */
  collapsed: boolean;
  setCollapsed: (v: boolean) => void;
  /** Collapses or expands the rail; opens or closes the drawer on a narrow window. */
  toggle: () => void;
  sidebarId: string;
  /** The window is narrow: the sidebar is a drawer. */
  narrow: boolean;
  drawerOpen: boolean;
  setDrawerOpen: (open: boolean) => void;
};

const ShellContext = createContext<ShellState | null>(null);

/** Sidebar state, for custom toggles. Returns null outside an AppShell. */
export function useAppShell(): ShellState | null {
  return useContext(ShellContext);
}

export type AppShellProps = {
  sidebar?: ReactNode;
  topbar?: ReactNode;
  children: ReactNode;
  collapsed?: boolean;
  defaultCollapsed?: boolean;
  onCollapsedChange?: (collapsed: boolean) => void;
  /** Skip-link target id (default "main"). Set to null to omit the skip link. */
  skipTo?: string | null;
  /** Media query below which the sidebar is a drawer (default SIDEBAR_DRAWER_QUERY, 600px); null keeps the rail. */
  drawerQuery?: string | null;
  /** Accessible name of the drawer (default "Navigation"). */
  drawerLabel?: string;
  className?: string;
};

export function AppShell({
  sidebar,
  topbar,
  children,
  collapsed: controlled,
  defaultCollapsed = false,
  onCollapsedChange,
  skipTo = "main",
  drawerQuery = SIDEBAR_DRAWER_QUERY,
  drawerLabel = "Navigation",
  className,
}: AppShellProps) {
  const [uncontrolled, setUncontrolled] = useState(defaultCollapsed);
  const narrow = useMediaQuery(drawerQuery ?? "not all") && drawerQuery !== null;
  const [drawerOpen, setDrawerOpen] = useState(false);
  // Widening the window closes the drawer; the rail takes over.
  useEffect(() => {
    if (!narrow) setDrawerOpen(false);
  }, [narrow]);
  const collapsed = !narrow && (controlled ?? uncontrolled);
  const sidebarId = useId();
  const setCollapsed = useCallback(
    (v: boolean) => {
      if (controlled === undefined) setUncontrolled(v);
      onCollapsedChange?.(v);
    },
    [controlled, onCollapsedChange],
  );
  const value = useMemo(
    () => ({
      collapsed,
      setCollapsed,
      toggle: narrow ? () => setDrawerOpen((o) => !o) : () => setCollapsed(!collapsed),
      sidebarId,
      narrow,
      drawerOpen,
      setDrawerOpen,
    }),
    [collapsed, setCollapsed, sidebarId, narrow, drawerOpen],
  );
  return (
    <ShellContext.Provider value={value}>
      {skipTo && <SkipLink href={`#${skipTo}`} />}
      <div className={cx(styles.shell, className)} data-collapsed={dataFlag(collapsed)} data-narrow={dataFlag(narrow)}>
        {narrow ? (
          <SidebarDrawer open={drawerOpen} onOpenChange={setDrawerOpen} label={drawerLabel} contentId={skipTo}>
            {sidebar}
          </SidebarDrawer>
        ) : (
          sidebar
        )}
        <div className={styles.column}>
          {topbar}
          {children}
        </div>
      </div>
    </ShellContext.Provider>
  );
}

export type SkipLinkProps = { href?: string; children?: ReactNode };

/** Visually hidden until focused; jumps to the main content. */
export function SkipLink({ href = "#main", children = "Skip to content" }: SkipLinkProps) {
  return (
    <a href={href} className={styles.skipLink}>
      {children}
    </a>
  );
}

/* ---------------- Sidebar ---------------- */

export type SidebarProps = {
  children: ReactNode;
  /** Names the sidebar landmark (default "Sidebar"). Give each sidebar on a page its own name. */
  label?: string;
  className?: string;
};

/** The app sidebar: an <aside> landmark holding the header, scrolling content and footer. */
export function Sidebar({ children, label = "Sidebar", className }: SidebarProps) {
  const shell = useAppShell();
  return (
    <aside id={shell?.sidebarId} aria-label={label} className={cx(styles.sidebar, className)} data-collapsed={dataFlag(shell?.collapsed)}>
      {children}
    </aside>
  );
}

export function SidebarHeader({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx(styles.sidebarHeader, className)}>{children}</div>;
}

export type SidebarContentProps = {
  children: ReactNode;
  /** Names the scroll region when the content overflows (default "Sidebar navigation"). */
  scrollLabel?: string;
  className?: string;
};

const currentSelector = '[aria-current="page"]';

/** The element that scrolls `el` (the ScrollArea's viewport). */
function scrollParent(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p; p = p.parentElement) {
    const style = getComputedStyle(p);
    // jsdom leaves overflow-y at "visible" for an inline `overflow: scroll`; read the shorthand then.
    const overflow = style.overflowY === "visible" ? style.overflow : style.overflowY;
    if (overflow === "auto" || overflow === "scroll") return p;
  }
  return null;
}

/**
 * Keeps the current page's item fully in view: on mount, when another item
 * becomes the current page, when the section holding it opens, and when the
 * layout changes while it was in view (items that load late above it, the
 * rail collapsing, the window getting shorter). Once the person scrolls it
 * out of view, layout changes leave the scroll position alone. A visible
 * item stays where it is.
 *
 * It scrolls the viewport itself rather than calling scrollIntoView(): Chrome
 * moves the sequential focus starting point to an element scrolled into
 * view, so the first Tab after a page load would land after the current item
 * instead of on the skip link.
 */
function useRevealCurrent() {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const root = ref.current;
    const viewport = root && scrollParent(root);
    if (!root || !viewport) return;
    const offsets = () => {
      const item = root.querySelector<HTMLElement>(currentSelector);
      if (!item) return null;
      const v = viewport.getBoundingClientRect();
      const r = item.getBoundingClientRect();
      return { above: v.top - r.top, below: r.bottom - v.bottom };
    };
    // Whether the current item was in view after the last reveal or scroll.
    let pinned = true;
    const reveal = () => {
      const o = offsets();
      if (!o) return;
      if (o.above > 0.5) viewport.scrollTop -= o.above;
      // Down far enough to show its bottom edge, but never past its top.
      else if (o.below > 0.5) viewport.scrollTop += Math.min(o.below, -o.above);
      pinned = true;
    };
    const onScroll = () => {
      const o = offsets();
      pinned = !o || (o.above <= 0.5 && o.below <= 0.5);
    };
    reveal();
    viewport.addEventListener("scroll", onScroll, { passive: true });
    const ro =
      typeof ResizeObserver === "undefined"
        ? null
        : new ResizeObserver(() => {
            if (pinned) reveal();
          });
    ro?.observe(viewport);
    ro?.observe(root);
    const mo =
      typeof MutationObserver === "undefined"
        ? null
        : new MutationObserver((records) => {
            const moved = records.some((r) => {
              const t = r.target as HTMLElement;
              if (r.attributeName === "aria-current") return t.getAttribute("aria-current") === "page";
              return !t.hidden && t.querySelector(currentSelector) !== null;
            });
            if (moved) reveal();
          });
    mo?.observe(root, { subtree: true, attributes: true, attributeFilter: ["aria-current", "hidden"] });
    return () => {
      viewport.removeEventListener("scroll", onScroll);
      ro?.disconnect();
      mo?.disconnect();
    };
  }, []);
  return ref;
}

/** The sidebar's middle part: grows to fill the sidebar, scrolls on its own and keeps the current page's item in view. */
export function SidebarContent({ children, scrollLabel = "Sidebar navigation", className }: SidebarContentProps) {
  const ref = useRevealCurrent();
  return (
    <ScrollArea label={scrollLabel} className={styles.sidebarScroll} contentClassName={cx(styles.sidebarContent, className)}>
      <div ref={ref}>{children}</div>
    </ScrollArea>
  );
}

export function SidebarFooter({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx(styles.sidebarFooter, className)}>{children}</div>;
}

export type SidebarNavProps = { "aria-label": string; children: ReactNode; className?: string };

/** The navigation landmark inside the sidebar. */
export function SidebarNav({ children, className, ...props }: SidebarNavProps) {
  return (
    <nav aria-label={props["aria-label"]} className={cx(styles.nav, className)}>
      {children}
    </nav>
  );
}

type SidebarDrawerProps = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  label: string;
  /** The main content's id (the skip-link target): focus goes there after following a link. */
  contentId: string | null;
  children: ReactNode;
};

/** The drawer that holds the sidebar on a narrow window. */
function SidebarDrawer({ open, onOpenChange, label, contentId, children }: SidebarDrawerProps) {
  const followed = useRef(false);
  // Following a link (a page in the sidebar, or a link in one of its menus) closes the drawer.
  // Router links cancel the browser's navigation (defaultPrevented) to do their own, so that isn't checked;
  // a modified click opens a new tab and leaves the drawer open.
  const onClick = (e: MouseEvent) => {
    const link = (e.target as Element).closest?.("a[href]");
    if (!link || e.metaKey || e.ctrlKey || e.shiftKey) return;
    followed.current = true;
    onOpenChange(false);
  };
  // After following a link, focus goes to the new page's content itself (the main landmark, not its first
  // control, which Base UI would pick for an element that isn't a tab stop), not back to the toggle, whose
  // page is gone. Escape, the backdrop and the close button return it to the toggle. Base UI may ask more
  // than once while closing, so the flag is only reset when the drawer opens again.
  const finalFocus = () => {
    const content = followed.current && contentId ? document.getElementById(contentId) : null;
    if (!content) return true;
    queueMicrotask(() => content.focus({ preventScroll: true }));
    return false;
  };
  if (open && followed.current) followed.current = false;
  return (
    <BaseDialog.Root open={open} onOpenChange={(o) => onOpenChange(o)}>
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className={styles.drawerBackdrop} />
        <BaseDialog.Popup className={styles.drawerPopup} aria-label={label} onClick={onClick} finalFocus={finalFocus}>
          {children}
          <BaseDialog.Close className={styles.drawerClose} aria-label={`Close ${label.toLowerCase()}`}>
            <X aria-hidden />
          </BaseDialog.Close>
        </BaseDialog.Popup>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

export type SidebarSectionProps = {
  label?: ReactNode;
  children: ReactNode;
  className?: string;
  /**
   * The label is a button that shows or hides the items (needs a label).
   * On the collapsed icon rail every item shows. Keep the section of the
   * current page open: its items stay hidden otherwise.
   */
  collapsible?: boolean;
  /** Open state of a collapsible section (controlled). */
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
};

/** A labelled list of items. The label is small muted uppercase text, or a disclosure button when `collapsible`. */
export function SidebarSection({ label, children, className, collapsible, open: controlled, defaultOpen = true, onOpenChange }: SidebarSectionProps) {
  const id = useId();
  const listId = useId();
  const shell = useAppShell();
  const [uncontrolled, setUncontrolled] = useState(defaultOpen);
  const railed = shell?.collapsed ?? false;
  const toggles = Boolean(collapsible && label) && !railed;
  const open = !toggles || (controlled ?? uncontrolled);
  const setOpen = (o: boolean) => {
    if (controlled === undefined) setUncontrolled(o);
    onOpenChange?.(o);
  };
  return (
    <div className={cx(styles.section, className)} data-collapsible={dataFlag(toggles)}>
      {label &&
        (toggles ? (
          <button type="button" id={id} className={styles.sectionToggle} aria-expanded={open} aria-controls={listId} onClick={() => setOpen(!open)}>
            <span className={styles.sectionToggleText}>{label}</span>
            <ChevronRight aria-hidden className={styles.sectionChevron} />
          </button>
        ) : (
          <p id={id} className={cx(styles.sectionLabel, railed && "sr-only")}>
            {label}
          </p>
        ))}
      <ul id={listId} aria-labelledby={label ? id : undefined} className={styles.items} hidden={!open}>
        {children}
      </ul>
    </div>
  );
}

export type SidebarItemProps = {
  label: ReactNode;
  /** Decorative icon (aria-hidden). */
  icon?: ReactNode;
  href?: string;
  /** Router link: `render={<Link to="/admin/users" />}` (sets aria-current itself). */
  render?: useRender.RenderProp;
  /** Mark as the current page (plain links). */
  current?: boolean;
  /** Trailing count or badge. */
  trailing?: ReactNode;
  /** A small "new" dot in --color-highlight (decorative; say "new" in the label if it matters). */
  dot?: boolean;
  /** A second, muted line under the label (e.g. the agent of a conversation), so items with the same label can be told apart. */
  description?: ReactNode;
  className?: string;
};

export function SidebarItem({ label, icon, href, render, current, trailing, dot, description, className }: SidebarItemProps) {
  const shell = useAppShell();
  const collapsed = shell?.collapsed ?? false;
  const textual = (x: ReactNode) => typeof x === "string" || typeof x === "number";
  const spoken = description && textual(label) && textual(description) ? `${label}, ${description}` : undefined;
  const link = useRender({
    render,
    defaultTagName: "a",
    props: {
      href,
      ...(current ? { "aria-current": "page" } : {}),
      className: cx(styles.item, className),
      "data-description": dataFlag(Boolean(description) && !collapsed),
      children: (
        <>
          {icon && <span className={styles.itemIcon}>{icon}</span>}
          {description ? (
            <span className={cx(styles.itemText, collapsed && "sr-only")}>
              {spoken && (
                // Text labels are read as one text ("Question?, Agent"): split over the two lines, browsers
                // join the parts with a space ("Question? , Agent").
                <span className="sr-only">{spoken}</span>
              )}
              <span className={styles.itemLabel} aria-hidden={spoken ? true : undefined}>
                {label}
              </span>
              <span className={styles.itemDescription} aria-hidden={spoken ? true : undefined}>
                {!spoken && <span className="sr-only">, </span>}
                {description}
              </span>
            </span>
          ) : (
            <span className={cx(styles.itemLabel, collapsed && "sr-only")}>{label}</span>
          )}
          {dot && <span aria-hidden className={styles.newDot} />}
          {trailing && !collapsed && <span className={styles.itemTrailing}>{trailing}</span>}
        </>
      ),
    },
  });
  return (
    <li className={styles.itemRow}>
      {collapsed && typeof label === "string" ? (
        <Tooltip content={label} side="right">
          {link}
        </Tooltip>
      ) : (
        link
      )}
    </li>
  );
}

/** Collapses/expands the sidebar, or opens the drawer on a narrow window. Place in the TopBar or SidebarFooter. */
export function SidebarToggle({ className }: { className?: string }) {
  const shell = useAppShell();
  if (!shell) return null;
  const label = shell.narrow ? (shell.drawerOpen ? "Close navigation" : "Open navigation") : shell.collapsed ? "Expand sidebar" : "Collapse sidebar";
  return (
    <IconButton
      size="sm"
      icon={<PanelLeft aria-hidden />}
      label={label}
      aria-expanded={shell.narrow ? shell.drawerOpen : !shell.collapsed}
      aria-controls={shell.sidebarId}
      onClick={shell.toggle}
      className={className}
    />
  );
}

/* ---------------- Mode switch ---------------- */

export type SidebarModeItem = {
  label: string;
  /** Decorative icon (aria-hidden). */
  icon: ReactNode;
  /** Router link: `render={<Link to="/admin" />}`, or a plain href. */
  render?: useRender.RenderProp;
  href?: string;
  current: boolean;
};

/**
 * A segmented switch between top-level modes of the app (e.g. Workspace and
 * Admin). Each option is a link, so it's a small navigation landmark with
 * aria-current on the active mode, not a toggle. Collapsed: stacked icons
 * with tooltips.
 */
export function SidebarModeSwitch({ label, items, className }: { label: string; items: SidebarModeItem[]; className?: string }) {
  const shell = useAppShell();
  const collapsed = shell?.collapsed ?? false;
  return (
    <nav aria-label={label} className={cx(styles.modeSwitch, className)}>
      {items.map((item) => (
        <ModeLink key={item.label} item={item} collapsed={collapsed} />
      ))}
    </nav>
  );
}

function ModeLink({ item, collapsed }: { item: SidebarModeItem; collapsed: boolean }) {
  const link = useRender({
    render: item.render,
    defaultTagName: "a",
    props: {
      href: item.href,
      "aria-current": item.current ? "page" : undefined,
      className: styles.modeOption,
      children: (
        <>
          <span className={styles.modeIcon}>{item.icon}</span>
          <span className={cx(collapsed && "sr-only")}>{item.label}</span>
        </>
      ),
    },
  });
  return collapsed ? (
    <Tooltip content={item.label} side="right">
      {link}
    </Tooltip>
  ) : (
    link
  );
}

/* ---------------- Brand, workspace switcher, user ---------------- */

export type BrandProps = {
  name: ReactNode;
  href?: string;
  /** Your logo (decorative; it is hidden from assistive tech because `name` is the link text). Defaults to a primary tile with a highlight accent. */
  logo?: ReactNode;
  render?: useRender.RenderProp;
  className?: string;
};

/** Product mark and name. The mark uses --color-primary and --color-highlight unless you pass `logo`. */
export function Brand({ name, href = "/", logo, render, className }: BrandProps) {
  const shell = useAppShell();
  return useRender({
    render,
    defaultTagName: "a",
    props: {
      href,
      className: cx(styles.brand, className),
      children: (
        <>
          {logo ? (
            <span aria-hidden className={styles.brandLogo}>
              {logo}
            </span>
          ) : (
            <span aria-hidden className={styles.brandMark}>
              <span className={styles.brandAccent} />
            </span>
          )}
          <span className={cx(styles.brandName, shell?.collapsed && "sr-only")}>{name}</span>
        </>
      ),
    },
  });
}

export type WorkspaceSwitcherProps = {
  /** Current workspace/team name. */
  name: string;
  /** Secondary line, e.g. the user's role. */
  description?: ReactNode;
  /** Menu content: MenuItem / MenuLinkItem / MenuGroup / MenuSeparator. */
  children: ReactNode;
  className?: string;
};

/** Sidebar-top dropdown for switching team/workspace. */
export function WorkspaceSwitcher({ name, description, children, className }: WorkspaceSwitcherProps) {
  const shell = useAppShell();
  const collapsed = shell?.collapsed ?? false;
  return (
    <Menu
      align="start"
      width="trigger"
      trigger={
        <button type="button" className={cx(styles.switcher, className)}>
          <Avatar name={name} shape="square" size="sm" decorative />
          <span className={cx(styles.switcherText, collapsed && "sr-only")}>
            <span className="sr-only">Current workspace: </span>
            <span className={styles.switcherName}>{name}</span>
            {description && <span className={styles.switcherDescription}>{description}</span>}
          </span>
          {!collapsed && <ChevronsUpDown aria-hidden className={styles.switcherChevron} />}
        </button>
      }
    >
      {children}
    </Menu>
  );
}

export type SidebarUserProps = {
  name: string;
  email?: string;
  avatarSrc?: string;
  /** Menu content, e.g. a Sign out MenuItem. */
  children: ReactNode;
  className?: string;
};

/** Sidebar-bottom account button with a dropdown menu. */
export function SidebarUser({ name, email, avatarSrc, children, className }: SidebarUserProps) {
  const shell = useAppShell();
  const collapsed = shell?.collapsed ?? false;
  return (
    <Menu
      side="top"
      align="start"
      width="trigger"
      trigger={
        <button type="button" className={cx(styles.switcher, className)}>
          <Avatar name={name} src={avatarSrc} size="sm" decorative />
          <span className={cx(styles.switcherText, collapsed && "sr-only")}>
            <span className="sr-only">Account: </span>
            <span className={styles.switcherName}>{name}</span>
            {email && <span className={styles.switcherDescription}>{email}</span>}
          </span>
          {!collapsed && <ChevronsUpDown aria-hidden className={styles.switcherChevron} />}
        </button>
      }
    >
      {children}
    </Menu>
  );
}

/* ---------------- Top bar and main ---------------- */

export type TopBarProps = {
  /** Left side, e.g. <Breadcrumbs />. */
  start?: ReactNode;
  /** Right side, e.g. <CommandPaletteTrigger />, actions. */
  end?: ReactNode;
  /** Show the sidebar toggle at the start (default true inside AppShell). */
  sidebarToggle?: boolean;
  className?: string;
};

/** The top bar (<header>, banner landmark). */
export function TopBar({ start, end, sidebarToggle = true, className }: TopBarProps) {
  return (
    <header className={cx(styles.topbar, className)}>
      <div className={styles.topbarStart}>
        {sidebarToggle && <SidebarToggle />}
        {start}
      </div>
      {end && <div className={styles.topbarEnd}>{end}</div>}
    </header>
  );
}

export type MainProps = {
  children: ReactNode;
  id?: string;
  /** Wrap content in the 1200px Container (default true). */
  contained?: boolean;
  /** Render another element (e.g. `<div />` when embedding a demo inside a page's <main>). */
  render?: ReactElement;
  className?: string;
};

/** The main landmark and skip-link target. */
export function Main({ children, id = "main", contained = true, render, className }: MainProps) {
  return useRender({
    render,
    defaultTagName: "main",
    props: {
      id,
      tabIndex: -1,
      className: cx(styles.main, className),
      children: contained ? <Container>{children}</Container> : children,
    },
  });
}

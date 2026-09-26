"use client";

import { useRender } from "@base-ui/react/use-render";
import { ChevronsUpDown, PanelLeft } from "lucide-react";
import { createContext, type ReactElement, type ReactNode, useCallback, useContext, useId, useMemo, useState } from "react";
import { Avatar } from "@/components/ui/avatar/avatar";
import { IconButton } from "@/components/ui/button/button";
import { Container } from "@/components/ui/layout/layout";
import { Menu } from "@/components/ui/menu/menu";
import { Tooltip } from "@/components/ui/tooltip/tooltip";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./app-shell.module.css";

/*
 * Generic, unopinionated app-shell primitives:
 *
 *   <AppShell sidebar={<Sidebar>…</Sidebar>} topbar={<TopBar start={…} end={…} />}>
 *     <Main>…page…</Main>
 *   </AppShell>
 *
 * - AppShell renders a skip link to #main and owns the sidebar collapsed state.
 * - Sidebar: SidebarHeader (Brand, WorkspaceSwitcher), SidebarContent
 *   (SidebarNav > SidebarSection > SidebarItem), SidebarFooter (SidebarUser).
 * - SidebarItem marks the current page with aria-current="page" (router
 *   links set it automatically). When collapsed, labels become visually
 *   hidden (still announced) and show as tooltips.
 * - Main is the <main id="main"> skip-link target, with a max-width container.
 */

type ShellState = { collapsed: boolean; setCollapsed: (v: boolean) => void; toggle: () => void; sidebarId: string };

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
  className,
}: AppShellProps) {
  const [uncontrolled, setUncontrolled] = useState(defaultCollapsed);
  const collapsed = controlled ?? uncontrolled;
  const sidebarId = useId();
  const setCollapsed = useCallback(
    (v: boolean) => {
      if (controlled === undefined) setUncontrolled(v);
      onCollapsedChange?.(v);
    },
    [controlled, onCollapsedChange],
  );
  const value = useMemo(
    () => ({ collapsed, setCollapsed, toggle: () => setCollapsed(!collapsed), sidebarId }),
    [collapsed, setCollapsed, sidebarId],
  );
  return (
    <ShellContext.Provider value={value}>
      {skipTo && <SkipLink href={`#${skipTo}`} />}
      <div className={cx(styles.shell, className)} data-collapsed={dataFlag(collapsed)}>
        {sidebar}
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

export type SidebarProps = { children: ReactNode; className?: string };

export function Sidebar({ children, className }: SidebarProps) {
  const shell = useAppShell();
  return (
    <div id={shell?.sidebarId} className={cx(styles.sidebar, className)} data-collapsed={dataFlag(shell?.collapsed)}>
      {children}
    </div>
  );
}

export function SidebarHeader({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx(styles.sidebarHeader, className)}>{children}</div>;
}

export function SidebarContent({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx(styles.sidebarContent, className)}>{children}</div>;
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

export type SidebarSectionProps = { label?: ReactNode; children: ReactNode; className?: string };

/** A labelled list of items. The label is small muted uppercase text. */
export function SidebarSection({ label, children, className }: SidebarSectionProps) {
  const id = useId();
  const shell = useAppShell();
  return (
    <div className={cx(styles.section, className)}>
      {label && (
        <p id={id} className={cx(styles.sectionLabel, shell?.collapsed && "sr-only")}>
          {label}
        </p>
      )}
      <ul aria-labelledby={label ? id : undefined} className={styles.items}>
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
  className?: string;
};

export function SidebarItem({ label, icon, href, render, current, trailing, dot, className }: SidebarItemProps) {
  const shell = useAppShell();
  const collapsed = shell?.collapsed ?? false;
  const link = useRender({
    render,
    defaultTagName: "a",
    props: {
      href,
      ...(current ? { "aria-current": "page" } : {}),
      className: cx(styles.item, className),
      children: (
        <>
          {icon && <span className={styles.itemIcon}>{icon}</span>}
          <span className={cx(styles.itemLabel, collapsed && "sr-only")}>{label}</span>
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

/** Collapses/expands the sidebar. Place in the TopBar or SidebarFooter. */
export function SidebarToggle({ className }: { className?: string }) {
  const shell = useAppShell();
  if (!shell) return null;
  return (
    <IconButton
      size="sm"
      icon={<PanelLeft aria-hidden />}
      label={shell.collapsed ? "Expand sidebar" : "Collapse sidebar"}
      aria-expanded={!shell.collapsed}
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

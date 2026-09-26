"use client";

import { Tabs as BaseTabs } from "@base-ui/react/tabs";
import { useRender } from "@base-ui/react/use-render";
import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./tabs.module.css";

/*
 * Tabs (Base UI) for in-page panels, and NavTabs for route navigation.
 *
 * Use Tabs when switching panels on the same page (role="tablist"/"tab"/
 * "tabpanel", arrow-key navigation). Use NavTabs when each tab is a URL: it
 * renders a <nav> of links and marks the current one with aria-current="page"
 * (TanStack Router's <Link> sets that automatically when active).
 */

export type TabsVariant = "underline" | "pills";

export type TabsProps = Omit<BaseTabs.Root.Props, "className"> & { className?: string };

export function Tabs({ className, ...props }: TabsProps) {
  return <BaseTabs.Root {...props} className={cx(styles.root, className)} />;
}

export type TabsListProps = Omit<BaseTabs.List.Props, "className"> & {
  className?: string;
  variant?: TabsVariant;
};

export function TabsList({ className, variant = "underline", children, ...props }: TabsListProps) {
  return (
    <BaseTabs.List {...props} data-variant={variant} className={cx(styles.list, className)}>
      {children}
      <BaseTabs.Indicator className={styles.indicator} data-variant={variant} />
    </BaseTabs.List>
  );
}

export type TabProps = Omit<BaseTabs.Tab.Props, "className"> & {
  className?: string;
  icon?: ReactNode;
  /** Small count shown after the label. */
  count?: number;
};

export function Tab({ className, icon, count, children, ...props }: TabProps) {
  return (
    <BaseTabs.Tab {...props} className={cx(styles.tab, className)}>
      {icon}
      {children}
      {count !== undefined && <span className={styles.count}>{count}</span>}
    </BaseTabs.Tab>
  );
}

export type TabsPanelProps = Omit<BaseTabs.Panel.Props, "className"> & { className?: string };

export function TabsPanel({ className, ...props }: TabsPanelProps) {
  return <BaseTabs.Panel {...props} className={cx(styles.panel, className)} />;
}

/* ------------------------------------------------------------------ */

export type NavTabsProps = ComponentPropsWithRef<"nav"> & {
  /** Accessible name of the navigation landmark, e.g. "Team". */
  "aria-label": string;
};

/** A link-based tab bar for route navigation. */
export function NavTabs({ className, children, ...props }: NavTabsProps) {
  return (
    <nav {...props} className={cx(styles.nav, className)}>
      <ul className={styles.navList}>{children}</ul>
    </nav>
  );
}

export type NavTabProps = Omit<ComponentPropsWithRef<"a">, "className"> & {
  className?: string;
  /** Mark as the current page (sets aria-current="page"). Router links set this themselves. */
  current?: boolean;
  icon?: ReactNode;
  count?: number;
  /** e.g. `render={<Link to="/teams/$team/sources" params={{ team }} />}` */
  render?: useRender.RenderProp;
};

export function NavTab({ className, current, icon, count, render, ref, children, ...props }: NavTabProps) {
  const link = useRender({
    render,
    defaultTagName: "a",
    ref,
    props: {
      ...(current ? { "aria-current": "page" } : {}),
      ...props,
      className: cx(styles.navTab, className),
      children: (
        <>
          {icon}
          {children}
          {count !== undefined && <span className={styles.count}>{count}</span>}
        </>
      ),
    },
  });
  return <li className={styles.navItem}>{link}</li>;
}

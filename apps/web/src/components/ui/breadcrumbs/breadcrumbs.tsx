"use client";

import { useRender } from "@base-ui/react/use-render";
import { ChevronRight } from "lucide-react";
import { type ReactNode, type RefObject, useLayoutEffect, useRef } from "react";
import { Menu, MenuLinkItem, type MenuLinkItemProps } from "@/components/ui/menu/menu";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./breadcrumbs.module.css";

/*
 * A breadcrumb trail. A long trail can collapse its middle into one "…"
 * item that opens a menu of the hidden crumbs:
 *
 *   <Breadcrumbs items={[
 *     { label: "Acme", href: "/" },
 *     { label: "Show 3 hidden levels", collapsed: [{ label: "Projects", href: "/p" }, { label: "Web", href: "/p/web" }, { label: "Releases", href: "/p/web/r" }] },
 *     { label: "v2.1" },
 *   ]} />
 *
 * The collapsed item's `label` is the menu button's accessible name (it
 * shows "…"); its title lists the hidden crumbs as a path.
 *
 * The trail stays on one line. While it fits, every crumb shows in full;
 * when it doesn't (a long record name, a phone), the current page is cut
 * first down to a readable floor, then the crumbs before it down to theirs,
 * with an ellipsis; a text label shows in full on hover (`title`).
 * `wrap` lets the trail wrap onto more lines instead.
 */

export type BreadcrumbLink = {
  label: ReactNode;
  href?: string;
  /** Router link, e.g. `<Link to="/teams/$team" params={{ team }} />`. */
  render?: useRender.RenderProp;
  icon?: ReactNode;
};

export type BreadcrumbItem = BreadcrumbLink & {
  /** Crumbs hidden behind this item: it renders as a "…" button opening a menu of these links, named by `label`. */
  collapsed?: BreadcrumbLink[];
};

export type BreadcrumbsProps = {
  items: BreadcrumbItem[];
  /** Landmark name (default "Breadcrumb"). */
  label?: string;
  /** Wrap onto more lines when the trail is too long, instead of cutting crumbs with an ellipsis (default false). */
  wrap?: boolean;
  className?: string;
};

/** The full text of a crumb as its tooltip, for when it's cut with an ellipsis. */
const titleOf = (item: BreadcrumbLink) => (typeof item.label === "string" ? item.label : undefined);

function Crumb({ item }: { item: BreadcrumbLink }) {
  return useRender({
    render: item.render,
    defaultTagName: "a",
    props: {
      href: item.href,
      title: titleOf(item),
      className: styles.link,
      children: (
        <>
          {item.icon}
          {item.label}
        </>
      ),
    },
  });
}

/** The hidden crumbs as a path ("Projects › Web › Releases") when their labels are text. */
function pathTitle(items: BreadcrumbLink[]) {
  const labels = items.map((i) => (typeof i.label === "string" ? i.label : ""));
  return labels.every(Boolean) ? labels.join(" › ") : undefined;
}

/** "…": a menu button listing the hidden crumbs, in order. */
function Collapsed({ item }: { item: BreadcrumbItem & { collapsed: BreadcrumbLink[] } }) {
  return (
    <Menu
      trigger={
        <button type="button" className={styles.more} title={pathTitle(item.collapsed)}>
          <span aria-hidden>…</span>
          <span className={styles.srOnly}>{item.label}</span>
        </button>
      }
    >
      {item.collapsed.map((c, i) => (
        // A crumb's render is an element (a router link) or a render function taking link props, which a menu link item accepts too.
        <MenuLinkItem key={i} href={c.href} render={c.render as MenuLinkItemProps["render"]} icon={c.icon}>
          {c.label}
        </MenuLinkItem>
      ))}
    </Menu>
  );
}

/**
 * Each crumb's full width (`--crumb-width`), the floor it may be cut to when
 * shorter than 5.5rem: CSS can't say "5.5rem or my own width, whichever is
 * smaller" for a nowrap text. Measured again when the trail's size changes.
 */
function useCrumbWidths(list: RefObject<HTMLOListElement | null>, key: string) {
  useLayoutEffect(() => {
    const ol = list.current;
    if (!ol) return;
    const measure = () => {
      for (const li of Array.from(ol.children) as HTMLElement[]) {
        const text = li.firstElementChild as HTMLElement | null;
        if (!text) continue;
        // The item's width with its text uncut: the text clips its overflow, so scrollWidth is its full width.
        const full = li.getBoundingClientRect().width - text.clientWidth + text.scrollWidth;
        if (full > 0) li.style.setProperty("--crumb-width", `${Math.ceil(full)}px`);
      }
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(ol);
    return () => ro.disconnect();
  }, [list, key]);
}

/** A trail of links; the last item is the current page (aria-current="page"). */
export function Breadcrumbs({ items, label = "Breadcrumb", wrap = false, className }: BreadcrumbsProps) {
  const list = useRef<HTMLOListElement>(null);
  useCrumbWidths(list, items.map((i) => (typeof i.label === "string" ? i.label : "·")).join("\u0000"));
  return (
    <nav aria-label={label} className={cx(styles.nav, className)} data-wrap={dataFlag(wrap)}>
      <ol ref={list} className={styles.list}>
        {items.map((item, i) => {
          const last = i === items.length - 1;
          return (
            <li key={i} className={styles.item}>
              {item.collapsed && item.collapsed.length > 0 && !last ? (
                <Collapsed item={{ ...item, collapsed: item.collapsed }} />
              ) : last ? (
                <span aria-current="page" className={styles.current} title={titleOf(item)}>
                  {item.icon}
                  {item.label}
                </span>
              ) : (
                <Crumb item={item} />
              )}
              {!last && <ChevronRight aria-hidden className={styles.separator} />}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}

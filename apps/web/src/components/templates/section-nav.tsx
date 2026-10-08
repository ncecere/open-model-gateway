/*
 * SectionNav: sticky in-page anchors for one long page (model detail: Routes,
 * Pricing, Data policy, Protocols…) with scroll-spy. A <nav> of real "#id"
 * links (open-in-new-tab and copy link keep working); the section in view gets
 * aria-current="location". Clicking scrolls (instantly under reduced motion)
 * and moves focus to the section so keyboard and screen-reader users land
 * there. A vertical list beside the content on wide screens; a horizontally
 * scrolling sticky row at 390px. Sections need matching `id`s.
 *
 *   <SectionNav label="On this page" items={[{ id: "routes", label: "Routes" }, { id: "pricing", label: "Pricing" }]} />
 */
import { type MouseEvent, type ReactNode, useEffect, useState } from "react";
import { cx } from "../../lib/bitop-utils";
import styles from "./section-nav.module.css";

export type SectionNavItem = { id: string; label: string; icon?: ReactNode };

/** The id of the first section (in order) whose top is in the upper part of the viewport; the first id until then. */
export function useScrollSpy(ids: string[], rootMargin = "-15% 0px -70% 0px"): [string | undefined, (id: string) => void] {
  const [active, setActive] = useState<string | undefined>(ids[0]);
  const key = ids.join("|");
  useEffect(() => {
    if (typeof IntersectionObserver === "undefined") return;
    const visible = new Set<string>();
    const observer = new IntersectionObserver(entries => {
      for (const entry of entries) {
        if (entry.isIntersecting) visible.add(entry.target.id);
        else visible.delete(entry.target.id);
      }
      const first = ids.find(id => visible.has(id));
      if (first) setActive(first);
    }, { rootMargin });
    for (const id of ids) {
      const el = document.getElementById(id);
      if (el) observer.observe(el);
    }
    return () => observer.disconnect();
  }, [key, rootMargin]); // `key` stands for `ids`
  return [active, setActive];
}

export type SectionNavProps = {
  /** Accessible name of the navigation landmark (default "On this page"). */
  label?: string;
  items: SectionNavItem[];
  className?: string;
};

export function SectionNav({ label = "On this page", items, className }: SectionNavProps) {
  const [active, setActive] = useScrollSpy(items.map(i => i.id));
  function go(event: MouseEvent<HTMLAnchorElement>, id: string) {
    const target = document.getElementById(id);
    if (!target || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) return;
    event.preventDefault();
    const reduce = typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    target.scrollIntoView?.({ behavior: reduce ? "auto" : "smooth", block: "start" });
    if (!target.hasAttribute("tabindex")) target.setAttribute("tabindex", "-1");
    target.focus({ preventScroll: true });
    setActive(id);
  }
  return (
    <nav aria-label={label} className={cx(styles.root, className)}>
      <ul className={styles.list}>
        {items.map(item => (
          <li key={item.id} className={styles.item}>
            <a href={`#${item.id}`} className={styles.link} aria-current={active === item.id ? "location" : undefined} onClick={event => go(event, item.id)}>
              {item.icon && <span aria-hidden className={styles.icon}>{item.icon}</span>}
              {item.label}
            </a>
          </li>
        ))}
      </ul>
    </nav>
  );
}

/** Page layout: the SectionNav column beside the content on wide screens, above it (sticky row) on a phone. */
export function SectionNavLayout({ nav, children }: { nav: ReactNode; children: ReactNode }) {
  return <div className={styles.layout}><div className={styles.navColumn}>{nav}</div><div className={styles.content}>{children}</div></div>;
}

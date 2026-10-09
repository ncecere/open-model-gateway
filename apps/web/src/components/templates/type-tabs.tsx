/*
 * TypeTabs: catalog type filter tabs with counts — "All 42 · Chat 30 ·
 * Embeddings 6 · Images 4 …" — using Bitop Tabs with the Grounded pill
 * variant (one sideways-scrolling strip with faded edges on a phone instead of
 * several wrapped rows; arrow keys move between tabs). Controlled with a
 * URL-friendly string value. An unknown count (null) shows no number rather
 * than 0. `children` is the content for the selected tab, rendered in its
 * tab panel. `end` sits after the tabs on the same line, e.g. the result
 * count ("14 models"), so it never takes a line of its own.
 *
 *   <TypeTabs label="Model type" value={type} onChange={setType}
 *     items={[{ value: "all", label: "All", count: 42 }, { value: "embeddings", label: "Embeddings", count: 6 }]}>
 *     <ModelList type={type} />
 *   </TypeTabs>
 */
import type { ReactNode } from "react";
import { useMediaQuery } from "../../lib/bitop-utils";
import { Tab, Tabs, TabsList, TabsPanel } from "../ui/tabs/tabs";
import { HEADER_COLLAPSE_QUERY } from "./header-actions";
import styles from "./templates.module.css";

export type TypeTabItem = { value: string; label: string; count?: number | null; icon?: ReactNode; disabled?: boolean };

export type TypeTabsProps = {
  /** Accessible name of the tab list. */
  label: string;
  items: TypeTabItem[];
  value: string;
  onChange: (value: string) => void;
  /** Content of the selected tab's panel. */
  children?: ReactNode;
  /** After the tabs on the same line, e.g. a result count. */
  end?: ReactNode;
  className?: string;
};

const known = (count: number | null | undefined): count is number => typeof count === "number" && Number.isFinite(count);

export function TypeTabs({ label, items, value, onChange, children, end, className }: TypeTabsProps) {
  const selected = items.some(i => i.value === value) ? value : items[0]?.value, narrow = useMediaQuery(HEADER_COLLAPSE_QUERY);
  return (
    <Tabs value={selected} onValueChange={next => { if (typeof next === "string" && next !== value) onChange(next); }} className={className}>
      {end !== undefined ? <div className={styles.tabsRow}>{list(items, label, narrow)}<span className={styles.tabsEnd}>{end}</span></div> : list(items, label, narrow)}
      {children !== undefined && selected !== undefined && <TabsPanel value={selected}>{children}</TabsPanel>}
    </Tabs>
  );
}

function list(items: TypeTabItem[], label: string, narrow: boolean) {
  return (
    <TabsList variant="pills" overflow={narrow ? "scroll" : "wrap"} aria-label={label}>
      {items.map(item => (
        // The accessible name is "Chat, 30" (not "Chat30" or "Chat , 30" from the separate count badge).
        <Tab key={item.value} value={item.value} icon={item.icon} disabled={item.disabled} count={known(item.count) ? item.count : undefined} aria-label={known(item.count) ? `${item.label}, ${item.count.toLocaleString("en-US")}` : undefined}>
          {item.label}
        </Tab>
      ))}
    </TabsList>
  );
}

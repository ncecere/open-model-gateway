"use client";

import { Autocomplete } from "@base-ui/react/autocomplete";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Search } from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";
import { KbdShortcut } from "@/components/ui/kbd/kbd";
import { cx } from "@/lib/bitop-utils";
import styles from "./command-palette.module.css";

/*
 * Command palette: Base UI Dialog + an inline Base UI Autocomplete (the
 * pattern from Base UI's "Command palette" example). Keyboard-first: type to
 * filter, ↑/↓ to move, Enter to run, Esc to close. Open it with ⌘K / Ctrl+K
 * via useCommandPaletteShortcut(). While typing, the best matches come first
 * (an exact name, then names starting with the text, then keywords), so
 * Enter runs the command that was named.
 */

export type Command = {
  id: string;
  label: string;
  /** Decorative icon. */
  icon?: ReactNode;
  /** Extra search terms. */
  keywords?: string[];
  /** Muted text on the right (e.g. "Team", "Page"). */
  hint?: ReactNode;
  /** Keys shown as a shortcut hint, e.g. ["G", "S"]. */
  shortcut?: string[];
  onSelect: () => void;
};

export type CommandGroup = {
  label: string;
  items: Command[];
  /**
   * Keep the items in the given order while typing (results a server search
   * already ranked). The group still moves up when it holds the best match.
   */
  keepOrder?: boolean;
};

export type CommandPaletteProps = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  groups: CommandGroup[];
  placeholder?: string;
  /** Accessible name of the dialog. */
  label?: string;
  emptyText?: ReactNode;
  /**
   * Where focus goes when the palette closes (Base UI Dialog.Popup
   * `finalFocus`). Defaults to the element that opened it. Pass a function
   * returning the new page's heading when a command navigates.
   */
  finalFocus?: BaseDialog.Popup.Props["finalFocus"];
  /** Class for the dialog popup, merged with the built-in styles (e.g. to scope token overrides). */
  className?: string;
  /**
   * The typed text, controlled: pass it with `onQueryChange` when results
   * depend on it (e.g. a server search as you type). Uncontrolled by default.
   * Reset it to "" when the palette closes.
   */
  query?: string;
  onQueryChange?: (query: string) => void;
};

type Group = { value: string; items: Command[] };

/** A typed word and its singular forms: "members" also looks for "member", "policies" for "policy", "searches" for "search". */
function wordForms(word: string): string[] {
  const forms = [word];
  if (word.length > 4 && word.endsWith("ies")) forms.push(word.slice(0, -3) + "y");
  if (word.length > 4 && word.endsWith("es")) forms.push(word.slice(0, -2));
  if (word.length > 3 && word.endsWith("s") && !word.endsWith("ss")) forms.push(word.slice(0, -1));
  return forms;
}

/**
 * Whether a command matches what was typed: every word of the query appears
 * in its label or keywords (case-insensitive), as a whole word, the start of
 * one ("memb") or anywhere inside, in the singular or the plural ("members"
 * finds "Add member").
 */
export function commandMatches(item: Pick<Command, "label" | "keywords">, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const haystack = [item.label, ...(item.keywords ?? [])].join(" ").toLowerCase();
  return q.split(/\s+/).every((word) => wordForms(word).some((form) => haystack.includes(form)));
}

const words = (text: string) => text.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);

/**
 * How well a matching command fits what was typed, best first: 0 its label is
 * the query ("legal holds" → Legal holds), 1 its label starts with it
 * ("budget" → Budgets), 2 every typed word starts a word of the label
 * ("migrations" → Profile migrations), 3 a keyword is the query, 4 any
 * other match (a word inside a keyword).
 */
export function commandRank(item: Pick<Command, "label" | "keywords">, query: string): number {
  const q = query.trim().toLowerCase().replace(/\s+/g, " ");
  if (!q) return 0;
  const label = item.label.trim().toLowerCase();
  const forms = wordForms(q);
  if (forms.includes(label)) return 0;
  if (label.startsWith(q)) return 1;
  const labelWords = words(label);
  const typed = words(q);
  if (typed.length > 0 && typed.every((w) => wordForms(w).some((f) => labelWords.some((l) => l.startsWith(f))))) return 2;
  if ((item.keywords ?? []).some((k) => forms.includes(k.trim().toLowerCase()))) return 3;
  return 4;
}

/**
 * The groups with the best matches first: items sorted by commandRank, and
 * groups by their best matching item (ties keep their order). A group with
 * `keepOrder` keeps its items' order. An exact name
 * comes first, so Enter runs it rather than a page that only lists the word
 * among its keywords.
 */
export function rankCommandGroups(groups: CommandGroup[], query: string): CommandGroup[] {
  if (!query.trim()) return groups;
  const ranked = groups.map((g, i) => {
    const items = g.items
      .map((item, j) => ({ item, j, rank: commandMatches(item, query) ? commandRank(item, query) : 5 }))
      .sort((a, b) => (g.keepOrder ? a.j - b.j : a.rank - b.rank || a.j - b.j));
    return { group: { ...g, items: items.map((x) => x.item) }, i, best: Math.min(5, ...items.map((x) => x.rank)) };
  });
  return ranked.sort((a, b) => a.best - b.best || a.i - b.i).map((x) => x.group);
}

export function CommandPalette({
  open,
  onOpenChange,
  groups,
  placeholder = "Search pages and actions…",
  label = "Command palette",
  emptyText = "No results found.",
  finalFocus,
  className,
  query,
  onQueryChange,
}: CommandPaletteProps) {
  const hintId = useId();
  const [ownQuery, setOwnQuery] = useState("");
  useEffect(() => {
    if (!open) setOwnQuery("");
  }, [open]);
  const text = query ?? ownQuery;
  // Best matches first (rankCommandGroups). The list stays the same object
  // while the groups and their order don't change, so typing or a parent's
  // re-render doesn't hand Autocomplete a new collection to re-index.
  const last = useRef<{ groups: CommandGroup[]; key: string; items: Group[] } | null>(null);
  const items: Group[] = useMemo(() => {
    const next = rankCommandGroups(groups, text)
      .filter((g) => g.items.length > 0)
      .map((g) => ({ value: g.label, items: g.items }));
    const key = next.map((g) => `${g.value}\n${g.items.map((i) => i.id).join("\n")}`).join("\n\n");
    if (last.current && last.current.groups === groups && last.current.key === key) return last.current.items;
    last.current = { groups, key, items: next };
    return next;
  }, [groups, text]);

  function run(cmd: Command) {
    onOpenChange(false);
    cmd.onSelect();
  }

  return (
    <BaseDialog.Root open={open} onOpenChange={(o) => onOpenChange(o)}>
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className={styles.backdrop} />
        <BaseDialog.Viewport className={styles.viewport}>
          <BaseDialog.Popup className={cx(styles.popup, className)} aria-label={label} finalFocus={finalFocus}>
            <Autocomplete.Root
              open
              inline
              items={items}
              autoHighlight="always"
              keepHighlight
              itemToStringValue={(item: Command) => item.label}
              filter={(item: Command, text: string) => commandMatches(item, text)}
              {...(query !== undefined ? { value: query } : {})}
              onValueChange={(next: string) => {
                setOwnQuery(next);
                onQueryChange?.(next);
              }}
            >
              <div className={styles.inputRow}>
                <Search aria-hidden className={styles.searchIcon} />
                <Autocomplete.Input className={styles.input} placeholder={placeholder} aria-label={label} aria-describedby={hintId} />
                <BaseDialog.Close className={styles.esc} aria-label="Close command palette">
                  Esc
                </BaseDialog.Close>
              </div>
              <div className={styles.results}>
                <Autocomplete.Empty className={styles.empty}>{emptyText}</Autocomplete.Empty>
                <Autocomplete.List className={styles.list}>
                  {(group: Group) => (
                    <Autocomplete.Group key={group.value} items={group.items} className={styles.group}>
                      <Autocomplete.GroupLabel className={styles.groupLabel}>{group.value}</Autocomplete.GroupLabel>
                      <Autocomplete.Collection>
                        {(item: Command) => (
                          <Autocomplete.Item key={item.id} value={item} onClick={() => run(item)} className={styles.item}>
                            {item.icon && <span className={styles.itemIcon}>{item.icon}</span>}
                            <span className={styles.itemLabel}>{item.label}</span>
                            {item.hint && <span className={styles.itemHint}>{item.hint}</span>}
                            {item.shortcut && (
                              <span aria-hidden className={styles.itemShortcut}>
                                <KbdShortcut keys={item.shortcut} size="sm" />
                              </span>
                            )}
                          </Autocomplete.Item>
                        )}
                      </Autocomplete.Collection>
                    </Autocomplete.Group>
                  )}
                </Autocomplete.List>
              </div>
              <div className={styles.footer}>
                <span id={hintId} className="sr-only">
                  Use the up and down arrow keys to move between results and Enter to run the highlighted one.
                </span>
                <span aria-hidden className={styles.footerHint}>
                  <KbdShortcut keys={["up"]} size="sm" />
                  <KbdShortcut keys={["down"]} size="sm" /> to navigate
                </span>
                <span aria-hidden className={styles.footerHint}>
                  <KbdShortcut keys={["enter"]} size="sm" /> to select
                </span>
                <span aria-hidden className={cx(styles.footerHint, styles.footerEnd)}>
                  <KbdShortcut keys={["esc"]} size="sm" /> to close
                </span>
              </div>
            </Autocomplete.Root>
          </BaseDialog.Popup>
        </BaseDialog.Viewport>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

/** Toggles the palette on ⌘K (Apple) / Ctrl+K (others). */
export function useCommandPaletteShortcut(toggle: () => void) {
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        toggle();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggle]);
}

export type CommandPaletteTriggerProps = {
  onClick: () => void;
  label?: string;
  className?: string;
};

/** A search-field-looking button for the top bar: "Search… ⌘K". */
export function CommandPaletteTrigger({ onClick, label = "Search…", className }: CommandPaletteTriggerProps) {
  return (
    <button type="button" onClick={onClick} className={cx(styles.trigger, className)} aria-keyshortcuts="Meta+K Control+K">
      <Search aria-hidden className={styles.triggerIcon} />
      <span className={styles.triggerLabel}>{label}</span>
      <span aria-hidden className={styles.triggerKbd}>
        <KbdShortcut keys={["mod", "K"]} size="sm" />
      </span>
    </button>
  );
}

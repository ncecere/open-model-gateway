"use client";

import { Autocomplete } from "@base-ui/react/autocomplete";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Search } from "lucide-react";
import { type ReactNode, useEffect, useId } from "react";
import { KbdShortcut } from "@/components/ui/kbd/kbd";
import { cx } from "@/lib/bitop-utils";
import styles from "./command-palette.module.css";

/*
 * Command palette: Base UI Dialog + an inline Base UI Autocomplete (the
 * pattern from Base UI's "Command palette" example). Keyboard-first: type to
 * filter, ↑/↓ to move, Enter to run, Esc to close. Open it with ⌘K / Ctrl+K
 * via useCommandPaletteShortcut().
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

export type CommandGroup = { label: string; items: Command[] };

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
};

type Group = { value: string; items: Command[] };

function matches(item: Command, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const haystack = [item.label, ...(item.keywords ?? [])].join(" ").toLowerCase();
  return q.split(/\s+/).every((word) => haystack.includes(word));
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
}: CommandPaletteProps) {
  const hintId = useId();
  const items: Group[] = groups.filter((g) => g.items.length > 0).map((g) => ({ value: g.label, items: g.items }));

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
              filter={(item: Command, query: string) => matches(item, query)}
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

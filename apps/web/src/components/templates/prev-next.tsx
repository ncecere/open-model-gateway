/*
 * PrevNext: record navigation on a detail page ("4 / 10  ‹ Previous  Next ›")
 * so people step through requests/routes without going back to the list — the
 * page replacement for OpenRouter's drawer arrows. Each side is a link
 * (`href`/`render`, e.g. a router Link) or a button (`onSelect`); a missing
 * side is a disabled button. Buttons are named after the target ("Next
 * request: req_91c2…"). Optional `shortcuts` adds j (next) / k (previous),
 * ignored while typing in a field or with modifier keys, and advertised with
 * aria-keyshortcuts and a visible hint.
 *
 *   <PrevNext noun="request" position={{ index: 3, total: 10 }} shortcuts
 *     prev={prev && { label: prev.id, render: <Link to="…" /> }} next={next && { label: next.id, onSelect: () => open(next.id) }} />
 */
import { ChevronLeft, ChevronRight } from "lucide-react";
import { type ReactElement, useEffect, useRef } from "react";
import { Button } from "../ui/button/button";
import { Kbd } from "../ui/kbd/kbd";
import { cx } from "../../lib/bitop-utils";
import styles from "./prev-next.module.css";

export type PrevNextTarget = { /** Plain-text name of the target record. */ label: string; href?: string; render?: ReactElement; onSelect?: () => void };

export type PrevNextProps = {
  prev?: PrevNextTarget | null;
  next?: PrevNextTarget | null;
  /** Zero-based index and total; total null = unknown ("4 of many"). */
  position?: { index: number; total: number | null };
  /** What the records are, lower-case: "request". */
  noun?: string;
  /** Enable j / k keyboard shortcuts. */
  shortcuts?: boolean;
  className?: string;
};

function typingIn(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName) || target.closest("[role='dialog'],[role='alertdialog'],[role='menu'],[role='listbox']") !== null;
}

export function PrevNext({ prev, next, position, noun = "record", shortcuts = false, className }: PrevNextProps) {
  const prevRef = useRef<HTMLElement>(null), nextRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (!shortcuts) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || typingIn(event.target)) return;
      const key = event.key.toLowerCase();
      const target = key === "j" ? next : key === "k" ? prev : undefined;
      const el = key === "j" ? nextRef.current : prevRef.current;
      if (!target || !el) return;
      event.preventDefault();
      el.click();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [shortcuts, prev, next]);

  const side = (target: PrevNextTarget | null | undefined, dir: "prev" | "next") => {
    const word = dir === "prev" ? "Previous" : "Next";
    const icon = dir === "prev" ? <ChevronLeft aria-hidden /> : <ChevronRight aria-hidden />;
    const content = dir === "prev" ? <>{icon}<span className={styles.word}>{word}</span></> : <><span className={styles.word}>{word}</span>{icon}</>;
    const ref = dir === "prev" ? prevRef : nextRef;
    const keys = shortcuts ? (dir === "prev" ? "k" : "j") : undefined;
    if (!target) return <Button size="sm" variant="secondary" disabled aria-label={`No ${word.toLowerCase()} ${noun}`}>{content}</Button>;
    const render = target.render ?? (target.href !== undefined ? <a href={target.href} /> : undefined);
    return (
      <Button ref={ref as never} size="sm" variant="secondary" render={render} onClick={target.onSelect} aria-label={`${word} ${noun}: ${target.label}`} aria-keyshortcuts={keys}>
        {content}
      </Button>
    );
  };

  return (
    <nav aria-label={`${noun[0]!.toUpperCase()}${noun.slice(1)} navigation`} className={cx(styles.root, className)}>
      {position && <span className={styles.position}>{position.index + 1} / {position.total === null ? "?" : position.total}<span className="sr-only">{position.total === null ? `: ${noun} ${position.index + 1}, total unknown` : `: ${noun} ${position.index + 1} of ${position.total}`}</span></span>}
      <span className={styles.buttons}>
        {side(prev, "prev")}
        {side(next, "next")}
      </span>
      {shortcuts && <span className={styles.hint} aria-hidden><Kbd size="sm">k</Kbd> <Kbd size="sm">j</Kbd></span>}
    </nav>
  );
}

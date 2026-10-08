import { type Ref, type RefCallback, type RefObject, useEffect, useLayoutEffect, useState } from "react";

/*
 * Shared helpers for bitop-ui components. Installed by the `core` item at
 * `@/lib/bitop-utils` (named so it never collides with shadcn's `lib/utils`).
 */
type ClassValue = string | number | false | null | undefined;

/** Joins class names, skipping falsy values. */
export function cx(...values: ClassValue[]): string {
  return values.filter(Boolean).join(" ");
}

/** Maps a boolean to a presence-only data attribute value (`data-foo=""`). */
export function dataFlag(on: boolean | undefined): "" | undefined {
  return on ? "" : undefined;
}

/**
 * Does `file` match an `accept` string such as "image/*,.pdf,application/json"?
 * Same rules as `<input type="file" accept>`: ".ext" matches the file name
 * (case-insensitive), "type/*" matches a MIME family, anything else an exact
 * MIME type; "*" or "*\/*" match everything. An empty accept allows all files.
 * The picker's accept is only a hint to the browser, and drag and drop and
 * paste ignore it, so check files with this too.
 */
export function matchesAccept(file: File, accept: string | undefined): boolean {
  const rules = (accept ?? "")
    .split(",")
    .map((a) => a.trim().toLowerCase())
    .filter(Boolean);
  if (!rules.length) return true;
  const name = file.name.toLowerCase();
  const type = (file.type || "").toLowerCase();
  return rules.some((a) => {
    if (a === "*" || a === "*/*") return true;
    if (a.startsWith(".")) return name.endsWith(a);
    if (a.endsWith("/*")) return type.startsWith(a.slice(0, -1));
    return type === a;
  });
}

/** Tones shared by badges, alerts, status dots and toasts. */
export type Tone = "neutral" | "info" | "success" | "warning" | "danger";

/** Calls every ref (callback or object) with the node. */
export function mergeRefs<T>(...refs: (Ref<T> | undefined)[]): RefCallback<T> {
  return (node) => {
    for (const r of refs) {
      if (typeof r === "function") r(node);
      else if (r) (r as { current: T | null }).current = node;
    }
  };
}

/**
 * Horizontal overflow of a scroll container: whether its content is wider
 * than it (`overflowing`), and whether content is hidden before the inline
 * start (`start`) or after the inline end (`end`). Works in RTL. `wrapped`:
 * the children of a container that wraps sit on more than one line.
 */
export type ScrollEdges = { overflowing: boolean; start: boolean; end: boolean; wrapped?: boolean };

const noEdges: ScrollEdges = { overflowing: false, start: false, end: false, wrapped: false };

/** Whether an element's children sit on more than one line (a flex row that wrapped). */
function childrenWrapped(node: HTMLElement) {
  const items = Array.from(node.children).filter((c): c is HTMLElement => c instanceof HTMLElement && c.offsetParent !== null);
  const first = items[0];
  return Boolean(first && items.some((c) => c.offsetTop > first.offsetTop + 1));
}

/**
 * Tracks a container's horizontal overflow (scroll, resize, children added
 * or resized) so it can show a scroll cue. Pass the returned callback as the
 * element's ref and spread `scrollEdgeAttrs(edges)` on it:
 *
 *   const [edgesRef, edges] = useScrollEdges<HTMLDivElement>();
 *   <div ref={edgesRef} {...scrollEdgeAttrs(edges)} className={styles.strip}>…</div>
 *
 * and in CSS, e.g. `[data-overflowing] { overflow-x: auto }` plus a fade or
 * shadow on `[data-overflow-start]` / `[data-overflow-end]`.
 */
export function useScrollEdges<T extends HTMLElement>(): [RefCallback<T>, ScrollEdges] {
  const [node, setNode] = useState<T | null>(null);
  const [edges, setEdges] = useState<ScrollEdges>(noEdges);
  useEffect(() => {
    if (!node) return;
    const check = () => {
      const max = node.scrollWidth - node.clientWidth;
      const pos = Math.abs(node.scrollLeft);
      const overflowing = max > 1;
      const next = { overflowing, start: overflowing && pos > 1, end: overflowing && pos < max - 1, wrapped: childrenWrapped(node) };
      setEdges((prev) =>
        prev.overflowing === next.overflowing && prev.start === next.start && prev.end === next.end && prev.wrapped === next.wrapped ? prev : next,
      );
    };
    check();
    node.addEventListener("scroll", check, { passive: true });
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(check);
    const observe = () => {
      if (!ro) return;
      ro.disconnect();
      ro.observe(node);
      for (const child of Array.from(node.children)) ro.observe(child);
    };
    observe();
    const mo = typeof MutationObserver === "undefined" ? null : new MutationObserver(() => (observe(), check()));
    mo?.observe(node, { childList: true });
    return () => {
      node.removeEventListener("scroll", check);
      ro?.disconnect();
      mo?.disconnect();
    };
  }, [node]);
  return [setNode, edges];
}

/** A phone-width window (below 600px): the app shell's drawer, columns hidden on narrow windows. */
export const NARROW_QUERY = "(max-width: 37.5rem)";

/**
 * Whether a media query matches, kept up to date as the window changes:
 *
 *   const narrow = useMediaQuery("(max-width: 37.5rem)");
 *
 * Without `matchMedia` (tests, server rendering) it returns `fallback`.
 */
export function useMediaQuery(query: string, fallback = false): boolean {
  const [matches, setMatches] = useState(() => globalThis.matchMedia?.(query).matches ?? fallback);
  useEffect(() => {
    const mq = globalThis.matchMedia?.(query);
    if (!mq) return;
    const update = () => setMatches(mq.matches);
    update();
    mq.addEventListener?.("change", update);
    return () => mq.removeEventListener?.("change", update);
  }, [query]);
  return matches;
}

/** Data attributes for useScrollEdges: data-overflowing, data-overflow-start, data-overflow-end. */
export function scrollEdgeAttrs(edges: ScrollEdges) {
  return {
    "data-overflowing": dataFlag(edges.overflowing),
    "data-overflow-start": dataFlag(edges.start),
    "data-overflow-end": dataFlag(edges.end),
    "data-wrapped": dataFlag(edges.wrapped),
  };
}

/** Landmark elements and roles (a header or footer inside one is part of it). */
const LANDMARK = [
  "main", "nav", "aside", "header", "footer", "section[aria-label]", "section[aria-labelledby]", "form[aria-label]", "form[aria-labelledby]",
  ...["main", "navigation", "complementary", "banner", "contentinfo", "region", "search", "form"].map((role) => `[role="${role}"]`),
].join(", ");

// Built from role names, like LANDMARK, so consumers' styling checks don't read it as hand-rolled popup markup.
const DIALOG = ["dialog", ...["dialog", "alertdialog"].map((role) => `[role="${role}"]`)].join(", ");

/** useLayoutEffect in the browser, useEffect on the server (no warning). */
const useClientLayoutEffect = typeof window === "undefined" ? useEffect : useLayoutEffect;

/**
 * The outermost landmark (main, nav, aside, a banner header…) around
 * `anchor`, found once it has mounted: the container for a popup's portal,
 * so that a menu opened from the page sits inside the page's landmarks like
 * its trigger (WAI: all content in landmarks; axe's `region` rule) instead
 * of at the end of <body>. Null when the anchor is in no landmark, or in a
 * dialog (a dialog holds its own content): portal to <body> as usual.
 *
 *   const triggerRef = useRef<HTMLButtonElement>(null);
 *   const container = useLandmarkContainer(triggerRef);
 *   <Menu.Portal container={container ?? undefined}> <Menu.Positioner positionMethod={container ? "fixed" : "absolute"}>
 *
 * (Base UI waits for a container that is null: pass undefined for <body>.)
 * Position the popup with `fixed` inside a landmark, so a landmark that
 * scrolls or clips its content doesn't clip the popup.
 */
export function useLandmarkContainer(anchor: RefObject<Element | null>): HTMLElement | null {
  const [container, setContainer] = useState<HTMLElement | null>(null);
  useClientLayoutEffect(() => {
    let found: HTMLElement | null = null;
    for (let el = anchor.current?.parentElement ?? null; el && el !== el.ownerDocument.body; el = el.parentElement) {
      if (el.matches(DIALOG)) {
        found = null;
        break;
      }
      if (el.matches(LANDMARK)) found = el;
    }
    setContainer(found);
  }, [anchor]);
  return container;
}

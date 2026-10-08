"use client";

import type { useRender } from "@base-ui/react/use-render";
import { ChevronLeft, ChevronRight, MoreHorizontal } from "lucide-react";
import type { ComponentPropsWithRef, ReactElement, ReactNode } from "react";
import { Button, type ButtonSize } from "@/components/ui/button/button";
import { cx } from "@/lib/bitop-utils";
import styles from "./pagination.module.css";

/*
 * Pagination: a navigation landmark with previous/next and page links.
 *
 * Composable parts (links, for URL-driven pages):
 *
 *   <Pagination>
 *     <PaginationContent>
 *       <PaginationItem><PaginationPrevious href="?page=1" /></PaginationItem>
 *       <PaginationItem><PaginationLink href="?page=1">1</PaginationLink></PaginationItem>
 *       <PaginationItem><PaginationLink href="?page=2" isActive>2</PaginationLink></PaginationItem>
 *       <PaginationItem><PaginationEllipsis /></PaginationItem>
 *       <PaginationItem><PaginationNext href="?page=3" /></PaginationItem>
 *     </PaginationContent>
 *   </Pagination>
 *
 * Or the all-in-one Paginator, which computes the range with
 * getPaginationRange(): with `getHref`/`renderLink` it renders links, with
 * only `onPageChange` it renders buttons (client-side tables).
 *
 * The current page carries aria-current="page". A link can't be disabled, so
 * at either end previous/next render as disabled native buttons.
 */

/* ---------- range helper ---------- */

export type PaginationRangeItem = number | "start-ellipsis" | "end-ellipsis";

export type PaginationRangeOptions = {
  /** Pages shown on each side of the current page (default 1). */
  siblings?: number;
  /** Pages always shown at the start and end (default 1). */
  boundaries?: number;
};

const span = (start: number, end: number) => (end < start ? [] : Array.from({ length: end - start + 1 }, (_, i) => start + i));

/**
 * The pages to show for `page` of `pageCount` (both 1-based), with ellipsis
 * markers where pages are skipped. The list length stays constant as the
 * current page moves, so the controls don't jump around.
 *
 *   getPaginationRange(5, 10) → [1, "start-ellipsis", 4, 5, 6, "end-ellipsis", 10]
 */
export function getPaginationRange(page: number, pageCount: number, { siblings = 1, boundaries = 1 }: PaginationRangeOptions = {}): PaginationRangeItem[] {
  const count = Math.max(0, Math.floor(pageCount));
  if (count === 0) return [];
  const current = Math.min(Math.max(1, Math.floor(page)), count);
  const startPages = span(1, Math.min(boundaries, count));
  const endPages = span(Math.max(count - boundaries + 1, boundaries + 1), count);
  const siblingsStart = Math.max(Math.min(current - siblings, count - boundaries - siblings * 2 - 1), boundaries + 2);
  const siblingsEnd = Math.min(Math.max(current + siblings, boundaries + siblings * 2 + 2), endPages.length > 0 ? endPages[0]! - 2 : count - 1);
  return [
    ...startPages,
    ...(siblingsStart > boundaries + 2 ? (["start-ellipsis"] as const) : boundaries + 1 < count - boundaries ? [boundaries + 1] : []),
    ...span(siblingsStart, siblingsEnd),
    ...(siblingsEnd < count - boundaries - 1 ? (["end-ellipsis"] as const) : count - boundaries > boundaries ? [count - boundaries] : []),
    ...endPages,
  ];
}

/* ---------- parts ---------- */

export type PaginationProps = Omit<ComponentPropsWithRef<"nav">, "aria-label"> & {
  /** Name of the navigation landmark (default "Pagination"). Make it unique per page. */
  label?: string;
};

export function Pagination({ label = "Pagination", className, ...props }: PaginationProps) {
  return <nav aria-label={label} {...props} className={cx(styles.nav, className)} />;
}

export function PaginationContent({ className, ...props }: ComponentPropsWithRef<"ul">) {
  return <ul {...props} className={cx(styles.list, className)} />;
}

export function PaginationItem({ className, ...props }: ComponentPropsWithRef<"li">) {
  return <li {...props} className={cx(styles.item, className)} />;
}

export type PaginationLinkProps = Omit<ComponentPropsWithRef<"a">, "className"> & {
  className?: string;
  /** The current page: aria-current="page" and a raised style. */
  isActive?: boolean;
  /** Renders a disabled native button instead of a link (not focusable, announced as dimmed). */
  disabled?: boolean;
  size?: ButtonSize;
  /** Router link, e.g. `render={<Link to="/docs" search={{ page: 2 }} />}`. */
  render?: useRender.RenderProp;
};

export function PaginationLink({ isActive = false, disabled = false, size = "md", render, href, className, children, ...props }: PaginationLinkProps) {
  // A link can't be disabled, so a disabled step becomes a disabled <button>.
  const element = disabled ? undefined : (render ?? <a href={href} />);
  return (
    <Button
      {...(props as object)}
      variant={isActive ? "secondary" : "ghost"}
      size={size}
      render={element}
      disabled={disabled}
      aria-current={isActive ? "page" : undefined}
      data-active={isActive ? "" : undefined}
      className={cx(styles.page, className)}
    >
      {children}
    </Button>
  );
}

export type PaginationStepProps = Omit<PaginationLinkProps, "isActive" | "children"> & {
  /** Visible text (default "Previous" / "Next"). */
  text?: ReactNode;
  /** Hide the text visually on narrow screens (still announced). */
  compact?: boolean;
};

export function PaginationPrevious({ text = "Previous", compact, className, ...props }: PaginationStepProps) {
  return (
    <PaginationLink aria-label="Go to previous page" {...props} className={cx(styles.step, className)} data-compact={compact ? "" : undefined}>
      <ChevronLeft aria-hidden className={styles.flip} />
      <span className={styles.stepText}>{text}</span>
    </PaginationLink>
  );
}

export function PaginationNext({ text = "Next", compact, className, ...props }: PaginationStepProps) {
  return (
    <PaginationLink aria-label="Go to next page" {...props} className={cx(styles.step, className)} data-compact={compact ? "" : undefined}>
      <span className={styles.stepText}>{text}</span>
      <ChevronRight aria-hidden className={styles.flip} />
    </PaginationLink>
  );
}

export type PaginationEllipsisProps = ComponentPropsWithRef<"span"> & {
  /** Announced text (default "More pages"). */
  label?: string;
};

/** Marks skipped pages. Not focusable. */
export function PaginationEllipsis({ label = "More pages", className, ...props }: PaginationEllipsisProps) {
  return (
    <span {...props} className={cx(styles.ellipsis, className)}>
      <MoreHorizontal aria-hidden />
      <span className="sr-only">{label}</span>
    </span>
  );
}

/* ---------- all-in-one ---------- */

export type PaginatorProps = PaginationRangeOptions & {
  /** Current page, 1-based. */
  page: number;
  pageCount: number;
  /** Called with the new page. Without getHref/renderLink the controls are buttons. */
  onPageChange?: (page: number) => void;
  /** URL of a page: renders links. */
  getHref?: (page: number) => string;
  /** Router link for a page, e.g. `(p) => <Link to="." search={{ page: p }} />`. */
  renderLink?: (page: number) => ReactElement;
  /** Accessible name for a page control (default "Page N"). */
  pageLabel?: (page: number) => string;
  label?: string;
  size?: ButtonSize;
  /** Hide "Previous"/"Next" text visually on narrow screens. */
  compact?: boolean;
  className?: string;
};

export function Paginator({
  page,
  pageCount,
  onPageChange,
  getHref,
  renderLink,
  pageLabel = (p) => `Page ${p}`,
  siblings,
  boundaries,
  label,
  size = "md",
  compact,
  className,
}: PaginatorProps) {
  const range = getPaginationRange(page, pageCount, { siblings, boundaries });
  const current = Math.min(Math.max(1, page), Math.max(1, pageCount));
  const asLinks = Boolean(getHref || renderLink);
  const go = (p: number) => () => onPageChange?.(p);
  const atStart = current <= 1;
  const atEnd = current >= pageCount;

  const control = (p: number, isActive: boolean, children: ReactNode, extra: { "aria-label": string; className?: string }, disabled = false) =>
    asLinks ? (
      <PaginationLink
        {...extra}
        size={size}
        isActive={isActive}
        disabled={disabled}
        href={getHref?.(p)}
        render={renderLink?.(p)}
        onClick={onPageChange ? go(p) : undefined}
      >
        {children}
      </PaginationLink>
    ) : (
      <Button
        {...extra}
        variant={isActive ? "secondary" : "ghost"}
        size={size}
        disabled={disabled}
        aria-current={isActive ? "page" : undefined}
        data-active={isActive ? "" : undefined}
        className={cx(styles.page, extra.className)}
        onClick={go(p)}
      >
        {children}
      </Button>
    );

  return (
    <Pagination label={label} className={className} data-compact={compact ? "" : undefined}>
      <PaginationContent>
        <PaginationItem>
          {control(
            current - 1,
            false,
            <>
              <ChevronLeft aria-hidden className={styles.flip} />
              <span className={styles.stepText}>Previous</span>
            </>,
            { "aria-label": "Go to previous page", className: cx(styles.page, styles.step) },
            atStart,
          )}
        </PaginationItem>
        {range.map((item) =>
          typeof item === "number" ? (
            <PaginationItem key={item}>{control(item, item === current, item, { "aria-label": pageLabel(item) })}</PaginationItem>
          ) : (
            <PaginationItem key={item}>
              <PaginationEllipsis />
            </PaginationItem>
          ),
        )}
        <PaginationItem>
          {control(
            current + 1,
            false,
            <>
              <span className={styles.stepText}>Next</span>
              <ChevronRight aria-hidden className={styles.flip} />
            </>,
            { "aria-label": "Go to next page", className: cx(styles.page, styles.step) },
            atEnd,
          )}
        </PaginationItem>
      </PaginationContent>
    </Pagination>
  );
}

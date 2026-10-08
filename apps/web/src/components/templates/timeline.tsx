/*
 * Timeline: the ordered upstream attempts of one request (or any ordered
 * events) — an <ol> where each step has a status icon with its word for
 * screen readers, a title, meta (route, HTTP status, region), a duration, an
 * optional marker badge ("Fallback", "Selected") and optional detail. With
 * `showDurationBars` each known duration gets a proportional bar. The total
 * line says "at least" when any duration is unknown; it never treats unknown
 * as 0 ms.
 *
 *   <Timeline label="Attempts" items={attempts.map(a => ({ id: a.id, status: a.ok ? "success" : "failed",
 *     title: a.route_name, meta: `HTTP ${a.status}`, durationMs: a.latency_ms, marker: a.fallback ? "Fallback" : undefined }))} showTotal />
 */
import { Ban, CircleCheck, CircleDashed, CircleHelp, CircleX, LoaderCircle, type LucideIcon } from "lucide-react";
import type { CSSProperties, ReactNode } from "react";
import { Badge } from "../ui/badge/badge";
import { cx, type Tone } from "../../lib/bitop-utils";
import { formatDurationMs } from "./kit-format";
import styles from "./timeline.module.css";

export type TimelineStatus = "success" | "failed" | "cancelled" | "skipped" | "pending" | "unknown";

const statusMeta: Record<TimelineStatus, { icon: LucideIcon; word: string }> = {
  success: { icon: CircleCheck, word: "Succeeded" },
  failed: { icon: CircleX, word: "Failed" },
  cancelled: { icon: Ban, word: "Cancelled" },
  skipped: { icon: CircleDashed, word: "Skipped" },
  pending: { icon: LoaderCircle, word: "In progress" },
  unknown: { icon: CircleHelp, word: "Unknown outcome" },
};

export type TimelineItem = {
  id: string;
  status: TimelineStatus;
  title: ReactNode;
  /** Muted line under the title (route, HTTP status, region…). */
  meta?: ReactNode;
  /** Milliseconds; null/undefined = unknown. */
  durationMs?: number | null;
  /** A small badge after the title, e.g. "Fallback" or "Selected". */
  marker?: ReactNode;
  markerTone?: Tone;
  /** Extra content under the step (error text, IDs). */
  detail?: ReactNode;
};

export type TimelineProps = {
  /** Accessible name of the list, e.g. "Upstream attempts". */
  label: string;
  items: TimelineItem[];
  /** Proportional duration bars (relative to the longest known duration). */
  showDurationBars?: boolean;
  /** A "Total" line after the steps. */
  showTotal?: boolean;
  /** Shown instead of an empty list. */
  empty?: ReactNode;
  className?: string;
};

export function Timeline({ label, items, showDurationBars = false, showTotal = false, empty = "No attempts recorded.", className }: TimelineProps) {
  if (items.length === 0) return <p className={styles.empty}>{empty}</p>;
  const known = items.map(i => i.durationMs).filter((d): d is number => typeof d === "number" && Number.isFinite(d) && d >= 0);
  const longest = Math.max(0, ...known);
  const total = known.reduce((a, b) => a + b, 0);
  const partial = known.length < items.length;
  return (
    <div className={cx(styles.root, className)}>
      <ol className={styles.list} aria-label={label}>
        {items.map((item, index) => {
          const { icon: Icon, word } = statusMeta[item.status] ?? statusMeta.unknown;
          const duration = formatDurationMs(item.durationMs);
          const width = typeof item.durationMs === "number" && longest > 0 ? `${Math.max(2, (item.durationMs / longest) * 100)}%` : undefined;
          return (
            <li key={item.id} className={styles.item} data-status={item.status}>
              <span className={styles.rail} aria-hidden>
                <Icon className={styles.icon} />
              </span>
              <div className={styles.body}>
                <div className={styles.head}>
                  <span className={styles.title}>
                    <span className="sr-only">{`Step ${index + 1}, ${word}: `}</span>
                    {item.title}
                  </span>
                  {item.marker && <Badge size="sm" tone={item.markerTone ?? "neutral"} variant="outline">{item.marker}</Badge>}
                  <span className={styles.duration} data-unknown={duration === "Unknown" ? "" : undefined}>
                    <span className="sr-only">Duration </span>{duration}
                  </span>
                </div>
                {item.meta && <div className={styles.meta}>{item.meta}</div>}
                {showDurationBars && width && <span aria-hidden className={styles.track}><span className={styles.bar} style={{ "--width": width } as CSSProperties} /></span>}
                {item.detail && <div className={styles.detail}>{item.detail}</div>}
              </div>
            </li>
          );
        })}
      </ol>
      {showTotal && (
        <p className={styles.total}>
          Total: {known.length === 0 ? "Unknown" : `${partial ? "at least " : ""}${formatDurationMs(total)}`}
        </p>
      )}
    </div>
  );
}

/*
 * Dates and times with the shared formatters (lib/format.ts, review rule 4), as
 * semantic <time> elements:
 *
 *   <TableTime value={row.started_at} />   "Oct 8, 2:30 AM" (title: "Oct 8, 2026, 2:30 AM EDT")
 *   <DetailTime value={key.created_at} />  "Oct 8, 2026, 2:30 AM EDT"
 *
 * Missing or invalid values render `fallback` (default "—"), never a fabricated date.
 */
import type { ReactNode } from "react";
import { detailTime, tableTime } from "../../lib/format";

const parse = (value: string | null | undefined) => { if (!value) return undefined; const d = new Date(value); return Number.isFinite(d.getTime()) ? d : undefined; };

export function TableTime({ value, fallback = "—" }: { value: string | null | undefined; fallback?: ReactNode }) {
  const d = parse(value);
  if (!d) return <>{fallback}</>;
  return <time dateTime={d.toISOString()} title={detailTime(value!)} suppressHydrationWarning>{tableTime(value!)}</time>;
}

export function DetailTime({ value, fallback = "—" }: { value: string | null | undefined; fallback?: ReactNode }) {
  const d = parse(value);
  if (!d) return <>{fallback}</>;
  return <time dateTime={d.toISOString()} suppressHydrationWarning>{detailTime(value!)}</time>;
}

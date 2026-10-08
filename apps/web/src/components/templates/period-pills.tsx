/*
 * PeriodPills: choose a limit/budget reset period — Daily · Weekly · Monthly ·
 * Lifetime — as a joined Bitop ToggleGroup (one always pressed, arrow keys
 * move between items). Values are URL-friendly ("day" | "week" | "month" |
 * "lifetime"); offer a subset with `periods`. `showReset` writes the
 * plain-language reset rule under the pills.
 *
 * PeriodBadge is the read-only pill shown next to a usage amount in lists.
 *
 *   <PeriodPills label="Reset period" value={period} onChange={setPeriod} periods={["day", "week", "month"]} showReset />
 */
import { useId } from "react";
import { Badge } from "../ui/badge/badge";
import { ToggleGroup, ToggleGroupItem } from "../ui/toggle-group/toggle-group";
import { type LimitPeriod, limitPeriodLabel, limitPeriods } from "./kit-format";
import styles from "./period-pills.module.css";

export type PeriodPillsProps = {
  /** Visible label of the group (its accessible name). */
  label: string;
  hideLabel?: boolean;
  value: LimitPeriod;
  onChange: (period: LimitPeriod) => void;
  /** Offered periods in order (default all four). */
  periods?: LimitPeriod[];
  /** Show the reset rule ("Resets monthly on the 1st at 00:00 UTC") under the pills. */
  showReset?: boolean;
  disabled?: boolean;
  /** Why the control is disabled, e.g. "Set a limit first". */
  disabledReason?: string;
};

export function PeriodPills({ label, hideLabel, value, onChange, periods = ["day", "week", "month", "lifetime"], showReset, disabled, disabledReason }: PeriodPillsProps) {
  const id = useId();
  const reset = limitPeriods.find(p => p.value === value)?.reset;
  const describedBy = [showReset && reset ? `${id}-reset` : "", disabled && disabledReason ? `${id}-reason` : ""].filter(Boolean).join(" ") || undefined;
  return (
    <div className={styles.root}>
      <span id={`${id}-label`} className={hideLabel ? "sr-only" : styles.label}>{label}</span>
      <ToggleGroup aria-labelledby={`${id}-label`} aria-describedby={describedBy} joined variant="outline" size="sm" disabled={disabled} value={[value]} onValueChange={next => { const picked = next[0] as LimitPeriod | undefined; if (picked && picked !== value) onChange(picked); }}>
        {periods.map(p => <ToggleGroupItem key={p} value={p}>{limitPeriodLabel(p)}</ToggleGroupItem>)}
      </ToggleGroup>
      {showReset && reset && <p id={`${id}-reset`} className={styles.hint}>{reset}</p>}
      {disabled && disabledReason && <p id={`${id}-reason`} className={styles.hint}>{disabledReason}</p>}
    </div>
  );
}

/** A small outline pill naming a period, e.g. next to "$12.40 / $50.00". */
export function PeriodBadge({ period }: { period: LimitPeriod }) {
  return <Badge size="sm" variant="outline" className={styles.badge}>{limitPeriodLabel(period)}</Badge>;
}

/* Usage & costs: URL state, the header period control and the scope line shared by every tab and the chart page. */
import { useId, useState } from "react";
import type { Workspace } from "../../lib/api";
import type { DashboardSearch } from "../../lib/permissions";
import { customRangeError, periodToSearch, usageRanges, type UsagePeriod, type UsageRange } from "../../lib/usage";
import { useDashboardNavigation } from "../../components/navigation-link";
import { Button, FormField, Input, NativeSelect } from "../../components/ui";
import u from "./usage.module.css";

export type UsageNav = { search: DashboardSearch; navigate: (patch: Partial<DashboardSearch>) => void };
/** The dashboard URL when routed; local state otherwise (embedding, unit tests). Every change resets paging. */
export function useUsageSearch(workspace?: Workspace): UsageNav {
  const nav = useDashboardNavigation(), base: DashboardSearch = { page: workspace ? "costs" : "platform-costs", ws: workspace?.id };
  const [local, setLocal] = useState<DashboardSearch>(base);
  if (nav) return { search: nav.search, navigate: patch => nav.navigate({ ...nav.search, offset: undefined, ...patch }) };
  return { search: local, navigate: patch => setLocal(prev => ({ ...prev, offset: undefined, ...patch })) };
}

/** "Everyone in Product" (workspace-wide), "Your usage in Product" (own keys), "Your usage" (Personal). */
export function scopeLine(workspace?: Workspace): string {
  if (!workspace) return "Everyone on this installation · personal workspaces appear as totals only";
  if (workspace.kind === "personal") return "Your usage";
  return workspace.capabilities.view_all_activity ? `Everyone in ${workspace.name}` : `Your usage in ${workspace.name}`;
}

/** Header period picker: presets plus an inline custom range (inclusive dates, UTC). No dialog. */
export function PeriodControl({ period, navigate }: { period: UsagePeriod | undefined; navigate: UsageNav["navigate"] }) {
  const id = useId(), preset: UsageRange = period?.preset ?? "custom";
  const [start, setStart] = useState(period?.start_date ?? ""), [last, setLast] = useState(period?.last_date ?? ""), [error, setError] = useState<string>();
  const choose = (next: UsageRange) => { setError(undefined); if (next === "custom") navigate(periodToSearch("custom", { start: period?.start_date ?? start, last: period?.last_date ?? last })); else navigate(periodToSearch(next)); };
  const apply = () => { const problem = customRangeError(start, last); setError(problem); if (!problem) navigate(periodToSearch("custom", { start, last })); };
  return <div className={u.period}>
    <FormField label="Period" description="UTC days">
      <NativeSelect id={`${id}-preset`} size="sm" value={preset} onChange={e => choose(e.target.value as UsageRange)}>{usageRanges.map(r => <option key={r.value} value={r.value}>{r.label}</option>)}</NativeSelect>
    </FormField>
    {preset === "custom" && <form className={u.custom} onSubmit={e => { e.preventDefault(); apply(); }} aria-label="Custom period">
      <FormField label="From"><Input type="date" size="sm" value={start} max={last || undefined} onChange={e => setStart(e.target.value)} /></FormField>
      <FormField label="To (included)" error={error}><Input type="date" size="sm" value={last} min={start || undefined} onChange={e => setLast(e.target.value)} /></FormField>
      <Button type="submit" size="sm" variant="secondary">Apply</Button>
    </form>}
  </div>;
}

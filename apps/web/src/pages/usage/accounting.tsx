/*
 * Usage & costs › "Accounting" (ui-principles 13): one compact section for
 * workspace admins and platform readers. Four small stats (Cache hit rate,
 * Cost per 1M tokens, Costs pending, Unpriced), one status line ("All costs
 * known" or "N requests still have unknown cost · View records"), one cache
 * table with only non-zero categories (or "No prompt caching this period"),
 * media meters only when a workload produced them, and the coverage counts
 * behind a small "Coverage" disclosure. No explanatory paragraphs: caveats are
 * tooltips. Precision is unchanged: exact micro-USD, unknown is never zero,
 * and the overlapping cache-write aggregate is never shown as a charge.
 */
import { tokensText } from "../../lib/requests";
import { Fragment, type ReactNode } from "react";
import { AlertTriangle, CheckCircle2 } from "lucide-react";
import type { CostReport, CostComponents, MeterUsage, MeterCostComponents, UnitMeter, Cost, BillingUsage } from "../../lib/governance";
import { formatMicroUsd } from "../../lib/governance";
import { countLabel, formatCount } from "../../lib/reports";
import { countNoun, formatAudio, meterComponentLabels, meterUsageLabels } from "../../lib/pricing";
import { healthLabel } from "../../lib/people";
import { rateTexts, type UsageOverview } from "../../lib/usage";
import type { DashboardSearch } from "../../lib/permissions";
import { ErrorNotice, Stack, useApi } from "../../components/ui";
import { ResourceLink } from "../../components/navigation-link";
import { Card } from "../../components/ui/card/card";
import { Disclosure } from "../../components/ui/disclosure/disclosure";
import { Table, Td, Th, Tr } from "../../components/ui/table/table";
import { TooltipText } from "../../components/ui/tooltip/tooltip";
import u from "./usage.module.css";

const ZERO = /^0+$/;
/** A known zero: an integer string of zeros. Null/absent is unknown, never zero. */
const isZero = (v: string | null | undefined) => v != null && ZERO.test(v);
const exact = (v: string | null | undefined) => v != null && /^\d+$/.test(v) ? BigInt(v) : null;
/** Zero relevant attempts: "—", never "Unknown" (which reads as broken). */
const tokenText = (value: string | null | undefined, attempts?: string) => attempts === "0" ? "—" : formatCount(value);

/** The six charge categories (the overlapping "cache writes" aggregate is the sum of the three write rows and is never charged). */
type CacheRow = { key: string; label: string; tokens?: keyof BillingUsage; cost: keyof CostComponents };
const cacheRows: CacheRow[] = [
  { key: "uncached", label: "Uncached input", tokens: "uncached_input_tokens", cost: "uncached_input_microusd" },
  { key: "read", label: "Cache read", tokens: "cache_read_input_tokens", cost: "cache_read_microusd" },
  { key: "write_default", label: "Cache write · default", tokens: "cache_write_default_input_tokens", cost: "cache_write_default_microusd" },
  { key: "write_5m", label: "Cache write · 5-minute", tokens: "cache_write_5m_input_tokens", cost: "cache_write_5m_microusd" },
  { key: "write_1h", label: "Cache write · 1-hour", tokens: "cache_write_1h_input_tokens", cost: "cache_write_1h_microusd" },
  { key: "output", label: "Output", cost: "output_microusd" },
];
const cacheTokenKeys: (keyof BillingUsage)[] = ["cache_read_input_tokens", "cache_write_input_tokens", "cache_write_default_input_tokens", "cache_write_5m_input_tokens", "cache_write_1h_input_tokens"];
const cacheCostKeys: (keyof CostComponents)[] = ["cache_read_microusd", "cache_write_default_microusd", "cache_write_5m_microusd", "cache_write_1h_microusd"];

/** Every cache counter and cache charge is a known zero (unknown counters keep the table, as "Unknown"). */
export function noPromptCaching(billing: BillingUsage | null, components: CostComponents | null): boolean {
  return !!billing && !!components && cacheTokenKeys.every(k => isZero(billing[k])) && cacheCostKeys.every(k => isZero(components[k]));
}

/**
 * Tokens and spend per charge category in one table, only the categories that aren't a known zero. No caching at
 * all: one line instead. Legacy (pricing-v1) charges stay a separate row, never folded into uncached input.
 */
export function CacheAccounting({ billing, components, attempts, legacy }: { billing: CostReport["billing_usage"] | null; components: CostComponents | null; attempts?: string; legacy?: string | null }) {
  const showLegacy = legacy != null && !isZero(legacy);
  const legacyLine = showLegacy && <p className={u.note}>Legacy pinned pricing: <span className={u.num}>{formatMicroUsd(legacy)}</span></p>;
  if (noPromptCaching(billing, components)) return <><p className={u.note}>No prompt caching this period</p>{legacyLine}</>;
  // A row is hidden only when both its tokens and its spend are a known zero (unknown is shown as "Unknown").
  const rows = cacheRows.filter(r => { const noTokens = !r.tokens || attempts === "0" || isZero(billing?.[r.tokens]); return !noTokens || !isZero(components?.[r.cost]); });
  if (!rows.length && !showLegacy) return <p className={u.note}>No charges this period</p>;
  return <Table caption="Charges by category" stack className={u.acctTable} columns={["Category", { label: "Tokens", numeric: true, width: "9rem" }, { label: "Spent", numeric: true, width: "10rem" }]}>
    {rows.map(r => <Tr key={r.key}><Th scope="row">{r.label}</Th><Td numeric>{r.tokens ? tokenText(billing?.[r.tokens], attempts) : "—"}</Td><Td numeric>{formatMicroUsd(components?.[r.cost])}</Td></Tr>)}
    {showLegacy && <Tr><Th scope="row"><TooltipText content="Pricing-v1 charges, kept apart from the six categories.">Legacy pinned pricing</TooltipText></Th><Td numeric>—</Td><Td numeric>{formatMicroUsd(legacy)}</Td></Tr>}
  </Table>;
}
/**
 * One meter's report total. With per-meter counts (newer gateways): meters no attempt could produce are omitted
 * (`undefined`); a fully observed meter is a known value, possibly zero; any unobserved relevant attempt makes it
 * "Unknown · partial" with the observed lower bound. Without counts, an absent total is unknown.
 */
export function meterUsageText(key: UnitMeter, usage: MeterUsage | null, relevant?: Record<UnitMeter, string>, unknown?: Record<UnitMeter, string>): string | undefined {
  const value = usage?.[key] ?? null, audio = key.endsWith("_audio_seconds_ms"), show = (v: string) => audio ? formatAudio(v) : formatCount(v);
  if (!relevant || !unknown) return value == null ? "Unknown" : show(value);
  if (relevant[key] === "0") return undefined;
  if (unknown[key] !== "0") return value == null || /^0+$/.test(value) && unknown[key] === relevant[key] ? "Unknown" : `Unknown · partial (at least ${show(value)})`;
  return show(value ?? "0");
}
/**
 * Non-token meters (one table: usage and spend side by side), plus provider-reported cost as evidence only (never the
 * charge). Nothing at all when no workload could produce a meter and the provider reported nothing.
 */
export function MeterAccounting({ usage, components, providerCost, relevant, unknown }: { usage: MeterUsage | null; components: Partial<MeterCostComponents> | null; providerCost: string | null; relevant?: Record<UnitMeter, string>; unknown?: Record<UnitMeter, string> }) {
  const rows = meterUsageLabels.flatMap((r, i) => { const text = meterUsageText(r.key, usage, relevant, unknown); return text === undefined ? [] : [{ ...r, text, cost: meterComponentLabels[i]!.key }]; });
  if (!rows.length && providerCost == null) return null;
  return <Stack gap={3}>
    <h3 className={u.subhead}>Media and unit meters</h3>
    {rows.length > 0 && <Table caption="Media and unit meters" stack className={u.acctTable} columns={["Meter", { label: "Usage", numeric: true, width: "12rem" }, { label: "Spent", numeric: true, width: "10rem" }]}>
      {rows.map(r => <Tr key={r.key}><Th scope="row">{r.label}</Th><Td numeric>{r.text}</Td><Td numeric>{formatMicroUsd(components?.[r.cost])}</Td></Tr>)}
    </Table>}
    {providerCost != null && <p className={u.note}><TooltipText content="What the provider says it charged (for example OpenRouter usage.cost). Used to check estimates, never billed: spent amounts come from configured prices.">Provider-reported (for checking only)</TooltipText>: <span className={u.num}>{formatMicroUsd(providerCost)}</span></p>}
  </Stack>;
}
/** Compact per-record meter evidence: only observed counters, "Not reported" when none were. */
export function meterSummary(cost: Pick<Cost, "meter_usage" | "output_image_variant">): string {
  const u = cost.meter_usage; if (!u) return "Not reported";
  const parts = meterUsageLabels.filter(l => u[l.key] != null).map(l => l.audio ? `${l.label.toLowerCase()} ${formatAudio(u[l.key])}` : l.key === "output_images" ? `${formatCount(u[l.key])} image${u[l.key] === "1" ? "" : "s"}${cost.output_image_variant ? ` (${cost.output_image_variant})` : ""}` : countNoun(l.key, u[l.key]!));
  return parts.length ? parts.join(" · ") : "Not reported";
}
/** Tokens as everywhere else (rule 5, lib/format): "145 in · 0 out", "Not applicable" for audio, "Unknown" when unreported. */
export function tokenUsageText(c: Pick<Cost, "workload_kind" | "input_tokens" | "output_tokens">): string {
  return tokensText(c.input_tokens, c.output_tokens, c.workload_kind);
}

/**
 * Attempts whose cost isn't final: pending + unknown (exact). Aged holds are a subset of those two states, so they
 * are named in the breakdown but never added again. Null when the server sent a malformed count.
 */
export function costsPending(health: CostReport["health"]): { total: bigint | null; pending: string; unknown: string; aged: string } {
  const pending = exact(health.pending_attempts), unknown = exact(health.unknown_attempts);
  return { total: pending === null || unknown === null ? null : pending + unknown, pending: health.pending_attempts, unknown: health.unknown_attempts, aged: health.aged_hold_attempts };
}

function Stat({ label, children }: { label: string; children: ReactNode }) {
  return <div className={u.acctStat}><dt>{label}</dt><dd>{children}</dd></div>;
}
/** "All costs known" (green) or "N requests still have unknown cost · View records" (amber). */
function StatusLine({ total, unresolved }: { total: bigint | null; unresolved?: DashboardSearch }) {
  if (total === 0n) return <p className={`${u.acctStatus} ${u.acctOk}`}><CheckCircle2 aria-hidden size={16} />All costs known</p>;
  const n = total === null ? null : total.toString();
  return <p className={`${u.acctStatus} ${u.acctWarn}`}><AlertTriangle aria-hidden size={16} /><span>{n === null ? "Some costs may not be known yet" : `${countLabel(n, "request")} still ${n === "1" ? "has" : "have"} unknown cost`}{unresolved && <> · <ResourceLink search={unresolved}>View records</ResourceLink></>}</span></p>;
}
/** Coverage and health counts (they overlap), one click away. */
function Coverage({ report }: { report: CostReport }) {
  const c = report.coverage;
  const items: [string, string][] = [["Requests", report.totals.root_requests], ["Attempts", report.totals.attempts], ["Priced", c.priced_attempts], ["Settled", c.settled_attempts], ["Complete billing", c.complete_billing_attempts], ["Incomplete billing", c.incomplete_billing_attempts], ["Legacy pricing", c.legacy_pricing_attempts], ...Object.entries(report.health).map(([key, value]) => [healthLabel(key), value] as [string, string])];
  return <Disclosure title="Coverage" summary="Attempt counts; they overlap" keepMounted>
    <dl className={u.acctCoverage}>{items.map(([label, value]) => <Fragment key={label}><dt>{label}</dt><dd>{formatCount(value)}</dd></Fragment>)}</dl>
    {report.breakdowns_truncated && <p className={u.note}>Breakdowns are limited to 100 entries per dimension.</p>}
  </Disclosure>;
}

/**
 * The accounting summary of one cost report (no fetching): stats, the status line, the cache table, meters when
 * present and the Coverage disclosure. `rates` (the overview tiles) adds cache hit rate and cost per 1M tokens.
 */
export function AccountingReport({ report, rates, unresolved, statusFilter = false }: { report: CostReport; rates?: Pick<UsageOverview, "tiles">; /** Where "View records" goes. */ unresolved?: DashboardSearch; /** The records page filters by cost status (workspace Cost records). */ statusFilter?: boolean }) {
  const pending = costsPending(report.health), r = rates && rateTexts(rates);
  const target = unresolved && statusFilter ? { ...unresolved, cost_status: isZero(report.health.unknown_attempts) ? "on_hold" as const : "cost_unknown" as const } : unresolved;
  const breakdown = `Pending ${formatCount(pending.pending)} · Unknown ${formatCount(pending.unknown)} · Aged holds ${formatCount(pending.aged)} (included above)`;
  return <Stack gap={4}>
    <dl className={u.acctStats} aria-label="Accounting summary">
      {r && <Stat label="Cache hit rate">{r.cacheHit}</Stat>}
      {r && <Stat label="Cost per 1M tokens"><span title={r.blended.exact !== r.blended.text ? `${r.blended.exact} (exact)` : undefined}>{r.blended.text}</span></Stat>}
      <Stat label="Costs pending"><TooltipText content={breakdown}>{pending.total === null ? "Unknown" : formatCount(pending.total.toString())}</TooltipText></Stat>
      <Stat label="Unpriced"><span className={isZero(report.health.unpriced_attempts) ? undefined : u.acctWarnText}>{formatCount(report.health.unpriced_attempts)}</span></Stat>
    </dl>
    <StatusLine total={pending.total} unresolved={target} />
    <Stack gap={3}>
      <h3 className={u.subhead}>Cache</h3>
      <CacheAccounting billing={report.billing_usage} components={report.cost_components} attempts={report.coverage.total_attempts} legacy={report.cost_components.legacy_microusd} />
    </Stack>
    {(report.meter_usage !== undefined || report.provider_reported_cost_microusd !== undefined) && <MeterAccounting usage={report.meter_usage ?? null} components={report.cost_components} providerCost={report.provider_reported_cost_microusd ?? null} relevant={report.meter_relevant_attempts} unknown={report.meter_unknown_attempts} />}
    <Coverage report={report} />
  </Stack>;
}
/** Usage & costs › Overview's "Accounting" card: loads the period's cost report (only rendered for roles that act on it). */
export function AccountingSection({ path, rates, unresolved, statusFilter }: { path: string; rates?: Pick<UsageOverview, "tiles">; unresolved?: DashboardSearch; statusFilter?: boolean }) {
  const report = useApi<CostReport>(path);
  return <Card title="Accounting" titleAs="h2">
    {report.isPending ? <p role="status" className={u.note}>Loading accounting…</p> : report.isError ? <ErrorNotice error={report.error} retry={() => void report.refetch()} /> : !report.data?.health || !report.data.coverage || !report.data.cost_components ? <ErrorNotice error={new Error("The gateway returned an incomplete accounting report.")} /> : <AccountingReport report={report.data} rates={rates} unresolved={unresolved} statusFilter={statusFilter} />}
  </Card>;
}

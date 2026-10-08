/*
 * Usage & costs › "Accounting details": the cost report's accounting internals
 * (health, coverage, cache categories, unit meters, provider-reported evidence,
 * legacy pricing), kept verbatim for admins and auditors but collapsed in place
 * (no drawer). The report is fetched only once the disclosure is opened.
 */
import { tokensText } from "../../lib/requests";
import { Fragment, useState } from "react";
import type { CostReport, CostComponents, MeterUsage, MeterCostComponents, UnitMeter, Cost } from "../../lib/governance";
import { formatMicroUsd } from "../../lib/governance";
import { billingLabels, formatCount } from "../../lib/reports";
import { countNoun, formatAudio, meterComponentLabels, meterUsageLabels } from "../../lib/pricing";
import { healthLabel } from "../../lib/people";
import { ErrorNotice, Stack, Table, useApi } from "../../components/ui";
import { Card } from "../../components/ui/card/card";
import { Disclosure } from "../../components/ui/disclosure/disclosure";
import s from "../shared.module.css";

const componentLabels: { key: keyof CostComponents; label: string }[] = [{ key: "uncached_input_microusd", label: "Uncached input" }, { key: "cache_read_microusd", label: "Cache read" }, { key: "cache_write_default_microusd", label: "Cache write · default" }, { key: "cache_write_5m_microusd", label: "Cache write · 5-minute" }, { key: "cache_write_1h_microusd", label: "Cache write · 1-hour" }, { key: "output_microusd", label: "Output" }];
/** Zero relevant attempts: "—", never "Unknown" (which reads as broken). */
const tokenText = (value: string | null | undefined, attempts?: string) => attempts === "0" ? "—" : formatCount(value);

export function CacheAccounting({ billing, components, attempts }: { billing: CostReport["billing_usage"] | null; components: CostComponents | null; attempts?: string }) {
  return <Card title="Cache-aware accounting" titleAs="h3"><Stack gap={4}><p className={s.note}>Six separate charge categories: uncached input, cache read, three cache-write durations and output. "Cache writes" is the total of the three write rows and is never charged again.</p>
    <Table label="Observed billing tokens" rows={billingLabels.map(b => ({ ...b, label: b.key === "cache_write_input_tokens" ? "Cache writes (total of the three rows below)" : b.label }))} rowKey={r => r.key} columns={[{ title: "Category", render: r => r.label }, { title: "Tokens", numeric: true, render: r => tokenText(billing?.[r.key], attempts) }]} />
    <Table label="Settled cost components" rows={componentLabels} rowKey={r => r.key} columns={[{ title: "Component", render: r => r.label }, { title: "Spent", numeric: true, render: r => formatMicroUsd(components?.[r.key]) }]} /></Stack></Card>;
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
/** Non-token meters next to token totals, plus provider-reported cost as evidence only (never the charge). */
export function MeterAccounting({ usage, components, providerCost, relevant, unknown }: { usage: MeterUsage | null; components: Partial<MeterCostComponents> | null; providerCost: string | null; relevant?: Record<UnitMeter, string>; unknown?: Record<UnitMeter, string> }) {
  const rows = meterUsageLabels.flatMap(r => { const text = meterUsageText(r.key, usage, relevant, unknown); return text === undefined ? [] : [{ ...r, text }]; });
  const componentRows = relevant ? meterComponentLabels.filter((_, i) => relevant[meterUsageLabels[i].key] !== "0") : meterComponentLabels;
  return <Card title="Media and unit meters" titleAs="h3"><Stack gap={4}><p className={s.note}>Images, characters, audio and search units from non-token workloads. Counters that weren't reported are unknown, not zero; a partial total is a lower bound.</p>{rows.length ? <><Table label="Observed meter usage" rows={rows} rowKey={r => r.key} columns={[{ title: "Meter", render: r => r.label }, { title: "Usage", numeric: true, render: r => r.text }]} /><Table label="Settled meter cost components" rows={componentRows} rowKey={r => r.key} columns={[{ title: "Component", render: r => r.label }, { title: "Spent", numeric: true, render: r => formatMicroUsd(components?.[r.key]) }]} /></> : <p className={s.muted}>— No image, speech, transcription, rerank or System One requests in this period.</p>}<dl className={s.details}><dt>Provider-reported (for checking only)</dt><dd>{formatMicroUsd(providerCost)}</dd></dl><p className={s.note}>What the provider says it charged, for example OpenRouter <code className={s.mono}>usage.cost</code>. Used to check our estimates, never billed: spent amounts come from configured prices.</p></Stack></Card>;
}
/** Compact per-record meter evidence: only observed counters, "Not reported" when none were. */
export function meterSummary(cost: Pick<Cost, "meter_usage" | "output_image_variant">): string {
  const u = cost.meter_usage; if (!u) return "Not reported";
  const parts = meterUsageLabels.filter(l => u[l.key] != null).map(l => l.audio ? `${l.label.toLowerCase()} ${formatAudio(u[l.key])}` : l.key === "output_images" ? `${formatCount(u[l.key])} image${u[l.key] === "1" ? "" : "s"}${cost.output_image_variant ? ` (${cost.output_image_variant})` : ""}` : countNoun(l.key, u[l.key]!));
  return parts.length ? parts.join(" · ") : "Not reported";
}
/** Token counts, or "Not applicable" for speech/transcription attempts that reported no tokens. */
/** Tokens as everywhere else (rule 5, lib/format): "145 in · 0 out", "Not applicable" for audio, "Unknown" when unreported. */
export function tokenUsageText(c: Pick<Cost, "workload_kind" | "input_tokens" | "output_tokens">): string {
  return tokensText(c.input_tokens, c.output_tokens, c.workload_kind);
}
/** The accounting internals of one cost report (no fetching). */
export function AccountingReport({ report }: { report: CostReport }) {
  const c = report.coverage;
  return <Stack gap={6}>
    <Card title="Accounting health" titleAs="h3"><Stack gap={3}><p className={s.note}>These overlap; don't add them up.</p><dl className={s.details}>{Object.entries(report.health).map(([key, value]) => <Fragment key={key}><dt>{healthLabel(key)}</dt><dd>{formatCount(value)}</dd></Fragment>)}</dl>
      <p className={s.note}>Attempts: {formatCount(report.totals.attempts)} for {formatCount(report.totals.root_requests)} requests. Coverage: {formatCount(c.priced_attempts)} priced; {formatCount(c.settled_attempts)} settled; {formatCount(c.complete_billing_attempts)} with complete billing; {formatCount(c.total_attempts)} total attempts. Legacy pricing: {formatCount(c.legacy_pricing_attempts)}; incomplete billing: {formatCount(c.incomplete_billing_attempts)}.</p></Stack></Card>
    <CacheAccounting billing={report.billing_usage} components={report.cost_components} attempts={c.total_attempts} />
    {(report.meter_usage !== undefined || report.provider_reported_cost_microusd !== undefined) && <MeterAccounting usage={report.meter_usage ?? null} components={report.cost_components} providerCost={report.provider_reported_cost_microusd ?? null} relevant={report.meter_relevant_attempts} unknown={report.meter_unknown_attempts} />}
    <Card title="Legacy pinned pricing" titleAs="h3"><p className={s.note}>Pricing-v1 charges are kept separate from the six categories above, not counted as uncached input: {formatMicroUsd(report.cost_components.legacy_microusd)}.</p></Card>
    {report.breakdowns_truncated && <p className={s.note}>Breakdowns are limited to 100 entries per dimension.</p>}
  </Stack>;
}
/** Collapsed "Accounting details": in-place disclosure; the cost report loads only when opened. */
export function AccountingDetails({ path, defaultOpen = false }: { path: string; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  return <Disclosure title="Accounting details" summary="Health, coverage, cache categories, unit meters, provider-reported cost, legacy pricing" open={open} onOpenChange={setOpen}>
    {open && <LoadedAccounting path={path} />}
  </Disclosure>;
}
function LoadedAccounting({ path }: { path: string }) {
  const report = useApi<CostReport>(path);
  return report.isPending ? <p role="status">Loading accounting details…</p> : report.isError ? <ErrorNotice error={report.error} retry={() => void report.refetch()} /> : <AccountingReport report={report.data} />;
}

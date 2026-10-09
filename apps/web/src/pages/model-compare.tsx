/*
 * Model compare (`/workspaces/{ws}/models/compare?ids=…`, `/admin/models/compare?ids=…`):
 * 2–4 models side by side. One column per model, one row per fact: type,
 * protocols, serving, input/output/other prices (exact micro-USD from the first
 * enabled route's latest price; unknown stays "Unknown", never $0), token
 * ceilings, and 30 days of observed metrics the viewer may see (workspace:
 * their own or workspace-wide activity; admin: Team/Project activity only).
 * The best known value per row is marked subtly (bold + check), only when
 * values differ. Models are picked with the checkboxes on the Models pages.
 */
import type { ReactNode } from "react";
import { Check, Columns3 } from "lucide-react";
import type { DashboardSearch } from "../lib/permissions";
import { formatMicroUsd } from "../lib/governance";
import { protocolLabel } from "../lib/model-setup";
import { workloadLabels } from "../lib/pricing";
import { bestIndexes, compareReady, comparePath, decimalKey, lineAmount, msText, notApplicable, otherLines, percentText, tokensText, MAX_COMPARE, type CompareModel, type CompareResponse, type CompareScope } from "../lib/model-compare";
import { Button, ErrorNotice, Stack, useApi } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { IconCell, LabIcon } from "../components/provider-icon";
import { BackLink } from "../components/templates/form-page";
import { HintBadge } from "../components/templates/hint-badge";
import { PageHeader } from "../components/templates/page-header";
import { Checkbox } from "../components/ui/checkbox/checkbox";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Table, Td, Th, Tr } from "../components/ui/table/table";
import { TooltipText } from "../components/ui/tooltip/tooltip";
import s from "./shared.module.css";
import c from "./model-compare.module.css";

// ---------------------------------------------------------------------------
// Picking models on the Models pages
// ---------------------------------------------------------------------------
/** A row's compare checkbox before its name; unchecked boxes are disabled once four are picked. */
export function CompareSelect({ name, checked, full, onChange, children }: { name: string; checked: boolean; full: boolean; onChange: (on: boolean) => void; children: ReactNode }) {
  return <span className={c.select}><Checkbox label={<span className="sr-only">Compare {name}</span>} checked={checked} disabled={!checked && full} onCheckedChange={on => onChange(on)} />{children}</span>;
}
/** "2 selected · Compare · Clear" while models are picked. */
export function CompareBar({ count, target, onClear }: { count: number; target: DashboardSearch; onClear: () => void }) {
  if (!count) return null;
  return <div className={c.bar} role="group" aria-label="Compare models">
    <span className={c.count}>{count} selected{count >= MAX_COMPARE ? " (max)" : ""}</span>
    {count >= 2 ? <Button size="sm" render={<ResourceLink search={target} />}><Columns3 aria-hidden />Compare</Button> : <Button size="sm" disabled title="Pick 2 to 4 models"><Columns3 aria-hidden />Compare</Button>}
    <Button size="sm" variant="ghost" onClick={onClear}>Clear</Button>
  </div>;
}

// ---------------------------------------------------------------------------
// The compare page
// ---------------------------------------------------------------------------
type Row = { label: string; hint?: string; cell: (m: CompareModel) => ReactNode; value?: (m: CompareModel) => bigint | number | null; better?: "min" | "max" };
const unknown = (hint = "Not known yet · not zero") => <TooltipText content={hint} className={c.unknown}>Unknown</TooltipText>;
const none = (hint: string) => <TooltipText content={hint} className={s.muted}>—</TooltipText>;
const count = (v: string) => /^\d+$/.test(v) ? BigInt(v).toLocaleString("en-US") : v;
function price(meter: "input_tokens" | "output_tokens") {
  return (m: CompareModel) => {
    if (!m.price) return m.enabled_routes ? unknown("The route has no price · not free") : none("No enabled route");
    if (notApplicable(m.price, meter)) return <span className={s.muted}>Not applicable</span>;
    const amount = lineAmount(m.price, meter);
    return amount === null ? unknown() : <span>{formatMicroUsd(amount.toString())}<span className={s.muted}> /M</span></span>;
  };
}
const ceiling = (v: number | null | undefined) => v != null && v > 0 ? BigInt(v) : null;
const noData = (m: CompareModel) => m.metrics.requests === "0";
export const compareRows: Row[] = [
  { label: "Type", cell: m => workloadLabels[m.workload] ?? m.workload },
  { label: "Protocols", cell: m => m.protocols.map(protocolLabel).join(" · ") },
  { label: "Serving", cell: m => !m.enabled ? <HintBadge tone="neutral" hint="Turned off">Disabled</HintBadge> : (m.serving_routes ?? m.enabled_routes) > 0 ? <HintBadge tone="success" hint={`${m.enabled_routes} enabled route${m.enabled_routes === 1 ? "" : "s"}`}>Ready</HintBadge> : <HintBadge tone="warning" hint={m.enabled_routes > 0 ? "No enabled route's connection can serve this model" : "No enabled route"}>Not serving</HintBadge> },
  { label: "Input price", hint: "Per million input tokens, first enabled route", cell: price("input_tokens"), value: m => lineAmount(m.price, "input_tokens"), better: "min" },
  { label: "Output price", hint: "Per million output tokens, first enabled route", cell: price("output_tokens"), value: m => lineAmount(m.price, "output_tokens"), better: "min" },
  { label: "Other prices", cell: m => { const lines = otherLines(m.price); return lines.length ? <span className={c.lines}>{lines.map(l => <span key={l}>{l}</span>)}</span> : <span className={s.muted}>—</span>; } },
  { label: "Context", hint: "Input token ceiling", cell: m => m.price ? (ceiling(m.price.input_token_limit) !== null ? `${tokensText(m.price.input_token_limit)} tokens` : <span className={s.muted}>—</span>) : unknown(), value: m => ceiling(m.price?.input_token_limit), better: "max" },
  { label: "Max output", hint: "Output token ceiling", cell: m => m.price ? (ceiling(m.price.output_token_limit) !== null ? `${tokensText(m.price.output_token_limit)} tokens` : <span className={s.muted}>—</span>) : unknown(), value: m => ceiling(m.price?.output_token_limit), better: "max" },
  { label: "Requests", hint: "Last 30 days", cell: m => count(m.metrics.requests) },
  { label: "Error rate", cell: m => percentText(m.metrics.error_rate) ?? none("No finished requests"), value: m => decimalKey(m.metrics.error_rate), better: "min" },
  { label: "Median latency", cell: m => msText(m.metrics.latency_p50_ms) ?? none("No finished requests"), value: m => m.metrics.latency_p50_ms, better: "min" },
  { label: "Median time to first token", cell: m => msText(m.metrics.ttft_p50_ms) ?? none(noData(m) ? "No requests" : "No streamed requests"), value: m => m.metrics.ttft_p50_ms, better: "min" },
  { label: "Tokens per second", cell: m => m.metrics.tokens_per_second ?? none(noData(m) ? "No requests" : "Not measured"), value: m => decimalKey(m.metrics.tokens_per_second), better: "max" },
];
const activityText: Record<CompareResponse["activity"], string> = { own: "your requests", workspace: "this workspace's requests", shared_workspaces: "team and project requests" };

export function ModelComparePage({ scope, ids }: { scope: CompareScope; ids: string[] }) {
  const nav = useDashboardNavigation(), ready = compareReady(ids);
  const q = useApi<CompareResponse>(comparePath(scope, ids), ready);
  useResourceName("Compare models");
  const back: { label: string; search: DashboardSearch } = scope.kind === "platform" ? { label: "Models", search: { page: "models" } } : { label: "Models", search: { page: "grants", ws: scope.workspace.id } };
  const modelSearch = (m: CompareModel): DashboardSearch => scope.kind === "platform" ? { page: "model-detail", record: m.id } : { page: "workspace-model", ws: scope.workspace.id, record: m.id };
  const header = (description?: string) => <PageHeader title="Compare models" description={description} breadcrumbs={<BackLink {...back} />} />;
  if (!ready) return <Stack gap={6} className={s.page}>{header()}<EmptyState icon={<Columns3 />} titleAs="h2" title="Pick 2 to 4 models" description="Tick models on the Models page, then Compare." action={<Button variant="secondary" render={<ResourceLink search={back.search} />}>Back to models</Button>} /></Stack>;
  if (q.isPending) return <Stack gap={6} className={s.page}>{header()}<p role="status">Loading models…</p></Stack>;
  if (q.isError) return <Stack gap={6} className={s.page}>{header()}<ErrorNotice error={q.error} retry={() => void q.refetch()} /></Stack>;
  const models = q.data.data;
  const remove = (id: string) => nav?.navigate({ ...back.search, page: scope.kind === "platform" ? "platform-model-compare" : "model-compare", ids: ids.filter(x => x !== id).join(",") });
  return <Stack gap={6} className={s.page}>
    {header(`Metrics: last 30 days of ${activityText[q.data.activity]}.`)}
    <Table caption="Model comparison" framed density="compact" columns={[{ label: "Model", hideLabel: true, width: "12rem" }, ...models.map(m => ({ label: m.display_name || m.public_name }))]}>
      <Tr>
        <Th scope="row"><span className="sr-only">Model</span></Th>
        {models.map(m => <Td key={m.id}><span className={c.head}><IconCell icon={<LabIcon model={[m.public_name, m.display_name]} />}><ResourceLink search={modelSearch(m)}>{m.display_name || m.public_name}</ResourceLink><code className={c.id}>{m.public_name}</code></IconCell>{models.length > 2 && <Button size="sm" variant="ghost" onClick={() => remove(m.id)} aria-label={`Remove ${m.display_name || m.public_name}`}>Remove</Button>}</span></Td>)}
      </Tr>
      {compareRows.map(row => { const best = row.value && row.better ? bestIndexes(models.map(row.value), row.better) : new Set<number>();
        return <Tr key={row.label}>
          <Th scope="row">{row.hint ? <TooltipText content={row.hint}>{row.label}</TooltipText> : row.label}</Th>
          {models.map((m, i) => <Td key={m.id} className={best.has(i) ? c.best : undefined}>{row.cell(m)}{best.has(i) && <><Check aria-hidden className={c.check} /><span className="sr-only"> (best)</span></>}</Td>)}
        </Tr>; })}
    </Table>
  </Stack>;
}

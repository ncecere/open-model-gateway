/*
 * Workspace › Models › one model: a read-only page for the people who write
 * code against it (finding #8). It shows what the workspace's catalog API
 * already serves to members (GET /workspaces/{ws}/catalog): API name, type,
 * cheapest prices, availability and how to call it. Routes, connections and
 * routing policy stay in Admin. "Back to Models" and Previous/Next follow the
 * catalog's name order; no drawer.
 */
import { FileQuestion } from "lucide-react";
import { wsPath } from "../lib/api";
import { eligibilityLabels, protocolEndpoints, protocolLabel, workloadModalities, type WorkspaceCatalogModel } from "../lib/model-setup";
import { formatDecimalMicroUsd, workloadLabels } from "../lib/pricing";
import { notServing } from "../lib/home";
import type { DashboardSearch } from "../lib/permissions";
import { Button, DateTime, ErrorNotice, Stack, useChoices } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { BackLink } from "../components/templates/form-page";
import { CopyId } from "../components/templates/copy-id";
import { PrevNext } from "../components/templates/prev-next";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { Badge } from "../components/ui/badge/badge";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { PageHeader } from "../components/ui/page-header/page-header";
import { EligibilityBadge, NotServingBadge, workspaceModelSearch } from "./model-catalog";
import type { Scope } from "./workspace";
import s from "./shared.module.css";
import m from "./models.module.css";

const byName = (a: WorkspaceCatalogModel, b: WorkspaceCatalogModel) => a.display_name.localeCompare(b.display_name, undefined, { sensitivity: "base" }) || a.model_id.localeCompare(b.model_id);

export function WorkspaceModelPage({ workspace, id }: Scope & { id: string }) {
  const catalog = useChoices<WorkspaceCatalogModel>(`${wsPath(workspace.id)}/catalog`);
  const rows = [...(catalog.data ?? [])].sort(byName), index = rows.findIndex(r => r.model_id === id), model = index >= 0 ? rows[index] : undefined;
  useResourceName(model?.display_name ?? "Model");
  const back = { label: "Models", search: { page: "grants", ws: workspace.id } as DashboardSearch };
  if (catalog.isPending) return <p role="status">Loading model…</p>;
  if (catalog.isError) return <Stack gap={6} className={s.page}><PageHeader title="Model" breadcrumbs={<BackLink {...back} />} /><ErrorNotice error={catalog.error} retry={() => void catalog.refetch()} /></Stack>;
  if (!model) return <Stack gap={6} className={s.page}>
    <PageHeader title="Model not found" breadcrumbs={<BackLink {...back} />} />
    <EmptyState icon={<FileQuestion />} titleAs="h2" title="This model doesn't exist or you can't see it" description="Only models this workspace can use, or add from its catalogs, are listed." action={<Button variant="secondary" render={<ResourceLink search={back.search} />}>Back to Models</Button>} />
  </Stack>;
  const target = (row?: WorkspaceCatalogModel) => row ? { label: row.display_name, render: <ResourceLink search={workspaceModelSearch(workspace.id, row.model_id)} /> } : null;
  const input = formatDecimalMicroUsd(model.min_input_microusd_per_million), output = formatDecimalMicroUsd(model.min_output_microusd_per_million);
  const tokens = model.workload === "generation" || model.workload === "embeddings" || model.workload === "systemone";
  const several = Number(model.routes) > 1, off = notServing(model);
  return <Stack gap={6} className={s.page}>
    <PageHeader title={model.display_name} meta={<><EligibilityBadge eligibility={model.eligibility} />{off && <NotServingBadge />}</>} breadcrumbs={<BackLink {...back} />}
      description={<><code className={s.mono}>{model.public_name}</code>{model.description ? <> · {model.description}</> : null}</>}
      actions={<PrevNext noun="model" position={{ index, total: rows.length }} prev={target(rows[index - 1])} next={target(rows[index + 1])} />} />
    <StatTileGrid columns={4} label="Model summary">
      <StatTile label="Type" value={workloadModalities[model.workload] ?? workloadLabels[model.workload] ?? model.workload} hint={model.protocols.map(protocolLabel).join(" · ")} />
      <StatTile label="Input price" value={tokens ? input ?? null : "Not token-priced"} hint={tokens ? input ? `per M input tokens · ${several ? "cheapest route" : "estimate"}` : "Unknown · not free" : "Priced per unit"} />
      <StatTile label="Output price" value={model.workload === "generation" ? output ?? null : "Not applicable"} hint={model.workload === "generation" ? output ? `per M output tokens · ${several ? "cheapest route" : "estimate"}` : "Unknown · not free" : undefined} />
      <StatTile label="Availability" value={eligibilityLabels[model.eligibility] ?? "Unknown"} hint={off ? "Not serving: no route is turned on" : model.reason} />
    </StatTileGrid>
    <Card title="Overview" titleAs="h2"><DescriptionList dividers items={[
      { label: "API model name", value: <CopyId value={model.public_name} label="API model name" head={200} tail={0} /> },
      { label: "Type", value: <>{workloadLabels[model.workload] ?? model.workload} <span className={s.secondary}>{workloadModalities[model.workload]}</span></> },
      { label: "In this workspace", value: <><EligibilityBadge eligibility={model.eligibility} /> <span className={s.secondary}>{model.reason}</span></> },
      ...(off ? [{ label: "Serving", value: <span className={m.unknown}>No route to a provider is turned on, so requests fail. A platform admin can enable one.</span> }] : []),
      ...(model.created_at && model.eligibility !== "available_from_catalog" ? [{ label: "Added", value: <DateTime value={model.created_at} /> }] : []),
    ]} /></Card>
    <Card title="How to call it" titleAs="h2" description="Send requests to this gateway with an API key from this workspace. Anything not listed is refused, never silently dropped.">
      <Stack gap={5}>{model.protocols.map(p => { const e = protocolEndpoints[p]; if (!e) return null;
        return <section key={p} className={m.endpoint} aria-label={protocolLabel(p)}>
          <h3 className={m.endpointLine}><Badge size="sm" variant="outline">{e.method}</Badge><code className={m.code}>{e.path}</code><span className={s.muted}>{protocolLabel(p)}</span></h3>
          <DescriptionList dividers items={[
            { label: "Headers", value: <ul className={m.chips}>{e.headers.map(h => <li key={h.name}><code className={m.code}>{h.name}: {h.value}</code></li>)}</ul> },
            { label: "Body", value: e.contentType },
            { label: "Key parameters", value: <ul className={s.plainList}>{e.params.map(x => <li key={x.name}><code className={m.code}>{x.name}</code>{x.required ? " (required)" : ""} · {x.name === "model" ? <code className={m.code}>{model.public_name}</code> : x.note}</li>)}</ul> },
            { label: "Not supported", value: e.unsupported },
          ]} />
        </section>; })}</Stack>
    </Card>
    <Card title="Pricing" titleAs="h2" description="Configured estimates for budgets, not provider invoices. A missing rate is unknown, never free.">
      <DescriptionList dividers items={[
        { label: "Input", value: tokens ? input ? <>{several ? "From " : ""}{input} / M input tokens</> : <span className={m.unknown}>Unknown · not free</span> : "Not token-priced" },
        ...(model.workload === "generation" ? [{ label: "Output", value: output ? <>{several ? "From " : ""}{output} / M output tokens</> : <span className={m.unknown}>Unknown · not free</span> }] : []),
      ]} />
    </Card>
    <Card title="Usage" titleAs="h2" description="Your requests and spend with this model.">
      <div className={m.inlineActions}>
        <Button variant="secondary" render={<ResourceLink search={{ page: "requests", ws: workspace.id, model: model.public_name }} />}>Requests with this model</Button>
        <Button variant="secondary" render={<ResourceLink search={{ page: "costs", ws: workspace.id, model_id: model.model_id }} />}>Usage &amp; costs for this model</Button>
      </div>
    </Card>
  </Stack>;
}

/*
 * Effective access: every layer that applies to a workspace (or one key) in
 * order — installation, type defaults or platform override, this workspace,
 * the key — with its limits and cumulative Available / Partly available /
 * Unavailable model counts (LayerTable), then "Why can't I use this model?":
 * one row per model with its reasons opening in place (ExpandableRow, never a
 * drawer). Read-only; the server decides visibility (installation limits only
 * for platform readers) and never reports installation headroom.
 */
import { useState } from "react";
import { ArrowRight, ShieldCheck } from "lucide-react";
import { wsPath, type Workspace, type WorkspaceKind } from "../lib/api";
import { accessStatusLabel, blockingReason, layerLabel, layerLimits, reasonText, type AccessLayer, type AccessModel, type AccessResponse } from "../lib/effective-access";
import { limitsSummary } from "../lib/limits";
import type { KeyStatus } from "../lib/keys";
import { Alert, ErrorNotice, useApi } from "./ui";
import { ResourceLink } from "./navigation-link";
import { LayerTable, type Layer, type LayerKind } from "./templates/layer-table";
import { ExpandableRow, expandColumn } from "./templates/table-rows";
import { Badge, StatusBadge } from "./ui/badge/badge";
import { Button } from "./ui/button/button";
import { Card } from "./ui/card/card";
import { Table, Td, Th, Tr } from "./ui/table/table";
import { ToggleGroup, ToggleGroupItem } from "./ui/toggle-group/toggle-group";
import s from "../pages/shared.module.css";

const tone = { available: "success", partial: "warning", unavailable: "danger" } as const;
export function AccessStatus({ status }: { status: AccessModel["status"] }) { return <StatusBadge tone={tone[status]} size="sm">{accessStatusLabel[status]}</StatusBadge>; }

function layerRow(layer: AccessLayer, kind: WorkspaceKind, names: { workspace: string; key?: string }): Layer {
  const limits = layerLimits(layer), counts = layer.models;
  const base = { id: layer.layer, counts };
  switch (layer.layer) {
    case "platform": return { ...base, kind: "platform", name: "Installation", source: <Badge size="sm" variant="outline">Applies to everyone</Badge>, value: layer.visible ? limitsSummary(limits) : "Applies to everyone · details for platform staff", state: layer.visible && limits ? "set" : "none" };
    case "type_default": return { ...base, kind: kind as LayerKind, name: layerLabel("type_default", kind), source: <Badge size="sm" variant="outline">{layer.applies ? "Type default" : "Not used · overridden"}</Badge>, value: <>{limitsSummary(limits)}{layer.catalogs && <span className={s.secondary}>Catalogs{layer.catalogs_apply === false ? " (replaced)" : ""}: {layer.catalogs.length ? layer.catalogs.map(c => c.name).join(", ") : "none"}</span>}</>, state: layer.applies ? "set" : "inherited" };
    case "workspace_override": return { ...base, kind: "platform", name: "Platform override", source: <Badge size="sm" variant="outline">{layer.applies || layer.catalogs_apply ? "Replaced for this team" : "None"}</Badge>, value: layer.applies || layer.catalogs_apply ? <>{layer.applies ? limitsSummary(limits) : "Limits: type defaults"}{layer.catalogs && <span className={s.secondary}>Catalogs: {layer.catalogs.length ? layer.catalogs.map(c => c.name).join(", ") : "none"}</span>}</> : "Not set", state: layer.applies || layer.catalogs_apply ? "set" : "none" };
    case "workspace": return { ...base, kind: "workspace", name: names.workspace, source: <Badge size="sm" variant="outline">This team's own limits</Badge>, value: <>{limitsSummary(limits, "No extra caps")}{layer.selections && <span className={s.secondary}>{layer.selections.catalog} added from catalogs · {layer.selections.direct} assigned by a Platform Admin</span>}</>, state: limitsSummary(limits, "") ? "set" : "inherited" };
    case "key": return { ...base, kind: "key", name: names.key ?? "Key", source: <Badge size="sm" variant="outline">Key limits</Badge>, value: <>{limitsSummary(limits, "No extra caps")}{layer.restriction && <span className={s.secondary}>{layer.restriction.mode === "restricted" ? `Only ${layer.restriction.model_ids?.length ?? 0} selected model${layer.restriction.model_ids?.length === 1 ? "" : "s"}` : "All workspace models"}</span>}</>, state: limitsSummary(limits, "") || layer.restriction?.mode === "restricted" ? "set" : "inherited" };
  }
}

/** Models with their status and in-place reasons. Unavailable and partly available models first. */
export function WhyUnavailable({ models, kind, truncated, canManageModels }: { models: AccessModel[]; kind: WorkspaceKind; truncated?: boolean; /** The viewer can add models here (their own Personal, or a workspace admin): reasons say "You can add it". */ canManageModels?: boolean }) {
  const blocked = models.filter(m => m.status !== "available"), [show, setShow] = useState<"blocked" | "all">(blocked.length ? "blocked" : "all");
  const rows = (show === "blocked" ? blocked : models).slice().sort((a, b) => ["unavailable", "partial", "available"].indexOf(a.status) - ["unavailable", "partial", "available"].indexOf(b.status) || a.public_name.localeCompare(b.public_name));
  return <div>
    <div className={s.toolbar}>
      <ToggleGroup aria-label="Models shown" joined variant="outline" size="sm" value={[show]} onValueChange={next => { const v = next[0] as "blocked" | "all" | undefined; if (v) setShow(v); }}>
        <ToggleGroupItem value="blocked">Not available ({blocked.length})</ToggleGroupItem>
        <ToggleGroupItem value="all">All models ({models.length})</ToggleGroupItem>
      </ToggleGroup>
    </div>
    {rows.length === 0 ? <p className={s.note}>{show === "blocked" ? "Every model listed here can be used." : "No models are available to this workspace yet."}</p> :
      <Table caption="Why can't I use this model?" columns={[expandColumn, "Model", "Status", "Why"]}>
        {rows.map(m => {
          const cells = <><Th><span className={s.primary}>{m.display_name}</span><span className={s.secondary}>{m.public_name}</span></Th><Td><AccessStatus status={m.status} /></Td><Td>{m.reasons.length === 0 ? <span className={s.muted}>Nothing blocks it</span> : `${m.reasons.length} reason${m.reasons.length === 1 ? "" : "s"}${m.reasons.some(blockingReason) ? "" : " · not blocking"}`}</Td></>;
          return m.reasons.length ? <ExpandableRow key={m.model_id} label={m.display_name} colSpan={4} details={<ul className={s.plainList} aria-label={`Reasons for ${m.display_name}`}>{m.reasons.map((r, i) => <li key={i}>{reasonText(r, kind, canManageModels)} <span className={s.muted}>· {layerLabel(r.layer, kind)}</span></li>)}</ul>}>{cells}</ExpandableRow> : <Tr key={m.model_id}><Td />{cells}</Tr>;
        })}
      </Table>}
    {truncated && <p className={s.note}>Showing the first 200 models.</p>}
  </div>;
}

const deadKey: Partial<Record<KeyStatus, string>> = { revoked: "Revoked keys can't use any models", expired: "Expired keys can't use any models" };
/**
 * The layer table and per-model reasons for a workspace or one key. A revoked or expired key can never call anything
 * again, so it shows that (0 usable) instead of what its layers would allow; a disabled key says it can't be used until
 * it's enabled, above what it would get then.
 */
export function EffectiveAccess({ workspace, keyId, keyName, keyStatus, title = "Effective access", canManageModels }: { workspace: Pick<Workspace, "id" | "name" | "kind">; keyId?: string; keyName?: string; keyStatus?: KeyStatus; title?: string; canManageModels?: boolean }) {
  const path = keyId ? `${wsPath(workspace.id)}/keys/${encodeURIComponent(keyId)}/access` : `${wsPath(workspace.id)}/access`;
  const dead = keyStatus ? deadKey[keyStatus] : undefined;
  const q = useApi<AccessResponse>(path, !dead);
  if (dead) return <Card id="access" title={title} titleAs="h2"><p className={s.note}><span className={s.primary}>{dead}.</span> 0 models are usable with this key. Create a new key to call models again.</p></Card>;
  return <Card id="access" title={title} titleAs="h2" description={keyId ? "Every layer applies to this key, in order. The lowest limit wins, and a model must pass every layer." : "Every layer applies to this workspace's keys, in order. The lowest limit wins, and a model must pass every layer."}>
    {keyStatus === "disabled" && <Alert tone="warning" title="Disabled: no models can be used right now">Requests with this key fail until it's enabled again. Below is what it can use once enabled.</Alert>}
    {q.isPending ? <p role="status">Loading effective access…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <>
      <LayerTable caption={`${title} by layer`} valueHeader="Limits" showCounts layers={q.data.layers.filter(l => keyId || l.layer !== "key").map(l => layerRow(l, workspace.kind, { workspace: workspace.name, key: keyName }))} effective={{ value: "Lowest of each limit", counts: q.data.summary }} />
      <h3 className={s.groupHeading}>Why can't I use this model?</h3>
      <WhyUnavailable models={q.data.models} kind={workspace.kind} truncated={q.data.truncated} canManageModels={canManageModels} />
    </>}
  </Card>;
}

/** Workspace Overview card: model availability counts with a link to the reasons. */
export function AccessCard({ workspace }: { workspace: Workspace }) {
  const q = useApi<AccessResponse>(`${wsPath(workspace.id)}/access`);
  // One link button in the card header (review #41): "See why" when some models are limited, else "Limits".
  const limited = !!q.data && q.data.summary.unavailable + q.data.summary.partial > 0;
  const link = <Button variant="secondary" size="sm" render={<ResourceLink search={{ page: "workspace-settings", ws: workspace.id, tab: "limits" }} />}>{limited ? "See why" : "Limits"} <ArrowRight aria-hidden /></Button>;
  return <Card title="Access" description="Models your keys can use here, after every limit and catalog." actions={link}>
    {q.isPending ? <p role="status">Loading access…</p> : q.isError ? <ErrorNotice error={q.error} retry={() => void q.refetch()} /> : <p className={`${s.note} ${s.iconLine}`}><ShieldCheck aria-hidden size={14} /><span>{q.data.summary.available} available · {q.data.summary.partial} partly available · {q.data.summary.unavailable} unavailable</span></p>}
  </Card>;
}

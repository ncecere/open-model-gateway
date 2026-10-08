/*
 * Effective access by layer and "Why can't I use this model?" reasons
 * (ux-api-contract "Layers + why-unavailable"): GET /workspaces/{ws}/access and
 * /workspaces/{ws}/keys/{key}/access. Counts are cumulative up to each layer.
 */
import type { BudgetPeriod, PolicyBudget } from "./governance";
import type { WorkspaceKind } from "./api";
import { periodName, resetText, type Limits, type RateKey } from "./limits";
import { kindLabels } from "./people";

export type AccessCounts = { available: number; partial: number; unavailable: number };
export type AccessRates = Record<RateKey, number | null>;
export type AccessLayerName = "platform" | "type_default" | "workspace_override" | "workspace" | "key";
export type AccessLayer = {
  layer: AccessLayerName; source: string; applies: boolean; visible?: boolean; catalogs_apply?: boolean;
  limits: AccessRates | null; budgets: PolicyBudget[] | null; catalogs: { id: string; name: string }[] | null;
  selections?: { catalog: number; direct: number }; restriction?: { mode: "inherit" | "restricted"; model_ids: string[] | null } | null; models: AccessCounts;
};
export type AccessReason = { code: string; layer: AccessLayerName; period?: BudgetPeriod };
export type AccessModel = { model_id: string; public_name: string; display_name: string; status: "available" | "partial" | "unavailable"; reasons: AccessReason[] };
export type AccessResponse = { workspace_id: string; key_id: string | null; /** Only platform readers can open a disabled workspace (read-only). */ workspace_disabled?: boolean; truncated: boolean; layers: AccessLayer[]; summary: AccessCounts; models: AccessModel[] };

export const accessStatusLabel: Record<AccessModel["status"], string> = { available: "Available", partial: "Partly available", unavailable: "Unavailable" };
export function layerLabel(layer: AccessLayerName, kind: WorkspaceKind): string {
  return { platform: "Installation", type_default: `${kindLabels[kind]} defaults`, workspace_override: "Platform override", workspace: "This workspace", key: "This key" }[layer];
}
export const layerLimits = (layer: Pick<AccessLayer, "limits" | "budgets">): Limits | null => layer.limits ? { ...layer.limits, budgets: layer.budgets ?? [] } : null;
const whereText = (layer: AccessLayerName, kind: WorkspaceKind) => ({ platform: "the installation", type_default: `the ${kindLabels[kind]} defaults`, workspace_override: "the platform override", workspace: "this workspace", key: "this key" }[layer]);
/** One reason in plain words. Unknown codes are shown as they are, never hidden. */
export function reasonText(reason: AccessReason, kind: WorkspaceKind, canManageModels = false): string {
  const period = reason.period ? periodName[reason.period].toLowerCase() : "";
  switch (reason.code) {
    case "model_disabled": return "A Platform Admin turned this model off.";
    case "workspace_disabled": return `This ${kindLabels[kind].toLowerCase()} is disabled, so no model can be used until a Platform Admin enables it.`;
    case "no_enabled_route": return "No route to a provider is turned on for this model.";
    case "some_routes_unavailable": return "Some of its routes are turned off. Requests use the remaining routes.";
    case "not_in_catalog": return `It isn't in a catalog available to this ${kindLabels[kind].toLowerCase()}, and a Platform Admin hasn't assigned it directly.`;
    case "not_selected": return canManageModels ? "It's available from a catalog, but this workspace hasn't added it. You can add it from Models." : "It's available from a catalog, but this workspace hasn't added it. A workspace admin can add it from Models.";
    case "key_restriction": return "This key is limited to other models. Model access is fixed when a key is created; create a new key to change it.";
    case "budget_exhausted": return `The ${period} budget of ${whereText(reason.layer, kind)} is used up.${reason.period ? ` ${resetText(reason.period)}.` : ""}`;
    case "unresolved_usage_blocking": return `Some ${period} costs of ${whereText(reason.layer, kind)} aren't known yet, so new requests that need this budget wait until they're resolved.`;
    case "protocol_unsupported": return "None of its routes supports the protocol used.";
    default: return `Unavailable (${reason.code}).`;
  }
}
/** Blocking reasons make a model unavailable; the rest (some routes off) only make it partly available. */
export const blockingReason = (reason: AccessReason) => reason.code !== "some_routes_unavailable";

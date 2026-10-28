/* #48: frozen browser-only reads. Contract: management/provider_costs.rs and
 * provider_costs/matching.rs:110; docs/provider-cost-reconciliation.md.
 * No imports/lines/mapping details are fetched; they can contain private evidence.
 */
import { api, platformPath } from "./api";
import { formatMicroUsd } from "./governance";

export const reconciliationPath = `${platformPath}/provider-reconciliations`;
export const reconciliationPageSize = 20;
export function formatSignedMicroUsd(value: string | null | undefined): string {
  if (value == null || value.length > 128 || !/^-?\d+$/.test(value)) return "Unknown";
  const amount = BigInt(value);
  return `${amount < 0n ? "-" : ""}${formatMicroUsd((amount < 0n ? -amount : amount).toString())}`;
}
export type EstimateTotals = {
  settled_estimate_microusd: string; held_floor_microusd: string; holds_upper_bound_microusd: string | null;
  attempts: string; unresolved_attempts: string; unbounded_holds: string; complete: boolean;
};
export type PersonalActivity = {
  estimates: EstimateTotals; exclusively_attributed_imported_microusd: string;
  exclusively_attributed_imported_by_category: Record<string, string>; reasons: string[];
};
export type ComparisonRow = {
  day: string; workspace_id: string | null; provider_workspace_id?: string | null; model: string | null; service_tier: string | null;
  category?: string; imported_microusd: string | null; variance_microusd: string | null; reasons: string[];
  workspace_estimates?: { workspace_id: string; totals: EstimateTotals }[]; estimate?: EstimateTotals; unresolved_coverage?: EstimateTotals;
};
export type ReconciliationMetadata = { id: string; account_id: string; import_id: string; mapping_id: string; observed_at: string };
export type ImportMetadata = { id: string; provider: string; period_start: string; period_end: string };
export type ListedReconciliation = ReconciliationMetadata & { report: ImportMetadata };
export type Reconciliation = ReconciliationMetadata & { snapshot: {
  import_id: string; mapping_id: string; query_version: string; observed_at: string;
  period: { start: string; end: string }; imported_microusd: string; estimates: EstimateTotals;
  personal_activity: PersonalActivity; rows: ComparisonRow[]; category_coverage: { category: string; status: string }[];
  mapping_provenance: { provider_workspace_id: string | null; workspace_id: string | null; connection_id: string;
    effective_start: string; effective_end: string; evidence_reference: string; exclusive: boolean; non_omg_traffic: boolean }[];
} };
type Page<T> = { data: T[]; limit: number; offset: number };

/** The API lists have only limit/offset, not period/provider filters. Join to
 * import METADATA pages for periods; never download raw evidence or recompute a
 * snapshot. Filters apply to loaded snapshots, with an explicit Load more UX.
 */
export async function readReconciliationPage(offset: number, signal: AbortSignal) {
  const page = await api<Page<ReconciliationMetadata>>(`${reconciliationPath}?limit=${reconciliationPageSize}&offset=${offset}`, { signal });
  const needed = new Set(page.data.map(r => r.import_id)), reports = new Map<string, ImportMetadata>();
  for (let start = 0; needed.size && start <= 100000; start += 200) {
    const imports = await api<Page<ImportMetadata>>(`${platformPath}/provider-cost-imports?limit=200&offset=${start}`, { signal });
    for (const report of imports.data) if (needed.delete(report.id)) reports.set(report.id, report);
    if (imports.data.length < 200) break;
  }
  if (needed.size) throw new Error("Import metadata is unavailable within the API paging limit. No periods have been inferred.");
  const full = page.data.length === reconciliationPageSize, nextOffset = offset + reconciliationPageSize;
  return { rows: page.data.map(row => ({ ...row, report: reports.get(row.import_id)! })), next: full && nextOffset <= 100000 ? nextOffset : undefined, ...(full && nextOffset > 100000 ? { limitReached: true } : {}) };
}

const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value);
const exactKeys = (value: Record<string, unknown>, keys: string[]) => Object.keys(value).length === keys.length && Object.keys(value).every(key => keys.includes(key));
/** Fail closed if a future/incorrect API attaches identity or model detail to
 * personal totals. Only the frozen identity-free projection may be displayed.
 * This supplements, never replaces, the backend's pre-storage projection.
 */
export function assertPersonalActivity(value: unknown): asserts value is PersonalActivity {
  const invalid = () => { throw new Error("Personal activity must contain identity-free totals only."); };
  if (!object(value) || !exactKeys(value, ["estimates", "exclusively_attributed_imported_microusd", "exclusively_attributed_imported_by_category", "reasons"])) return invalid();
  const totals = value.estimates;
  if (!object(totals) || !exactKeys(totals, ["settled_estimate_microusd", "held_floor_microusd", "holds_upper_bound_microusd", "attempts", "unresolved_attempts", "unbounded_holds", "complete"])) return invalid();
  const integer = (v: unknown) => typeof v === "string" && v.length <= 128 && /^-?\d+$/.test(v);
  for (const key of ["settled_estimate_microusd", "held_floor_microusd", "attempts", "unresolved_attempts", "unbounded_holds"]) if (!integer(totals[key])) return invalid();
  if (totals.holds_upper_bound_microusd !== null && !integer(totals.holds_upper_bound_microusd) || typeof totals.complete !== "boolean") return invalid();
  if (!integer(value.exclusively_attributed_imported_microusd) || !object(value.exclusively_attributed_imported_by_category) || !Object.values(value.exclusively_attributed_imported_by_category).every(integer)) return invalid();
  if (!Array.isArray(value.reasons) || !value.reasons.every(v => typeof v === "string")) return invalid();
}
export async function readReconciliation(id: string, signal: AbortSignal) {
  const result = await api<Reconciliation>(`${reconciliationPath}/${encodeURIComponent(id)}`, { signal });
  assertPersonalActivity(result.snapshot.personal_activity);
  return result;
}
const categoryLabels: Record<string, string> = { tokens: "Usage", usage: "Usage", credit: "Credit", credits: "Credits", discount: "Discount", discounts: "Discounts", tax: "Tax", taxes: "Taxes", tools: "Tools", web_search: "Web search", code_execution: "Code execution", session_usage: "Session usage", unknown: "Unknown category" };
const reasonLabels: Record<string, string> = { provider_only: "Provider only", estimate_only: "Estimate only", ambiguous_scope: "Ambiguous scope", unknown_model: "Unknown model", unsupported_category: "Unsupported category", shared_unallocated: "Shared / unallocated", incomplete_estimate: "Incomplete estimate", timing_basis: "Timing basis differs" };
export const categoryText = (category: string) => Object.hasOwn(categoryLabels, category) ? categoryLabels[category] : category;
export const reasonText = (reason: string) => Object.hasOwn(reasonLabels, reason) ? reasonLabels[reason] : reason;
export const comparisonState = (row: ComparisonRow) => row.reasons.includes("ambiguous_scope") ? "Ambiguous scope" : row.reasons.includes("incomplete_estimate") ? "Incomplete" : row.variance_microusd != null ? "Matched" : "Unmatched";
export const varianceText = (row: ComparisonRow) => row.variance_microusd == null ? row.reasons.includes("incomplete_estimate") ? "Incomplete" : "Unknown" : formatSignedMicroUsd(row.variance_microusd);

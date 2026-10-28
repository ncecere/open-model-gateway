/* Admin-only upload/mapping workflow. No parsing or rewriting uploaded evidence.
 * Contracts: management/provider_costs.rs, provider_costs/parser.rs; immutable revisions.
 */
import { api, platformPath } from "./api";
import type { ImportMetadata } from "./provider-costs";
export const importsPath = `${platformPath}/provider-cost-imports`;
export const mappingsPath = `${platformPath}/provider-cost-mappings`;
export const maxUploadBytes = 5 * 1024 * 1024;
export type CostImport = ImportMetadata & { account_id: string; supersedes: string | null; line_count: number };
export type Binding = {
  provider_workspace_id: string | null; connection_id: string; deployment_id: string | null; workspace_id: string | null;
  effective_start: string; effective_end: string; exclusive: boolean; non_omg_traffic: boolean; attested: boolean;
  evidence_reference: string; model_mappings: { provider_model: string; upstream_model: string }[];
};
export type MappingMetadata = { id: string; account_id: string; supersedes: string | null };
export type MappingRevision = MappingMetadata & { evidence_reference: string; bindings: Binding[] };
export type ImportSummary = { total: string; dimensions: (string | null)[] };
export function validateReportFile(file?: File): asserts file is File {
  if (!file) throw new Error("Choose an Anthropic report JSON file.");
  if (!file.size) throw new Error("The report file is empty.");
  if (file.size > maxUploadBytes) throw new Error("The report exceeds the 5 MiB (5,242,880 bytes) upload limit.");
  if (!/^(application\/json|application\/[a-z0-9!#$&^_.+-]+\+json)$/i.test(file.type)) throw new Error("Choose a JSON file type (application/json). Renaming a file does not validate its contents.");
}
// All paging uses supported parameters only. Refuse partial lists at the API cap.
export async function readCostPages<T>(path: string, signal: AbortSignal): Promise<T[]> {
  const rows: T[] = [];
  for (let offset = 0; offset <= 100000; offset += 200) {
    const page = await api<{ data: T[] }>(`${path}?limit=200&offset=${offset}`, { signal });
    rows.push(...page.data);
    if (page.data.length < 200) return rows;
  }
  throw new Error("API paging limit reached. Complete revisions could not be loaded; no write is enabled.");
}
export async function readImportSummary(report: CostImport, signal: AbortSignal): Promise<ImportSummary> {
  if (!Number.isInteger(report.line_count) || report.line_count < 0 || report.line_count > 50000) throw new Error("Invalid import line count.");
  const dimensions = new Set<string | null>(); let total = 0n, count = 0;
  for (let offset = 0; offset < report.line_count; offset += 200) {
    const page = await api<{ data: { amount_microusd: string; dimensions: { workspace_id: string | null } }[] }>(`${importsPath}/${encodeURIComponent(report.id)}/lines?limit=200&offset=${offset}`, { signal });
    if (!page.data.length) throw new Error("Import lines are incomplete. Reload before mapping or freezing.");
    for (const line of page.data) {
      if (typeof line.amount_microusd !== "string" || !/^-?\d{1,128}$/.test(line.amount_microusd)) throw new Error("Invalid signed import amount.");
      total += BigInt(line.amount_microusd); dimensions.add(line.dimensions.workspace_id); count++;
    }
  }
  if (count !== report.line_count) throw new Error("Import line count changed. Reload before continuing.");
  return { total: total.toString(), dimensions: [...dimensions].sort((a, b) => a === null ? -1 : b === null ? 1 : a.localeCompare(b)) };
}
export async function readLatestMapping(account: string, signal: AbortSignal): Promise<MappingRevision | null> {
  const all = (await readCostPages<MappingMetadata>(mappingsPath, signal)).filter(m => m.account_id === account);
  const replaced = new Set(all.map(m => m.supersedes));
  const current = all.filter(m => !replaced.has(m.id));
  if (!current.length && !all.length) return null;
  if (current.length !== 1) throw new Error("Cannot determine the current exact mapping revision. Reload before continuing.");
  const revision = await api<MappingRevision>(`${mappingsPath}/${encodeURIComponent(current[0].id)}`, { signal });
  return { ...revision, bindings: revision.bindings.map(writeBinding) };
}
/** GET includes binding_number; POST deny_unknown_fields requires only this allowlist. */
export function writeBinding(b: Binding): Binding {
  return { provider_workspace_id: b.provider_workspace_id, connection_id: b.connection_id, deployment_id: b.deployment_id, workspace_id: b.workspace_id,
    effective_start: b.effective_start, effective_end: b.effective_end, exclusive: b.exclusive, non_omg_traffic: b.non_omg_traffic, attested: b.attested,
    evidence_reference: b.evidence_reference, model_mappings: b.model_mappings.map(m => ({ provider_model: m.provider_model, upstream_model: m.upstream_model })) };
}
export const dimensionLabel = (id: string | null) => id === null ? "Provider default / missing identifier" : id;
export function coversPeriod(b: Binding, dimension: string | null, report: CostImport) {
  return b.provider_workspace_id === dimension && b.attested && Date.parse(b.effective_start) <= Date.parse(report.period_start) && Date.parse(b.effective_end) >= Date.parse(report.period_end);
}

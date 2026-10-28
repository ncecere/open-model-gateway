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
const dayMs = 86400000;
/** Date.parse truncates to milliseconds, so test the fraction separately.
 * Bindings are stored as PostgreSQL microseconds (sqlx truncates), which is
 * what matching.rs compares: only the first six fractional digits count.
 * Normalize integral seconds/offsets exactly; refuse leap seconds and
 * non-RFC offsets. */
function exactUtcDay(value: string): { day: number; midnight: boolean } | undefined {
  const match = /^(\d{4}-\d{2}-\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?([Zz]|[+-]\d{2}:\d{2})$/.exec(value);
  if (!match) return;
  const [, date, hours, minutes, seconds, fraction, offset] = match;
  const dateMs = Date.parse(`${date}T00:00:00Z`);
  if (!Number.isFinite(dateMs) || new Date(dateMs).toISOString().slice(0, 10) !== date || +hours > 23 || +minutes > 59 || +seconds > 59) return;
  let offsetSeconds = 0;
  if (!/^[Zz]$/.test(offset)) {
    const h = +offset.slice(1, 3), m = +offset.slice(4, 6);
    if (h > 23 || m > 59) return;
    offsetSeconds = (offset[0] === "+" ? 1 : -1) * (h * 3600 + m * 60);
  }
  const utcSeconds = +hours * 3600 + +minutes * 60 + +seconds - offsetSeconds;
  return { day: dateMs + Math.floor(utcSeconds / 86400) * dayMs, midnight: utcSeconds % 86400 === 0 && !/[1-9]/.test((fraction ?? "").slice(0, 6)) };
}
/** Reports use half-open UTC 1d buckets. A binding must cover a whole bucket
 * (matching.rs); joining two partial-day attestations cannot prove that day.
 * Union the complete buckets, not the raw timestamp intervals. */
export function coversPeriod(bindings: Binding[], dimension: string | null, report: CostImport) {
  const start = exactUtcDay(report.period_start), end = exactUtcDay(report.period_end);
  if (!start?.midnight || !end?.midnight || start.day >= end.day || end.day - start.day > 31 * dayMs) return false;
  const intervals = bindings.filter(b => b.provider_workspace_id === dimension && b.attested)
    .flatMap(b => {
      const a = exactUtcDay(b.effective_start), z = exactUtcDay(b.effective_end);
      if (!a || !z) return [];
      const first = a.day + (a.midnight ? 0 : dayMs);
      return first < z.day ? [[first, z.day]] : [];
    }).sort(([a], [b]) => a - b);
  let covered = start.day;
  for (const [a, b] of intervals) {
    if (a > covered) return false;
    covered = Math.max(covered, b);
    if (covered >= end.day) return true;
  }
  return false;
}

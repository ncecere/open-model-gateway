/*
 * Model compare (docs/management-api.md "Model compare"): 2–4 models side by
 * side. Prices are exact integer micro-USD price lines (BigInt; unknown is
 * never zero), ceilings come from the same price, metrics are 30 days of the
 * viewer's visible activity. "Best" is computed only among known values and
 * only when they differ.
 */
import type { ModelProtocol, WorkspaceKind } from "./api";
import type { WorkloadKind } from "./governance";
import { identifier } from "./permissions";

export type CompareLine = { meter: string; microusd_per_batch?: string; batch?: number; unit_label?: string; sku_label?: string; variant?: string; min_prompt_tokens?: number; not_applicable?: true };
export type ComparePrice = { pricing_version: number; lines: CompareLine[]; display_lines: string[] | null; input_token_limit: number | null; output_token_limit: number | null; created_at: string };
export type CompareMetrics = { requests: string; completed: string; failed: string; error_rate: string | null; latency_p50_ms: number | null; ttft_p50_ms: number | null; ttft_requests: string; tokens_per_second: string | null };
export type CompareModel = { id: string; public_name: string; display_name: string; enabled: boolean; protocols: ModelProtocol[]; workload: WorkloadKind; routes: number; enabled_routes: number; priced_enabled_routes: number; price: ComparePrice | null; metrics: CompareMetrics };
export type CompareResponse = { scope: "workspace" | "platform"; activity: "own" | "workspace" | "shared_workspaces"; period: { start: string; end: string }; data: CompareModel[] };
export type CompareScope = { kind: "workspace"; workspace: { id: string; name: string; kind: WorkspaceKind } } | { kind: "platform" };

export const MIN_COMPARE = 2, MAX_COMPARE = 4;
/** The URL's `ids`: distinct identifiers, at most four. */
export function parseCompareIds(raw?: string): string[] {
  return [...new Set((raw ?? "").split(","))].map(identifier).filter((v): v is string => !!v).slice(0, MAX_COMPARE);
}
export const compareReady = (ids: string[]) => ids.length >= MIN_COMPARE && ids.length <= MAX_COMPARE;
/** Toggle one id in a selection, never beyond four. */
export function toggleCompare(ids: string[], id: string, on: boolean): string[] {
  if (!on) return ids.filter(x => x !== id);
  return ids.includes(id) || ids.length >= MAX_COMPARE ? ids : [...ids, id];
}

const isInt = (v: unknown): v is string => typeof v === "string" && /^\d{1,19}$/.test(v);
/** The base (untiered, unvariant) priced line of a meter; undefined when absent or not applicable. */
export function baseLine(price: ComparePrice | null, meter: string): CompareLine | undefined {
  return price?.lines.find(l => l.meter === meter && l.min_prompt_tokens === undefined && l.variant === undefined && isInt(l.microusd_per_batch));
}
/** Exact micro-USD per batch of a meter's base line, or null (unknown, or not applicable). */
export function lineAmount(price: ComparePrice | null, meter: string): bigint | null {
  const l = baseLine(price, meter);
  return l?.microusd_per_batch !== undefined ? BigInt(l.microusd_per_batch) : null;
}
export const notApplicable = (price: ComparePrice | null, meter: string) => !!price?.lines.some(l => l.meter === meter && l.not_applicable);
/** Display text of every line except the base input/output token lines. */
export function otherLines(price: ComparePrice | null): string[] {
  if (!price) return [];
  const inBase = new Set([baseLine(price, "input_tokens"), baseLine(price, "output_tokens")]);
  return price.lines.flatMap((l, i) => inBase.has(l) || l.not_applicable ? [] : [price.display_lines?.[i] ?? `${l.sku_label ?? l.meter}`]);
}
/** Indexes of the best known values (ties all win); empty when fewer than two are known or all are equal. */
export function bestIndexes(values: (bigint | number | null | undefined)[], better: "min" | "max"): Set<number> {
  const known = values.map((v, i) => [v, i] as const).filter((e): e is readonly [bigint | number, number] => e[0] !== null && e[0] !== undefined);
  if (known.length < 2) return new Set();
  const cmp = (a: bigint | number, b: bigint | number) => (a < b ? -1 : a > b ? 1 : 0);
  const best = known.reduce((acc, [v]) => (better === "min" ? cmp(v, acc) < 0 : cmp(v, acc) > 0) ? v : acc, known[0]![0]);
  if (known.every(([v]) => cmp(v, best) === 0)) return new Set();
  return new Set(known.filter(([v]) => cmp(v, best) === 0).map(([, i]) => i));
}
/** Exact decimal strings ("0.3333", "41.5") as scaled BigInt for comparison; null stays null. */
export function decimalKey(v: string | null | undefined, scale = 6): bigint | null {
  if (v == null || !/^\d{1,30}(?:\.\d{1,12})?$/.test(v)) return null;
  const [w, f = ""] = v.split(".");
  return BigInt(w + f.slice(0, scale).padEnd(scale, "0"));
}
export function percentText(rate: string | null | undefined): string | null {
  const k = decimalKey(rate, 4);
  if (k === null) return null;
  const tenths = (k + 5n) / 10n; // basis points → tenths of a percent, rounded
  return `${tenths / 10n}.${tenths % 10n}%`;
}
export const msText = (ms: number | null | undefined) => ms == null ? null : ms >= 10000 ? `${(ms / 1000).toFixed(1)} s` : `${ms.toLocaleString("en-US")} ms`;
export const tokensText = (n: number | null | undefined) => n == null ? null : n.toLocaleString("en-US");
export const compareSearchIds = (ids: string[]) => ids.join(",");
export const comparePath = (scope: CompareScope, ids: string[]) => `${scope.kind === "platform" ? "/api/v1/platform" : `/api/v1/workspaces/${encodeURIComponent(scope.workspace.id)}`}/models/compare?ids=${ids.map(encodeURIComponent).join(",")}`;

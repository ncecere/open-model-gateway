/*
 * Pricing v3 editor model: OpenRouter-style price lines (docs/cache-pricing.md
 * "Pricing v3"). Operators type US dollars per display unit ("$ per M input
 * tokens", "$ per image", "$ per minute of audio"); the API receives exact
 * integer micro-USD per batch. Every conversion is BigInt decimal arithmetic:
 * no Number, parseFloat or toFixed ever touches money.
 *
 * A meter without a line is UNKNOWN (never free); "Free" publishes an explicit
 * "0" line; "Not applicable" publishes {meter, not_applicable:true}. Meters that
 * cannot apply to the model's workload are published as not applicable.
 */
import type { ModelProtocol } from "./api";
import { MAX_MICROUSD, type Meter, type Price, type PriceLine, type UnitMeter, type WorkloadKind } from "./governance";

export const METERS: Meter[] = ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens", "output_images", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "search_units", "requests"];
export const TOKEN_METERS = new Set<Meter>(METERS.slice(0, 6));
/** Input-family token meters: the input token ceiling bounds all of them. */
export const INPUT_TOKEN_METERS: Meter[] = METERS.slice(0, 6).filter(m => m !== "output_tokens");
/** Realtime-only audio-token meters (per M tokens, no tiers, no max units). Other workloads never publish them. */
export const AUDIO_TOKEN_METERS: Meter[] = ["input_audio_tokens", "cache_read_audio_tokens", "output_audio_tokens"];
/** Video-job-only meter (per second of generated video, resolution variants). Other workloads never publish it. */
export const VIDEO_METERS: Meter[] = ["output_video_seconds_ms"];
const ALL_METERS: Meter[] = [...METERS, ...AUDIO_TOKEN_METERS, ...VIDEO_METERS];
/** The meters a workload's price publishes lines for (realtime adds the audio-token meters, video jobs the video meter). */
export const metersFor = (workload: WorkloadKind): Meter[] => workload === "realtime" ? [...METERS, ...AUDIO_TOKEN_METERS] : workload === "videos" ? [...METERS, ...VIDEO_METERS] : METERS;
export const MAX_PRICE_LINES = 64;
export const MAX_TOKEN_LIMIT = 2147483647;
const MAX_USD_INPUT_LENGTH = 40;

// ---------------------------------------------------------------------------
// Exact decimals
// ---------------------------------------------------------------------------
export type UsdConversion = { ok: true; microusd: bigint } | { ok: false; reason: "format" | "precision" | "range" };
/** Exact USD decimal → integer micro-USD. Sub-micro-dollar precision is rejected, never rounded. */
export function usdToMicroUsd(text: string): UsdConversion {
  const value = text.trim();
  if (value.length > MAX_USD_INPUT_LENGTH || !/^\d+(?:\.\d+)?$/.test(value)) return { ok: false, reason: "format" };
  const [whole, fraction = ""] = value.split(".");
  if (/[1-9]/.test(fraction.slice(6))) return { ok: false, reason: "precision" };
  const microusd = BigInt(whole) * 1000000n + BigInt(fraction.slice(0, 6).padEnd(6, "0"));
  return microusd > MAX_MICROUSD ? { ok: false, reason: "range" } : { ok: true, microusd };
}
/** Exact integer micro-USD as the editable USD text: "15", "0.10", "0.0205". */
export function microUsdToUsdText(microusd: bigint): string {
  const whole = microusd / 1000000n, fraction = (microusd % 1000000n).toString().padStart(6, "0");
  if (fraction === "000000") return whole.toString();
  return `${whole}.${fraction.replace(/0+$/, "").padEnd(2, "0")}`;
}
/** Display money exactly like the gateway's display strings: "$15", "$0.10", "$0.0205". */
export const formatUsd = (microusd: bigint) => `$${microUsdToUsdText(microusd)}`;
const grouped = (n: bigint | number | string) => BigInt(n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
/** `ceil(count × microusd / batch)`, as the gateway charges each meter. */
export const chargeBatch = (count: bigint, microusd: bigint, batch: bigint) => (count * microusd + batch - 1n) / batch;
/**
 * The same price expressed for another batch, only when it stays a whole number of micro-dollars.
 * E.g. $0.00000333/second is 3.33 µUSD (not exact) but $0.011988/hour is 11,988 µUSD.
 */
export function convertUsd(text: string, fromBatch: number, toBatch: number): string | undefined {
  const value = text.trim();
  if (value.length > MAX_USD_INPUT_LENGTH || !/^\d+(?:\.\d+)?$/.test(value)) return;
  const [whole, fraction = ""] = value.split(".");
  const units = BigInt(whole + fraction), denominator = 10n ** BigInt(fraction.length) * BigInt(fromBatch);
  const numerator = units * 1000000n * BigInt(toBatch);
  if (numerator % denominator !== 0n) return;
  const microusd = numerator / denominator;
  return microusd > MAX_MICROUSD ? undefined : microUsdToUsdText(microusd);
}

// ---------------------------------------------------------------------------
// Meters, units and workloads
// ---------------------------------------------------------------------------
export type MeterUnit = { batch: number; unitLabel: string; /** "/M input tokens" */ display: string; /** "$ per M input tokens" */ input: string; /** "per hour" */ name: string };
export type MeterSpec = { meter: Meter; title: string; sku: string; noun: string; units: MeterUnit[]; tiers: boolean; variants: boolean; maxUnits?: { label: string; /** UI units → metered count (audio seconds → ms). */ scale: bigint; help: string }; help: string };
const tokenUnit = (noun: string): MeterUnit[] => [{ batch: 1000000, unitLabel: "/M tokens", display: `/M ${noun}`, input: `$ per M ${noun}`, name: "per M tokens" }];
const audioUnits = (what: string): MeterUnit[] => [
  { batch: 1000, unitLabel: "/second", display: "/second", input: `$ per second of ${what}`, name: "per second" },
  { batch: 60000, unitLabel: "/minute", display: "/minute", input: `$ per minute of ${what}`, name: "per minute" },
  { batch: 3600000, unitLabel: "/hour", display: "/hour", input: `$ per hour of ${what}`, name: "per hour" },
];
const token = (meter: Meter, title: string, sku: string, noun: string, help: string): MeterSpec => ({ meter, title, sku, noun, units: tokenUnit(noun), tiers: true, variants: false, help });
export const METER_SPECS: Record<Meter, MeterSpec> = {
  input_tokens: token("input_tokens", "Input tokens", "Input", "input tokens", "Uncached prompt tokens. Prompt-size tiers apply when the inclusive prompt is strictly larger than the threshold."),
  output_tokens: token("output_tokens", "Output tokens", "Output", "output tokens", "Generated tokens, including reasoning unless the provider prices it separately."),
  cache_read_tokens: token("cache_read_tokens", "Cache read", "Cache read", "cache read tokens", "Prompt tokens read from the provider's cache. A rate does not enable caching."),
  cache_write_tokens: token("cache_write_tokens", "Cache write (default)", "Cache write", "cache write tokens", "Default-TTL cache writes. TTL allocations are disjoint and never charged twice."),
  cache_write_5m_tokens: token("cache_write_5m_tokens", "Cache write (5-minute)", "Cache write (5m)", "5-minute cache write tokens", "Anthropic-style 5-minute cache writes."),
  cache_write_1h_tokens: token("cache_write_1h_tokens", "Cache write (1-hour)", "Cache write (1h)", "1-hour cache write tokens", "Anthropic-style 1-hour cache writes."),
  output_images: { meter: "output_images", title: "Image output", sku: "Image output", noun: "output images", units: [{ batch: 1, unitLabel: "/image", display: "/image", input: "$ per image", name: "per image" }], tiers: false, variants: true, maxUnits: { label: "Max images per request", scale: 1n, help: "Trusted ceiling, e.g. the largest n a request may ask for." }, help: "Per generated image. Add resolution tiers (768, 1K, 1024x1024…) for variant-specific prices; a line without a variant is the default." },
  input_characters: { meter: "input_characters", title: "Input characters", sku: "Characters", noun: "input characters", units: [{ batch: 1000000, unitLabel: "/M characters", display: "/M characters", input: "$ per M characters", name: "per M characters" }], tiers: false, variants: false, maxUnits: { label: "Max input characters per request", scale: 1n, help: "Trusted ceiling, e.g. the speech input limit (4096)." }, help: "Text-to-speech input text, per million characters." },
  input_audio_seconds_ms: { meter: "input_audio_seconds_ms", title: "Audio input", sku: "Audio input", noun: "input audio", units: audioUnits("audio input"), tiers: false, variants: false, maxUnits: { label: "Max input audio per request (seconds)", scale: 1000n, help: "Trusted ceiling for uploaded audio length, in whole seconds." }, help: "Speech-to-text audio length. Choose the unit the provider quotes; a rate that isn't a whole micro-dollar per second may be exact per hour." },
  output_audio_seconds_ms: { meter: "output_audio_seconds_ms", title: "Audio output", sku: "Audio output", noun: "output audio", units: audioUnits("audio output"), tiers: false, variants: false, maxUnits: { label: "Max output audio per request (seconds)", scale: 1000n, help: "Trusted ceiling for generated audio length, in whole seconds." }, help: "Generated speech length, for models priced per second/minute of audio." },
  search_units: { meter: "search_units", title: "Search units", sku: "Search units", noun: "search units", units: [{ batch: 1, unitLabel: "/search", display: "/search", input: "$ per search", name: "per search" }], tiers: false, variants: false, maxUnits: { label: "Max search units per request", scale: 1n, help: "Trusted ceiling for rerank search units per request." }, help: "Rerank search units (Cohere-style). OpenRouter's catalog does not publish these; enter them by hand." },
  input_audio_tokens: { ...token("input_audio_tokens", "Audio input tokens", "Audio input", "input audio tokens", "Realtime: uncached input audio tokens. Text tokens use the input/cache/output token lines."), tiers: false },
  cache_read_audio_tokens: { ...token("cache_read_audio_tokens", "Cached audio input", "Cached audio input", "cached input audio tokens", "Realtime: input audio tokens read from the provider's cache."), tiers: false },
  output_audio_tokens: { ...token("output_audio_tokens", "Audio output tokens", "Audio output", "output audio tokens", "Realtime: generated audio tokens."), tiers: false },
  output_video_seconds_ms: { meter: "output_video_seconds_ms", title: "Video output", sku: "Video output", noun: "output video", units: audioUnits("generated video"), tiers: false, variants: true, maxUnits: { label: "Max video per request (seconds)", scale: 1000n, help: "Optional trusted ceiling; each request's own seconds already bound its hold." }, help: "Per second of generated video (async video jobs). Add resolution tiers (720x1280, 1792x1024…); a line without a variant is the default." },
  requests: { meter: "requests", title: "Requests", sku: "Request", noun: "requests", units: [{ batch: 1, unitLabel: "/request", display: "/request", input: "$ per request", name: "per request" }], tiers: false, variants: false, maxUnits: { label: "Max requests per attempt", scale: 1n, help: "Normally 1: each upstream attempt is one request." }, help: "A fixed fee per upstream request. Free unless the provider lists one." },
};
export const unitFor = (meter: Meter, batch: number) => METER_SPECS[meter].units.find(u => u.batch === batch);
export const defaultBatch = (meter: Meter) => meter.endsWith("_audio_seconds_ms") ? 60000 : METER_SPECS[meter].units[0].batch;
/** Price lines of a video price: per-second video lines are present. */
const isVideoPrice = (lines: PriceLine[]) => lines.some(l => VIDEO_METERS.includes(l.meter));

export const workloadLabels: Record<WorkloadKind, string> = { generation: "Text generation", embeddings: "Embeddings", images: "Images", audio_transcriptions: "Speech to text", audio_speech: "Text to speech", rerank: "Rerank", systemone: "System One decisions", realtime: "Realtime audio", videos: "Video generation", batches: "Batch chat completions" };
/** Meters shown for a workload. All others are published as not applicable (as the OpenRouter suggestion does). */
export const WORKLOAD_METERS: Record<WorkloadKind, Meter[]> = {
  generation: ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens", "requests"],
  embeddings: ["input_tokens", "requests"],
  images: ["output_images", "input_tokens", "output_tokens", "requests"],
  audio_transcriptions: ["input_audio_seconds_ms", "input_tokens", "output_tokens", "requests"],
  audio_speech: ["input_characters", "output_audio_seconds_ms", "requests"],
  rerank: ["search_units", "input_tokens", "output_tokens", "requests"],
  systemone: ["input_tokens", "output_tokens", "requests"],
  realtime: ["input_tokens", "cache_read_tokens", "output_tokens", "input_audio_tokens", "cache_read_audio_tokens", "output_audio_tokens", "requests"],
  videos: ["output_video_seconds_ms", "requests"],
  // One hold covers the whole file: the input ceiling × lines plus every line's maximum.
  batches: ["input_tokens", "output_tokens", "cache_read_tokens", "requests"],
};
const generation = new Set<string>(["chat_completions", "responses", "messages"]);
/** One workload per model (Phase 1): chat/responses/messages are text generation; every other protocol is its own workload. */
export function workloadOf(protocols: readonly (ModelProtocol | string)[]): WorkloadKind {
  const first = protocols[0];
  if (!first || generation.has(first)) return "generation";
  return first as WorkloadKind;
}

// ---------------------------------------------------------------------------
// Editor draft
// ---------------------------------------------------------------------------
export type MeterMode = "unknown" | "priced" | "free" | "not_applicable";
export const meterModes: { value: MeterMode; label: string }[] = [{ value: "priced", label: "Priced" }, { value: "free", label: "Free · explicit $0" }, { value: "unknown", label: "Unknown · cannot assume free" }, { value: "not_applicable", label: "Not applicable" }];
/** One rate. `variant`/`minPromptTokens` are present (possibly empty) only on tier rows. */
export type RateRow = { id: string; usd: string; sku: string; variant?: string; minPromptTokens?: string; review?: boolean; note?: string; /** Native batch rate (when the draft publishes batch prices). */ batchUsd?: string };
export type MeterDraft = { mode: MeterMode; batch: number; rows: RateRow[]; maxUnits: string; review?: boolean; note?: string };
export type ImportInfo = { model: string; needsReview: boolean; warnings: string[]; endpoint?: SuggestionEndpoint; ceilings?: SuggestionCeilings; /** Ceilings the admin had entered, kept instead of the imported ones until confirmed. */ keptCeilings?: { input: string; output: string; importedInput: string; importedOutput: string } };
export type PriceDraft = { workload: WorkloadKind; shown: Meter[]; meters: Record<Meter, MeterDraft>; inputTokenLimit: string; outputTokenLimit: string; import?: ImportInfo; /** Also publish a batch price list (native batch APIs; the provider's published batch rates, never derived). */ batchPrices?: boolean };
export type PriceBody = { pricing_version: 3; input_token_limit: number; output_token_limit: number; price_lines: PriceLine[]; max_units: Partial<Record<UnitMeter, string>>; batch_price_lines?: PriceLine[] };

let rowCounter = 0;
export const newRow = (meter: Meter, extra: Partial<RateRow> = {}): RateRow => ({ id: `r${++rowCounter}`, usd: "", sku: METER_SPECS[meter].sku, ...extra });
const meterDraft = (meter: Meter, mode: MeterMode = "unknown"): MeterDraft => ({ mode, batch: defaultBatch(meter), rows: [newRow(meter)], maxUnits: "" });
/** A fresh v3 draft: every applicable meter starts Unknown, so nothing is silently free. */
export function emptyDraft(workload: WorkloadKind): PriceDraft {
  return { workload, shown: WORKLOAD_METERS[workload], meters: Object.fromEntries(ALL_METERS.map(m => [m, meterDraft(m, WORKLOAD_METERS[workload].includes(m) ? "unknown" : "not_applicable")])) as Record<Meter, MeterDraft>, inputTokenLimit: "", outputTokenLimit: workload === "embeddings" ? "0" : "" };
}
const shownFor = (workload: WorkloadKind, extra: Meter[]) => metersFor(workload).filter(m => WORKLOAD_METERS[workload].includes(m) || extra.includes(m));
const maxUnitsText = (meter: Meter, value: string | undefined) => {
  const scale = METER_SPECS[meter].maxUnits?.scale ?? 1n;
  if (!value || !/^\d+$/.test(value)) return "";
  return ((BigInt(value) + scale - 1n) / scale).toString(); // ms → whole seconds, rounded up (conservative)
};
type AnyLine = { meter: Meter; microusd_per_batch?: string | null; batch?: number; sku_label?: string; variant?: string; min_prompt_tokens?: number; not_applicable?: boolean; needs_review?: boolean; note?: string };
/** Group lines per meter into editor rows, re-expressing every row in the meter's largest batch (always exact). */
function metersFromLines(workload: WorkloadKind, lines: AnyLine[], maxUnits: Record<string, string> | null | undefined): Pick<PriceDraft, "shown" | "meters"> {
  const meters = emptyDraft(workload).meters, extra: Meter[] = [];
  for (const meter of metersFor(workload)) {
    const own = lines.filter(l => l.meter === meter);
    if (!own.length) { meters[meter] = meterDraft(meter, "unknown"); continue; }
    const review = own.some(l => l.needs_review), note = own.map(l => l.note).filter(Boolean).join(" ") || undefined;
    if (own.some(l => l.not_applicable)) { meters[meter] = { ...meterDraft(meter, "not_applicable"), review, note }; if (review) extra.push(meter); continue; }
    extra.push(meter);
    const batch = Math.max(...own.map(l => unitFor(meter, l.batch ?? 0) ? l.batch! : defaultBatch(meter)));
    const free = own.length === 1 && own[0].microusd_per_batch === "0" && own[0].variant === undefined && own[0].min_prompt_tokens === undefined;
    const rows = own.map(l => newRow(meter, {
      usd: typeof l.microusd_per_batch === "string" && /^\d+$/.test(l.microusd_per_batch) ? microUsdToUsdText(BigInt(l.microusd_per_batch) * BigInt(batch / (l.batch || batch))) : "",
      sku: l.sku_label ?? METER_SPECS[meter].sku, ...(l.variant !== undefined ? { variant: l.variant } : {}), ...(l.min_prompt_tokens !== undefined ? { minPromptTokens: String(l.min_prompt_tokens) } : {}), review: l.needs_review || undefined, note: l.note,
    }));
    // The base (no tier, no variant) row leads; tiers follow in threshold order.
    rows.sort((a, b) => Number(a.variant !== undefined || a.minPromptTokens !== undefined) - Number(b.variant !== undefined || b.minPromptTokens !== undefined));
    meters[meter] = { mode: free ? "free" : "priced", batch, rows: free ? [newRow(meter)] : rows, maxUnits: maxUnitsText(meter, maxUnits?.[meter]), review, note };
  }
  return { shown: shownFor(workload, extra), meters };
}
/** Prefill from the current price: v3 lines as-is; v1/v2 token rates become lines (v1/v2 had no request fee). */
export function draftFromPrice(price: Price, workload: WorkloadKind): PriceDraft {
  const base = { workload, inputTokenLimit: String(price.input_token_limit), outputTokenLimit: String(price.output_token_limit) };
  if (price.pricing_version === 3 && price.price_lines) return withBatchRates({ ...base, ...metersFromLines(workload, price.price_lines, price.max_units as Record<string, string>) }, price.batch_price_lines);
  const line = (meter: Meter, microusd: string | null | undefined): AnyLine[] => microusd == null ? [] : [{ meter, microusd_per_batch: microusd, batch: 1000000, sku_label: METER_SPECS[meter].sku }];
  const cache = (meter: Meter, key: "read" | "write" | "write_5m" | "write_1h"): AnyLine[] => { const r = price.cache_pricing?.[key]; return !r ? [] : r.status === "priced" ? line(meter, r.microusd_per_million) : r.status === "not_applicable" ? [{ meter, not_applicable: true }] : []; };
  const lines = [...line("input_tokens", price.input_microusd_per_million), ...line("output_tokens", price.output_microusd_per_million), ...cache("cache_read_tokens", "read"), ...cache("cache_write_tokens", "write"), ...cache("cache_write_5m_tokens", "write_5m"), ...cache("cache_write_1h_tokens", "write_1h"), { meter: "requests" as Meter, microusd_per_batch: "0", batch: 1 }];
  const draft = { ...base, ...metersFromLines(workload, lines, null) };
  for (const meter of metersFor(workload)) if (!WORKLOAD_METERS[workload].includes(meter) && !lines.some(l => l.meter === meter)) draft.meters[meter] = meterDraft(meter, "not_applicable");
  return draft;
}
/** Fill each priced row's batch rate from a published batch list (same meter, variant and tier). */
function withBatchRates(draft: PriceDraft, batch: PriceLine[] | null | undefined): PriceDraft {
  if (!batch?.length) return draft;
  const meters = { ...draft.meters };
  for (const meter of draft.shown) {
    const m = meters[meter];
    if (m.mode !== "priced") continue;
    meters[meter] = { ...m, rows: m.rows.map(row => {
      const line = batch.find(l => l.meter === meter && "batch" in l && (l.variant ?? undefined) === (row.variant?.trim() || undefined) && (l.min_prompt_tokens === undefined ? undefined : String(l.min_prompt_tokens)) === (row.minPromptTokens?.trim() || undefined)) as Extract<PriceLine, { batch: number }> | undefined;
      return line && /^\d+$/.test(line.microusd_per_batch) ? { ...row, batchUsd: microUsdToUsdText(BigInt(line.microusd_per_batch) * BigInt(m.batch / (line.batch || m.batch))) } : row;
    }) };
  }
  return { ...draft, meters, batchPrices: true };
}
export type SuggestionLine = AnyLine & { needs_review: boolean; source?: string; unit_label?: string };
/** Which OpenRouter endpoint the imported rates came from. */
export type SuggestionEndpoint = { selection: "evidence" | "cheapest" | "catalog"; provider?: string; matched_attempts?: number; endpoint_count?: number; input_rate?: string | null };
export type SuggestionCeilings = { input_token_limit: number; output_token_limit: number; context_length: number | null; max_completion_tokens: number | null };
export type PriceSuggestion = { deployment_id: string; upstream_model: string; source: "openrouter_public_catalog"; catalog_model_id: string; workload: WorkloadKind; needs_review: boolean; lines: SuggestionLine[]; warnings: string[]; display_lines: string[] | null; endpoint?: SuggestionEndpoint; ceilings?: SuggestionCeilings; draft: { pricing_version: 3; input_token_limit: number | null; output_token_limit: number | null; price_lines: PriceLine[]; max_units: Record<string, string> } };
/** An OpenRouter suggestion as an unpublished draft. Lines without a catalog price stay empty and flagged for review. */
export function draftFromSuggestion(suggestion: PriceSuggestion, workload: WorkloadKind): PriceDraft {
  const { shown, meters } = metersFromLines(workload, suggestion.lines, suggestion.draft.max_units);
  return { workload, shown, meters, inputTokenLimit: suggestion.draft.input_token_limit == null ? "" : String(suggestion.draft.input_token_limit), outputTokenLimit: suggestion.draft.output_token_limit == null ? "" : String(suggestion.draft.output_token_limit), import: { model: suggestion.catalog_model_id, needsReview: suggestion.needs_review, warnings: suggestion.warnings, endpoint: suggestion.endpoint, ceilings: suggestion.ceilings } };
}
/**
 * Re-import over a draft: rates (lines, units, per-request maxima) come from the import, but token ceilings the
 * admin already entered are kept. When they differ from the imported defaults, `import.keptCeilings` records both
 * so the editor can ask before replacing them.
 */
export function mergeImport(current: PriceDraft, imported: PriceDraft): PriceDraft {
  const applies = tokenCeilings(imported), entered = (v: string) => v.trim() !== "";
  const keepInput = applies.input && entered(current.inputTokenLimit), keepOutput = applies.output && entered(current.outputTokenLimit);
  const input = keepInput ? current.inputTokenLimit : imported.inputTokenLimit, output = keepOutput ? current.outputTokenLimit : imported.outputTokenLimit;
  const differs = (keepInput && current.inputTokenLimit.trim() !== imported.inputTokenLimit.trim()) || (keepOutput && current.outputTokenLimit.trim() !== imported.outputTokenLimit.trim());
  return { ...imported, batchPrices: current.batchPrices, inputTokenLimit: input, outputTokenLimit: output, import: imported.import && { ...imported.import, keptCeilings: differs ? { input, output, importedInput: imported.inputTokenLimit, importedOutput: imported.outputTokenLimit } : undefined } };
}
/** Accept the imported token ceilings after the admin confirms. */
export function acceptImportedCeilings(draft: PriceDraft): PriceDraft {
  const kept = draft.import?.keptCeilings;
  return kept ? { ...draft, inputTokenLimit: kept.importedInput, outputTokenLimit: kept.importedOutput, import: { ...draft.import!, keptCeilings: undefined } } : draft;
}
/** "Free model" shortcut: every shown meter that can apply publishes an explicit $0 line. */
export function markAllFree(draft: PriceDraft): PriceDraft {
  const meters = { ...draft.meters };
  for (const meter of draft.shown) if (meters[meter].mode !== "not_applicable") meters[meter] = { ...meters[meter], mode: "free", review: false, rows: meters[meter].rows.map(r => ({ ...r, review: false })) };
  return { ...draft, meters };
}
/**
 * Which token ceilings apply. Workloads whose input-family (or output) token meters are all not applicable
 * (e.g. text to speech, per-second transcription) publish 0 and hide the field; the gateway accepts a zero input
 * ceiling only in exactly that case.
 */
export function tokenCeilings(draft: Pick<PriceDraft, "shown" | "meters">): { input: boolean; output: boolean } {
  const applies = (m: Meter) => draft.shown.includes(m) && draft.meters[m].mode !== "not_applicable";
  return { input: INPUT_TOKEN_METERS.some(applies), output: applies("output_tokens") };
}
/** The same check for a published price (a missing v3 line is unknown, so it applies). */
export function priceTokenCeilings(price: Pick<Price, "pricing_version" | "price_lines">): { input: boolean; output: boolean } {
  if (price.pricing_version !== 3 || !price.price_lines) return { input: true, output: true };
  const na = (m: Meter) => price.price_lines!.some(l => l.meter === m && "not_applicable" in l);
  return { input: INPUT_TOKEN_METERS.some(m => !na(m)), output: !na("output_tokens") };
}

// ---------------------------------------------------------------------------
// Validation and body
// ---------------------------------------------------------------------------
/** Stable error keys: "limits.input", "<meter>.mode", "<meter>.rows", "<meter>.max", "<meter>.<row>.usd|variant|tier". */
export const rowKey = (meter: Meter, row: RateRow, part: "usd" | "variant" | "tier" | "batch") => `${meter}.${row.id}.${part}`;
const intError = (value: string, min: number, max: number, what: string) => /^\d+$/.test(value.trim()) && BigInt(value.trim()) >= BigInt(min) && BigInt(value.trim()) <= BigInt(max) ? undefined : `Enter ${what} from ${grouped(min)} to ${grouped(max)}, without exponent notation.`;
export const tierLabel = (row: Pick<RateRow, "variant" | "minPromptTokens">) => [row.variant?.trim() || undefined, row.minPromptTokens?.trim() && /^\d+$/.test(row.minPromptTokens.trim()) ? `prompt > ${grouped(row.minPromptTokens.trim())} tokens` : undefined].filter(Boolean).join(", ");
/** Larger units in which this exact price becomes whole micro-dollars (always offered as a fix, never applied silently). */
export function exactAlternatives(meter: Meter, usd: string, batch: number): { batch: number; usd: string; unit: MeterUnit }[] {
  return METER_SPECS[meter].units.filter(u => u.batch !== batch).flatMap(unit => { const converted = convertUsd(usd, batch, unit.batch); return converted !== undefined ? [{ batch: unit.batch, usd: converted, unit }] : []; });
}
export function usdError(meter: Meter, usd: string, batch: number): string | undefined {
  const unit = unitFor(meter, batch)!, result = usdToMicroUsd(usd);
  if (!usd.trim()) return `Enter a price ${unit.input.slice(2)}, or choose Free, Unknown or Not applicable.`;
  if (result.ok) return;
  if (result.reason === "format") return "Enter a non-negative USD amount, without commas, currency symbols or exponent notation.";
  if (result.reason === "range") return "Amount cannot exceed $9,223,372,036,854.775807 USD.";
  const better = exactAlternatives(meter, usd, batch).filter(a => a.batch > batch);
  const per = unit.input.slice(2);
  return better.length ? `$${usd.trim()} ${per} is not a whole number of micro-dollars ($0.000001). Use ${better[0].unit.name} instead ($${better[0].usd}${better[0].unit.display}).` : `$${usd.trim()} ${per} needs more precision than one micro-dollar ($0.000001) and cannot be stored exactly.`;
}
export function validateDraft(draft: PriceDraft): Record<string, string> {
  const errors: Record<string, string> = {}, ceilings = tokenCeilings(draft);
  const input = ceilings.input ? intError(draft.inputTokenLimit, 1, MAX_TOKEN_LIMIT, "a whole number of tokens") : undefined;
  if (input) errors["limits.input"] = input;
  const output = ceilings.output ? intError(draft.outputTokenLimit, 0, MAX_TOKEN_LIMIT, "a whole number of tokens") : undefined;
  if (output) errors["limits.output"] = output;
  for (const meter of draft.shown) {
    const m = draft.meters[meter], spec = METER_SPECS[meter];
    if (m.mode !== "priced") continue;
    const seen = new Set<string>();
    for (const row of m.rows) {
      const usd = usdError(meter, row.usd, m.batch);
      if (usd) errors[rowKey(meter, row, "usd")] = usd;
      if (draft.batchPrices) { const batch = usdError(meter, row.batchUsd ?? "", m.batch); if (batch) errors[rowKey(meter, row, "batch")] = batch.replace("or choose Free, Unknown or Not applicable", "or turn batch prices off"); }
      if (row.variant !== undefined && !/^[A-Za-z0-9._:-]{1,32}$/.test(row.variant.trim())) errors[rowKey(meter, row, "variant")] = "Use 1–32 letters, digits, dots, colons, underscores or hyphens (e.g. 768, 1K, 1024x1024).";
      if (row.minPromptTokens !== undefined) { const tier = intError(row.minPromptTokens, 1, MAX_TOKEN_LIMIT, "a prompt-size threshold"); if (tier) errors[rowKey(meter, row, "tier")] = tier; }
      const key = `${row.variant?.trim() ?? ""}|${row.minPromptTokens?.trim() ?? ""}`;
      if (seen.has(key)) errors[rowKey(meter, row, row.minPromptTokens !== undefined ? "tier" : row.variant !== undefined ? "variant" : "usd")] = "Each tier needs a distinct threshold or variant.";
      seen.add(key);
    }
    const groups = new Set(m.rows.map(r => r.variant?.trim() ?? ""));
    if ([...groups].some(g => !m.rows.some(r => (r.variant?.trim() ?? "") === g && r.minPromptTokens === undefined))) errors[`${meter}.rows`] = "Add a base price without a prompt-size threshold for each variant; tiers only apply above their threshold.";
    if (spec.maxUnits && m.maxUnits.trim()) {
      const max = /^\d{1,19}$/.test(m.maxUnits.trim()) ? BigInt(m.maxUnits.trim()) * spec.maxUnits.scale : -1n;
      if (max <= 0n || max > MAX_MICROUSD) errors[`${meter}.max`] = "Enter a positive whole number, or leave blank (budgeted requests are then refused).";
    }
  }
  if (draft.shown.reduce((n, meter) => n + (draft.meters[meter].mode === "priced" ? draft.meters[meter].rows.length : draft.meters[meter].mode === "unknown" ? 0 : 1), 0) + metersFor(draft.workload).filter(m => !draft.shown.includes(m)).length > MAX_PRICE_LINES) errors["limits.input"] ??= `Use at most ${MAX_PRICE_LINES} price lines.`;
  if (!Object.keys(errors).length) {
    const bound = priceBound(draftBody(draft));
    if (bound.overflow) errors["limits.input"] = "Maximum reservation exceeds $9,223,372,036,854.775807 USD. Reduce rates or ceilings.";
  }
  return errors;
}
/** The v3 POST body. Throws on invalid money: validate first. */
export function draftBody(draft: PriceDraft): PriceBody {
  const price_lines: PriceLine[] = [], batch_lines: PriceLine[] = [], max_units: Partial<Record<UnitMeter, string>> = {};
  for (const meter of metersFor(draft.workload)) {
    const m = draft.meters[meter], unit = unitFor(meter, m.batch) ?? unitFor(meter, defaultBatch(meter))!;
    if (!draft.shown.includes(meter) || m.mode === "not_applicable") { price_lines.push({ meter, not_applicable: true }); batch_lines.push({ meter, not_applicable: true }); continue; }
    if (m.mode === "unknown") continue;
    const rows = m.mode === "free" ? [{ ...m.rows[0], usd: "0", variant: undefined, minPromptTokens: undefined, sku: m.rows[0]?.sku || METER_SPECS[meter].sku }] : m.rows;
    for (const row of rows) {
      const money = usdToMicroUsd(row.usd);
      if (!money.ok) throw new Error(`Invalid ${METER_SPECS[meter].title} price.`);
      const line = { meter, microusd_per_batch: money.microusd.toString(), batch: unit.batch, unit_label: unit.unitLabel, sku_label: row.sku.trim() || METER_SPECS[meter].sku, ...(row.variant !== undefined ? { variant: row.variant.trim() } : {}), ...(row.minPromptTokens !== undefined ? { min_prompt_tokens: Number(row.minPromptTokens.trim()) } : {}) };
      price_lines.push(line);
      if (draft.batchPrices) {
        // Free meters stay free in the batch list; priced rows take their entered batch rate.
        const batch = m.mode === "free" ? usdToMicroUsd("0") : usdToMicroUsd(row.batchUsd ?? "");
        if (!batch.ok) throw new Error(`Invalid ${METER_SPECS[meter].title} batch price.`);
        batch_lines.push({ ...line, microusd_per_batch: batch.microusd.toString() });
      }
    }
    const spec = METER_SPECS[meter].maxUnits;
    if (spec && m.mode === "priced" && /^\d+$/.test(m.maxUnits.trim())) max_units[meter as UnitMeter] = (BigInt(m.maxUnits.trim()) * spec.scale).toString();
  }
  // Non-applicable token ceilings are sent as 0 (the gateway accepts a zero input ceiling only then).
  const ceilings = tokenCeilings(draft);
  return { pricing_version: 3, input_token_limit: ceilings.input ? Number(draft.inputTokenLimit.trim()) : 0, output_token_limit: ceilings.output ? Number(draft.outputTokenLimit.trim()) : 0, price_lines, max_units, ...(draft.batchPrices ? { batch_price_lines: batch_lines } : {}) };
}
export type Bound = { microusd: bigint | null; unbounded: { meter: Meter; reason: "unknown" | "no_base" | "no_ceiling" }[]; overflow: boolean };
/** Mirrors the gateway's conservative admission hold (billing::v3::bound) for a preview; the server stays authoritative. */
export function priceBound(body: Pick<PriceBody, "price_lines" | "max_units" | "input_token_limit" | "output_token_limit">): Bound {
  let total = 0n; const unbounded: Bound["unbounded"] = [];
  // A price with audio-token lines is a realtime price: one response window, where audio tokens share the token
  // ceilings, a response is one request, and no other unit meter can occur (as the gateway bounds it).
  const realtime = body.price_lines.some(l => AUDIO_TOKEN_METERS.includes(l.meter));
  // A video price bounds its video meter on max_units (each request's seconds tighten it at admission).
  for (const meter of realtime ? [...METERS, ...AUDIO_TOKEN_METERS] : isVideoPrice(body.price_lines) ? [...METERS, ...VIDEO_METERS] : METERS) {
    const lines = body.price_lines.filter(l => l.meter === meter);
    const unitCeiling = body.max_units[meter as UnitMeter] != null ? BigInt(body.max_units[meter as UnitMeter]!) : null;
    const ceiling = meter === "output_tokens" || meter === "output_audio_tokens" ? BigInt(body.output_token_limit) : TOKEN_METERS.has(meter) || AUDIO_TOKEN_METERS.includes(meter) ? BigInt(body.input_token_limit) : !realtime ? unitCeiling : meter === "requests" ? (unitCeiling !== null && unitCeiling < 1n ? null : 1n) : 0n;
    if (lines.some(l => "not_applicable" in l) || ceiling === 0n) continue;
    if (!lines.length) { unbounded.push({ meter, reason: "unknown" }); continue; }
    const priced = lines as Extract<PriceLine, { batch: number }>[];
    if ([...new Set(priced.map(l => l.variant))].some(v => !priced.some(l => l.variant === v && l.min_prompt_tokens === undefined))) { unbounded.push({ meter, reason: "no_base" }); continue; }
    const applicable = priced.filter(l => l.min_prompt_tokens === undefined || BigInt(body.input_token_limit) > BigInt(l.min_prompt_tokens));
    if (applicable.every(l => l.microusd_per_batch === "0")) continue;
    if (ceiling === null) { unbounded.push({ meter, reason: "no_ceiling" }); continue; }
    total += applicable.reduce((max, l) => { const c = chargeBatch(ceiling, BigInt(l.microusd_per_batch), BigInt(l.batch)); return c > max ? c : max; }, 0n);
  }
  return { microusd: unbounded.length ? null : total, unbounded, overflow: total > MAX_MICROUSD };
}

// ---------------------------------------------------------------------------
// Display (OpenRouter-style lines). Prefer the gateway's display strings.
// ---------------------------------------------------------------------------
const capitalized = (text: string) => text[0].toUpperCase() + text.slice(1);
/** Client mirror of the gateway's `display_line`, e.g. "$0.10/M input tokens", "$0.0205/image (768)". */
export function lineDisplay(line: PriceLine): string {
  if ("not_applicable" in line) return `${capitalized(METER_SPECS[line.meter].noun)}: not applicable`;
  const unit = unitFor(line.meter, line.batch)?.display ?? line.unit_label, qualifiers = tierLabel({ variant: line.variant, minPromptTokens: line.min_prompt_tokens?.toString() });
  // An explicit zero rate reads "Free", like the gateway's display strings.
  const amount = line.microusd_per_batch === "0" ? `${freeText(line.meter)}` : `${formatUsd(BigInt(line.microusd_per_batch))}${unit}`;
  return `${amount}${qualifiers ? ` (${qualifiers})` : ""}`;
}
/** "Requests: Free" — the one way a zero price is shown. */
export const freeText = (meter: Meter) => `${capitalized(METER_SPECS[meter].noun)}: Free`;
export type DisplayLine = { label: string; text: string; notApplicable?: boolean; unknown?: boolean };
const scalar = (label: string, noun: string, microusd: string | null | undefined): DisplayLine => microusd == null || !/^\d+$/.test(microusd) ? { label, text: `${capitalized(noun)}: unknown`, unknown: true } : { label, text: `${formatUsd(BigInt(microusd))}/M ${noun}` };
/** Every price version as labelled OpenRouter-style lines. v3 uses the server's exact `display_lines` when present. */
export function priceDisplayLines(price: Price): DisplayLine[] {
  if (price.pricing_version === 3) {
    const lines = price.price_lines ?? [];
    return lines.map((line, i) => ({ label: "not_applicable" in line ? METER_SPECS[line.meter]?.title ?? line.meter : line.sku_label, text: price.display_lines?.[i] ?? lineDisplay(line), notApplicable: "not_applicable" in line || undefined }));
  }
  const lines = [scalar("Input", "input tokens", price.input_microusd_per_million), scalar("Output", "output tokens", price.output_microusd_per_million)];
  if (!price.cache_pricing) return [...lines, { label: "Cache", text: "Cache: not configured (pricing v1)", unknown: true }];
  for (const [key, meter] of [["read", "cache_read_tokens"], ["write", "cache_write_tokens"], ["write_5m", "cache_write_5m_tokens"], ["write_1h", "cache_write_1h_tokens"]] as const) {
    const rate = price.cache_pricing[key], spec = METER_SPECS[meter];
    lines.push(rate?.status === "priced" ? scalar(spec.sku, spec.noun, rate.microusd_per_million) : rate?.status === "not_applicable" ? { label: spec.sku, text: `${capitalized(spec.noun)}: not applicable`, notApplicable: true } : { label: spec.sku, text: `${capitalized(spec.noun)}: unknown`, unknown: true });
  }
  return lines;
}
/** Meters a v3 price leaves without any line: their cost is unknown, never free. */
export const unpricedMeters = (price: Price) => price.pricing_version === 3 ? METERS.filter(m => !(price.price_lines ?? []).some(l => l.meter === m)) : [];
/** One-line summary like OpenRouter's: priced lines only, joined with " · ". */
export function priceSummary(price: Price, max?: number): string {
  const lines = priceDisplayLines(price).filter(l => !l.notApplicable);
  const shown = max === undefined ? lines : lines.slice(0, max);
  return shown.map(l => l.text).join(" · ") + (max !== undefined && lines.length > max ? ` · +${lines.length - max} more` : "");
}
/** Summary of an editor draft, for the live preview and the Add model summary. */
export function draftSummary(draft: PriceDraft): string[] {
  const lines: string[] = [];
  for (const meter of draft.shown) {
    const m = draft.meters[meter], unit = unitFor(meter, m.batch)!;
    if (m.mode === "free") lines.push(freeText(meter));
    else if (m.mode === "unknown") lines.push(`${capitalized(METER_SPECS[meter].noun)}: unknown`);
    else if (m.mode === "priced") for (const row of m.rows) { const money = usdToMicroUsd(row.usd), q = tierLabel(row); lines.push(money.ok ? `${money.microusd === 0n ? freeText(meter) : `${formatUsd(money.microusd)}${unit.display}`}${q ? ` (${q})` : ""}` : `${METER_SPECS[meter].title}: incomplete`); }
  }
  return lines;
}

// ---------------------------------------------------------------------------
// Structured display for the PriceLine kit (model and route pages).
// Amounts stay integer micro-USD strings per display unit; nothing is rounded.
// ---------------------------------------------------------------------------
/** The unit after "/" for a line ("M input tokens", "image", "minute"). */
export const unitText = (meter: Meter, batch: number) => (unitFor(meter, batch)?.display ?? `${batch} ${METER_SPECS[meter].noun}`).replace(/^\//, "");
const compactTokens = (n: number) => n >= 1000000 && n % 1000000 === 0 ? `${n / 1000000}M` : n >= 1000 && n % 1000 === 0 ? `${n / 1000}K` : grouped(n);
export type PriceTierItem = { label: string; amount: string | null; unit: string };
/** One labelled row for `PriceLines`: a single amount, or prompt-size tiers. */
export type PriceItem = { key: string; label: string; meter: Meter; amount: string | null; unit: string; tiers?: PriceTierItem[]; notApplicable?: boolean };
type PricedLine = Extract<PriceLine, { batch: number }>;
/**
 * Labelled price rows: v3 lines grouped by meter and variant (prompt-size tiers become "≤N" / ">N" rows), meters
 * the workload can use but the price leaves out as unknown (never free), and every not-applicable meter in one row.
 * v1/v2 token rates and cache statuses map to the same rows.
 */
export function priceItems(price: Price, workload: WorkloadKind): PriceItem[] {
  const items: PriceItem[] = [];
  if (price.pricing_version === 3 && price.price_lines) {
    const lines = price.price_lines, priced = lines.filter((l): l is PricedLine => !("not_applicable" in l));
    for (const meter of metersFor(workload)) {
      const own = priced.filter(l => l.meter === meter);
      if (!own.length) { if (WORKLOAD_METERS[workload].includes(meter) && !lines.some(l => l.meter === meter)) items.push({ key: meter, label: METER_SPECS[meter].sku, meter, amount: null, unit: unitText(meter, defaultBatch(meter)) }); continue; }
      for (const variant of [...new Set(own.map(l => l.variant))]) {
        const group = own.filter(l => l.variant === variant).sort((a, b) => (a.min_prompt_tokens ?? -1) - (b.min_prompt_tokens ?? -1));
        const base = group.find(l => l.min_prompt_tokens === undefined), label = `${base?.sku_label ?? group[0].sku_label}${variant ? ` (${variant})` : ""}`;
        const tiered = group.filter(l => l.min_prompt_tokens !== undefined);
        if (!tiered.length) { items.push({ key: `${meter}:${variant ?? ""}`, label, meter, amount: base!.microusd_per_batch, unit: unitText(meter, base!.batch) }); continue; }
        const tiers: PriceTierItem[] = [];
        if (base) tiers.push({ label: `≤${compactTokens(tiered[0].min_prompt_tokens!)}`, amount: base.microusd_per_batch, unit: unitText(meter, base.batch) });
        for (const l of tiered) tiers.push({ label: `>${compactTokens(l.min_prompt_tokens!)}`, amount: l.microusd_per_batch, unit: unitText(meter, l.batch) });
        items.push({ key: `${meter}:${variant ?? ""}`, label, meter, amount: base?.microusd_per_batch ?? null, unit: unitText(meter, (base ?? group[0]).batch), tiers });
      }
    }
    const na = lines.filter(l => "not_applicable" in l && WORKLOAD_METERS[workload].includes(l.meter));
    if (na.length) items.push({ key: "not_applicable", label: "Not applicable", meter: na[0].meter, amount: null, unit: "", notApplicable: true });
    return items;
  }
  const token = (meter: Meter, amount: string | null | undefined) => items.push({ key: meter, label: METER_SPECS[meter].sku, meter, amount: amount ?? null, unit: unitText(meter, 1000000) });
  token("input_tokens", price.input_microusd_per_million);
  if (workload !== "embeddings") token("output_tokens", price.output_microusd_per_million);
  if (workload === "generation") for (const [key, meter] of [["read", "cache_read_tokens"], ["write", "cache_write_tokens"], ["write_5m", "cache_write_5m_tokens"], ["write_1h", "cache_write_1h_tokens"]] as const) {
    const rate = price.cache_pricing?.[key];
    if (rate?.status === "not_applicable") continue;
    token(meter, rate?.status === "priced" ? rate.microusd_per_million : null);
  }
  return items;
}
/** The meters a workload's headline prices use: tokens for text, the native unit for images, audio and rerank. */
export const HEADLINE_METERS: Record<WorkloadKind, Meter[]> = { generation: ["input_tokens", "output_tokens"], embeddings: ["input_tokens"], images: ["output_images"], audio_transcriptions: ["input_audio_seconds_ms"], audio_speech: ["input_characters", "output_audio_seconds_ms"], rerank: ["search_units", "input_tokens"], systemone: ["input_tokens", "output_tokens"], realtime: ["input_audio_tokens", "output_audio_tokens"], videos: ["output_video_seconds_ms"], batches: ["input_tokens", "output_tokens"] };
export type BasePrice = { amount: string; batch: number; unit: string } | { amount: null; notApplicable: boolean; unit: string };
/** The untiered price of one meter (the cheapest variant when variants differ); unknown when the price leaves it out. */
export function basePrice(price: Price | null | undefined, meter: Meter): BasePrice {
  const fallbackUnit = unitText(meter, defaultBatch(meter));
  if (!price) return { amount: null, notApplicable: false, unit: fallbackUnit };
  if (price.pricing_version !== 3 || !price.price_lines) {
    const amount = meter === "input_tokens" ? price.input_microusd_per_million : meter === "output_tokens" ? price.output_microusd_per_million : null;
    return amount != null && /^\d+$/.test(amount) ? { amount, batch: 1000000, unit: unitText(meter, 1000000) } : { amount: null, notApplicable: false, unit: fallbackUnit };
  }
  const own = price.price_lines.filter(l => l.meter === meter);
  if (own.some(l => "not_applicable" in l)) return { amount: null, notApplicable: true, unit: fallbackUnit };
  const base = (own as PricedLine[]).filter(l => l.min_prompt_tokens === undefined).sort((a, b) => cheaper(a, b));
  return base[0] ? { amount: base[0].microusd_per_batch, batch: base[0].batch, unit: unitText(meter, base[0].batch) } : { amount: null, notApplicable: false, unit: fallbackUnit };
}
/** Exact comparison of two per-batch rates (a/b vs c/d by cross-multiplication). */
function cheaper(a: { microusd_per_batch: string; batch: number }, b: { microusd_per_batch: string; batch: number }) {
  const left = BigInt(a.microusd_per_batch) * BigInt(b.batch), right = BigInt(b.microusd_per_batch) * BigInt(a.batch);
  return left < right ? -1 : left > right ? 1 : 0;
}
/**
 * The cheapest known base price of `meter` across several route prices, plus whether any route leaves it unknown.
 * Rates in different units are compared exactly; the result keeps its own unit.
 */
export function cheapestPrice(prices: (Price | null | undefined)[], meter: Meter): { price: BasePrice; someUnknown: boolean; varies: boolean } {
  const all = prices.map(p => basePrice(p, meter)), known = all.filter((b): b is Extract<BasePrice, { batch: number }> => b.amount !== null);
  const someUnknown = all.some(b => b.amount === null && !b.notApplicable);
  if (!known.length) return { price: all.find(b => !("batch" in b) && !b.notApplicable) ?? all[0] ?? { amount: null, notApplicable: false, unit: unitText(meter, defaultBatch(meter)) }, someUnknown, varies: false };
  const best = [...known].sort((a, b) => cheaper({ microusd_per_batch: a.amount, batch: a.batch }, { microusd_per_batch: b.amount, batch: b.batch }))[0];
  const varies = known.some(k => cheaper({ microusd_per_batch: k.amount, batch: k.batch }, { microusd_per_batch: best.amount, batch: best.batch }) !== 0);
  return { price: best, someUnknown, varies };
}
/**
 * An exact decimal micro-USD string (the catalog's per-million rates may be fractional, e.g. "0.5") as US dollars:
 * "$0.10", "$15.00", "$0.0000005". null/invalid is unknown (null), never zero.
 */
export function formatDecimalMicroUsd(value: string | null | undefined): string | null {
  if (value == null || value.length > 64 || !/^\d+(?:\.\d+)?$/.test(value)) return null;
  const [whole, fraction = ""] = value.split("."), digits = (whole + fraction).replace(/^0+(?=\d)/, ""), scale = 6 + fraction.length;
  const padded = digits.padStart(scale + 1, "0"), dollars = padded.slice(0, padded.length - scale), cents = padded.slice(padded.length - scale).replace(/0+$/, "").padEnd(2, "0");
  return `$${dollars.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${cents}`;
}
/** Exact comparison of two non-negative decimal strings; null (unknown) sorts last. */
export function compareDecimal(a: string | null | undefined, b: string | null | undefined): number {
  const valid = (v: string | null | undefined): v is string => v != null && /^\d+(?:\.\d+)?$/.test(v);
  if (!valid(a) || !valid(b)) return valid(a) ? -1 : valid(b) ? 1 : 0;
  const [aw, af = ""] = a.split("."), [bw, bf = ""] = b.split("."), width = Math.max(af.length, bf.length);
  const x = BigInt(aw + af.padEnd(width, "0")), y = BigInt(bw + bf.padEnd(width, "0"));
  return x < y ? -1 : x > y ? 1 : 0;
}
/** A USD amount typed per M tokens ("0.15") as an exact decimal micro-USD string ("150000"); undefined when invalid. */
export function usdPerMillionFilter(text: string | undefined): string | undefined {
  if (!text?.trim()) return;
  const money = usdToMicroUsd(text);
  return money.ok ? money.microusd.toString() : undefined;
}

// ---------------------------------------------------------------------------
// Meter usage (reports)
// ---------------------------------------------------------------------------
/** Exact audio duration from milliseconds: "12 min 3.5 s". Unknown stays unknown. */
export function formatAudio(ms: string | null | undefined): string {
  if (ms == null || !/^\d+$/.test(ms) || ms.length > 128) return "Unknown";
  const total = BigInt(ms), minutes = total / 60000n, rest = total % 60000n, seconds = `${rest / 1000n}${rest % 1000n ? `.${(rest % 1000n).toString().padStart(3, "0").replace(/0+$/, "")}` : ""}`;
  return minutes ? `${grouped(minutes)} min ${seconds} s` : `${seconds} s`;
}
const singular: Partial<Record<Meter, string>> = { output_images: "output image", input_characters: "input character", search_units: "search unit", requests: "request" };
/** "1 output image", "2 output images", "1 request" (counts are exact decimal strings). */
export function countNoun(meter: Meter, count: string | number | bigint): string {
  const text = String(count), noun = text === "1" ? singular[meter] ?? METER_SPECS[meter].noun : METER_SPECS[meter].noun;
  return `${/^\d+$/.test(text) ? grouped(text) : text} ${noun}`;
}
export const meterUsageLabels: { key: UnitMeter; label: string; audio?: boolean }[] = [{ key: "output_images", label: "Images generated" }, { key: "input_characters", label: "Input characters" }, { key: "input_audio_seconds_ms", label: "Audio input", audio: true }, { key: "output_audio_seconds_ms", label: "Audio output", audio: true }, { key: "search_units", label: "Search units" }, { key: "requests", label: "Metered requests" }, { key: "output_video_seconds_ms", label: "Video output", audio: true }];
export const meterComponentLabels: { key: "output_images_microusd" | "input_characters_microusd" | "input_audio_microusd" | "output_audio_microusd" | "search_units_microusd" | "requests_microusd" | "output_video_microusd"; label: string }[] = [{ key: "output_images_microusd", label: "Images" }, { key: "input_characters_microusd", label: "Characters" }, { key: "input_audio_microusd", label: "Audio input" }, { key: "output_audio_microusd", label: "Audio output" }, { key: "search_units_microusd", label: "Search units" }, { key: "requests_microusd", label: "Requests" }, { key: "output_video_microusd", label: "Video" }];

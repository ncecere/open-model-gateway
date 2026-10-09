import { describe, expect, it } from "vitest";
import type { Price, PriceLine } from "./governance";
import { METERS as METER_LIST, acceptImportedCeilings, convertUsd, countNoun, draftBody, draftFromPrice, draftFromSuggestion, draftSummary, emptyDraft, exactAlternatives, formatAudio, lineDisplay, markAllFree, mergeImport, newRow, priceBound, priceDisplayLines, priceSummary, priceTokenCeilings, rowKey, tokenCeilings, unpricedMeters, usdError, usdToMicroUsd, validateDraft, workloadOf, type MeterMode, type PriceDraft, type PriceSuggestion } from "./pricing";
import { toggleProtocol, protocolSetError, workloadGroups } from "./model-setup";
import type { Meter } from "./governance";

/** Set one meter's mode/rows on a draft (rows given as [usd, extras]). */
function priced(draft: PriceDraft, meter: Meter, rows: [string, { variant?: string; minPromptTokens?: string }?][], extra: { mode?: MeterMode; batch?: number; maxUnits?: string } = {}): PriceDraft {
  return { ...draft, meters: { ...draft.meters, [meter]: { ...draft.meters[meter], mode: extra.mode ?? "priced", batch: extra.batch ?? draft.meters[meter].batch, maxUnits: extra.maxUnits ?? "", rows: rows.map(([usd, x]) => newRow(meter, { usd, ...x })) } } };
}
const limits = (draft: PriceDraft, input = "100000", output = "1000") => ({ ...draft, inputTokenLimit: input, outputTokenLimit: output });

describe("exact USD → integer micro-USD per batch", () => {
  it("converts display-unit dollars without floating point", () => {
    expect(usdToMicroUsd("0.0205")).toEqual({ ok: true, microusd: 20500n }); // $0.0205/image
    expect(usdToMicroUsd("15")).toEqual({ ok: true, microusd: 15000000n }); // $15/M characters
    expect(usdToMicroUsd(" 0.10 ")).toEqual({ ok: true, microusd: 100000n });
    expect(usdToMicroUsd("0.1000000000")).toEqual({ ok: true, microusd: 100000n }); // trailing zeros are exact
    expect(usdToMicroUsd("9223372036854.775807")).toEqual({ ok: true, microusd: 9223372036854775807n });
    expect(usdToMicroUsd("9223372036854.775808")).toEqual({ ok: false, reason: "range" });
    for (const bad of ["", "-1", "1e3", "$1", "1,000", ".5", "Infinity", "0x10"]) expect(usdToMicroUsd(bad)).toEqual({ ok: false, reason: "format" });
    expect(usdToMicroUsd("0.00000333")).toEqual({ ok: false, reason: "precision" });
  });
  it("rejects sub-micro-dollar rates and offers only exact larger units ($0.00000333/second)", () => {
    expect(convertUsd("0.00000333", 1000, 60000)).toBeUndefined(); // 199.8 µUSD per minute
    expect(convertUsd("0.00000333", 1000, 3600000)).toBe("0.011988"); // 11,988 µUSD per hour
    expect(convertUsd("0.20", 60000, 1000)).toBeUndefined(); // $0.00333…/second is not terminating
    expect(convertUsd("0.0025", 1000, 60000)).toBe("0.15"); // Seed Audio: $0.0025/s = $0.15/minute
    expect(exactAlternatives("input_audio_seconds_ms", "0.00000333", 1000).map(a => [a.batch, a.usd])).toEqual([[3600000, "0.011988"]]);
    expect(usdError("input_audio_seconds_ms", "0.00000333", 1000)).toBe("$0.00000333 per second of audio input is not a whole number of micro-dollars ($0.000001). Use per hour instead ($0.011988/hour).");
    expect(usdError("input_tokens", "0.0000001", 1000000)).toContain("cannot be stored exactly");
    expect(usdError("input_tokens", "", 1000000)).toContain("Free, Unknown or Not applicable");
  });
});

describe("price draft → v3 body", () => {
  it("publishes images per image with resolution variant tiers and a ceiling", () => {
    let draft = limits(emptyDraft("images"), "4000", "0");
    draft = priced(draft, "output_images", [["0.0205", { variant: "768" }], ["0.024", { variant: "1K" }], ["0.3035", { variant: "4K" }]], { maxUnits: "4" });
    draft = priced(draft, "requests", [["0"]], { mode: "free" });
    expect(validateDraft(draft)).toEqual({});
    const body = draftBody(draft);
    expect(body.price_lines.filter(l => l.meter === "output_images")).toEqual([
      { meter: "output_images", microusd_per_batch: "20500", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "768" },
      { meter: "output_images", microusd_per_batch: "24000", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "1K" },
      { meter: "output_images", microusd_per_batch: "303500", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "4K" },
    ]);
    expect(body.max_units).toEqual({ output_images: "4" });
    expect(body.price_lines).toContainEqual({ meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" });
    expect(body.price_lines).toContainEqual({ meter: "search_units", not_applicable: true }); // hidden for images
    expect(body.price_lines.filter(l => l.meter === "input_tokens")).toEqual([{ meter: "input_tokens", unknown: true }]); // left Unknown: stated explicitly, never free
    expect(priceBound(body)).toMatchObject({ microusd: null, unbounded: [{ meter: "input_tokens", reason: "unknown" }] }); // a zero output ceiling cannot be used
  });
  it("publishes $15/M characters and audio per minute with second ceilings in milliseconds", () => {
    let draft = limits(emptyDraft("audio_speech"), "1", "0");
    draft = priced(draft, "input_characters", [["15"]], { maxUnits: "4096" });
    draft = priced(draft, "output_audio_seconds_ms", [["0.15"]], { batch: 60000, maxUnits: "600" });
    draft = priced(draft, "requests", [["0"]], { mode: "free" });
    const body = draftBody(draft);
    expect(body.price_lines).toContainEqual({ meter: "input_characters", microusd_per_batch: "15000000", batch: 1000000, unit_label: "/M characters", sku_label: "Characters" });
    expect(body.price_lines).toContainEqual({ meter: "output_audio_seconds_ms", microusd_per_batch: "150000", batch: 60000, unit_label: "/minute", sku_label: "Audio output" });
    expect(body.max_units).toEqual({ input_characters: "4096", output_audio_seconds_ms: "600000" });
    // ceil(4096 × 15,000,000 / 1,000,000) + ceil(600,000 × 150,000 / 60,000) = 61,440 + 1,500,000
    expect(priceBound(body)).toEqual({ microusd: 1561440n, unbounded: [], overflow: false });
    expect(lineDisplay(body.price_lines.find(l => l.meter === "output_audio_seconds_ms")! as PriceLine)).toBe("$0.15/minute");
  });
  it("publishes prompt-size tiers and requires a base line, distinct thresholds and valid tiers", () => {
    let draft = limits(emptyDraft("generation"), "400000", "4000");
    draft = priced(draft, "input_tokens", [["0.10"], ["0.20", { minPromptTokens: "272000" }]]);
    const body = draftBody(priced(draft, "output_tokens", [["0.50"]]));
    expect(body.price_lines.filter(l => l.meter === "input_tokens")).toEqual([{ meter: "input_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" }, { meter: "input_tokens", microusd_per_batch: "200000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input", min_prompt_tokens: 272000 }]);
    expect(lineDisplay(body.price_lines[1] as PriceLine)).toBe("$0.20/M input tokens (prompt > 272,000 tokens)");
    const noBase = priced(draft, "input_tokens", [["0.20", { minPromptTokens: "272000" }]]);
    expect(validateDraft(noBase)).toHaveProperty(["input_tokens.rows"]);
    const duplicate = priced(draft, "input_tokens", [["0.10"], ["0.20", { minPromptTokens: "272000" }], ["0.30", { minPromptTokens: "272000" }]]);
    expect(Object.keys(validateDraft(duplicate)).some(k => k.endsWith(".tier"))).toBe(true);
    for (const bad of ["0", "1e5", "2147483648", ""]) expect(Object.keys(validateDraft(priced(draft, "input_tokens", [["0.10"], ["0.20", { minPromptTokens: bad }]]))).some(k => k.endsWith(".tier"))).toBe(true);
    // Tiers above the input ceiling cannot apply to the hold.
    const free = priced(priced(priced(priced(priced(priced(draft, "output_tokens", [["0.50"]]), "cache_read_tokens", [["0"]], { mode: "free" }), "cache_write_tokens", [["0"]], { mode: "not_applicable" }), "cache_write_5m_tokens", [["0"]], { mode: "not_applicable" }), "cache_write_1h_tokens", [["0"]], { mode: "not_applicable" }), "requests", [["0"]], { mode: "free" });
    expect(priceBound(draftBody({ ...free, inputTokenLimit: "272000" })).microusd).toBe(27200n + 2000n); // base 0.10 on 272K + output
    expect(priceBound(draftBody(free)).microusd).toBe(80000n + 2000n); // > 272K tier on 400K
  });
  it("validates rows, ceilings and limits exactly", () => {
    const draft = priced(limits(emptyDraft("generation")), "input_tokens", [["0.1000001"]]);
    const row = draft.meters.input_tokens.rows[0];
    expect(validateDraft(draft)[rowKey("input_tokens", row, "usd")]).toContain("cannot be stored exactly");
    expect(validateDraft({ ...draft, inputTokenLimit: "0" })).toHaveProperty(["limits.input"]);
    expect(validateDraft({ ...draft, outputTokenLimit: "1e3" })).toHaveProperty(["limits.output"]);
    expect(validateDraft(priced(limits(emptyDraft("images")), "output_images", [["0.02"]], { maxUnits: "0" }))).toHaveProperty(["output_images.max"]);
    const badVariant = priced(limits(emptyDraft("images")), "output_images", [["0.02", { variant: "bad variant" }]]);
    expect(validateDraft(badVariant)).toHaveProperty([rowKey("output_images", badVariant.meters.output_images.rows[0], "variant")]);
    const huge = priced(limits(emptyDraft("generation"), "2147483647", "2147483647"), "output_tokens", [["9223372036854.775807"]]);
    expect(validateDraft(huge)["limits.input"]).toContain("Maximum reservation exceeds");
  });
});

describe("OpenRouter suggestion → editable draft", () => {
  const suggestion: PriceSuggestion = { deployment_id: "d", upstream_model: "openai/whisper-large-v3-turbo", source: "openrouter_public_catalog", catalog_model_id: "openai/whisper-large-v3-turbo", workload: "audio_transcriptions", needs_review: true, warnings: ["catalog discount not applied; list prices are conservative"], display_lines: null,
    lines: [
      { meter: "input_audio_seconds_ms", microusd_per_batch: "11988", batch: 3600000, unit_label: "/hour", sku_label: "Audio input", needs_review: true, source: "pricing.prompt", note: "transcription prompt units are model-specific (second, hour or token); interpreted as USD per second" },
      { meter: "output_tokens", microusd_per_batch: null, batch: 1000000, unit_label: "/M tokens", sku_label: "Output", needs_review: true, source: "pricing.completion", note: "no catalog price; enter manually" },
      { meter: "input_tokens", not_applicable: true, needs_review: true, source: "workload", note: "token-priced transcription models need an input token rate instead" },
      { meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request", needs_review: false, source: "absent pricing.request" },
      ...(["cache_read_tokens", "cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens", "output_images", "input_characters", "output_audio_seconds_ms", "search_units"] as Meter[]).map(meter => ({ meter, not_applicable: true, needs_review: false })),
    ],
    draft: { pricing_version: 3, input_token_limit: null, output_token_limit: 448, price_lines: [], max_units: {} } };
  it("prefills a draft, keeps needs_review flags and leaves unknown prices empty", () => {
    const draft = draftFromSuggestion(suggestion, "audio_transcriptions");
    expect(draft.import).toEqual({ model: "openai/whisper-large-v3-turbo", needsReview: true, warnings: suggestion.warnings });
    expect(draft.meters.input_audio_seconds_ms).toMatchObject({ mode: "priced", batch: 3600000, review: true, rows: [{ usd: "0.011988", review: true }] });
    expect(draft.meters.output_tokens).toMatchObject({ mode: "priced", review: true, rows: [{ usd: "", review: true, note: "no catalog price; enter manually" }] });
    expect(draft.meters.input_tokens).toMatchObject({ mode: "not_applicable", review: true });
    expect(draft.meters.requests.mode).toBe("free");
    expect(draft).toMatchObject({ inputTokenLimit: "", outputTokenLimit: "448" });
    // Never publishable as-is: the empty price must be completed first. No input token meter applies (every
    // input-family meter is not applicable), so the input ceiling is hidden, not required, and published as 0.
    const errors = validateDraft(draft);
    expect(errors).not.toHaveProperty(["limits.input"]);
    expect(tokenCeilings(draft)).toEqual({ input: false, output: true });
    expect(Object.keys(errors).some(k => k.startsWith("output_tokens.") && k.endsWith(".usd"))).toBe(true);
  });
  it("re-expresses mixed-batch lines in the largest unit exactly and keeps image variants", () => {
    const mixed = draftFromSuggestion({ ...suggestion, lines: [{ meter: "input_audio_seconds_ms", microusd_per_batch: "3", batch: 1000, unit_label: "/second", sku_label: "Audio input", needs_review: false }, { meter: "input_audio_seconds_ms", microusd_per_batch: "100", batch: 60000, unit_label: "/minute", sku_label: "Audio input", min_prompt_tokens: 10, needs_review: false }] }, "audio_transcriptions");
    expect(mixed.meters.input_audio_seconds_ms.batch).toBe(60000);
    expect(mixed.meters.input_audio_seconds_ms.rows.map(r => r.usd)).toEqual(["0.00018", "0.0001"]);
    const images = draftFromSuggestion({ ...suggestion, workload: "images", lines: [{ meter: "output_images", microusd_per_batch: "20500", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "768", needs_review: true }, { meter: "output_images", microusd_per_batch: "303500", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "4K", needs_review: true }] }, "images");
    expect(images.meters.output_images.rows.map(r => [r.usd, r.variant])).toEqual([["0.0205", "768"], ["0.3035", "4K"]]);
  });
});

describe("display strings", () => {
  const v3: Price = { id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 100, output_token_limit: 0, cache_pricing: null, created_at: "2026-01-01T00:00:00Z",
    price_lines: [{ meter: "input_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" }, { meter: "output_tokens", microusd_per_batch: "500000", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" }, { meter: "cache_read_tokens", microusd_per_batch: "10000", batch: 1000000, unit_label: "/M tokens", sku_label: "Cache read" }, { meter: "output_images", microusd_per_batch: "20500", batch: 1, unit_label: "/image", sku_label: "Image", variant: "768" }, { meter: "search_units", not_applicable: true }],
    display_lines: ["$0.10/M input tokens", "$0.50/M output tokens", "$0.01/M cache read tokens", "$0.0205/image (768)", "Search units: not applicable"] };
  it("mirrors the gateway's display_line format", () => {
    expect(v3.price_lines!.map(lineDisplay)).toEqual(v3.display_lines);
    expect(lineDisplay({ meter: "input_characters", microusd_per_batch: "15000000", batch: 1000000, unit_label: "/M characters", sku_label: "Characters" })).toBe("$15/M characters");
    expect(lineDisplay({ meter: "input_audio_seconds_ms", microusd_per_batch: "200000", batch: 60000, unit_label: "/minute", sku_label: "Audio" })).toBe("$0.20/minute");
  });
  it("prefers server display strings, labels SKUs and keeps unknown meters visible", () => {
    const fromServer = { ...v3, display_lines: ["SERVER 1", "SERVER 2", "SERVER 3", "SERVER 4", "SERVER 5"] };
    expect(priceDisplayLines(fromServer).map(l => [l.label, l.text])).toEqual([["Input", "SERVER 1"], ["Output", "SERVER 2"], ["Cache read", "SERVER 3"], ["Image", "SERVER 4"], ["Search units", "SERVER 5"]]);
    expect(priceSummary(v3)).toBe("$0.10/M input tokens · $0.50/M output tokens · $0.01/M cache read tokens · $0.0205/image (768)");
    expect(priceSummary(v3, 2)).toBe("$0.10/M input tokens · $0.50/M output tokens · +2 more");
    expect(unpricedMeters(v3)).toEqual(["cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "requests"]);
  });
  it("renders v1/v2 history as the same OpenRouter-style lines", () => {
    const v2: Price = { ...v3, pricing_version: 2, price_lines: null, display_lines: null, input_microusd_per_million: "100000", output_microusd_per_million: "500000", cache_pricing: { read: { status: "priced", microusd_per_million: "10000" }, write: { status: "unknown" }, write_5m: { status: "not_applicable" }, write_1h: { status: "unknown" } } };
    expect(priceSummary(v2)).toBe("$0.10/M input tokens · $0.50/M output tokens · $0.01/M cache read tokens · Cache write tokens: unknown · 1-hour cache write tokens: unknown");
    expect(priceSummary({ ...v2, pricing_version: 1, cache_pricing: null })).toBe("$0.10/M input tokens · $0.50/M output tokens · Cache: not configured (pricing v1)");
    const prefill = draftFromPrice(v2, "generation");
    expect(prefill.meters.input_tokens.rows[0].usd).toBe("0.10");
    expect(prefill.meters.cache_write_5m_tokens.mode).toBe("not_applicable");
    expect(prefill.meters.cache_write_tokens.mode).toBe("unknown");
    expect(draftSummary(prefill)).toContain("$0.01/M cache read tokens");
  });
  it("formats exact audio durations and never invents zero", () => { expect(formatAudio("61500")).toBe("1 min 1.5 s"); expect(formatAudio("3600000000")).toBe("60,000 min 0 s"); expect(formatAudio("999")).toBe("0.999 s"); expect(formatAudio(null)).toBe("Unknown"); });
});

describe("workload grouping", () => {
  it("maps protocols to one workload and keeps workloads mutually exclusive", () => {
    expect(workloadOf(["responses", "messages"])).toBe("generation");
    expect(workloadOf(["audio_speech"])).toBe("audio_speech");
    expect(toggleProtocol(["chat_completions"], "responses", true)).toEqual(["chat_completions", "responses"]);
    expect(toggleProtocol(["chat_completions", "responses"], "images", true)).toEqual(["images"]);
    expect(toggleProtocol(["images"], "embeddings", true)).toEqual(["embeddings"]);
    expect(toggleProtocol(["rerank"], "rerank", false)).toEqual([]);
    for (const group of workloadGroups) expect(protocolSetError(JSON.stringify(group.protocols.map(p => p.value)))).toBeUndefined();
    expect(workloadGroups.map(g => g.label)).toEqual(["Text", "Embeddings", "Images", "Speech to text", "Text to speech", "Rerank", "System One", "Realtime audio", "Video", "Batch"]);
  });
  it("shows only the workload's meters and publishes the rest as not applicable", () => {
    const body = draftBody(limits(emptyDraft("embeddings"), "8192", "0"));
    // Every meter is stated: input_tokens and requests explicitly Unknown, the rest not applicable.
    expect(body.price_lines).toHaveLength(12);
    expect(body.price_lines.filter(l => "unknown" in l)).toEqual([{ meter: "input_tokens", unknown: true }, { meter: "requests", unknown: true }]);
    expect(body.price_lines.filter(l => !("unknown" in l)).every(l => "not_applicable" in l)).toBe(true);
    expect(priceBound(body).unbounded).toEqual([{ meter: "input_tokens", reason: "unknown" }, { meter: "requests", reason: "unknown" }]);
    expect(emptyDraft("rerank").shown).toEqual(["search_units", "input_tokens", "output_tokens", "requests"]);
  });
});

describe("token ceilings, free display and merges", () => {
  it("needs token ceilings only while a token meter can apply", () => {
    const tts = emptyDraft("audio_speech");
    expect(tokenCeilings(tts)).toEqual({ input: false, output: false });
    expect(validateDraft({ ...tts, inputTokenLimit: "", outputTokenLimit: "" })).not.toHaveProperty(["limits.input"]);
    const stt = emptyDraft("audio_transcriptions");
    expect(tokenCeilings(stt)).toEqual({ input: true, output: true });
    const sttNoTokens = { ...stt, meters: { ...stt.meters, input_tokens: { ...stt.meters.input_tokens, mode: "not_applicable" as MeterMode }, output_tokens: { ...stt.meters.output_tokens, mode: "not_applicable" as MeterMode } } };
    expect(tokenCeilings(sttNoTokens)).toEqual({ input: false, output: false });
    expect(validateDraft(emptyDraft("generation"))).toHaveProperty(["limits.input"]);
  });
  it("marks every applicable meter free and shows Free everywhere", () => {
    const free = markAllFree(emptyDraft("rerank"));
    expect(free.shown.map(m => free.meters[m].mode)).toEqual(["free", "free", "free", "free"]);
    expect(free.meters.output_images.mode).toBe("not_applicable");
    expect(draftSummary(free)).toEqual(["Search units: Free", "Input tokens: Free", "Output tokens: Free", "Requests: Free"]);
    const body = draftBody({ ...free, inputTokenLimit: "4000", outputTokenLimit: "0" });
    expect(body.input_token_limit).toBe(4000);
    expect(body.price_lines).toContainEqual({ meter: "search_units", microusd_per_batch: "0", batch: 1, unit_label: "/search", sku_label: "Search units" });
    expect(lineDisplay({ meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" })).toBe("Requests: Free");
    expect(lineDisplay({ meter: "output_images", microusd_per_batch: "0", batch: 1, unit_label: "/image", sku_label: "Image", variant: "1K" })).toBe("Output images: Free (1K)");
  });
  it("pluralises meter counts", () => {
    expect(countNoun("output_images", "1")).toBe("1 output image");
    expect(countNoun("output_images", "4")).toBe("4 output images");
    expect(countNoun("requests", "1")).toBe("1 request");
    expect(countNoun("input_characters", "4096")).toBe("4,096 input characters");
  });
  it("re-imports rates without overwriting entered ceilings until confirmed", () => {
    const current = { ...emptyDraft("generation"), inputTokenLimit: "8000", outputTokenLimit: "64" };
    const imported = { ...emptyDraft("generation"), inputTokenLimit: "8192", outputTokenLimit: "1024", import: { model: "z-ai/glm", needsReview: false, warnings: [] } };
    const merged = mergeImport(current, imported);
    expect([merged.inputTokenLimit, merged.outputTokenLimit]).toEqual(["8000", "64"]);
    expect(merged.import?.keptCeilings).toEqual({ input: "8000", output: "64", importedInput: "8192", importedOutput: "1024" });
    const accepted = acceptImportedCeilings(merged);
    expect([accepted.inputTokenLimit, accepted.outputTokenLimit, accepted.import?.keptCeilings]).toEqual(["8192", "1024", undefined]);
    // Empty ceilings take the import; equal ceilings need no question.
    expect(mergeImport(emptyDraft("generation"), imported).inputTokenLimit).toBe("8192");
    expect(mergeImport({ ...current, inputTokenLimit: "8192", outputTokenLimit: "1024" }, imported).import?.keptCeilings).toBeUndefined();
  });
  it("hides ceilings of not-applicable token meters on published prices", () => {
    const base: Price = { id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 0, output_token_limit: 0, cache_pricing: null, created_at: "2026-01-01T00:00:00Z", price_lines: METER_LIST.map(meter => ({ meter, not_applicable: true as const })) };
    expect(priceTokenCeilings(base)).toEqual({ input: false, output: false });
    expect(priceTokenCeilings({ ...base, price_lines: base.price_lines!.filter(l => l.meter !== "output_tokens") })).toEqual({ input: false, output: true });
    expect(priceTokenCeilings({ ...base, pricing_version: 2 })).toEqual({ input: true, output: true });
  });
});

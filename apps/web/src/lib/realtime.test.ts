import { describe, expect, it } from "vitest";
import type { Meter, PriceLine } from "./governance";
import { AUDIO_TOKEN_METERS, draftBody, emptyDraft, metersFor, newRow, priceBound, priceItems, type PriceDraft, type PublishLine } from "./pricing";
/** The stored form of published lines: an explicit unknown is stored as no line. */
const stored = (lines: PublishLine[]) => lines.filter((l): l is PriceLine => !("unknown" in l));
import { protocolEndpoints, protocolOptions, protocolSetError, toggleProtocol, workloadGroups } from "./model-setup";
import { modalityText, realtimeResponseState, realtimeStatusLabel } from "./requests";

const priced = (draft: PriceDraft, meter: Meter, usd: string): PriceDraft => ({ ...draft, meters: { ...draft.meters, [meter]: { ...draft.meters[meter], mode: "priced", rows: [newRow(meter, { usd })] } } });

describe("realtime model setup", () => {
  it("is its own workload with one WebSocket protocol", () => {
    const group = workloadGroups.find(g => g.workload === "realtime");
    expect(group?.protocols.map(p => p.value)).toEqual(["realtime"]);
    expect(protocolOptions.some(o => o.value === "realtime")).toBe(true);
    expect(toggleProtocol(["chat_completions"], "realtime", true)).toEqual(["realtime"]);
    expect(protocolSetError(JSON.stringify(["realtime", "chat_completions"]))).toBeTruthy();
    expect(protocolEndpoints.realtime.method).toBe("GET");
    expect(protocolEndpoints.realtime.path).toContain("/v1/realtime");
  });
});

describe("realtime pricing", () => {
  it("publishes text and audio token lines only for realtime", () => {
    expect(metersFor("generation").some(m => AUDIO_TOKEN_METERS.includes(m))).toBe(false);
    expect(draftBody({ ...emptyDraft("generation"), inputTokenLimit: "10", outputTokenLimit: "10" }).price_lines.some(l => AUDIO_TOKEN_METERS.includes(l.meter))).toBe(false);
    let draft: PriceDraft = { ...emptyDraft("realtime"), inputTokenLimit: "1000", outputTokenLimit: "100" };
    for (const [meter, usd] of [["input_tokens", "4"], ["cache_read_tokens", "0.40"], ["output_tokens", "16"], ["input_audio_tokens", "32"], ["cache_read_audio_tokens", "0.40"], ["output_audio_tokens", "64"]] as const) draft = priced(draft, meter, usd);
    draft = { ...draft, meters: { ...draft.meters, requests: { ...draft.meters.requests, mode: "free" } } };
    const body = draftBody(draft);
    expect(body.price_lines.find(l => l.meter === "output_audio_tokens")).toMatchObject({ microusd_per_batch: "64000000", batch: 1000000, unit_label: "/M tokens" });
    expect(body.price_lines.find(l => l.meter === "cache_write_tokens")).toEqual({ meter: "cache_write_tokens", not_applicable: true });
    // Mirrors the gateway's one-window hold: 1000 × ($4 + $0.40 + $32 + $0.40)/M + 100 × ($16 + $64)/M.
    expect(priceBound(body)).toEqual({ microusd: 44800n, unbounded: [], overflow: false });
    const missing = { ...body, price_lines: body.price_lines.filter(l => l.meter !== "output_audio_tokens") };
    expect(priceBound(missing).unbounded).toEqual([{ meter: "output_audio_tokens", reason: "unknown" }]);
    const items = priceItems({ id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 1000, output_token_limit: 100, cache_pricing: null, price_lines: stored(body.price_lines), created_at: "2026-10-08T00:00:00Z" }, "realtime");
    expect(items.map(i => i.meter)).toContain("input_audio_tokens");
  });
});

describe("realtime responses in Logs", () => {
  it("never turns unknown usage into zero", () => {
    expect(modalityText("12", "340", "40")).toBe("12 text · 340 audio (40 cached)");
    expect(modalityText(null, "340")).toBe("Unknown");
    expect(realtimeResponseState({ state: "settled", status: "completed" })).toBe("succeeded");
    expect(realtimeResponseState({ state: "unknown", status: null })).toBe("unknown");
    expect(realtimeResponseState({ state: "pending", status: null })).toBe("in_progress");
  });

  it("shows a response stopped by max_output_tokens as incomplete, not failed", () => {
    // Live acceptance: a 64-token audio response ended `incomplete` and was billed from its usage.
    expect(realtimeResponseState({ state: "settled", status: "incomplete" })).toBe("succeeded");
    expect(realtimeStatusLabel("incomplete")).toBe("Incomplete");
    expect(realtimeResponseState({ state: "settled", status: "failed" })).toBe("failed");
    expect(realtimeStatusLabel("failed")).toBe("Failed");
    expect(realtimeStatusLabel("completed")).toBe("Succeeded");
    expect(realtimeStatusLabel("cancelled")).toBe("Cancelled");
  });
});

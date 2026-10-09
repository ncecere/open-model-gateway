import { describe, expect, it } from "vitest";
import { batchStatus, batchStatusLabel, batchStatusTone, batchesPath, costSoFar, lineOutcomeLabel, priceListLabel, progress, stopReason, type BatchRow } from "./batches";
import { draftBody, draftFromPrice, emptyDraft, rowKey, validateDraft } from "./pricing";
import { ruleBody, ruleErrors, newDraft, conditionText } from "./alerts";
import type { Price } from "./governance";

const row = (patch: Partial<BatchRow> = {}): BatchRow => ({
  id: "batch_1", state: "in_progress", upstream_status: "in_progress", mode: "gateway", endpoint: "/v1/chat/completions", model: "company/smart", provider: "openai",
  price_tier: null, batch_price: null, total: 40, completed: 10, failed: 2, error_code: null, created_at: "2026-10-09T10:00:00Z", in_progress_at: null, finalizing_at: null, completed_at: null,
  cancel_requested_at: null, last_progress_at: null, input_file_id: "file-a", output_file_id: null, error_file_id: null, settled_microusd: "1250", held_microusd: "0", cost_unknown: false,
  workspace_id: "w", workspace_name: "Team", workspace_kind: "team", ...patch,
});

describe("batches", () => {
  it("shows the provider step while running and the final state after", () => {
    expect(batchStatus(row())).toBe("in_progress");
    expect(batchStatus(row({ state: "queued", upstream_status: null }))).toBe("validating");
    expect(batchStatus(row({ cancel_requested_at: "2026-10-09T11:00:00Z" }))).toBe("cancelling");
    expect(batchStatus(row({ state: "failed", error_code: "budget_exceeded" }))).toBe("failed");
    expect(batchStatusLabel("finalizing")).toBe("Finalizing");
    expect(batchStatusTone("completed")).toBe("success");
    expect(batchStatusTone("expired")).toBe("danger");
    expect(stopReason("budget_exceeded")).toBe("A budget was exhausted");
  });
  it("names a line outcome's state once, then its count", () => {
    expect(lineOutcomeLabel({ state: "interrupted", code: "interrupted", lines: 2 })).toBe("Interrupted · 2 lines");
    expect(lineOutcomeLabel({ state: "failed", code: "rate_limited", lines: 1 })).toBe("Failed · rate limited · 1 line");
    expect(lineOutcomeLabel({ state: "succeeded", code: null, lines: 1200 })).toBe("Succeeded · 1,200 lines");
  });
  it("counts finished lines and never shows unknown cost as zero", () => {
    expect(progress(row())).toEqual({ done: 12, total: 40, text: "12 / 40" });
    expect(costSoFar(row()).text).toBe("$0.0013"); // rounded for reading; the exact amount is the tooltip
    expect(costSoFar(row()).hint).toBeNull();
    expect(costSoFar(row()).title).toBe("Exactly $0.00125");
    // A running native batch with nothing settled yet shows its hold, never a bare $0.00 (seen live).
    const running = costSoFar(row({ mode: "native", settled_microusd: "0", held_microusd: "1888" }));
    expect(running.text).toBe("$0.00 + $0.0019 on hold");
    expect(running.hint).toBe("$0.00 settled · $0.0019 on hold");
    expect(running.title).toBe("Exactly $0.00 settled · $0.001888 on hold");
    // The hint never shows unrounded amounts (seen live: "$1.0801 · $0.230784"); exact amounts are the tooltip.
    const big = costSoFar(row({ settled_microusd: "1080100", held_microusd: "230784" }));
    expect(big.text).toBe("$1.08 + $0.23 on hold");
    expect(big.hint).toBe("$1.08 settled · $0.23 on hold");
    expect(big.title).toBe("Exactly $1.0801 settled · $0.230784 on hold");
    // One failed line (unknown cost) makes the total a lower bound, not "Unknown" and never a bare amount.
    const unknown = costSoFar(row({ cost_unknown: true, cost_unknown_attempts: "1", settled_microusd: "8271628", held_microusd: "500" }));
    expect(unknown.text).toBe("At least $8.27 + $0.00050 on hold");
    expect(unknown.hint).toBe("1 attempt with unknown cost · at least $8.27 settled · $0.00050 on hold");
    expect(unknown.title).toBe("1 attempt with unknown cost · at least $8.271628 settled · $0.0005 on hold");
    const older = costSoFar(row({ cost_unknown: true, settled_microusd: "0", held_microusd: "0" }));
    expect(older.text).toBe("At least $0.00"); expect(older.hint).toMatch(/^Some attempts' cost is unknown/);
    expect(costSoFar(row({ cost_unknown: false, settled_microusd: "8271628", held_microusd: "0" })).text).toBe("$8.27");
  });
  it("flags native batches without a published batch price", () => {
    expect(priceListLabel(row())).toBeNull();
    expect(priceListLabel(row({ mode: "native", batch_price: true }))?.text).toBe("Batch prices");
    expect(priceListLabel(row({ mode: "native", batch_price: false }))?.text).toBe("No batch price");
  });
  it("builds list paths for the workspace and the platform", () => {
    expect(batchesPath({ kind: "workspace", ws: "w1" }, "active")).toBe("/api/v1/workspaces/w1/batches?limit=50&status=active");
    expect(batchesPath({ kind: "platform" }, "bogus", 50)).toBe("/api/v1/platform/batches?limit=50&offset=50");
  });
});

describe("batch price lists", () => {
  it("publishes entered batch rates beside the standard ones, never derived", () => {
    const draft = emptyDraft("generation");
    draft.inputTokenLimit = "1000"; draft.outputTokenLimit = "100";
    for (const meter of draft.shown) draft.meters[meter] = { ...draft.meters[meter], mode: "not_applicable" };
    draft.meters.input_tokens = { ...draft.meters.input_tokens, mode: "priced", rows: [{ ...draft.meters.input_tokens.rows[0]!, usd: "2.50" }] };
    draft.meters.output_tokens = { ...draft.meters.output_tokens, mode: "priced", rows: [{ ...draft.meters.output_tokens.rows[0]!, usd: "10" }] };
    expect(draftBody(draft).batch_price_lines).toBeUndefined();
    draft.batchPrices = true;
    const errors = validateDraft(draft);
    expect(errors[rowKey("input_tokens", draft.meters.input_tokens.rows[0]!, "batch")]).toBeDefined();
    draft.meters.input_tokens.rows[0]!.batchUsd = "1.25";
    draft.meters.output_tokens.rows[0]!.batchUsd = "5";
    expect(validateDraft(draft)).toEqual({});
    const body = draftBody(draft);
    const rate = (lines: typeof body.price_lines, meter: string) => (lines.find(l => l.meter === meter) as { microusd_per_batch: string }).microusd_per_batch;
    expect(rate(body.price_lines, "input_tokens")).toBe("2500000");
    expect(rate(body.batch_price_lines!, "input_tokens")).toBe("1250000");
    expect(rate(body.batch_price_lines!, "output_tokens")).toBe("5000000");
    // The batch list covers exactly the same meters (not-applicable ones too).
    expect(body.batch_price_lines!.map(l => l.meter)).toEqual(body.price_lines.map(l => l.meter));
    // Round trip from a published price.
    const price = { id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 1000, output_token_limit: 100, cache_pricing: null, price_lines: body.price_lines, batch_price_lines: body.batch_price_lines, max_units: {}, created_at: "" } as Price;
    const again = draftFromPrice(price, "generation");
    expect(again.batchPrices).toBe(true);
    expect(again.meters.input_tokens.rows[0]!.batchUsd).toBe("1.25");
  });
});

describe("batch alert rules", () => {
  it("validates the stall window and sends only its fields", () => {
    const scope = { kind: "workspace" as const, ws: "w" };
    const stalled = { ...newDraft(scope, "batch_stalled"), name: "Stuck batches" };
    expect(stalled.window).toBe("60");
    expect(ruleErrors(stalled)).toEqual({});
    expect(ruleBody(stalled, scope)).toMatchObject({ kind: "batch_stalled", window_minutes: 60 });
    expect(ruleErrors({ ...stalled, window: "2" }).window).toBeDefined();
    const failed = { ...newDraft(scope, "batch_failed"), name: "Failed batches" };
    expect(ruleBody(failed, scope)).not.toHaveProperty("window_minutes");
    expect(conditionText({ kind: "batch_stalled", window_minutes: 30 } as Parameters<typeof conditionText>[0])).toBe("No progress for 30 min");
  });
});

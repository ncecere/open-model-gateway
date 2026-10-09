import { describe, expect, it } from "vitest";
import { jobStateTone, jobText, requestFilters, requestQuery } from "./requests";
import { dashboardSearch } from "./permissions";
import { draftBody, emptyDraft, metersFor, priceBound } from "./pricing";

describe("async jobs in Logs", () => {
  it("filters to jobs through the URL and the API query", () => {
    const search = dashboardSearch({ page: "requests", ws: "w", workload: "jobs" });
    expect(search.workload).toBe("jobs");
    expect(dashboardSearch({ page: "requests", workload: "everything" }).workload).toBeUndefined();
    expect(requestQuery(requestFilters(search)).query.get("workload")).toBe("jobs");
    expect(requestQuery(requestFilters({ page: "requests" })).query.has("workload")).toBe(false);
  });
  it("labels job states with the provider status when it differs", () => {
    expect(jobText({ id: "batch_1", kind: "batch", state: "in_progress", upstream_status: "finalizing" })).toBe("Batch · In progress (finalizing)");
    expect(jobText({ id: "video_1", kind: "video", state: "completed", upstream_status: "completed" })).toBe("Video · Completed");
    expect(jobStateTone("failed")).toBe("danger");
    expect(jobStateTone("queued")).toBe("info");
  });
});

describe("video and batch pricing", () => {
  it("publishes the video meter only for video models and bounds it on its ceiling", () => {
    expect(metersFor("videos")).toContain("output_video_seconds_ms");
    expect(metersFor("generation")).not.toContain("output_video_seconds_ms");
    expect(metersFor("batches")).not.toContain("output_video_seconds_ms");
    const draft = emptyDraft("videos");
    expect(draft.shown).toEqual(["output_video_seconds_ms", "requests"]);
    const lines = [
      { meter: "output_video_seconds_ms" as const, microusd_per_batch: "100000", batch: 1000, unit_label: "/second", sku_label: "Video", variant: "720x1280" },
      { meter: "output_video_seconds_ms" as const, microusd_per_batch: "500000", batch: 1000, unit_label: "/second", sku_label: "Video", variant: "1792x1024" },
      { meter: "requests" as const, microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" },
      ...(["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens", "output_images", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "search_units"] as const).map(meter => ({ meter, not_applicable: true as const })),
    ];
    // 12 s at the highest tier ($0.50/s) = $6.
    expect(priceBound({ price_lines: lines, max_units: { output_video_seconds_ms: "12000" }, input_token_limit: 0, output_token_limit: 0 }).microusd).toBe(6_000_000n);
    expect(priceBound({ price_lines: lines, max_units: {}, input_token_limit: 0, output_token_limit: 0 }).unbounded).toEqual([{ meter: "output_video_seconds_ms", reason: "no_ceiling" }]);
    // A video draft publishes every other meter as not applicable.
    expect(draftBody({ ...draft, inputTokenLimit: "0", outputTokenLimit: "0" }).price_lines.filter(l => "not_applicable" in l).map(l => l.meter)).not.toContain("output_video_seconds_ms");
  });
});

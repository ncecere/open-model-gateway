import { describe, expect, it } from "vitest";
import { bestIndexes, compareReady, comparePath, decimalKey, lineAmount, msText, notApplicable, otherLines, parseCompareIds, percentText, toggleCompare, type ComparePrice } from "./model-compare";
import { dashboardHref, parseDashboardLocation } from "./locations";

const price: ComparePrice = { pricing_version: 3, created_at: "2026-01-01T00:00:00Z", input_token_limit: 128000, output_token_limit: 0, display_lines: ["$0.15/M tokens", "$0.50/M tokens (prompt > 200,000 tokens)", "$9,007,199,254.740993/M tokens", "$0.075/M tokens", "Cache write: not applicable"], lines: [
  { meter: "input_tokens", microusd_per_batch: "150000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" },
  { meter: "input_tokens", microusd_per_batch: "500000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input long", min_prompt_tokens: 200000 },
  { meter: "output_tokens", microusd_per_batch: "9007199254740993", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" },
  { meter: "cache_read_tokens", microusd_per_batch: "75000", batch: 1000000, unit_label: "/M tokens", sku_label: "Cache read" },
  { meter: "cache_write_tokens", not_applicable: true },
] };

describe("model compare", () => {
  it("keeps 2 to 4 distinct ids from the URL and while picking", () => {
    expect(parseCompareIds("a,b,a,c,d,e")).toEqual(["a", "b", "c", "d"]);
    expect(parseCompareIds("a,../x")).toEqual(["a"]);
    expect(compareReady(["a"])).toBe(false);
    expect(compareReady(["a", "b"])).toBe(true);
    expect(toggleCompare(["a", "b", "c", "d"], "e", true)).toEqual(["a", "b", "c", "d"]);
    expect(toggleCompare(["a", "b"], "a", false)).toEqual(["b"]);
  });
  it("reads exact base prices with BigInt and never treats unknown or not-applicable as zero", () => {
    expect(lineAmount(price, "input_tokens")).toBe(150000n);
    expect(lineAmount(price, "output_tokens")).toBe(9007199254740993n);
    expect(lineAmount(price, "embeddings")).toBeNull();
    expect(lineAmount(null, "input_tokens")).toBeNull();
    expect(notApplicable(price, "cache_write_tokens")).toBe(true);
    expect(otherLines(price)).toEqual(["$0.50/M tokens (prompt > 200,000 tokens)", "$0.075/M tokens"]);
  });
  it("marks the best known values only when they differ", () => {
    expect([...bestIndexes([150000n, null, 90000n], "min")]).toEqual([2]);
    expect([...bestIndexes([5, 9, 9], "max")]).toEqual([1, 2]);
    expect(bestIndexes([7, 7], "min").size).toBe(0);
    expect(bestIndexes([7, null, undefined], "min").size).toBe(0);
    expect([...bestIndexes([decimalKey("41.5"), decimalKey("41.25")], "max")]).toEqual([0]);
  });
  it("formats rates and durations simply", () => {
    expect(percentText("0.3333")).toBe("33.3%");
    expect(percentText("0")).toBe("0.0%");
    expect(percentText(null)).toBeNull();
    expect(msText(850)).toBe("850 ms");
    expect(msText(12500)).toBe("12.5 s");
    expect(msText(null)).toBeNull();
  });
  it("routes workspace and admin compare pages and scopes the API path", () => {
    expect(dashboardHref({ page: "model-compare", ws: "team", ids: "a,b" })).toBe("/workspaces/team/models/compare?ids=a%2Cb");
    expect(parseDashboardLocation("/workspaces/team/models/compare?ids=a,b")).toMatchObject({ page: "model-compare", ws: "team", ids: "a,b" });
    // A model record is still a record.
    expect(parseDashboardLocation("/workspaces/team/models/m1")).toMatchObject({ page: "workspace-model", record: "m1" });
    expect(dashboardHref({ page: "platform-model-compare", ids: "a,b,c" })).toBe("/admin/models/compare?ids=a%2Cb%2Cc");
    expect(parseDashboardLocation("/admin/models/compare?ids=a,b")).toMatchObject({ page: "platform-model-compare", ids: "a,b" });
    expect(comparePath({ kind: "platform" }, ["a", "b"])).toBe("/api/v1/platform/models/compare?ids=a,b");
    expect(comparePath({ kind: "workspace", workspace: { id: "team", name: "Product", kind: "team" } }, ["a", "b"])).toBe("/api/v1/workspaces/team/models/compare?ids=a,b");
  });
});

import { afterEach, describe, expect, it, vi } from "vitest";
import { assertPersonalActivity, categoryText, formatSignedMicroUsd, readReconciliationPage } from "./provider-costs";

describe("signed reconciliation money", () => {
  it.each([
    ["-9007199254740993", "-$9,007,199,254.740993"],
    ["9007199254740993", "$9,007,199,254.740993"],
    ["-1", "-$0.000001"], ["-1234560", "-$1.23456"],
    ["1000000", "$1.00"], ["0", "$0.00"], ["-0", "$0.00"],
  ])("formats %s exactly", (value, expected) => expect(formatSignedMicroUsd(value)).toBe(expected));
  it.each([null, undefined, "", "-", "+1", "1.1", "1e6", " 1", "9".repeat(129)])("keeps invalid or missing %s unknown", value => {
    expect(formatSignedMicroUsd(value)).toBe("Unknown");
  });
});
afterEach(() => vi.unstubAllGlobals());
describe("existing frozen read contract", () => {
  it.each(["__proto__", "constructor", "toString"])("keeps an unknown provider category %s as plain text", category => {
    expect(categoryText(category)).toBe(category);
  });
  it("joins only paginated import metadata, stopping as soon as periods are known", async () => {
    const record = { id: "frozen", account_id: "account", import_id: "target", mapping_id: "mapping", observed_at: "2026-10-03T00:00:00Z" };
    const report = { id: "target", provider: "anthropic", period_start: "2026-10-01T00:00:00Z", period_end: "2026-10-02T00:00:00Z" };
    const fetch = vi.fn().mockResolvedValueOnce(Response.json({ data: Array.from({ length: 20 }, (_, i) => ({ ...record, id: `snapshot-${i}` })) }))
      .mockResolvedValueOnce(Response.json({ data: Array.from({ length: 200 }, (_, i) => ({ ...report, id: `other-${i}` })) }))
      .mockResolvedValueOnce(Response.json({ data: [report] }));
    vi.stubGlobal("fetch", fetch);
    const page = await readReconciliationPage(0, new AbortController().signal);
    expect(page.next).toBe(20); expect(page.rows).toHaveLength(20); expect(page.rows[0].report).toEqual(report);
    expect(fetch.mock.calls.map(([url]) => url)).toEqual(["/api/v1/platform/provider-reconciliations?limit=20&offset=0", "/api/v1/platform/provider-cost-imports?limit=200&offset=0", "/api/v1/platform/provider-cost-imports?limit=200&offset=200"]);
  });
  it("reports the API paging ceiling instead of claiming all snapshots were loaded", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json({ data: Array.from({ length: 20 }, (_, i) => ({ id: `frozen-${i}`, import_id: "target" })) })).mockResolvedValueOnce(Response.json({ data: [{ id: "target", provider: "anthropic", period_start: "2026-10-01T00:00:00Z", period_end: "2026-10-02T00:00:00Z" }] }));
    vi.stubGlobal("fetch", fetch);
    const page = await readReconciliationPage(100000, new AbortController().signal);
    expect(page.next).toBeUndefined(); expect(page).toHaveProperty("limitReached", true);
  });
  it("never invents a period when an import metadata page is unavailable", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(Response.json({ data: [{ id: "frozen", import_id: "missing" }] })).mockResolvedValueOnce(Response.json({ data: [] })));
    await expect(readReconciliationPage(0, new AbortController().signal)).rejects.toThrow("No periods have been inferred");
  });
  it("does not fetch imports for an empty frozen list", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ data: [] })); vi.stubGlobal("fetch", fetch);
    expect(await readReconciliationPage(0, new AbortController().signal)).toEqual({ rows: [], next: undefined });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  it.each([null, { owner_user_id: "private" }, { estimates: { model: "private-model" } }])("rejects identity-bearing or absent personal projections", value => {
    expect(() => assertPersonalActivity(value)).toThrow("identity-free totals only");
  });
});

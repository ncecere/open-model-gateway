import { afterEach, describe, expect, it, vi } from "vitest";
import { readImportSummary, readLatestMapping, readCostPages, coversPeriod, validateReportFile, type CostImport, type Binding } from "./provider-cost-writes";
const report: CostImport = { id: "import/id", account_id: "account", provider: "anthropic", period_start: "2026-10-01T00:00:00Z", period_end: "2026-10-02T00:00:00Z", supersedes: null, line_count: 201 };
afterEach(() => vi.unstubAllGlobals());
describe("provider-cost write reads", () => {
  it("pages every normalized line, sums signed BigInts exactly and distinguishes default from named dimensions", async () => {
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(Response.json({ data: url.endsWith("offset=0") ? Array.from({ length: 200 }, () => ({ amount_microusd: "-9007199254740993", dimensions: { workspace_id: null } })) : [{ amount_microusd: "3", dimensions: { workspace_id: "default" } }] })));
    vi.stubGlobal("fetch", fetch);
    expect(await readImportSummary(report, new AbortController().signal)).toEqual({ total: (-9007199254740993n * 200n + 3n).toString(), dimensions: [null, "default"] });
    expect(fetch.mock.calls.map(([url]) => url)).toEqual(["/api/v1/platform/provider-cost-imports/import%2Fid/lines?limit=200&offset=0", "/api/v1/platform/provider-cost-imports/import%2Fid/lines?limit=200&offset=200"]);
  });
  it.each([-1, 50001, 1.5, NaN])("fails closed for unbounded or invalid line_count %s", async line_count => {
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    await expect(readImportSummary({ ...report, line_count }, new AbortController().signal)).rejects.toThrow("Invalid import line count");
    expect(fetch).not.toHaveBeenCalled();
  });
  it("refuses truncated line pages, rather than presenting a partial total", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ data: [] })));
    await expect(readImportSummary(report, new AbortController().signal)).rejects.toThrow("Import lines are incomplete");
  });
  it("recognizes a valid empty report as an exact zero, not missing usage", async () => {
    expect(await readImportSummary({ ...report, line_count: 0 }, new AbortController().signal)).toEqual({ total: "0", dimensions: [] });
  });
  it("loads the exact current whole revision for the account and strips read-only binding numbers", async () => {
    const binding = { provider_workspace_id: null, connection_id: "conn", deployment_id: null, workspace_id: null, effective_start: report.period_start, effective_end: report.period_end, attested: true, exclusive: false, non_omg_traffic: true, evidence_reference: "proof", model_mappings: [{ provider_model: "claude", upstream_model: "historical-model" }] };
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(Response.json(url.includes("?") ? { data: [
      { id: "old", account_id: "account", supersedes: null }, { id: "new", account_id: "account", supersedes: "old" }, { id: "unrelated", account_id: "other", supersedes: null },
    ] } : { id: "new", account_id: "account", bindings: [{ ...binding, binding_number: 1 }] })));
    vi.stubGlobal("fetch", fetch);
    expect((await readLatestMapping("account", new AbortController().signal))?.bindings).toEqual([binding]);
    expect(fetch.mock.calls.map(([url]) => url)).toEqual(["/api/v1/platform/provider-cost-mappings?limit=200&offset=0", "/api/v1/platform/provider-cost-mappings/new"]);
  });
  it("refuses ambiguous current revisions instead of choosing one silently", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ data: [{ id: "one", account_id: "account", supersedes: null }, { id: "two", account_id: "account", supersedes: null }] })));
    await expect(readLatestMapping("account", new AbortController().signal)).rejects.toThrow("current exact mapping revision");
  });
  it("never uses unsupported account filters and refuses incomplete metadata at the paging cap", async () => {
    const fetch = vi.fn().mockImplementation(() => Promise.resolve(Response.json({ data: Array.from({ length: 200 }, () => ({})) }))); vi.stubGlobal("fetch", fetch);
    await expect(readCostPages("/api/v1/platform/provider-cost-mappings", new AbortController().signal)).rejects.toThrow("paging limit");
    expect(fetch.mock.calls).toHaveLength(501);
    expect(fetch.mock.calls.at(-1)![0]).toBe("/api/v1/platform/provider-cost-mappings?limit=200&offset=100000");
  });
  it("requires full effective coverage and keeps provider default distinct from a wildcard", () => {
    const b = { provider_workspace_id: null, attested: true, effective_start: report.period_start, effective_end: report.period_end } as Binding;
    expect(coversPeriod([b], null, report)).toBe(true); expect(coversPeriod([b], "default", report)).toBe(false);
    expect(coversPeriod([{ ...b, effective_end: "2026-10-01T12:00:00Z" }], null, report)).toBe(false);
  });
  it.each([
    { name: "adjacent days", intervals: [[0, 1], [1, 3]], expected: true },
    { name: "overlapping full days", intervals: [[1, 3], [0, 2]], expected: true },
    { name: "a gap", intervals: [[0, 1], [2, 3]], expected: false },
    { name: "subday adjacency cannot cover a daily bucket", intervals: [[0, 1.5], [1.5, 3]], expected: false },
    { name: "subday overlap covering complete buckets", intervals: [[0, 2.5], [1.5, 3]], expected: true },
  ])("computes attested daily interval-union coverage: $name", ({ intervals, expected }) => {
    const start = Date.parse(report.period_start), day = 86400000;
    const bindings = intervals.map(([a, b]) => ({ provider_workspace_id: null, attested: true, effective_start: new Date(start + a * day).toISOString(), effective_end: new Date(start + b * day).toISOString() }) as Binding);
    const period = { ...report, period_end: new Date(start + 3 * day).toISOString() };
    expect(coversPeriod(bindings, null, period)).toBe(expected);
    expect(coversPeriod(bindings.map(b => ({ ...b, attested: false })), null, period)).toBe(false);
    expect(coversPeriod(bindings, "different", period)).toBe(false);
  });
  it.each([
    { name: "fractional-midnight start excludes the day", start: "2026-10-01T00:00:00.000001Z", end: report.period_end, expected: false },
    { name: "fractional-midnight end covers the previous day", start: report.period_start, end: "2026-10-02T00:00:00.000001Z", expected: true },
    { name: "all-zero fractional midnight is exact", start: "2026-10-01T00:00:00.000000000Z", end: "2026-10-02T00:00:00.000000000Z", expected: true },
    // Stored bindings truncate microseconds toward zero relative to 2000-01-01.
    // On/after that epoch, sub-microsecond digits cannot move a day.
    { name: "sub-microsecond start truncates to stored midnight", start: "2026-10-01T00:00:00.0000009Z", end: report.period_end, expected: true },
    { name: "excess fractional digits truncate to stored midnight", start: "2026-10-01T00:00:00.0000000001Z", end: report.period_end, expected: true },
    { name: "sub-microsecond end before midnight stays on the previous day", start: report.period_start, end: "2026-10-01T23:59:59.9999999Z", expected: false },
    { name: "leap second is refused", start: "2026-09-30T23:59:60Z", end: report.period_end, expected: false },
    { name: "pre-2000 sub-microsecond start rounds up from midnight", start: "1999-01-01T00:00:00.0000009Z", end: "1999-01-02T00:00:00Z", expected: false, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-2000 sub-microsecond end rolls into next midnight", start: "1999-01-01T00:00:00Z", end: "1999-01-01T23:59:59.9999999Z", expected: true, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-2000 exact microsecond end does not roll over", start: "1999-01-01T00:00:00Z", end: "1999-01-01T23:59:59.999999000Z", expected: false, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-2000 sub-nanosecond start is discarded by chrono before storage", start: "1999-01-01T00:00:00.0000000001Z", end: "1999-01-02T00:00:00Z", expected: true, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-2000 sub-nanosecond end cannot cause a stored day rollover", start: "1999-01-01T00:00:00Z", end: "1999-01-01T23:59:59.9999990001Z", expected: false, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-2000 all-zero fraction remains midnight", start: "1999-01-01T00:00:00.000000000Z", end: "1999-01-02T00:00:00Z", expected: true, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "negative epoch remainder rounds into the PostgreSQL epoch", start: "1999-12-31T23:59:59.9999999Z", end: "2000-01-02T00:00:00Z", expected: true, period: { period_start: "2000-01-01T00:00:00Z", period_end: "2000-01-02T00:00:00Z" } },
    { name: "positive epoch remainder truncates to the PostgreSQL epoch", start: "2000-01-01T00:00:00.0000009Z", end: "2000-01-02T00:00:00Z", expected: true, period: { period_start: "2000-01-01T00:00:00Z", period_end: "2000-01-02T00:00:00Z" } },
    { name: "UTC offset controls the pre-2000 rounding direction", start: "1999-01-01T01:00:00.0000009+01:00", end: "1999-01-02T01:00:00+01:00", expected: false, period: { period_start: "1999-01-01T00:00:00Z", period_end: "1999-01-02T00:00:00Z" } },
    { name: "pre-epoch dates normalize exactly", start: "1969-12-31T19:00:00-05:00", end: "1970-01-02T00:00:00Z", expected: true, period: { period_start: "1970-01-01T00:00:00Z", period_end: "1970-01-02T00:00:00Z" } },
    { name: "positive offsets normalize to UTC midnight", start: "2026-10-01T05:30:00+05:30", end: "2026-10-02T05:30:00+05:30", expected: true },
    { name: "negative offsets cross calendar dates", start: "2026-09-30T20:00:00-04:00", end: "2026-10-01T20:00:00-04:00", expected: true },
    { name: "offset fractional-midnight start excludes the day", start: "2026-10-01T05:30:00.000001+05:30", end: "2026-10-02T05:30:00+05:30", expected: false },
    { name: "offset fractional-midnight end covers the previous day", start: "2026-09-30T20:00:00-04:00", end: "2026-10-01T20:00:00.000001-04:00", expected: true },
    { name: "non-midnight offset does not cover the day", start: "2026-10-01T00:00:00-01:00", end: "2026-10-02T00:00:00-01:00", expected: false },
    { name: "invalid offset is refused", start: "2026-10-01T00:00:00+24:00", end: report.period_end, expected: false },
    { name: "negative zero offset still identifies an exact UTC time", start: "2026-10-01T00:00:00-00:00", end: report.period_end, expected: true },
    { name: "unsupported offset format is refused", start: "2026-10-01T00:00:00+0000", end: report.period_end, expected: false },
    { name: "invalid calendar day is refused", start: "2026-09-31T00:00:00Z", end: report.period_end, expected: false },
    { name: "non-RFC format is refused", start: "2026-10-01", end: report.period_end, expected: false },
    { name: "trailing newline is refused", start: "2026-10-01T00:00:00Z\n", end: report.period_end, expected: false },
    { name: "24-hour rollover is refused", start: "2026-09-30T24:00:00Z", end: report.period_end, expected: false },
  ] as { name: string; start: string; end: string; expected: boolean; period?: Pick<CostImport, "period_start" | "period_end"> }[])("uses exact RFC 3339 whole-day boundaries: $name", ({ start, end, expected, period }) => {
    const b = { provider_workspace_id: null, attested: true, effective_start: start, effective_end: end } as Binding;
    expect(coversPeriod([b], null, period ? { ...report, ...period } : report)).toBe(expected);
  });
  it.each([
    { name: "fractional start", start: "2026-10-01T00:00:00.000001Z", end: report.period_end },
    { name: "fractional end", start: report.period_start, end: "2026-10-02T00:00:00.000001Z" },
    { name: "sub-microsecond report start", start: "2026-10-01T00:00:00.0000001Z", end: report.period_end },
    { name: "sub-microsecond report end", start: report.period_start, end: "2026-10-02T00:00:00.0000001Z" },
    { name: "non-midnight start", start: "2026-10-01T01:00:00Z", end: report.period_end },
    { name: "invalid calendar date", start: "2026-09-31T00:00:00Z", end: report.period_end },
    { name: "invalid timestamp", start: "invalid", end: report.period_end },
    { name: "empty period", start: report.period_start, end: report.period_start },
    { name: "reversed period", start: report.period_end, end: report.period_start },
    { name: "over 31 days", start: report.period_start, end: "2026-11-02T00:00:00Z" },
  ])("refuses non-aligned or invalid report periods: $name", ({ start, end }) => {
    const b = { provider_workspace_id: null, attested: true, effective_start: "2026-01-01T00:00:00Z", effective_end: "2027-01-01T00:00:00Z" } as Binding;
    expect(coversPeriod([b], null, { ...report, period_start: start, period_end: end })).toBe(false);
  });
  it("accepts an exactly 31-day report normalized from numeric offsets", () => {
    const period = { ...report, period_start: "2026-10-01T01:00:00+01:00", period_end: "2026-11-01T01:00:00+01:00" };
    const b = { provider_workspace_id: null, attested: true, effective_start: period.period_start, effective_end: period.period_end } as Binding;
    expect(coversPeriod([b], null, period)).toBe(true);
  });
  it("allows the exact 5 MiB boundary for JSON-typed files but not a filename-only guess", () => {
    expect(() => validateReportFile({ size: 5 * 1024 * 1024, type: "application/json" } as File)).not.toThrow();
    expect(() => validateReportFile({ size: 4, type: "" } as File)).toThrow("JSON file type");
  });
});

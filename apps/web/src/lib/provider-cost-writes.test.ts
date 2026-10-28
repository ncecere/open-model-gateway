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
    expect(coversPeriod(b, null, report)).toBe(true); expect(coversPeriod(b, "default", report)).toBe(false);
    expect(coversPeriod({ ...b, effective_end: "2026-10-01T12:00:00Z" }, null, report)).toBe(false);
  });
  it("allows the exact 5 MiB boundary for JSON-typed files but not a filename-only guess", () => {
    expect(() => validateReportFile({ size: 5 * 1024 * 1024, type: "application/json" } as File)).not.toThrow();
    expect(() => validateReportFile({ size: 4, type: "" } as File)).toThrow("JSON file type");
  });
});

import { describe, expect, it } from "vitest";
import { ApiError } from "../lib/api";
import { filesQuery, isFilePurpose, purposeLabel, uploadErrorText, uploadPurposes, type FilesResponse } from "../lib/files";
import { composeLimits, draftErrors, draftLimits, draftOf, limitsBody, limitsOf, limitsSummary, noLimits, parseStorage, policyRejection, storageError, storageText } from "../lib/limits";
import { dashboardHref, parseDashboardLocation } from "../lib/locations";
import { dashboardSearch } from "../lib/permissions";
import { markup, member, policy, session, team } from "../lib/test-fixtures";
import { usagePeriod } from "../lib/usage";
import { FilesPage } from "../pages/files";
import { bytesText, bytesTitle } from "./templates/storage-bar";
import { StorageUsageCard } from "../pages/usage/storage";
import { ScopeLimits } from "./scope-limits";

const GB = 1073741824;
const list: FilesResponse = {
  data: [
    { id: "file-0123456789abcdef0123456789abcdef", filename: "requests.jsonl", purpose: "batch", bytes: 2048, content_type: "application/jsonl", created_at: "2026-10-01T00:00:00Z", expires_at: "2026-10-08T00:00:00Z", status: "processed", mine: true, via_key: true },
    { id: "file-fedcba9876543210fedcba9876543210", filename: "results.jsonl", purpose: "batch_output", bytes: 512, content_type: null, created_at: "2026-10-02T00:00:00Z", expires_at: null, status: "processed", mine: false, via_key: true },
  ],
  has_more: false, scope: "workspace", storage: { quota_bytes: GB, used_bytes: 2560 }, store: { configured: true, batch: true, user_files: false }, max_bytes: 200 * 1048576,
};
const path = "/api/v1/workspaces/team/files?limit=50";

describe("Files API store in the workspace", () => {
  it("routes /workspaces/{ws}/files with a validated purpose filter", () => {
    expect(dashboardHref({ page: "files", ws: "team", purpose: "batch" })).toBe("/workspaces/team/files?purpose=batch");
    expect(parseDashboardLocation("/workspaces/team/files?purpose=vision")).toMatchObject({ page: "files", ws: "team", purpose: "vision" });
    expect(dashboardSearch({ page: "files", purpose: "fine-tune" }).purpose).toBeUndefined();
    expect(filesQuery("team", { q: "req", purpose: "batch", after: "file-1" })).toBe("/api/v1/workspaces/team/files?limit=50&search=req&purpose=batch&after=file-1");
    expect(filesQuery("team", { purpose: "nope" })).toBe(path);
    expect(isFilePurpose("batch_output")).toBe(true); expect(purposeLabel("user_data")).toBe("User data");
  });
  it("offers uploads only for purposes allowed in Settings, and explains refusals plainly", () => {
    expect(uploadPurposes(list.store).map(p => p.value)).toEqual(["batch"]);
    expect(uploadPurposes({ configured: true, batch: false, user_files: true }).map(p => p.value)).toEqual(["user_data", "vision", "assistants", "evals"]);
    expect(uploadPurposes({ configured: false, batch: true, user_files: true })).toEqual([]);
    expect(uploadErrorText(new ApiError(413, "413", "x", "storage_quota_exceeded"))).toContain("Not enough storage");
    expect(uploadErrorText(new ApiError(403, "403", "x", "file_purpose_disabled"))).toContain("turned off");
    expect(uploadErrorText(new Error("boom"))).toContain("upload failed");
  });
  it("lists files compactly with the storage bar and an Upload action", () => {
    const html = markup(<FilesPage session={session} workspace={team} />, [[path, list]]);
    for (const text of ["Files", "requests.jsonl", "results.jsonl", "Batch input", "Batch output", "Upload", "Search files", "Storage"]) expect(html).toContain(text);
    expect(html).toContain("1 GB");
    expect(html).not.toContain("File storage is off");
    expect(html).not.toContain("{&quot;custom_id"); // contents are never shown
  });
  it("says when the store is off and disables Upload", () => {
    const off = { ...list, data: [], store: { configured: false, batch: false, user_files: false } };
    const html = markup(<FilesPage session={session} workspace={member} />, [[path, { ...off, scope: "own", storage: { quota_bytes: GB, used_bytes: null } }]]);
    expect(html).toContain("File storage is off"); expect(html).toContain("Files you uploaded");
    expect(html).toMatch(/<button[^>]*disabled[^>]*>(?:<[^>]+>)*[^<]*Upload/);
    expect(html).toContain("No files yet");
  });
});

describe("one size unit everywhere (review #9)", () => {
  it("writes a file's plaintext size the same on Files, Storage cards and Settings (binary MB, exact bytes in the title)", () => {
    const file = 118 * 1048576 + 123; // the 118 MB user file that read "124 MB" on the decimal Storage card
    expect(bytesText(file)).toBe("118 MB");
    expect(bytesTitle(file)).toBe("123,732,091 bytes");
    expect(bytesText(1024 * 1048576)).toBe("1 GB"); // quotas use the same units: 1 GB = 1024 MB
  });
});

describe("Storage quota limit", () => {
  it("parses and prints sizes exactly", () => {
    expect(parseStorage("5")).toBe(5 * GB); expect(parseStorage("1.5 GB")).toBe(1.5 * GB); expect(parseStorage("500mb")).toBe(500 * 1048576);
    expect(parseStorage("2 TB")).toBe(2048 * GB); expect(parseStorage("1048577 B")).toBe(1048577); expect(parseStorage("1.0001 GB")).toBeUndefined(); expect(parseStorage("-1")).toBeUndefined();
    expect(storageText(GB)).toBe("1 GB"); expect(storageText(1.5 * GB)).toBe("1.5 GB"); expect(storageText(500 * 1048576)).toBe("500 MB"); expect(storageText(1000)).toBe("1,000 B"); expect(storageText(null)).toBe("No limit");
    expect(storageError("")).toBeUndefined(); expect(storageError("0")).toBeDefined(); expect(storageError("lots")).toBeDefined();
    for (const b of [GB, 3 * 1048576, 1000, 2047 * GB]) expect(parseStorage(storageText(b).replace(/,/g, ""))).toBe(b);
  });
  it("stacks like other limits: minimum wins, workspace layers are tighten-only", () => {
    const platform = { ...noLimits, storage_bytes: GB }, local = { ...noLimits, storage_bytes: 2 * GB };
    expect(composeLimits(platform, local).storage_bytes).toBe(GB);
    const draft = { ...draftOf(noLimits), storage: "2 GB" };
    expect(draftErrors(draft, "tighten", [{ label: "Team default", limits: platform }]).storage).toContain("Team default quota (1 GB)");
    expect(draftErrors({ ...draft, storage: "" }, "tighten", [], { ...noLimits, storage_bytes: GB }).storage).toContain("can't be removed");
    expect(draftErrors({ ...draft, storage: "500 MB" }, "tighten", [{ label: "Team default", limits: platform }], { ...noLimits, storage_bytes: GB }).storage).toBeUndefined();
    expect(draftLimits({ ...draft, storage: "500 MB" }).storage_bytes).toBe(500 * 1048576);
    expect(limitsBody(draftLimits(draft), true)).toMatchObject({ storage_bytes: 2 * GB });
    expect(limitsBody(draftLimits(draft))).not.toHaveProperty("storage_bytes");
    expect(limitsSummary({ ...noLimits, storage_bytes: GB })).toBe("1 GB storage");
    expect(policyRejection(new ApiError(400, "400", "x", "exceeds_parent_rate", { limit: "storage_bytes" }))).toMatchObject({ storage: true });
    expect(limitsOf({ ...policy, storage_bytes: GB }).storage_bytes).toBe(GB);
  });
  it("shows Storage in the workspace limits with a used/quota bar", () => {
    const p = { ...policy, storage_bytes: null }, response = { policy: p, effective: { ...p, storage_bytes: GB }, provenance: { platform_source: "type_default", platform: { ...p, storage_bytes: GB }, local: p, key: null }, mode: "inherit", budgets: [], storage: { quota_bytes: GB, used_bytes: 300 * 1048576, usage_visible: true } };
    const html = markup(<ScopeLimits mode="local" path="/api/v1/workspaces/team/policy" writable />, [["/api/v1/workspaces/team/policy", response]]);
    expect(html).toContain(">Storage<"); expect(html).toContain("Inherited: 1 GB"); expect(html).toContain("Storage used"); expect(html).toContain("300 MB");
    const key = markup(<ScopeLimits mode="key" path="/api/v1/workspaces/team/keys/k/policy" writable />, [["/api/v1/workspaces/team/keys/k/policy", response]]);
    expect(key).not.toContain(">Storage<"); expect(key).not.toContain("Storage used");
  });
});

describe("Storage usage (not charged)", () => {
  const period = usagePeriod({}), q = `?start_date=${period.start_date}&end_date=${period.end_date}`;
  it("shows GB-days with a deliberate Not charged state, never $0 or Unknown", () => {
    const usage = { cost_state: "not_charged", unit: "gb_day", total: { byte_seconds: "139156940390400", gb_days: "1.5" }, by_purpose: [{ purpose: "batch_input", byte_seconds: "92771293593600", gb_days: "1" }, { purpose: "user_file", byte_seconds: "46385646796800", gb_days: "0.5" }], recorded_through: "2026-10-09T00:00:00Z", current: { used_bytes: 1048576, quota_bytes: GB } };
    const html = markup(<StorageUsageCard workspace={team} period={period} />, [[`/api/v1/workspaces/team/usage/storage${q}`, usage]]);
    expect(html).toContain("1.5 GB-days"); expect(html).toContain("Not charged"); expect(html).toContain("Batch input"); expect(html).toContain("User files");
    expect(html).not.toContain("$0"); expect(html).not.toContain("Unknown");
  });
  it("lists workspaces for Admin with personal workspaces as totals only, and hides it from members", () => {
    const usage = { cost_state: "not_charged", total: { byte_seconds: "1", gb_days: "0.25" }, by_purpose: [], recorded_through: "2026-10-09T00:00:00Z", workspaces: [{ workspace_id: "p", name: "Alex", kind: "personal", byte_seconds: "1", gb_days: "0.25", current_bytes: 1048576, quota_bytes: GB, by_purpose: null }] };
    const html = markup(<StorageUsageCard period={period} />, [[`/api/v1/platform/usage/storage${q}`, usage]]);
    expect(html).toContain("Alex"); expect(html).toContain("Personal"); expect(html).toContain("1 MB / 1 GB"); expect(html).toContain("Not charged");
    expect(markup(<StorageUsageCard workspace={member} period={period} />)).toBe("");
  });
});

import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { agedHoldLink, ageText, canReconcileHold, floorAmount, floorText, holdRequestLink, holdsPath, holdsQuery, holdsScopeText, holdsUnavailable, mergeHoldPages, reasonText, type HoldRow, type HoldsPage } from "./holds";
import { admin, auditor, member, personal, session, team } from "./test-fixtures";

const row = (patch: Partial<HoldRow> = {}): HoldRow => ({ execution_id: "e1", root_request_id: "r1", workspace: { id: "team", name: "Product", kind: "team" }, public_model: "company/smart", provider: "openai", workload_kind: "generation", unknown_since: "2026-10-01T00:00:00Z", age_hours: 30, held_microusd: "1500", unbounded_cost: false, unresolved_reason: "missing_billing_usage", can_reconcile: false, ...patch });
const page = (patch: Partial<HoldsPage> = {}): HoldsPage => ({ observed_at: "2026-10-02T06:00:00Z", data: [], totals: { count: "0", known_floor_microusd: null, unbounded_count: "0", oldest_age_hours: null }, personal: null, has_more: false, next_cursor: null, ...patch });

describe("hold floors", () => {
  it("renders exact decimal strings beyond 2^53 without number math", () => {
    expect(floorText("9007199254740993")).toBe("At least $9,007,199,254.740993");
    expect(floorText("9007199254740993123456789")).toBe("At least $9,007,199,254,740,993,123.456789");
  });
  it("keeps the micro-USD precision of small floors", () => {
    expect(floorText("1")).toBe("At least $0.000001");
    expect(floorText("1500")).toBe("At least $0.0015");
    expect(floorText("0")).toBe("At least $0.00");
  });
  it("treats a null or malformed floor as no known floor, never zero", () => {
    for (const bad of [null, undefined, "", "abc", "1.5", "-3", "1e9", "9".repeat(129)]) expect(floorText(bad)).toBe("No floor known");
  });
});

describe("hold floor amounts", () => {
  it("gives the bare exact amount for a tile labelled at least, else no floor", () => { expect(floorAmount("9007199254740993")).toBe("$9,007,199,254.740993"); expect(floorAmount("1500")).toBe("$0.0015"); expect(floorAmount(null)).toBe("No floor known"); expect(floorAmount("1.5")).toBe("No floor known"); });
});

describe("hold age", () => {
  it.each([[24, "1 day"], [25, "1 day 1 hour"], [26, "1 day 2 hours"], [48, "2 days"], [49, "2 days 1 hour"], [168, "7 days"], [8760, "365 days"]])("%i hours is %s", (hours, text) => { expect(ageText(hours)).toBe(text); });
  it("shows sub-day and missing ages honestly", () => { expect(ageText(0)).toBe("Under 1 hour"); expect(ageText(5)).toBe("5 hours"); expect(ageText(1)).toBe("1 hour"); expect(ageText(null)).toBe("Unknown"); expect(ageText(undefined)).toBe("Unknown"); expect(ageText(-3)).toBe("Unknown"); expect(ageText(1.5)).toBe("Unknown"); });
});

describe("hold reasons", () => {
  it("uses the /costs vocabulary in words and keeps unknown codes readable", () => {
    expect(reasonText("missing_billing_usage")).toBe("Provider didn't report usage"); expect(reasonText("unbounded_cost_or_unknown_rate")).toBe("No upper limit known");
    expect(reasonText("some_new_reason")).toBe("Some new reason"); expect(reasonText(null)).toBe("Cost unknown");
  });
});

describe("hold requests", () => {
  it("builds a same-origin keyset path with limit and an encoded cursor", () => {
    expect(holdsPath()).toBe("/api/v1/platform/holds");
    expect(holdsPath("team")).toBe("/api/v1/workspaces/team/holds");
    expect(holdsPath("a/b")).toBe("/api/v1/workspaces/a%2Fb/holds");
    expect(holdsQuery()).toBe("limit=50");
    expect(holdsQuery("-1000000_00000000-0000-0000-0000-000000000001")).toBe("limit=50&cursor=-1000000_00000000-0000-0000-0000-000000000001");
    expect(holdsQuery("a b&c")).toBe("limit=50&cursor=a%20b%26c");
    expect(holdsQuery(null)).toBe("limit=50");
  });
  it("recognises an older gateway only by a 404", () => {
    expect(holdsUnavailable(new ApiError(404, "404", "Not found"))).toBe(true);
    for (const other of [new ApiError(403, "403", "No"), new ApiError(503, "503", "Slow"), new Error("boom"), undefined, null]) expect(holdsUnavailable(other)).toBe(false);
  });
  it("concatenates pages and keeps the newest totals", () => {
    const first = page({ data: [row({ execution_id: "a" })], totals: { count: "3", known_floor_microusd: "5", unbounded_count: "1", oldest_age_hours: 30 }, has_more: true, next_cursor: "c1" });
    const second = page({ data: [row({ execution_id: "b" }), row({ execution_id: "a" })], totals: { count: "2", known_floor_microusd: "4", unbounded_count: "1", oldest_age_hours: 31 }, has_more: false, next_cursor: null });
    const merged = mergeHoldPages([first, second]);
    // The last fetched page is the freshest snapshot: a hold that resolved mid-pagination leaves the totals. A row repeated across pages shows once.
    expect(merged.rows.map(r => r.execution_id)).toEqual(["a", "b"]);
    expect(merged.totals.count).toBe("2");
    expect(merged.hasMore).toBe(false); expect(merged.nextCursor).toBeNull();
    expect(mergeHoldPages([first]).hasMore).toBe(true); expect(mergeHoldPages([first]).nextCursor).toBe("c1");
    expect(mergeHoldPages([])).toMatchObject({ rows: [], hasMore: false, nextCursor: null });
  });
});

describe("hold visibility", () => {
  it("opens the request only where the existing request page is visible", () => {
    expect(holdRequestLink(session, row(), team)).toEqual({ page: "request-detail", ws: "team", record: "r1" });
    expect(holdRequestLink(session, row({ workspace: { id: "personal", name: "Personal", kind: "personal" } }), personal)).toEqual({ page: "request-detail", ws: "personal", record: "r1" });
    expect(holdRequestLink(admin, row())).toEqual({ page: "platform-log-detail", record: "r1" });
    expect(holdRequestLink(auditor, row())).toEqual({ page: "platform-log-detail", record: "r1" });
    // No platform authority on the platform route; a workspace the viewer cannot open; never a personal row on the platform route.
    expect(holdRequestLink(session, row())).toBeUndefined();
    expect(holdRequestLink(session, row(), { ...team, role: null })).toBeUndefined();
    expect(holdRequestLink(admin, row({ workspace: { id: "x", name: "Someone", kind: "personal" } }))).toBeUndefined();
  });
  it("offers reconcile only for a can_reconcile row to a Platform Admin", () => {
    expect(canReconcileHold(admin, row({ can_reconcile: true }))).toBe(true);
    expect(canReconcileHold(admin, row({ can_reconcile: false }))).toBe(false);
    expect(canReconcileHold(auditor, row({ can_reconcile: true }))).toBe(false);
    expect(canReconcileHold(session, row({ can_reconcile: true }))).toBe(false);
  });
  it("never implies a member sees more than their own holds", () => {
    expect(holdsScopeText(team)).toBe("Everyone in Product");
    expect(holdsScopeText(member)).toBe("Only holds from your own keys in Product");
    expect(holdsScopeText(personal)).toBe("Your holds");
    expect(holdsScopeText()).toContain("Team and Project");
    expect(holdsScopeText()).toContain("Personal workspaces appear only as one combined total");
  });
});

describe("aged unknown-hold notification link", () => {
  const team1 = { id: "team", name: "Product", kind: "team" as const };
  it("opens the viewer's workspace tab when they belong to it, else Admin's queue for platform readers, else nothing", () => {
    expect(agedHoldLink(session, { kind: "aged_unknown_hold", workspace: team1 })).toEqual({ page: "costs", ws: "team", tab: "holds" });
    // A platform reader outside the workspace portal uses the platform queue (the workspace route would refuse them).
    expect(agedHoldLink(admin, { kind: "aged_unknown_hold", workspace: { ...team1, id: "other" } })).toEqual({ page: "platform-costs", tab: "holds" });
    expect(agedHoldLink(auditor, { kind: "aged_unknown_hold", workspace: null })).toEqual({ page: "platform-costs", tab: "holds" });
    expect(agedHoldLink(session, { kind: "aged_unknown_hold", workspace: null })).toBeUndefined();
    expect(agedHoldLink(session, { kind: "aged_unknown_hold", workspace: { ...team1, id: "other" } })).toBeUndefined();
    expect(agedHoldLink(admin, { kind: "key_expiring", workspace: team1 })).toBeUndefined();
  });
});

import { describe, expect, it } from "vitest";
import { costText, customRangeError, dataPolicyOf, rangeDates, requestFilters, requestQuery, timelineStatus, tokensText } from "./requests";
import { canDisable, canEditKeyLimits, canEnable, canRevoke, canRotate, filterKeys, keyPill, keyStatus, type KeyRow } from "./keys";
import { canView, dashboardSearch } from "./permissions";
import { dashboardHref, parseDashboardLocation } from "./locations";
import { navParents } from "./navigation";
import { admin, member, personal, session, team } from "./test-fixtures";
import { requestColumnIds, requestView, requestViewSearch } from "../pages/requests";

const now = Date.parse("2026-10-08T15:00:00Z");
describe("request filters and API query", () => {
  it("keeps only request filters (key statuses and unknown values are dropped)", () => {
    expect(requestFilters({ status: "failed", model: "m", key_id: "k", q: "abcd" })).toEqual({ status: "failed", model: "m", key_id: "k", q: "abcd", range: undefined, start_date: undefined, end_date: undefined });
    expect(requestFilters({ status: "revoked" }).status).toBeUndefined();
    expect(dashboardSearch({ status: "bogus", range: "1y", cursor: "bad cursor!", cols: "DROP TABLE" })).toEqual({ ws: undefined, page: undefined });
  });
  it("uses the server default for 30 days and explicit UTC dates for other presets", () => {
    expect(requestQuery({}, now).query.toString()).toBe("");
    expect(requestQuery({ range: "7d" }, now).query.toString()).toBe("start_date=2026-10-02&end_date=2026-10-09");
    expect(rangeDates("today", now)).toEqual({ start_date: "2026-10-08", end_date: "2026-10-09" });
    expect(rangeDates("month", now)).toEqual({ start_date: "2026-10-01", end_date: "2026-10-09" });
    expect(requestQuery({ range: "custom", start_date: "2026-09-01", end_date: "2026-09-08", model: "a/b", status: "failed" }, now).query.toString()).toBe("start_date=2026-09-01&end_date=2026-09-08&model=a%2Fb&status=failed");
  });
  it("never sends a request the server would reject", () => {
    expect(requestQuery({ q: "ab" }, now).error).toContain("at least 4 characters");
    expect(requestQuery({ q: "zzzz" }, now).error).toBeDefined();
    expect(requestQuery({ q: " 1A2B " }, now).query.get("q")).toBe("1a2b");
    expect(customRangeError("2026-01-01", "2026-06-01", now)).toContain("93 days");
    expect(customRangeError("2026-10-08", "2026-10-11", now)).toContain("after today");
    expect(customRangeError("2026-10-08", "2026-10-08", now)).toBeDefined();
  });
  it("shows unknown cost as unknown with what's on hold, never $0", () => {
    expect(costText(null, "1900")).toBe("Unknown · $0.0019 on hold");
    expect(costText(null, "0")).toBe("Unknown");
    expect(costText("9", "0")).toBe("$0.000009");
    expect(tokensText(null, "2")).toBe("Unknown");
    expect(dataPolicyOf({ data_collection: "deny", basis: "current_configuration" })).toBe("no_keep");
    expect(dataPolicyOf(undefined)).toBe("unknown");
    expect(timelineStatus("started")).toBe("pending");
    expect(timelineStatus("indeterminate")).toBe("unknown");
  });
});
describe("URL state for requests and keys", () => {
  it("routes request and key pages under their lists and round trips filters and the table view", () => {
    const request = { page: "request-detail" as const, ws: "team", record: "1a2b-req", status: "failed" as const, model: "a/b", range: "7d" as const, cols: "none", density: "compact" as const };
    expect(dashboardHref(request)).toBe("/workspaces/team/requests/1a2b-req?model=a%2Fb&status=failed&range=7d&cols=none&density=compact");
    expect(parseDashboardLocation(dashboardHref(request))).toMatchObject(request);
    const list = { page: "requests" as const, ws: "team", key_id: "k1", cursor: "1759900000000000_00000000-0000-0000-0000-000000000001", q: "abcd" };
    expect(parseDashboardLocation(dashboardHref(list))).toMatchObject(list);
    expect(parseDashboardLocation(dashboardHref({ page: "key-detail", ws: "team", record: "key1", status: "disabled" }))).toMatchObject({ page: "key-detail", ws: "team", record: "key1", status: "disabled" });
    expect(parseDashboardLocation("/workspaces/team/requests/a/b")).toBeUndefined();
    expect(parseDashboardLocation("/workspaces/team/costs/x")).toBeUndefined();
    expect(navParents["request-detail"]).toBe("requests");
    expect(navParents["key-detail"]).toBe("keys");
  });
  it("keeps the column chooser and density in the URL, including showing hidden-by-default columns", () => {
    expect(requestView({})).toEqual({ hidden: ["ttft", "speed", "cached", "reasoning", "request", "session", "workload", "streamed", "cost_center"], density: "comfortable" });
    expect(requestViewSearch({ hidden: ["workload", "streamed", "cost_center", "cached", "reasoning", "request", "session", "speed", "ttft"], density: "comfortable" })).toEqual({ cols: undefined, density: undefined });
    expect(requestViewSearch({ hidden: [], density: "compact" })).toEqual({ cols: "none", density: "compact" });
    expect(requestView({ cols: "none", density: "compact" })).toEqual({ hidden: [], density: "compact" });
    expect(requestView({ cols: "model,unknown" }).hidden).toEqual(["model"]);
    expect(requestColumnIds).toContain("cost_center");
  });
});
describe("request and key privacy", () => {
  it("opens request and key pages only inside the caller's own workspaces", () => {
    const foreignPersonal = { ...personal, id: "foreign", owner_user_id: "someone-else" }, staff = { ...team, id: "research", role: null, membership_source: null };
    for (const page of ["request-detail", "key-detail", "requests", "keys"] as const) {
      expect(canView(page, session, personal)).toBe(true);
      expect(canView(page, session, member)).toBe(true);
      expect(canView(page, admin, foreignPersonal)).toBe(false);
      expect(canView(page, admin, { ...staff, capabilities: { ...staff.capabilities, manage_service_accounts: false } })).toBe(false);
    }
  });
});
describe("disabled vs revoked keys", () => {
  const key: KeyRow = { id: "k", name: "Key", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
  it("derives revoked > expired > disabled > active and prefers the server status", () => {
    expect(keyStatus({ ...key, revoked_at: "x", disabled_at: "y" })).toBe("revoked");
    expect(keyStatus({ ...key, expires_at: "2020-01-01T00:00:00Z", disabled_at: "y" })).toBe("expired");
    expect(keyStatus({ ...key, disabled_at: "y" })).toBe("disabled");
    expect(keyStatus(key)).toBe("active");
    expect(keyStatus({ ...key, status: "disabled" })).toBe("disabled");
    expect(keyPill("expired")).toEqual({ status: "revoked", label: "Expired" });
  });
  it("offers Enable only for disabled keys, never revoked or expired; rotation only for active keys", () => {
    const disabled = { ...key, status: "disabled" as const }, revoked = { ...key, status: "revoked" as const, revoked_at: "x" }, expired = { ...key, status: "expired" as const };
    expect(canEnable(session, team, disabled)).toBe(true);
    expect(canEnable(session, team, revoked)).toBe(false);
    expect(canEnable(session, team, expired)).toBe(false);
    expect(canEnable(session, team, key)).toBe(false);
    expect(canDisable(session, team, key)).toBe(true);
    expect(canDisable(session, team, disabled)).toBe(false);
    expect(canRotate(session, team, disabled)).toBe(false);
    expect(canRotate(session, team, key)).toBe(true);
    expect(canRevoke(session, team, revoked)).toBe(false);
    expect(canRevoke(session, team, disabled)).toBe(true);
    expect(canEditKeyLimits(session, team, revoked)).toBe(false);
  });
  it("lets members manage only their own keys", () => {
    const other = { ...key, issued_to_user_id: "other" };
    expect(canDisable(session, member, key)).toBe(true);
    expect(canDisable(session, member, other)).toBe(false);
    expect(canEditKeyLimits(session, member, key)).toBe(true);
    expect(canEditKeyLimits(session, member, other)).toBe(false);
  });
  it("filters by status or a name/ID search and sorts usable keys first", () => {
    const keys = [{ ...key, id: "r", name: "Old", status: "revoked" as const }, { ...key, id: "a", name: "CI runner" }, { ...key, id: "d", name: "Laptop", status: "disabled" as const }];
    expect(filterKeys(keys).map(k => k.id)).toEqual(["a", "d", "r"]);
    expect(filterKeys(keys, "disabled").map(k => k.id)).toEqual(["d"]);
    expect(filterKeys(keys, undefined, "runner").map(k => k.id)).toEqual(["a"]);
    expect(filterKeys(keys, "succeeded").map(k => k.id)).toEqual(["a", "d", "r"]);
  });
});

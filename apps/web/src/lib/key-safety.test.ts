import { describe, expect, it } from "vitest";
import { adminKeyLabel, compareSeverity, filterSafetyRows, findingHint, findingsByKey, keySafetyPath, rowFixes, type SafetyReport, type SafetyRow } from "./key-safety";
import { dashboardHref, parseDashboardLocation } from "./locations";
import { dashboardSearch } from "./permissions";

const row = (id: string, workspace: string, severity: SafetyRow["severity"], findings: SafetyRow["findings"]): SafetyRow => ({ key: { id, workspace: { id: `w-${workspace}`, name: workspace, kind: "team" }, holder: "member", created_at: "2026-01-01T00:00:00Z", expires_at: null, last_used_at: null, model_restricted: false }, severity, findings });

describe("key safety findings", () => {
  it("explains each finding in one short sentence with exact numbers", () => {
    expect(findingHint({ code: "no_expiry", severity: "high" })).toBe("Never expires.");
    expect(findingHint({ code: "expiry_beyond_max", severity: "medium", days: 300 }, { max_lifetime_days: 90 })).toBe("Expires in 300 days; new keys can last at most 90 days.");
    expect(findingHint({ code: "unused", severity: "low", days: 1 })).toBe("Last used 1 day ago.");
    expect(findingHint({ code: "broad_model_access", severity: "low", models: 12 })).toBe("Can call all 12 models here.");
    expect(findingHint({ code: "not_rotated", severity: "medium", days: 1200 })).toBe("Secret is 1,200 days old.");
  });
  it("maps findings to existing key actions, most severe first and without duplicates", () => {
    expect(rowFixes({ findings: [{ code: "unused", severity: "low" }, { code: "no_limits", severity: "high" }, { code: "no_budget", severity: "medium" }, { code: "no_expiry", severity: "high" }] })).toEqual(["add_budget", "set_expiry", "disable"]);
    expect(rowFixes({ findings: [{ code: "owner_lost_access", severity: "high" }] })).toEqual(["revoke"]);
    expect(rowFixes({ findings: [{ code: "broad_model_access", severity: "low" }] })).toEqual(["restrict_models"]);
    expect(["low", "high", "medium"].sort((a, b) => compareSeverity(a as "low", b as "low"))).toEqual(["high", "medium", "low"]);
  });
  it("names shared keys by their workspace in admin views and filters by severity or text", () => {
    const rows = [row("a", "Product", "high", [{ code: "no_limits", severity: "high" }]), row("b", "Research", "low", [{ code: "never_used", severity: "low" }])];
    expect(adminKeyLabel(rows[0]!)).toBe("API key in Product");
    expect(filterSafetyRows(rows, undefined, "low").map(r => r.key.id)).toEqual(["b"]);
    expect(filterSafetyRows(rows, "never", undefined).map(r => r.key.id)).toEqual(["b"]);
    expect(filterSafetyRows(rows, "prod", "high,medium").map(r => r.key.id)).toEqual(["a"]);
    const report: SafetyReport = { scope: "workspace", summary: { keys: 3, flagged: 2, high: 1, medium: 0, low: 1 }, data: rows, thresholds: { unused_days: 30, rotation_days: 180, max_lifetime_days: 365, broad_models: 5, never_used_grace_days: 7 } };
    expect(findingsByKey(report).get("a")?.severity).toBe("high");
    expect(findingsByKey(undefined).size).toBe(0);
  });
  it("builds scoped paths and routes the Admin page and the keys filter", () => {
    expect(keySafetyPath("/api/v1/workspaces/team", { key_id: "k1" })).toBe("/api/v1/workspaces/team/key-safety?key_id=k1");
    expect(keySafetyPath("/api/v1/platform")).toBe("/api/v1/platform/key-safety");
    expect(dashboardHref({ page: "key-safety", severity: "high" })).toBe("/admin/key-safety?severity=high");
    expect(parseDashboardLocation("/admin/key-safety?severity=high,low")).toMatchObject({ page: "key-safety", severity: "high,low" });
    expect(dashboardHref({ page: "keys", ws: "team", risk: "attention" })).toBe("/workspaces/team/keys?risk=attention");
    expect(dashboardSearch({ risk: "nope", severity: "bad" })).toEqual({ ws: undefined, page: undefined });
  });
});

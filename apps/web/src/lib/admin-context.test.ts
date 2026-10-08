import { describe, expect, it } from "vitest";
import { activeAdminGroup, adminGroups, navigation, navParents, hiddenPages, contextOptions, scopeSearch, rememberedPortal, rememberPortal } from "./navigation";
import { canAdminister } from "./permissions";
import { effectivePolicy } from "./governance";
import { admin, auditor, session, team, project, policy, storage } from "./test-fixtures";
describe("enterprise administration context", () => {
  it("fixes Admin to installation scope, even on shared resource details", () => { expect(adminGroups("workspace-detail")).toEqual(["", "People", "Models", "Usage & spend", "Records", "Settings"]); expect(activeAdminGroup("workspace-detail")).toBe("People"); expect(scopeSearch("platform-overview", team.id)).toEqual({ page: "platform-overview", ws: undefined }); });
  it("does not offer Admin to shared workspace administrators", () => { expect(canAdminister(session)).toBe(false); expect(canAdminister(admin)).toBe(true); expect(canAdminister(auditor)).toBe(true); });
  it("retains Personal/Teams/Projects switching without organizations", () => { expect(contextOptions(session).map(g => g.label)).toEqual(["Personal", "Teams", "Projects"]); expect(JSON.stringify(contextOptions(session))).not.toContain("organization"); });
  it("preserves the selected shared settings destination when returning from Admin", () => { const store = storage(); rememberPortal(store, admin, { page: "workspace-settings", ws: project.id, tab: "members" }); rememberPortal(store, admin, { page: "models" }); expect(rememberedPortal(store, admin, "workspace")).toMatchObject({ page: "workspace-settings", ws: project.id, tab: "members" }); expect(rememberedPortal(store, admin, "admin")).toMatchObject({ page: "models" }); });
  it("does not grant shared membership by remembering an administrative resource", () => { expect(scopeSearch("workspace-detail", team.id)).toEqual({ page: "workspace-detail", ws: undefined }); });
});
describe("Models navigation group", () => {
  it("shows Connections, Models and Catalogs; legacy and create pages highlight Models", () => { expect(navigation.filter(n => n.group === "Models").map(n => n.label)).toEqual(["Connections", "Models", "Catalogs"]); for (const page of ["model-new", "model-detail", "deployments", "deployment-detail", "routing"] as const) { expect(navParents[page]).toBe("models"); expect(activeAdminGroup(page)).toBe("Models"); } expect(hiddenPages["model-new"]).toBe("Add model"); });
});
describe("displayed composed caps", () => {
  it("retains platform ceilings and applies stricter local caps", () => { const parent = { ...policy, requests_per_minute: 120, concurrent_requests: 8, monthly_budget_microusd: "50000000" }; expect(effectivePolicy(policy, parent)).toEqual(parent); expect(effectivePolicy({ ...policy, requests_per_minute: 80, monthly_budget_microusd: "10000000" }, parent)).toMatchObject({ requests_per_minute: 80, concurrent_requests: 8, monthly_budget_microusd: "10000000" }); expect(effectivePolicy({ ...policy, requests_per_minute: 200 }, parent).requests_per_minute).toBe(120); });
  it("handles absent parents and exact money above the safe-number range", () => { expect(effectivePolicy(policy, policy)).toEqual(policy); expect(effectivePolicy({ ...policy, requests_per_minute: 40 }, policy).requests_per_minute).toBe(40); expect(effectivePolicy({ ...policy, monthly_budget_microusd: "9007199254740993" }, { ...policy, monthly_budget_microusd: "9007199254740992" }).monthly_budget_microusd).toBe("9007199254740992"); });
});

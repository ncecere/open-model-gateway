import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { permissions, canView, canManageKey, canRotateKey, dashboardSearch, dashboardTabs } from "./permissions";
import { session, admin, auditor, personal, team, member, none } from "./test-fixtures";
const key = { id: "key", name: "Example", issued_to_user_id: "other", service_account_id: null, created_at: "", expires_at: "", revoked_at: null };
describe("live enterprise capability presentation", () => {
  it("keeps personal workspaces private and nonshareable", () => { expect(permissions(session, personal)).toMatchObject({ workspaceAdmin: true, manageTeam: false, manageServiceAccounts: false }); expect(canView("members", session, personal)).toBe(false); expect(canView("service-accounts", session, personal)).toBe(false); });
  it("allows shared admins, never members, to manage membership", () => { expect(permissions(session, team).manageTeam).toBe(true); expect(permissions(session, member).manageTeam).toBe(false); });
  it("does not confuse personal ownership with platform authority", () => { expect(canView("models", session, personal)).toBe(false); expect(canView("grants", session, personal)).toBe(true); expect(permissions(session, personal).platformWrite).toBe(false); });
  it.each(["models", "providers", "pricing", "routing", "users", "policies"] as const)("gates %s on platform read rather than workspace authority", page => { expect(canView(page, session, team)).toBe(false); expect(canView(page, admin)).toBe(true); expect(canView(page, auditor)).toBe(true); });
  it("separates Auditor reads from Admin writes", () => { expect(permissions(auditor)).toMatchObject({ platformRead: true, platformWrite: false, manageProviders: false, managePricing: false, createWorkspace: false }); expect(permissions(admin).createWorkspace).toBe(true); });
  it("requires actual capability for human issuance under inherited staff authority", () => { const staff = { ...team, role: null, capabilities: { ...none, manage_members: true, manage_service_accounts: true } }; expect(permissions(admin, staff)).toMatchObject({ createUserKey: false, manageServiceAccounts: true }); expect(canRotateKey(admin, staff, { ...key, issued_to_user_id: "me" })).toBe(false); });
  it("does not infer capabilities from role labels", () => { expect(permissions(admin, { ...team, capabilities: none })).toMatchObject({ createUserKey: false, manageTeam: false, manageServiceAccounts: false, manageGrants: false, managePolicy: false }); });
  it("lets shared admins revoke other humans, never rotate them", () => { expect(canManageKey(session, team, key)).toBe(true); expect(canRotateKey(session, team, key)).toBe(false); expect(canRotateKey(session, team, { ...key, issued_to_user_id: "me" })).toBe(true); });
  it("allows members only their own credentials", () => { expect(canManageKey(session, member, key)).toBe(false); expect(canManageKey(session, member, { ...key, issued_to_user_id: "me" })).toBe(true); expect(canRotateKey(session, member, { ...key, issued_to_user_id: null, service_account_id: "service" })).toBe(false); });
  it("authorizes service-account rotation independently", () => { expect(canRotateKey(session, team, { ...key, issued_to_user_id: null, service_account_id: "service" })).toBe(true); });
  it.each([admin, auditor, session])("never authorizes foreign personal data, even for platform staff", actor => { const foreign = { ...personal, owner_user_id: "other" }; expect(canView("keys", actor, foreign)).toBe(false); expect(canView("costs", actor, foreign)).toBe(false); expect(canManageKey(actor, foreign, key)).toBe(false); expect(permissions(actor, foreign).reconcileCosts).toBe(false); });
  it("keeps profile and invitation acceptance independent of memberships", () => { expect(canView("profile", session)).toBe(true); expect(canView("accept-invitation", session)).toBe(true); expect(canView("overview", session)).toBe(false); });
  it("separates local policy, pricing and reconciliation writes", () => { expect(permissions(session, member)).toMatchObject({ managePolicy: false, managePricing: false, reconcileCosts: false }); expect(permissions(session, team).managePolicy).toBe(true); expect(permissions(admin, team).reconcileCosts).toBe(true); expect(permissions(admin).reconcileCosts).toBe(false); });
  it("drops organization, secret and unknown URL state", () => { expect(dashboardSearch({ org: "removed", ws: "team", page: "keys", token: "secret" })).toEqual({ ws: "team", page: "keys" }); expect(dashboardSearch({ ws: {}, page: "unknown" })).toEqual({ ws: undefined, page: undefined }); });
});

/** Every tab a ResourcePage/RecordPage can open must survive `validateSearch`, or `?tab=` silently falls back (Admin › Users › Activity). */
describe("tab allowlist", () => {
  const root = resolve(__dirname, "..");
  const files = (dir: string): string[] => readdirSync(dir, { withFileTypes: true }).flatMap(e => e.isDirectory() ? (e.name === "ui" ? [] : files(join(dir, e.name))) : e.name.endsWith(".tsx") && !e.name.includes(".test.") ? [join(dir, e.name)] : []);
  const sources = [...files(join(root, "pages")), ...files(join(root, "components"))].map(f => readFileSync(f, "utf8"));
  const tabValues = new Set<string>();
  for (const text of sources) {
    // ResourcePage tabs: { value: "x", label: …, [icon: …,] [count: …,] content: … }
    for (const m of text.matchAll(/\{ value: "([a-z-]+)", label: [^{}\n]{1,80}?(?:icon: <[A-Za-z0-9]+ aria-hidden \/>, )?(?:count: [^\n]{1,80}?, )?content:/g)) tabValues.add(m[1]!);
    // RecordPage in tab mode: its section ids become tabs.
    if (/<RecordPage[^\n]*onTabChange=/.test(text)) for (const m of text.matchAll(/\{ id: "([a-z-]+)", (?:title|tabLabel):/g)) tabValues.add(m[1]!);
  }
  // Workspace Settings tabs come from workspaceSettingsTabs().
  const settings = sources.find(t => t.includes("export function workspaceSettingsTabs"))!;
  for (const m of settings.match(/workspaceSettingsTabs[\s\S]*?\n\}/)![0].matchAll(/"([a-z-]+)"/g)) if (m[1] !== "personal") tabValues.add(m[1]!);
  it("finds the tabs (the scan itself works)", () => { for (const v of ["activity", "members", "audit", "limits", "model-access", "workspaces"]) expect(tabValues).toContain(v); });
  it.each([...tabValues])("allows ?tab=%s", value => { expect(dashboardTabs).toContain(value); expect(dashboardSearch({ tab: value }).tab).toBe(value); });
});

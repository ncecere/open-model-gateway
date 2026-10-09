import { describe, expect, it } from "vitest";
import { PlatformTeams, PlatformUsers, UserDetail, OidcMappings, CostCenters } from "../pages/hierarchy";
import { WorkspaceDetail } from "../pages/resource-details";
import { MembersTable, WorkspaceMembers } from "../pages/workspace";
import { AuditHistory } from "../pages/organization";
import { auditActionLabel, auditEventLabel, displayRole, grantLabel, grantRemoval, healthLabel, last30Days, resourceTypeLabel, splitUsage, userState } from "../lib/people";
import { admin, auditor, team, markup, report, testClient } from "../lib/test-fixtures";
import { DashboardNavigationProvider } from "./navigation-link";

const uid = "8e210000-0000-4000-8000-000000000003", wid = "8e210000-0000-4000-8000-000000000014", ccid = "8e210000-0000-4000-8000-000000000046", personalId = "8e210000-0000-4000-8000-0000000000aa";
/** Visible text only: identifiers may appear in hrefs, never as primary/secondary text. */
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");
const noop = () => {};
const alex = { id: uid, email: "alex@demo.invalid", platform_role: "user", disabled_at: null, created_at: "2026-01-01T00:00:00Z", last_sign_in_at: "2026-10-07T10:00:00Z", shared_workspace_count: 2, role_grants: [{ id: "g1", role: "user", source: "group" }, { id: "g2", role: "admin", source: "manual", revoked_at: "2026-02-01T00:00:00Z" }] };
const suspended = { id: "8e210000-0000-4000-8000-000000000009", email: "gone@demo.invalid", platform_role: null, disabled_at: "2026-09-01T00:00:00Z", last_sign_in_at: null, shared_workspace_count: 0, role_grants: [{ id: "g3", role: "auditor", source: "manual" }, { id: "g4", role: "user", source: "manual" }] };
const product = { ...team, id: wid, member_count: 3, cost_center_id: ccid, cost_center: { id: ccid, name: "Research", code: "R-1" }, created_at: "2026-01-01T00:00:00Z" };

describe("Grounded-style people pages", () => {
  it("labels roles, grants, health and audit codes for people", () => {
    expect(grantLabel({ role: "admin", source: "group" })).toBe("Admin · group");
    expect(displayRole(suspended as never)).toEqual({ role: "auditor", inactive: true });
    expect(displayRole(alex as never)).toEqual({ role: "user", inactive: false });
    expect(displayRole({ ...suspended, role_grants: [] } as never)).toEqual({ role: null, inactive: true });
    expect(auditActionLabel("workspace.manual_membership_set")).toBe("Set manual membership");
    expect(auditActionLabel("future.unknown_code")).toBe("future.unknown_code");
    expect(auditActionLabel("scim.user.deactivated")).toBe("Deactivated user (SCIM)"); expect(resourceTypeLabel("scim_group")).toBe("SCIM group");
    expect(auditEventLabel({ action: "model.granted", metadata: { source: "catalog" } })).toBe("Added model"); expect(auditActionLabel("key.enabled")).toBe("Enabled API key"); expect(auditActionLabel("key.disabled")).toBe("Disabled API key");
    expect(auditEventLabel({ action: "model.grant_revoked", metadata: { source: "catalog" } })).toBe("Removed model");
    expect(auditEventLabel({ action: "model.granted", metadata: { source: "direct" } })).toBe("Assigned model");
    expect(auditEventLabel({ action: "model.granted" })).toBe("Assigned model");
    expect(resourceTypeLabel("group_mapping")).toBe("SSO group mapping");
    expect(healthLabel("aged_hold_attempts")).toBe("Attempts with aged holds");
  });
  it("lists users with avatars, badges and filters but no visible identifiers", () => {
    const html = markup(<PlatformUsers session={admin} />, [["/api/v1/platform/users?limit=50&offset=0", { data: [alex, suspended], has_more: false }]]), visible = text(html);
    expect(html).toContain('data-shape="circle"');
    for (const label of ["Platform user", "User · group", "Active", "Suspended", "Platform auditor", "Never", "Platform role", "No role", "Platform auditor", "Rows 1–2 of 2", "Columns"]) expect(visible).toContain(label);
    expect(visible).not.toContain("Admin · manual"); // revoked grants are not current provenance
    expect(visible).not.toContain("No entitlement");
    expect(visible).not.toContain(uid);
    expect(html).toContain(`href="/admin/users/${uid}"`);
    expect(markup(<PlatformUsers session={auditor} />)).not.toContain("Add user</button>");
  });
  it("shows a denied, never-entitled sign-in as No access with their name, not Suspended", () => {
    const jordan = { id: "8e210000-0000-4000-8000-0000000000bb", email: "jordan.kim@example.edu", display_name: "Jordan Kim", platform_role: null, disabled_at: "2026-10-09T10:00:00Z", disable_reason: "entitlement_loss", cleanup_due_at: "2026-11-08T10:00:00Z", last_sign_in_at: null, shared_workspace_count: 0, role_grants: [] };
    expect(userState(jordan as never)).toBe("no_access");
    // Grant history (even revoked) or an admin suspension is a real suspension; unknown history stays "suspended".
    expect(userState({ ...jordan, role_grants: [{ role: "user", source: "group", revoked_at: "2026-10-01T00:00:00Z" }] } as never)).toBe("suspended");
    expect(userState({ ...jordan, disable_reason: "admin_suspension" } as never)).toBe("suspended");
    expect(userState({ ...jordan, role_grants: undefined } as never)).toBe("suspended");
    const html = markup(<PlatformUsers session={admin} />, [["/api/v1/platform/users?limit=50&offset=0", { data: [jordan], has_more: false }]]), visible = text(html);
    expect(visible).toContain("Jordan Kim"); expect(visible).toContain("jordan.kim@example.edu"); expect(visible).toContain("No access");
    expect(visible.split("Last sign-in")[1]).not.toContain("Suspended"); // only the filter option says it
  });
  it("puts search, role and status in one filter row above the table, read from the URL and sent to the server", () => {
    const search = { page: "users" as const, role: "admin" as const, status: "suspended", q: "al" };
    const html = markup(<DashboardNavigationProvider search={search} navigate={() => {}}><PlatformUsers session={admin} /></DashboardNavigationProvider>, [["/api/v1/platform/users?limit=50&offset=0&q=al&role=admin&status=suspended", { data: [suspended], has_more: false }]]);
    expect(html).not.toContain("<aside");
    expect(html.indexOf('aria-label="Filters"')).toBeLessThan(html.indexOf("<table"));
    expect(html.indexOf(">Columns<")).toBeLessThan(html.indexOf("<table")); // Columns ends the filter row, as in Grounded
    expect(text(html)).toMatch(/Filters \( ?3 active ?\)/);
    expect(html).toContain("Remove filter Search: al");
    expect(text(html)).toContain("Rows 1\u20131 of 1");
  });
  it("renders one user page of cards with shared memberships only", () => {
    const path = `/api/v1/platform/users/${uid}`, detail = { ...alex, first_sign_in_at: "2026-01-02T00:00:00Z", shared_memberships: [{ workspace_id: wid, name: "Product", kind: "team", role: "owner", sources: ["group", "manual"], grants: [{ role: "admin", source: "group" }, { role: "owner", source: "manual" }] }] };
    const html = ["overview", "workspaces"].map(tab => markup(<UserDetail session={admin} id={uid} tab={tab} onTabChange={noop} />, [[path, detail]])).join(""), visible = text(html);
    for (const label of ["Profile and access", "Shared workspaces", "Product", "Owner", "Admin · group", "Owner · manual", "First signed in", "Last sign-in", "Grant role"]) expect(visible).toContain(label);
    expect(visible).not.toContain("1 shared workspace"); // the tab count says it once
    expect(visible).not.toContain("private keys or request details");
    expect(html).toContain('aria-label="More actions"');
    expect(html).toContain('role="tablist"');
    expect(visible).not.toContain(uid);
    expect(visible).not.toContain(wid);
    expect(html).not.toMatch(/API key|Request details|>Personal</);
    expect(html).not.toContain(personalId);
    const read = markup(<UserDetail session={auditor} id={uid} onTabChange={noop} />, [[path, detail]]);
    expect(read).not.toContain("Grant role</button>");
    expect(read).not.toContain("Revoke…</button>");
  });
  it("explains a suspended user instead of a bare missing entitlement", () => {
    const path = `/api/v1/platform/users/${suspended.id}`, html = ["overview", "workspaces"].map(tab => text(markup(<UserDetail session={admin} id={suspended.id} tab={tab} onTabChange={noop} />, [[path, { ...suspended, first_sign_in_at: null, shared_memberships: [] }]]))).join(" ");
    expect(html).toContain("Suspended");
    expect(html).toContain("Role grants are retained but give no access while suspended");
    expect(html).toContain("Not a member of any team or project");
    expect(html).not.toContain("No entitlement");
  });
  it("lists teams with square avatars, member counts and cost centers by name", () => {
    const html = markup(<PlatformTeams session={admin} kind="team" />, [["/api/v1/platform/workspaces?kind=team&limit=50&offset=0", { data: [product] }]]), visible = text(html);
    expect(html).toContain('data-shape="square"');
    for (const label of ["Members", "3", "Research", "R-1", "Active", "Status", "Disabled"]) expect(visible).toContain(label);
    expect(visible).not.toContain(ccid);
    expect(visible).not.toContain(wid);
  });
  it("shows a Grounded header, icon tabs with counts and stat cards on the team page", () => {
    const path = `/api/v1/platform/workspaces/${wid}`, html = markup(<WorkspaceDetail session={admin} id={wid} onTabChange={noop} />, [[path, product], [`${path}/catalogs`, { mode: "inherit", catalog_ids: [], effective_catalog_ids: ["c1", "c2"] }]]), visible = text(html);
    for (const label of ["Team", "Members", "Research", "Available catalogs", "Uses team defaults", "Your membership"]) expect(visible).toContain(label);
    expect(html).toContain('aria-label="More actions"');
    expect(html).toMatch(/Members<span[^>]*>3<\/span>/);
    expect(visible).not.toContain("3 members"); // the stat card and tab count say it once
    // Catalogs and Models are one "Model access" tab (catalogs still counted in the stat card).
    expect(html).toContain(">Models<");
    expect(html).not.toMatch(/>Catalogs<span/);
    expect(visible).not.toContain(ccid);
    expect(visible).not.toContain(wid);
    const read = markup(<WorkspaceDetail session={auditor} id={wid} onTabChange={noop} />, [[path, product]]);
    expect(read).not.toContain("Authorized models"); // models only for writers or members
  });
  it("shows people as name over email, with (you) for the viewer (review #33)", () => {
    const client = testClient(); client.setQueryData(["api", undefined, "/api/v1/workspaces/team/members", "choices"], [{ user_id: uid, email: "alex@demo.invalid", display_name: "Alex Rivera", role: "owner" }, { user_id: "other", email: "blair@demo.invalid", display_name: null, role: "member" }]);
    const visible = text(markup(<MembersTable path="/api/v1/workspaces/team/members" title="Members" canManageOwners={false} writable={false} selfId={uid} />, [], client));
    expect(visible).toContain("Alex Rivera (you) alex@demo.invalid"); expect(visible).toContain("blair@demo.invalid"); expect(visible).not.toContain("Blair");
    client.clear();
  });
  it("cuts long emails with an ellipsis and the full address as a title, never mid-word wrapping", () => {
    const long = "avery.nguyen.research.computing@chemistry.example.edu";
    const html = markup(<PlatformUsers session={admin} />, [["/api/v1/platform/users?limit=50&offset=0", { data: [{ ...alex, display_name: "Avery Nguyen", email: long }, { ...suspended, email: `x${long}` }], has_more: false }]]);
    expect(html).toContain(`title="${long}">${long}</span>`); // secondary line under the name
    expect(html).toMatch(new RegExp(`title="x${long.replace(/\./g, "\\.")}"><a[^>]*>x${long.replace(/\./g, "\\.")}</a>`)); // email as the primary (linked) line
    expect(html).not.toContain('data-wrap="anywhere"');
  });
  it("renders members as a card with avatars, role and source badges", () => {
    const client = testClient(); client.setQueryData(["api", undefined, "/api/v1/workspaces/team/members", "choices"], [{ user_id: uid, email: "alex@demo.invalid", role: "owner", membership_source: "mixed", grants: [{ role: "admin", source: "group" }, { role: "owner", source: "manual" }] }]);
    const html = markup(<WorkspaceMembers session={admin} workspace={team} />, [], client), visible = text(html);
    for (const label of ["Members", "Add member", "alex@demo.invalid", "Owner", "Admin · group", "Owner · manual", "Rows 1–1 of 1"]) expect(visible).toContain(label);
    expect(visible).not.toContain(uid);
    expect(html).not.toMatch(/<h2[^>]*>Team members/);
    const read = markup(<MembersTable path="/api/v1/workspaces/team/members" title="Members" canManageOwners={false} writable={false} />, [], client);
    expect(read).not.toContain("Add member");
    client.clear();
  });
  it("names audit actors, actions and targets without showing identifiers", () => {
    const client = testClient();
    client.setQueryData(["api", undefined, "/api/v1/platform/users", "choices"], [alex]);
    client.setQueryData(["api", undefined, "/api/v1/platform/workspaces", "choices"], [product]);
    // Admin hides sign-in syncs by default (review #12): the first request already asks the server to.
    const html = markup(<AuditHistory />, [["/api/v1/platform/audit?limit=50&offset=0&hide_sign_ins=true", { data: [
      { id: "e1", actor_user_id: uid, action: "workspace.updated", resource_type: "workspace", resource_id: wid, workspace_id: wid, created_at: "2026-10-07T10:00:00Z" },
      { id: "e2", actor_user_id: null, action: "installation.bootstrap_demo", resource_type: "installation", resource_id: "8e210000-0000-4000-8000-0000000000ff", workspace_id: null, created_at: "2026-10-07T09:00:00Z" },
      { id: "e3", actor_user_id: uid, action: "key.created", resource_type: "key", resource_id: "d29303bb-fe48-42c3-9c27-40da48986a84", workspace_id: personalId, created_at: "2026-10-07T08:00:00Z" },
      { id: "e4", actor_user_id: uid, action: "deployment.created", resource_type: "deployment", resource_id: "9e210000-0000-4000-8000-000000000001", workspace_id: null, created_at: "2026-10-07T07:00:00Z", target_name: "gpt-6-luna on OpenAI" },
      { id: "e5", actor_user_id: uid, action: "price.created", resource_type: "price", resource_id: "9e210000-0000-4000-8000-000000000002", workspace_id: null, created_at: "2026-10-07T06:00:00Z", target_name: "Price v1 for gpt-6-luna on OpenAI" },
      { id: "e6", actor_user_id: uid, action: "model.granted", resource_type: "model", resource_id: "9e210000-0000-4000-8000-000000000003", workspace_id: wid, metadata: { source: "catalog" }, created_at: "2026-10-07T05:00:00Z" },
      { id: "e7", actor_user_id: uid, action: "model.granted", resource_type: "model", resource_id: "9e210000-0000-4000-8000-000000000003", workspace_id: wid, metadata: { source: "direct" }, created_at: "2026-10-07T04:00:00Z" },
      { id: "e8", actor_user_id: uid, action: "key.enabled", resource_type: "key", resource_id: "9e210000-0000-4000-8000-000000000004", workspace_id: wid, created_at: "2026-10-07T03:00:00Z" },
    ] }]], client), visible = text(html);
    // Admin never reads keys: a shared workspace's key is named by its team.
    expect(visible).toContain("API key in Product"); expect(visible).toContain("Enabled API key");
    expect(visible).not.toContain("API key in Product API key"); // the type isn't repeated under it
    for (const label of ["alex@demo.invalid", "Changed workspace", "Product", "System", "Seeded the demo", "API key", "gpt-6-luna on OpenAI", "Price v1 for gpt-6-luna on OpenAI", "Added model", "Assigned model"]) expect(visible).toContain(label);
    // The event code is a tooltip, not a second line under every row.
    expect(visible).not.toContain("workspace.updated"); expect(html).toContain('title="workspace.updated"');
    expect(html).toContain(`href="/admin/users/${uid}"`);
    expect(visible).not.toMatch(/[0-9a-f]{8}-[0-9a-f]{4}-/);
    client.clear();
  });
  it("keeps SSO groups and cost centers free of raw identifiers", () => {
    const client = testClient();
    client.setQueryData(["api", undefined, "/api/v1/platform/oidc/group-mappings", "choices"], [{ id: "m1", issuer: "https://idp.example", group_value: "omg/product-admin", target_kind: "workspace", platform_role: null, workspace_id: wid, workspace_role: "admin", enabled: true }]);
    client.setQueryData(["api", undefined, "/api/v1/platform/workspaces", "choices"], [product]);
    client.setQueryData(["api", undefined, "/api/v1/platform/cost-centers", "choices"], [{ id: ccid, name: "Research", code: "R-1", archived_at: null }]);
    const sso = text(markup(<OidcMappings session={admin} />, [], client));
    for (const label of ["omg/product-admin", "Product", "Admin", "Enabled", "Rows 1–1 of 1"]) expect(sso).toContain(label);
    expect(sso).not.toContain(wid);
    const centers = text(markup(<CostCenters session={admin} />, [], client));
    for (const label of ["Research", "R-1", "Active", "Assign workspace"]) expect(centers).toContain(label);
    expect(centers).not.toContain(ccid);
    expect(markup(<CostCenters session={auditor} />, [], client)).not.toContain("Assign workspace</button>");
    client.clear();
  });
});

describe("user role, grant removal and activity", () => {
  const unentitled = { ...suspended, id: "8e210000-0000-4000-8000-00000000000a", email: "unentitled@demo.invalid", role_grants: [] };
  const listPath = "/api/v1/platform/users?limit=50&offset=0";
  it("shows one role element per user: effective, retained-but-inactive, or No role", () => {
    const html = markup(<PlatformUsers session={admin} />, [[listPath, { data: [alex, suspended, unentitled], has_more: false }]]), visible = text(html);
    expect(visible).not.toMatch(/inactive while suspended|No grants|No platform role/);
    expect(html).toMatch(/<span[^>]*data-tone="neutral"[^>]*>Platform auditor<\/span>/); // suspended: neutral, not info
    expect(html).toMatch(/<span[^>]*data-tone="info"[^>]*>Platform user<\/span>/);
    expect(html).toMatch(/<span[^>]*data-tone="neutral"[^>]*>No role<\/span>/);
    expect(html).not.toMatch(/<(s|del)>/);
    expect(visible).toContain("—"); // empty grants are a muted dash
  });
  it("lists grants without inline removal (that happens on the user's page, review #44)", () => {
    const html = markup(<PlatformUsers session={admin} />, [[listPath, { data: [alex, suspended], has_more: false }]]);
    expect(html).not.toMatch(/aria-label="Remove /);
    expect(text(html)).toContain("Auditor · manual");
    expect(text(html)).toContain("From SSO group mapping; changes at next sign-in");
    const read = markup(<PlatformUsers session={auditor} />, [[listPath, { data: [alex, suspended], has_more: false }]]);
    expect(read).not.toContain("Remove Auditor manual grant");
    expect(read).not.toMatch(/aria-label="Remove /);
  });
  it("warns about entitlement loss and the last admin before removal", () => {
    const last = grantRemoval({ email: "a@demo.invalid", disabled_at: null, role_grants: [{ role: "admin", source: "manual" }] }, { role: "admin", source: "manual" });
    expect(last.title).toBe("Remove Admin manual grant from a@demo.invalid?");
    expect(last.description).toContain("revokes their sessions and user-owned keys");
    expect(last.description).toContain("The last Platform Admin is protected");
    const kept = grantRemoval({ email: "a@demo.invalid", disabled_at: null, role_grants: [{ role: "admin", source: "manual" }, { role: "admin", source: "group" }] }, { role: "admin", source: "manual" });
    expect(kept.description).not.toContain("last grant");
    expect(kept.description).not.toContain("last Platform Admin");
  });
  it("replaces the profile footnote with per-badge help and the same remove control", () => {
    const path = `/api/v1/platform/users/${suspended.id}`, record = { ...suspended, role_grants: [...suspended.role_grants, { id: "g5", role: "user", source: "group" }], first_sign_in_at: null, shared_memberships: [] };
    const html = markup(<UserDetail session={admin} id={suspended.id} tab="overview" onTabChange={noop} />, [[path, record]]), visible = text(html);
    expect(visible).not.toContain("Group grants are read-only and synchronize at sign-in");
    expect(visible).not.toContain("generic OIDC does not detect departures");
    expect(visible).toContain("SSO group grants update at sign-in");
    expect(html).toContain('aria-label="Remove Auditor manual grant"');
    expect(html).not.toContain('aria-label="Remove User group grant"');
    expect(markup(<UserDetail session={auditor} id={suspended.id} tab="overview" onTabChange={noop} />, [[path, record]])).not.toMatch(/aria-label="Remove /);
  });
  it("shows a privacy-preserving Activity tab: own changes and aggregate usage", () => {
    const client = testClient(), path = `/api/v1/platform/users/${uid}`, range = last30Days();
    client.setQueryData(["api", undefined, "/api/v1/platform/workspaces", "choices"], [product]);
    const totals = (n: string) => ({ known_cost_microusd: "1500000", held_microusd: "0", attempts: n, root_requests: n, unresolved_attempts: "0" });
    const usage = { ...report, scope: "platform", breakdowns: { ...report.breakdowns, workspaces: [{ id: wid, name: "Product", totals: totals("4") }, { id: personalId, name: "Alex private sandbox", totals: totals("8") }] } };
    const html = markup(<UserDetail session={auditor} id={uid} tab="activity" onTabChange={noop} />, [
      [path, { ...alex, first_sign_in_at: null, shared_memberships: [] }],
      [`/api/v1/platform/audit?actor_user_id=${uid}&limit=50&offset=0&hide_sign_ins=true`, { data: [{ id: "e1", actor_user_id: uid, action: "workspace.updated", resource_type: "workspace", resource_id: wid, workspace_id: wid, created_at: "2026-10-07T10:00:00Z" }], has_more: false }],
      [`/api/v1/platform/cost-report?start_date=${range.start_date}&end_date=${range.end_date}&actor_user_id=${uid}`, usage],
    ], client), visible = text(html);
    expect(html).toMatch(/Activity/);
    for (const label of ["Recent actions", "Sign-in events", "When", "Action", "Target", "Team or project", "Changed workspace", "workspace.updated", "Product", "Usage (last 30 days)", "Personal workspace (private)", "Totals only", "Requests", "On hold"]) expect(visible).toContain(label);
    expect(visible).not.toContain("Alex private sandbox");
    expect(html).not.toContain(personalId);
    expect(visible).not.toMatch(/[0-9a-f]{8}-[0-9a-f]{4}-/);
    expect(html).not.toMatch(/href="[^"]*(keys|executions|costs)/); // no links into private keys or request details
    expect(html).not.toMatch(/>Personal</); // no personal kind badge or name
    expect(client.getQueryCache().getAll().map(q => String(q.queryKey[2]))).not.toContainEqual(expect.stringMatching(/\/keys|\/executions|\/costs\b|\/usage/));
    client.clear();
  });
  it("says no changes, not 'no results', when the default sign-in filter leaves Activity empty", () => {
    const path = `/api/v1/platform/users/${uid}`, range = last30Days();
    const visible = text(markup(<UserDetail session={auditor} id={uid} tab="activity" onTabChange={noop} />, [
      [path, { ...alex, shared_memberships: [] }],
      [`/api/v1/platform/audit?actor_user_id=${uid}&limit=50&offset=0&hide_sign_ins=true`, { data: [], has_more: false }],
      [`/api/v1/platform/cost-report?start_date=${range.start_date}&end_date=${range.end_date}&actor_user_id=${uid}`, report],
    ]));
    expect(visible).toContain("No changes recorded.");
    expect(visible).not.toContain("No results match these filters");
  });
  it("aggregates every non-shared workspace into one private total", () => {
    const t = (n: string) => ({ known_cost_microusd: n, held_microusd: "1", attempts: n, root_requests: n, unresolved_attempts: "0" });
    const split = splitUsage([{ id: wid, name: "Product", totals: t("2") }, { id: personalId, name: "Personal", totals: t("9007199254740993") }, { id: null, name: "Unknown", totals: t("1") }], new Map([[wid, { name: "Product", kind: "team" as const }]]));
    expect(split.shared.map(w => w.name)).toEqual(["Product"]);
    expect(split.personal).toEqual({ known_cost_microusd: "9007199254740994", held_microusd: "2", attempts: "9007199254740994", root_requests: "9007199254740994", unresolved_attempts: "0" });
    expect(splitUsage([], new Map()).personal).toBeNull();
    expect(last30Days(new Date("2026-03-01T12:00:00Z"))).toEqual({ start_date: "2026-01-31", end_date: "2026-03-02" });
  });
});

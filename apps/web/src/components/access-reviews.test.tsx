// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { AccessReviewDetail, AccessReviewInbox, PlatformAccessReviewsPage } from "../pages/access-reviews";
import { campaignState, decisionPath, inboxPath, reviewClosePath, reviewerPath, reviewItemsPath, reviewPath, reviewsPath, type InboxItem, type ReviewCampaignDetail, type ReviewItem } from "../lib/access-reviews";
import { admin, auditor, session, team, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function serve(routes: Record<string, (init?: RequestInit) => unknown>) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const key = Object.keys(routes).sort((a, b) => b.length - a.length).find(r => path === r || path.startsWith(r.includes("?") ? `${r}&` : `${r}?`));
    if (key) return Promise.resolve(Response.json(routes[key]!(init)));
    return Promise.reject(new Error(`Unexpected ${init?.method ?? "GET"} ${path}`));
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === "POST").map(([path, init]) => [path, JSON.parse(String((init as RequestInit).body ?? "null"))]);
function mount(node: React.ReactElement) {
  render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={{ page: "access-reviews" }} navigate={vi.fn()}><ActionProvider>{node}</ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
  return userEvent.setup();
}
const base: ReviewItem = { id: "i-1", campaign_id: "c-1", subject_kind: "workspace_membership", workspace_id: "team", workspace_name: "Product", user_id: "u-1", email: "blair@example.invalid", display_name: "Blair", role: "member", source: "manual", granted_at: "2026-10-01T00:00:00Z", active: true, decision: null, decided_by: null, decided_at: null, reviewer_user_id: null, reviewable: true, api_key_id: null, key_name: null, holder_kind: null, service_account_id: null, service_account_name: null, expires_at: null, last_used_at: null };
/** A member's key as a reviewer sees it, and a service key as the platform views show it (never a name or holder). */
const humanKey: ReviewItem = { ...base, id: "k-1", subject_kind: "workspace_key", role: null, source: null, api_key_id: "0a1b2c3d-0000-4000-8000-000000000001", key_name: "zebra-human-key", holder_kind: "member", expires_at: "2099-01-01T00:00:00Z", last_used_at: null };
const serviceKey: ReviewItem = { ...humanKey, id: "k-2", api_key_id: "9f8e7d6c-0000-4000-8000-000000000002", key_name: "zebra-service-key", holder_kind: "service_account", user_id: null, email: null, display_name: null, service_account_id: "sa-1", service_account_name: "zebra-robot", last_used_at: "2026-11-01T00:00:00Z" };
const masked = (k: ReviewItem): ReviewItem => ({ ...k, user_id: null, email: null, display_name: null, key_name: null, service_account_id: null, service_account_name: null });
const asAdminOf = (ws: string) => ({ ...session, workspaces: [...session.workspaces.filter(w => w.id !== ws), { ...team, id: ws, role: "admin" as const }] });
const inbox = (patch: Partial<InboxItem>): InboxItem => ({ ...base, campaign_name: "Q4 review", due_at: "2026-11-20T00:00:00Z", ...patch });
const campaign: ReviewCampaignDetail = { id: "c-1", name: "Q4 review", subjects: ["workspace_memberships", "platform_roles"], workspace_ids: [], created_by: "admin", created_at: "2026-11-06T00:00:00Z", due_at: "2099-11-20T00:00:00Z", closed_at: null, item_count: 3,
  counts: { by_kind: { workspace_membership: 2, platform_role: 1, workspace_key: 0 }, by_source: { manual: 2, group: 1, bootstrap: 0 }, by_decision: { pending: 2, keep: 1, flag_idp: 0, remove: 0 }, inactive: 0, without_reviewer: 1 } };

describe("Review access (#90)", () => {
  it("offers keep and remove for manual access, keep and flag for group access, and says what a removal does", async () => {
    const fetch = serve({ [inboxPath]: () => ({ data: [inbox({}), inbox({ id: "i-2", user_id: "u-2", email: "grouped@example.invalid", display_name: null, source: "group" }), inbox({ id: "i-3", subject_kind: "platform_role", workspace_id: null, workspace_name: null, role: "auditor", user_id: "u-3", email: "aud@example.invalid", display_name: "Aud" })] }), [decisionPath("i-1")]: () => ({ ...base, decision: "remove", active: false }), [decisionPath("i-2")]: () => ({ ...base, decision: "flag_idp" }) });
    const user = mount(<AccessReviewInbox session={admin} />);
    expect((await screen.findAllByText("Member of Product")).length).toBe(2);
    expect(screen.getByText("Platform Auditor")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for Blair, Member of Product" }));
    const removeItem = await screen.findByRole("menuitem", { name: "Remove…" });
    expect(screen.queryByRole("menuitem", { name: /^Flag/ })).toBeNull();
    await user.click(removeItem);
    const dialog = await screen.findByRole("dialog", { name: "Remove Blair from Product?" });
    expect(dialog.textContent).toContain("Removes their membership in Product now");
    await user.click(within(dialog).getByRole("button", { name: "Remove access" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[decisionPath("i-1"), { decision: "remove" }]]));
    await user.click(screen.getByRole("button", { name: "Actions for grouped@example.invalid, Member of Product" }));
    const flag = await screen.findByRole("menuitem", { name: "Flag for identity provider…" });
    expect(screen.queryByRole("menuitem", { name: "Remove…" })).toBeNull();
    await user.click(flag);
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Flag" }));
    await waitFor(() => expect(writes(fetch)[1]).toEqual([decisionPath("i-2"), { decision: "flag_idp" }]));
    await user.click(screen.getByRole("button", { name: "Actions for Aud, Platform Auditor" }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove…" }));
    const platform = await screen.findByRole("dialog", { name: "Remove Aud's Platform Auditor role?" });
    expect(platform.textContent).toContain("The same role from an identity provider group, if they have one, stays");
    expect(platform.textContent).toContain("If they then hold no other platform role, they lose access to the gateway at once");
    expect(platform.textContent).toContain("their personal data is removed after 30 days unless they are reinstated first");
  });
  it("offers Remove on an owner's membership only to an owner of that workspace or a Platform Admin", async () => {
    const owner = inbox({ id: "i-o", user_id: "u-o", email: "own@example.invalid", display_name: "Owen", role: "owner" });
    serve({ [inboxPath]: () => ({ data: [owner] }) });
    let user = mount(<AccessReviewInbox session={asAdminOf("team")} />);
    await user.click(await screen.findByRole("button", { name: "Actions for Owen, Owner of Product" }));
    expect(await screen.findByRole("menuitem", { name: "Keep…" })).toBeTruthy();
    expect(screen.queryByRole("menuitem", { name: "Remove…" })).toBeNull();
    cleanup();
    serve({ [inboxPath]: () => ({ data: [owner] }) });
    user = mount(<AccessReviewInbox session={admin} />);
    await user.click(await screen.findByRole("button", { name: "Actions for Owen, Owner of Product" }));
    expect(await screen.findByRole("menuitem", { name: "Remove…" })).toBeTruthy();
  });
  it("shows the server's refusal and keeps the dialog open", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((path: string, init?: RequestInit) => init?.method === "POST"
      ? Promise.resolve(Response.json({ error: { code: "409", message: "Cannot remove the last workspace owner", reason: "access_review_removal_refused" } }, { status: 409 }))
      : Promise.resolve(Response.json({ data: [inbox({})] }))));
    const user = mount(<AccessReviewInbox session={admin} />);
    await user.click(await screen.findByRole("button", { name: "Actions for Blair, Member of Product" }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove…" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Remove access" }));
    expect(await within(dialog).findByText(/Cannot remove the last workspace owner/)).toBeTruthy();
  });
  it("shows an empty state when there is nothing to review", async () => {
    serve({ [inboxPath]: () => ({ data: [] }) });
    mount(<AccessReviewInbox session={admin} />);
    expect(await screen.findByText("Nothing to review.")).toBeTruthy();
  });
});

describe("Admin › Access reviews (#90)", () => {
  it("lists reviews and starts one with what the admin chose", async () => {
    const fetch = serve({ [reviewsPath]: init => init?.method === "POST" ? campaign : { data: [campaign] } });
    const user = mount(<PlatformAccessReviewsPage session={admin} />);
    await screen.findByText("Q4 review");
    expect(screen.getByText("Open")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /Start review/ }));
    const dialog = await screen.findByRole("dialog", { name: "Start an access review" });
    await user.type(within(dialog).getByRole("textbox", { name: "Name" }), "Roles");
    await user.click(within(dialog).getByRole("checkbox", { name: "Admin and Auditor roles" }));
    await user.click(within(dialog).getByRole("button", { name: "Start review" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    expect(writes(fetch)[0]).toEqual([reviewsPath, { name: "Roles", subjects: ["workspace_memberships", "platform_roles"], workspace_ids: [], due_in_days: 14 }]);
  });
  it("reviews platform roles alone with another due date, and offers the workspace filter only with memberships", async () => {
    const fetch = serve({ [reviewsPath]: init => init?.method === "POST" ? campaign : { data: [] } });
    const user = mount(<PlatformAccessReviewsPage session={admin} />);
    await user.click(await screen.findByRole("button", { name: /Start review/ }));
    const dialog = await screen.findByRole("dialog", { name: "Start an access review" });
    expect(within(dialog).getByRole("combobox", { name: /^Only these Teams and Projects/ })).toBeTruthy();
    await user.type(within(dialog).getByRole("textbox", { name: "Name" }), "Roles only");
    await user.click(within(dialog).getByRole("checkbox", { name: "Admin and Auditor roles" }));
    await user.click(within(dialog).getByRole("checkbox", { name: "Team and Project memberships" }));
    expect(within(dialog).queryByRole("combobox", { name: /^Only these Teams and Projects/ })).toBeNull();
    const days = within(dialog).getByRole("spinbutton", { name: "Due in (days)" });
    await user.clear(days);
    await user.type(days, "30");
    await user.click(within(dialog).getByRole("button", { name: "Start review" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[reviewsPath, { name: "Roles only", subjects: ["platform_roles"], workspace_ids: [], due_in_days: 30 }]]));
  });
  it("is read-only for an Auditor", async () => {
    serve({ [reviewsPath]: () => ({ data: [] }) });
    mount(<PlatformAccessReviewsPage session={auditor} />);
    await screen.findByText("No access reviews yet.");
    expect(screen.queryByRole("button", { name: /Start review/ })).toBeNull();
  });
});

describe("An access review (#90)", () => {
  const items = [base, { ...base, id: "i-2", user_id: "u-2", email: "x@example.invalid", display_name: "Xan", decision: "keep" as const, reviewer_user_id: "admin" }];
  it("summarizes the counts and lets an admin assign a reviewer, reset one and close the review", async () => {
    const fetch = serve({ [reviewPath("c-1")]: () => campaign, [reviewItemsPath("c-1")]: () => ({ data: items }), [reviewerPath("c-1", "i-1")]: () => base, ["/api/v1/platform/users?status=active&role=any"]: () => ({ data: [{ id: "u-rev", email: "rev@example.invalid", display_name: "Rev", platform_role: "user", disabled_at: null }] }), [reviewerPath("c-1", "i-2")]: () => base, [reviewClosePath("c-1")]: () => ({ ...campaign, closed_at: "2026-11-08T00:00:00Z" }) });
    const user = mount(<AccessReviewDetail session={admin} id="c-1" />);
    await screen.findByText("Blair");
    expect(screen.getByText(/2 undecided · 1 kept · 0 removed · 0 flagged · 1 without a reviewer/)).toBeTruthy();
    expect(screen.getByText("Kept")).toBeTruthy();
    expect(screen.getByText("Assigned")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Export CSV" }).getAttribute("href")).toBe("/api/v1/platform/access-reviews/c-1/export");
    await user.click(screen.getByRole("button", { name: "Actions for Xan, Member of Product" }));
    await user.click(await screen.findByRole("menuitem", { name: "Back to automatic reviewers…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[reviewerPath("c-1", "i-2"), { user_id: null }]]));
    // Assign Blair's item to a chosen reviewer.
    await user.click(screen.getByRole("button", { name: "Actions for Blair, Member of Product" }));
    await user.click(await screen.findByRole("menuitem", { name: "Assign reviewer…" }));
    const assign = await screen.findByRole("dialog", { name: "Choose who reviews Blair (Member of Product)" });
    await user.type(within(assign).getByRole("combobox", { name: /^Reviewer/ }), "rev");
    await user.click(await screen.findByRole("option", { name: "rev@example.invalid" }));
    await user.click(within(assign).getByRole("button", { name: "Assign" }));
    await waitFor(() => expect(writes(fetch)[1]).toEqual([reviewerPath("c-1", "i-1"), { user_id: "u-rev" }]));
    await user.click(screen.getByRole("button", { name: "Close review" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Close review" }));
    await waitFor(() => expect(writes(fetch).map(([p]) => p)).toEqual([reviewerPath("c-1", "i-2"), reviewerPath("c-1", "i-1"), reviewClosePath("c-1")]));
  });
  it("marks items nobody may review", async () => {
    // Decided or not, as the count counts them.
    serve({ [reviewPath("c-1")]: () => campaign, [reviewItemsPath("c-1")]: () => ({ data: [{ ...base, reviewable: false, role: "owner" as const }, { ...base, id: "i-9", user_id: "u-9", display_name: "Kept", reviewable: false, decision: "keep" as const }] }) });
    mount(<AccessReviewDetail session={admin} id="c-1" />);
    expect((await screen.findAllByText("No reviewer")).length).toBe(2);
  });
  it("offers no reviewer change on the admin's own access", async () => {
    serve({ [reviewPath("c-1")]: () => campaign, [reviewItemsPath("c-1")]: () => ({ data: [{ ...base, user_id: admin.user.id, subject_kind: "platform_role", workspace_id: null, workspace_name: null, role: "admin", display_name: "Me" }] }) });
    mount(<AccessReviewDetail session={admin} id="c-1" />);
    await screen.findByText("Me");
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
  });
  it("is overdue once its due date has passed", () => {
    expect(campaignState({ ...campaign, due_at: "2026-01-01T00:00:00Z" }, Date.parse("2026-01-02T00:00:00Z"))).toBe("overdue");
    expect(campaignState({ ...campaign, due_at: "2026-01-03T00:00:00Z" }, Date.parse("2026-01-02T00:00:00Z"))).toBe("open");
    expect(campaignState({ ...campaign, closed_at: "2026-01-02T00:00:00Z" })).toBe("closed");
  });
  it("offers nothing to an Auditor or on a closed review", async () => {
    serve({ [reviewPath("c-1")]: () => campaign, [reviewItemsPath("c-1")]: () => ({ data: items }) });
    mount(<AccessReviewDetail session={auditor} id="c-1" />);
    await screen.findByText("Blair");
    expect(screen.queryByRole("button", { name: "Close review" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    cleanup();
    serve({ [reviewPath("c-1")]: () => ({ ...campaign, closed_at: "2026-11-08T00:00:00Z" }), [reviewItemsPath("c-1")]: () => ({ data: items }) });
    mount(<AccessReviewDetail session={admin} id="c-1" />);
    await screen.findByText("Blair");
    expect(screen.getByText("Closed")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Close review" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
  });
});

describe("Keys in access reviews (#90 slice 6)", () => {
  it("shows a reviewer each key with its holder and offers only keep and revoke, saying what revoking does", async () => {
    const fetch = serve({ [inboxPath]: () => ({ data: [inbox(humanKey), inbox(serviceKey)] }), [decisionPath("k-1")]: () => ({ ...humanKey, decision: "remove", active: false }), [decisionPath("k-2")]: () => ({ ...serviceKey, decision: "keep" }) });
    const user = mount(<AccessReviewInbox session={asAdminOf("team")} />);
    expect(await screen.findByText("zebra-human-key")).toBeTruthy();
    expect(screen.getByText("Held by Blair")).toBeTruthy();
    expect(screen.getByText("Service account zebra-robot")).toBeTruthy();
    expect(screen.getAllByText("Key in Product").length).toBe(2);
    expect(screen.getByText(`Expires ${new Date("2099-01-01T00:00:00Z").toLocaleDateString()} · Never used`)).toBeTruthy();
    expect(screen.getByText(`Expires ${new Date("2099-01-01T00:00:00Z").toLocaleDateString()} · Last used ${new Date("2026-11-01T00:00:00Z").toLocaleDateString()}`)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for zebra-human-key, Held by Blair, Key in Product" }));
    const remove = await screen.findByRole("menuitem", { name: "Revoke…" });
    expect(screen.queryByRole("menuitem", { name: "Remove…" })).toBeNull();
    expect(screen.getByRole("menuitem", { name: "Keep…" })).toBeTruthy();
    expect(screen.queryByRole("menuitem", { name: /^Flag/ })).toBeNull();
    await user.click(remove);
    const dialog = await screen.findByRole("dialog", { name: "Revoke zebra-human-key in Product?" });
    expect(dialog.textContent).toContain("Held by Blair, ID 0a1b2c3d.");
    expect(dialog.textContent).toContain("New requests that use it are refused at once");
    expect(dialog.textContent).toContain("A revoked key never returns");
    await user.click(within(dialog).getByRole("button", { name: "Revoke key" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[decisionPath("k-1"), { decision: "remove" }]]));
    await user.click(screen.getByRole("button", { name: "Actions for zebra-service-key, Service account zebra-robot, Key in Product" }));
    await user.click(await screen.findByRole("menuitem", { name: "Keep…" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Keep zebra-service-key in Product?" })).getByRole("button", { name: "Keep" }));
    await waitFor(() => expect(writes(fetch)[1]).toEqual([decisionPath("k-2"), { decision: "keep" }]));
  });
  it("shows the platform views' keys without a name or holder, tells them apart, and never offers a reviewer change", async () => {
    const keys = { ...campaign, subjects: ["platform_roles", "workspace_keys", "workspace_memberships"] as ReviewCampaignDetail["subjects"], counts: { ...campaign.counts, by_kind: { ...campaign.counts.by_kind, workspace_key: 2 } } };
    serve({ [reviewPath("c-1")]: () => keys, [reviewItemsPath("c-1")]: () => ({ data: [masked(humanKey), { ...masked(serviceKey), decision: "remove" as const, active: false }, { ...masked(humanKey), id: "k-3", api_key_id: "11112222-0000-4000-8000-000000000003", reviewable: false }] }) });
    mount(<AccessReviewDetail session={admin} id="c-1" />);
    expect(await screen.findByText("Member's key 0a1b2c3d")).toBeTruthy();
    expect(screen.getAllByText("Owners and admins").length).toBe(2);
    expect(screen.getByText(/0 removed or revoked/)).toBeTruthy();
    expect(screen.getByText("Service account key 9f8e7d6c")).toBeTruthy();
    expect(screen.getByText("Revoked")).toBeTruthy();
    expect(screen.getByText("No reviewer")).toBeTruthy();
    expect(screen.getByText(/Admin and Auditor roles, Team and Project keys and Team and Project memberships/)).toBeTruthy();
    expect(screen.queryByText(/zebra/)).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
  });
  it("names a key whose holder left the workspace by its kind, and tells two keys of one name apart", async () => {
    const departed = { ...humanKey, id: "k-4", user_id: null, email: null, display_name: null };
    const twin = { ...humanKey, id: "k-5", api_key_id: "77776666-0000-4000-8000-000000000005", user_id: "u-7", email: "casey@example.invalid", display_name: "Casey" };
    serve({ [inboxPath]: () => ({ data: [inbox(humanKey), inbox(departed), inbox(twin)] }) });
    const user = mount(<AccessReviewInbox session={asAdminOf("team")} />);
    expect(await screen.findByRole("button", { name: "Actions for zebra-human-key, Member's key, Key in Product" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Actions for zebra-human-key, Held by Blair, Key in Product" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for zebra-human-key, Held by Casey, Key in Product" }));
    await user.click(await screen.findByRole("menuitem", { name: "Revoke…" }));
    expect((await screen.findByRole("dialog", { name: "Revoke zebra-human-key in Product?" })).textContent).toContain("Held by Casey, ID 77776666.");
  });
  it("starts a review of keys alone, narrowed to chosen workspaces", async () => {
    const fetch = serve({ [reviewsPath]: init => init?.method === "POST" ? campaign : { data: [] }, ["/api/v1/platform/workspaces?status=active"]: () => ({ data: [{ id: "team", name: "Product", kind: "team" }] }) });
    const user = mount(<PlatformAccessReviewsPage session={admin} />);
    await user.click(await screen.findByRole("button", { name: /Start review/ }));
    const dialog = await screen.findByRole("dialog", { name: "Start an access review" });
    await user.type(within(dialog).getByRole("textbox", { name: "Name" }), "Keys");
    await user.click(within(dialog).getByRole("checkbox", { name: "Team and Project keys" }));
    await user.click(within(dialog).getByRole("checkbox", { name: "Team and Project memberships" }));
    await user.type(within(dialog).getByRole("combobox", { name: /^Only these Teams and Projects/ }), "Prod");
    await user.click(await screen.findByRole("option", { name: "Product" }));
    await user.click(within(dialog).getByRole("button", { name: "Start review" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[reviewsPath, { name: "Keys", subjects: ["workspace_keys"], workspace_ids: ["team"], due_in_days: 14 }]]));
  });
});

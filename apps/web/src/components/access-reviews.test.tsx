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
const base: ReviewItem = { id: "i-1", campaign_id: "c-1", subject_kind: "workspace_membership", workspace_id: "team", workspace_name: "Product", user_id: "u-1", email: "blair@example.invalid", display_name: "Blair", role: "member", source: "manual", granted_at: "2026-10-01T00:00:00Z", active: true, decision: null, decided_by: null, decided_at: null, reviewer_user_id: null, reviewable: true };
const asAdminOf = (ws: string) => ({ ...session, workspaces: [...session.workspaces.filter(w => w.id !== ws), { ...team, id: ws, role: "admin" as const }] });
const inbox = (patch: Partial<InboxItem>): InboxItem => ({ ...base, campaign_name: "Q4 review", due_at: "2026-11-20T00:00:00Z", ...patch });
const campaign: ReviewCampaignDetail = { id: "c-1", name: "Q4 review", subjects: ["workspace_memberships", "platform_roles"], workspace_ids: [], created_by: "admin", created_at: "2026-11-06T00:00:00Z", due_at: "2099-11-20T00:00:00Z", closed_at: null, item_count: 3,
  counts: { by_kind: { workspace_membership: 2, platform_role: 1 }, by_source: { manual: 2, group: 1, bootstrap: 0 }, by_decision: { pending: 2, keep: 1, flag_idp: 0, remove: 0 }, inactive: 0, without_reviewer: 1 } };

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

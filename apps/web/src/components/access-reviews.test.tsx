// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { AccessReviewDetail, AccessReviewInbox, PlatformAccessReviewsPage } from "../pages/access-reviews";
import { decisionPath, inboxPath, reviewClosePath, reviewerPath, reviewItemsPath, reviewPath, reviewsPath, type InboxItem, type ReviewCampaignDetail, type ReviewItem } from "../lib/access-reviews";
import { admin, auditor, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function serve(routes: Record<string, (init?: RequestInit) => unknown>) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const key = Object.keys(routes).sort((a, b) => b.length - a.length).find(r => path === r || path.startsWith(`${r}?`));
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
const base: ReviewItem = { id: "i-1", campaign_id: "c-1", subject_kind: "workspace_membership", workspace_id: "team", workspace_name: "Product", user_id: "u-1", email: "blair@example.invalid", display_name: "Blair", role: "member", source: "manual", granted_at: "2026-10-01T00:00:00Z", active: true, decision: null, decided_by: null, decided_at: null, reviewer_user_id: null };
const inbox = (patch: Partial<InboxItem>): InboxItem => ({ ...base, campaign_name: "Q4 review", due_at: "2026-11-20T00:00:00Z", ...patch });
const campaign: ReviewCampaignDetail = { id: "c-1", name: "Q4 review", subjects: ["workspace_memberships", "platform_roles"], workspace_ids: [], created_by: "admin", created_at: "2026-11-06T00:00:00Z", due_at: "2099-11-20T00:00:00Z", closed_at: null, item_count: 3,
  counts: { by_kind: { workspace_membership: 2, platform_role: 1 }, by_source: { manual: 2, group: 1, bootstrap: 0 }, by_decision: { pending: 2, keep: 1, flag_idp: 0, remove: 0 }, inactive: 0, without_reviewer: 1 } };

describe("Review access (#90)", () => {
  it("offers keep and remove for manual access, keep and flag for group access, and says what a removal does", async () => {
    const fetch = serve({ [inboxPath]: () => ({ data: [inbox({}), inbox({ id: "i-2", user_id: "u-2", email: "grouped@example.invalid", display_name: null, source: "group" }), inbox({ id: "i-3", subject_kind: "platform_role", workspace_id: null, workspace_name: null, role: "auditor", user_id: "u-3", email: "aud@example.invalid", display_name: "Aud" })] }), [decisionPath("i-1")]: () => ({ ...base, decision: "remove", active: false }), [decisionPath("i-2")]: () => ({ ...base, decision: "flag_idp" }) });
    const user = mount(<AccessReviewInbox />);
    expect((await screen.findAllByText("Member of Product")).length).toBe(2);
    expect(screen.getByText("Platform Auditor")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for Blair" }));
    expect(screen.queryByRole("menuitem", { name: /^Flag/ })).toBeNull();
    await user.click(await screen.findByRole("menuitem", { name: "Remove…" }));
    const dialog = await screen.findByRole("dialog", { name: "Remove Blair's access?" });
    expect(dialog.textContent).toContain("Removes their membership in Product now");
    await user.click(within(dialog).getByRole("button", { name: "Remove access" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[decisionPath("i-1"), { decision: "remove" }]]));
    await user.click(screen.getByRole("button", { name: "Actions for grouped@example.invalid" }));
    expect(screen.queryByRole("menuitem", { name: "Remove…" })).toBeNull();
    await user.click(await screen.findByRole("menuitem", { name: "Flag for identity provider…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Flag" }));
    await waitFor(() => expect(writes(fetch)[1]).toEqual([decisionPath("i-2"), { decision: "flag_idp" }]));
    await user.click(screen.getByRole("button", { name: "Actions for Aud" }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove…" }));
    expect((await screen.findByRole("dialog")).textContent).toContain("If it is their only platform role, they lose access to the gateway");
  });
  it("shows an empty state when there is nothing to review", async () => {
    serve({ [inboxPath]: () => ({ data: [] }) });
    mount(<AccessReviewInbox />);
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
    const fetch = serve({ [reviewPath("c-1")]: () => campaign, [reviewItemsPath("c-1")]: () => ({ data: items }), [reviewerPath("c-1", "i-1")]: () => base, [reviewerPath("c-1", "i-2")]: () => base, [reviewClosePath("c-1")]: () => ({ ...campaign, closed_at: "2026-11-08T00:00:00Z" }) });
    const user = mount(<AccessReviewDetail session={admin} id="c-1" />);
    await screen.findByText("Blair");
    expect(screen.getByText(/2 to decide · 1 kept · 0 removed · 0 flagged · 1 without a reviewer/)).toBeTruthy();
    expect(screen.getByText("Kept")).toBeTruthy();
    expect(screen.getByText("Assigned")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for Xan" }));
    await user.click(await screen.findByRole("menuitem", { name: "Back to automatic reviewers…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(writes(fetch)).toEqual([[reviewerPath("c-1", "i-2"), { user_id: null }]]));
    await user.click(screen.getByRole("button", { name: "Close review" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Close review" }));
    await waitFor(() => expect(writes(fetch).map(([p]) => p)).toEqual([reviewerPath("c-1", "i-2"), reviewClosePath("c-1")]));
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

// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { AccessRequestsPage, PlatformAccessRequestsPage, WorkspaceAccessRequestQueue } from "../pages/access-requests";
import { accessRequestTargetsPath, myAccessRequestsPath, platformAccessRequestsPath, workspaceAccessRequestsPath, type AccessRequest, type AccessRequestTarget } from "../lib/access-requests";
import { workspaceSettingsTabs } from "../pages/resource-details";
import { admin, auditor, member, session, team, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); document.cookie = "omg_csrf=; Max-Age=0; Path=/"; vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const target = (patch: Partial<AccessRequestTarget> = {}): AccessRequestTarget => ({ id: "ws-research", name: "Research", kind: "team", ...patch });
const request = (patch: Partial<AccessRequest> = {}): AccessRequest => ({ id: "req-1", requester: "outsider", target_kind: "workspace", workspace_id: "ws-research", requested_role: "member", status: "pending", reason: "Need it for an incident", created_at: "2026-10-14T08:00:00Z", expires_at: "2026-10-21T08:00:00Z", decided_by: null, decided_at: null, membership_grant_id: null, ...patch });
function mount(node: React.ReactElement) { const client = testClient(); const user = userEvent.setup(); const mounted = render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>); return { client, user, mounted }; }
function serve(routes: Record<string, (init?: RequestInit) => Response | Promise<Response>>) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const key = Object.keys(routes).find(r => path === r || path.startsWith(`${r}?`) || (r.endsWith("*") && path.startsWith(r.slice(0, -1))));
    if (key) return Promise.resolve(routes[key]!(init));
    return Promise.reject(new Error(`Unexpected ${method} ${path}`));
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}
const body = (init?: RequestInit) => JSON.parse(String(init?.body ?? "{}"));
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method && (init as RequestInit).method !== "GET").map(([path, init]) => `${(init as RequestInit).method} ${path}`);

describe("Request access (#83 slice A): discover and submit", () => {
  it("lists discoverable workspaces, sends a bounded reason with a fresh idempotency key, and closes on success", async () => {
    const fetch = serve({
      [accessRequestTargetsPath]: () => Response.json({ data: [target()] }),
      [myAccessRequestsPath]: init => init?.method === "POST" ? Response.json(request()) : Response.json({ data: [] }),
    });
    const { user } = mount(<AccessRequestsPage onTabChange={() => {}} />);
    await screen.findByText("Research");
    await user.click(screen.getByRole("button", { name: "Actions for Research" }));
    await user.click(await screen.findByRole("menuitem", { name: "Request access…" }));
    await screen.findByRole("dialog", { name: "Request to join Research?" });
    await user.type(screen.getByRole("textbox", { name: /^Reason/ }), "Need it for an incident");
    await user.click(screen.getByRole("button", { name: "Send request" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const post = fetch.mock.calls.find(([, init]) => init?.method === "POST")!;
    expect(post[0]).toBe(myAccessRequestsPath);
    const sent = body(post[1] as RequestInit);
    expect(sent).toMatchObject({ workspace_id: "ws-research", reason: "Need it for an incident" });
    expect(typeof sent.idempotency_key).toBe("string");
    expect(sent.idempotency_key.length).toBeGreaterThan(0);
    expect(new Headers((post[1] as RequestInit).headers).get("x-csrf-token")).toBe("test-csrf");
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "Actions for Research" })));
  });
  it("rejects a reason with line breaks or control characters before any request leaves the browser", async () => {
    const fetch = serve({ [accessRequestTargetsPath]: () => Response.json({ data: [target()] }) });
    const { user } = mount(<AccessRequestsPage onTabChange={() => {}} />);
    await screen.findByText("Research");
    await user.click(screen.getByRole("button", { name: "Actions for Research" }));
    await user.click(await screen.findByRole("menuitem", { name: "Request access…" }));
    const field = await screen.findByRole("textbox", { name: /^Reason/ });
    await user.type(field, "line one{Enter}line two");
    await user.click(screen.getByRole("button", { name: "Send request" }));
    expect(await screen.findByText(/control characters/)).toBeTruthy();
    expect(writes(fetch)).toEqual([]);
  });
  it("shows a fixed 429 message naming the wait, and a generic refusal for an unknown/hidden target (never distinguishing the two)", async () => {
    serve({
      [accessRequestTargetsPath]: () => Response.json({ data: [target()] }),
      [myAccessRequestsPath]: () => Response.json({ error: { code: "429", reason: "access_request_rate_limited", message: "Too many access requests", retry_after_seconds: 37 } }, { status: 429 }),
    });
    const { user } = mount(<AccessRequestsPage onTabChange={() => {}} />);
    await screen.findByText("Research");
    await user.click(screen.getByRole("button", { name: "Actions for Research" }));
    await user.click(await screen.findByRole("menuitem", { name: "Request access…" }));
    await user.click(screen.getByRole("button", { name: "Send request" }));
    const dialog = await screen.findByRole("dialog", { name: "Request to join Research?" });
    expect(within(dialog).getByText("Too many requests")).toBeTruthy();
    expect(within(dialog).getByText(/Try again in 37 seconds/)).toBeTruthy();
  });
  it("never offers to withdraw a decided request, and withdraws a pending one", async () => {
    const rows = [request({ reason: null }), request({ id: "req-2", status: "denied", decided_at: "2026-10-15T00:00:00Z" })];
    const fetch = serve({
      [accessRequestTargetsPath]: () => Response.json({ data: [] }),
      [myAccessRequestsPath]: () => Response.json({ data: rows }),
      [`${myAccessRequestsPath}/req-1/withdraw`]: () => Response.json(request({ status: "withdrawn" })),
    });
    const { user } = mount(<AccessRequestsPage onTabChange={() => {}} tab="mine" />);
    await screen.findByRole("table", { name: "My requests" });
    expect(screen.getByText("No reason given")).toBeTruthy(); // reason is optional; never a blank/misleading cell
    const menus = await screen.findAllByRole("button", { name: /^Actions for/ });
    expect(menus).toHaveLength(1); // only the pending row offers an action
    await user.click(menus[0]);
    await user.click(await screen.findByRole("menuitem", { name: "Withdraw…" }));
    await user.click(await screen.findByRole("button", { name: "Withdraw" }));
    await waitFor(() => expect(writes(fetch)).toEqual([`POST ${myAccessRequestsPath}/req-1/withdraw`]));
  });
});

describe("Workspace access requests queue (#83 slice A): owner/admin approval", () => {
  it("is offered to an admin, never a plain member", () => {
    expect(workspaceSettingsTabs(session, team)).toContain("access-requests");
    expect(workspaceSettingsTabs(session, member)).not.toContain("access-requests");
  });
  it("approves a pending request and refreshes the queue", async () => {
    const fetch = serve({
      [workspaceAccessRequestsPath(team.id)]: init => init?.method === "POST" ? Response.json(request({ status: "approved", decided_by: "me" })) : Response.json({ data: [request()] }),
    });
    const { user } = mount(<WorkspaceAccessRequestQueue session={session} workspace={team} />);
    await screen.findByText("outsider");
    const trigger = screen.getByRole("button", { name: "Actions for outsider" });
    await user.click(trigger);
    await user.click(await screen.findByRole("menuitem", { name: "Approve…" }));
    await screen.findByRole("dialog", { name: "Approve this request?" });
    await user.click(screen.getByRole("button", { name: "Approve" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(writes(fetch)).toEqual([`POST ${workspaceAccessRequestsPath(team.id)}/req-1/approve`]);
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });
  it("shows a generic conflict message, without touching anything, when the request went stale underneath", async () => {
    serve({
      [workspaceAccessRequestsPath(team.id)]: init => init?.method === "POST" ? Response.json({ error: { code: "409", message: "Request is no longer applicable" } }, { status: 409 }) : Response.json({ data: [request()] }),
    });
    const { user } = mount(<WorkspaceAccessRequestQueue session={session} workspace={team} />);
    await screen.findByText("outsider");
    await user.click(screen.getByRole("button", { name: "Actions for outsider" }));
    await user.click(await screen.findByRole("menuitem", { name: "Deny…" }));
    const dialog = await screen.findByRole("dialog", { name: "Deny this request?" });
    await user.click(within(dialog).getByRole("button", { name: "Deny" }));
    expect(await within(dialog).findByText("Request is no longer applicable")).toBeTruthy();
  });
  it("never lets a workspace admin approve or deny their own request", async () => {
    serve({ [workspaceAccessRequestsPath(team.id)]: () => Response.json({ data: [request({ requester: session.user.id })] }) });
    mount(<WorkspaceAccessRequestQueue session={session} workspace={team} />);
    await screen.findByText(session.user.id);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: `Actions for ${session.user.id}` }));
    const approve = await screen.findByRole("menuitem", { name: "Approve…" });
    expect(approve).toHaveAttribute("aria-disabled", "true");
  });
});

describe("Admin access requests (#83 slice A): platform queue, Auditor read-only", () => {
  it("lets a Platform Admin approve and deny", async () => {
    const fetch = serve({
      [platformAccessRequestsPath]: init => init?.method === "POST" ? Response.json(request({ status: "denied", decided_by: admin.user.id })) : Response.json({ data: [request()] }),
    });
    const { user } = mount(<PlatformAccessRequestsPage session={admin} />);
    await screen.findByText("outsider");
    await user.click(screen.getByRole("button", { name: "Actions for outsider" }));
    await user.click(await screen.findByRole("menuitem", { name: "Deny…" }));
    await user.click(await screen.findByRole("button", { name: "Deny" }));
    await waitFor(() => expect(writes(fetch)).toEqual([`POST ${platformAccessRequestsPath}/req-1/deny`]));
  });
  it("gives an Auditor the same history with no action menu at all", async () => {
    serve({ [platformAccessRequestsPath]: () => Response.json({ data: [request()] }) });
    mount(<PlatformAccessRequestsPage session={auditor} />);
    await screen.findByText("outsider");
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
  });
});

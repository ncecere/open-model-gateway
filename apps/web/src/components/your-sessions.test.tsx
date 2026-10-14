// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { YourSessions, type DashboardSession } from "./your-sessions";
import { UserDetail } from "../pages/hierarchy";
import { admin, markup, testClient } from "../lib/test-fixtures";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const row = (id: string, client: string | null, current = false): DashboardSession => ({ id, client, created_at: "2026-10-14T08:00:00Z", last_seen_at: "2026-10-14T09:00:00Z", expires_at: "2026-10-14T20:00:00Z", current });

/** Serves the session list and records writes; after a write, the list holds only what's left. */
function serve(initial: DashboardSession[]) {
  let rows = initial;
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    if (path === "/api/v1/me/sessions" && method === "GET") return Promise.resolve(Response.json({ data: rows }));
    if (path === "/api/v1/me/sessions/revoke-others" && method === "POST") { const n = rows.filter(r => !r.current).length; rows = rows.filter(r => r.current); return Promise.resolve(Response.json({ revoked: n })); }
    const one = path.match(/^\/api\/v1\/me\/sessions\/([^/]+)$/);
    if (one && method === "DELETE") { rows = rows.filter(r => r.id !== one[1]); return Promise.resolve(Response.json({ revoked: 1 })); }
    return Promise.reject(new Error(`Unexpected ${method} ${path}`));
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}
const mount = () => render(<QueryClientProvider client={testClient()}><ActionProvider><YourSessions /></ActionProvider></QueryClientProvider>);
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method && (init as RequestInit).method !== "GET").map(([path, init]) => `${(init as RequestInit).method} ${path}`);

describe("Your sessions (#79)", () => {
  it("lists this browser first, never offers to sign it out here, and signs one other session out", async () => {
    const user = userEvent.setup(), fetch = serve([row("here", "Firefox on macOS", true), row("phone", "Safari on iOS"), row("old", null)]);
    mount();
    const table = await screen.findByRole("table", { name: "Your dashboard sessions" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows.map(r => r.textContent)).toEqual([expect.stringContaining("Firefox on macOS"), expect.stringContaining("Safari on iOS"), expect.stringContaining("Unknown browser")]);
    expect(within(rows[0]).getByText("This browser")).toBeTruthy();
    expect(within(rows[0]).queryByRole("button")).toBeNull();
    // Labels can repeat, so each button and dialog also names the sign-in time.
    await user.click(screen.getByRole("button", { name: "Sign out Safari on iOS, signed in Oct 14, 2026, 8:00 AM UTC" }));
    const dialog = await screen.findByRole("dialog", { name: "Sign out Safari on iOS, signed in Oct 14, 2026, 8:00 AM UTC?" });
    await user.click(within(dialog).getByRole("button", { name: "Sign out" }));
    await waitFor(() => expect(screen.queryByText("Safari on iOS")).toBeNull());
    expect(writes(fetch)).toEqual(["DELETE /api/v1/me/sessions/phone"]);
    const [, init] = fetch.mock.calls.find(([, i]) => (i as RequestInit | undefined)?.method === "DELETE")!;
    expect(new Headers((init as RequestInit).headers).get("x-csrf-token")).toBe("test-csrf");
  });

  it("signs out every other session after confirmation and keeps this one", async () => {
    const user = userEvent.setup(), fetch = serve([row("here", "Edge on Windows", true), row("a", "Chrome on Android"), row("b", "Safari on iPadOS")]);
    mount();
    await user.click(await screen.findByRole("button", { name: "Sign out others" }));
    const dialog = await screen.findByRole("dialog", { name: "Sign out your other sessions?" });
    expect(dialog.textContent).toContain("Ends 2 other sessions now");
    await user.click(within(dialog).getByRole("button", { name: "Sign out others" }));
    await waitFor(() => expect(screen.queryByText("Chrome on Android")).toBeNull());
    expect(screen.getByText("Edge on Windows")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Sign out others" })).toBeNull();
    expect(writes(fetch)).toEqual(["POST /api/v1/me/sessions/revoke-others"]);
  });

  it("shows admins a session count, never devices, and hides the action when there are none", () => {
    const uid = "11111111-1111-4111-8111-111111111111", path = `/api/v1/platform/users/${uid}`;
    const detail = { id: uid, email: "pat@example.invalid", display_name: null, platform_role: "user", disabled_at: null, cleaned_at: null, cleanup_due_at: null, created_at: "2026-10-01T00:00:00Z", last_sign_in_at: "2026-10-14T08:00:00Z", first_sign_in_at: "2026-10-01T00:00:00Z", role_grants: [], shared_memberships: [] };
    const html = markup(<UserDetail session={admin} id={uid} onTabChange={() => {}} />, [[path, { ...detail, active_sessions: 3 }]]);
    expect(html).toContain("Active sessions");
    expect(html).toMatch(/Active sessions.*?3/s);
    const none = markup(<UserDetail session={admin} id={uid} onTabChange={() => {}} />, [[path, { ...detail, active_sessions: 0 }]]);
    expect(none).toMatch(/Active sessions.*?0/s);
  });
});

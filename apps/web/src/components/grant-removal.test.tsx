// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, cleanup } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ActionProvider } from "./ui";
import { PlatformUsers, UserDetail } from "../pages/hierarchy";
import { abortRequests } from "../lib/api";
import { admin, auditor, testClient } from "../lib/test-fixtures";

const uid = "8e210000-0000-4000-8000-000000000009";
const user = { id: uid, email: "acceptance@demo.invalid", platform_role: null, disabled_at: "2026-09-01T00:00:00Z", last_sign_in_at: null, shared_workspace_count: 0, role_grants: [{ id: "g1", role: "auditor", source: "manual" }, { id: "g2", role: "user", source: "group" }] };
const listPath = "/api/v1/platform/users?limit=50&offset=0";
function mount(node: ReactNode, client = testClient()) { return { client, ...render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>) }; }
beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("removing a manual platform grant", () => {
  it("offers no inline removal in the Users list (review #44)", () => {
    const client = testClient();
    client.setQueryData(["api", undefined, listPath], { data: [user], has_more: false });
    mount(<PlatformUsers session={admin} />, client);
    expect(screen.getByText("Auditor · manual")).toBeDefined();
    expect(screen.queryAllByRole("button", { name: /^Remove / })).toHaveLength(0);
    client.clear();
  });
  it("confirms on the user's page, then calls DELETE /platform/users/{id}/roles/{role}", async () => {
    const actor = userEvent.setup(), client = testClient(), path = `/api/v1/platform/users/${uid}`;
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(url.endsWith("/roles/auditor") ? Response.json({ ok: true }) : Response.json({ ...user, first_sign_in_at: null, shared_memberships: [] })));
    vi.stubGlobal("fetch", fetch);
    client.setQueryData(["api", undefined, path], { ...user, first_sign_in_at: null, shared_memberships: [] });
    mount(<UserDetail session={admin} id={uid} tab="overview" onTabChange={() => {}} />, client);
    expect(screen.queryByRole("button", { name: "Remove User group grant" })).toBeNull();
    await actor.click(screen.getByRole("button", { name: "Remove Auditor manual grant" }));
    const dialog = await screen.findByRole("dialog", { name: "Remove Auditor manual grant from acceptance@demo.invalid?" });
    expect(dialog.textContent).toContain("Group grants stay untouched.");
    expect(dialog.textContent).toContain("Losing every role revokes their sessions and their own keys.");
    expect(fetch).not.toHaveBeenCalled(); // nothing happens before confirmation
    await actor.click(screen.getByRole("button", { name: "Remove grant" }));
    await waitFor(() => expect(fetch.mock.calls.some(([url, init]) => url === `/api/v1/platform/users/${uid}/roles/auditor` && (init as RequestInit).method === "DELETE")).toBe(true));
    client.clear();
  });
  it("shows the server's last-admin refusal in the dialog", async () => {
    const actor = userEvent.setup(), client = testClient(), path = `/api/v1/platform/users/${uid}`;
    const record = { ...user, platform_role: "admin", disabled_at: null, role_grants: [{ id: "g1", role: "admin", source: "bootstrap" }], first_sign_in_at: null, shared_memberships: [] };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { code: "409", message: "Cannot remove the last platform administrator" } }, { status: 409 })));
    client.setQueryData(["api", undefined, path], record);
    mount(<UserDetail session={admin} id={uid} tab="overview" onTabChange={() => {}} />, client);
    await actor.click(screen.getByRole("button", { name: "Remove Admin bootstrap grant" }));
    const dialog = await screen.findByRole("dialog", { name: "Remove Admin bootstrap grant from acceptance@demo.invalid?" });
    expect(dialog.textContent).toContain("The last Platform Admin is protected");
    expect(dialog.textContent).toContain("revokes their sessions and user-owned keys");
    await actor.click(screen.getByRole("button", { name: "Remove grant" }));
    expect(await screen.findByText(/Cannot remove the last platform administrator/)).toBeDefined();
    client.clear();
  });
  it("gives Auditors no remove controls", () => {
    const client = testClient();
    client.setQueryData(["api", undefined, listPath], { data: [user], has_more: false });
    mount(<PlatformUsers session={auditor} />, client);
    expect(screen.queryAllByRole("button", { name: /^Remove / })).toHaveLength(0);
    expect(screen.getByText("Auditor · manual")).toBeDefined();
    client.clear();
  });
});

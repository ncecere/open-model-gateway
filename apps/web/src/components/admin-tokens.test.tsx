// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionProvider } from "./ui";
import { AdminTokensPage, adminTokensPath } from "../pages/admin-tokens";
import { admin, testClient } from "../lib/test-fixtures";
import { abortRequests } from "../lib/api";
import type { AdminToken } from "../lib/admin-tokens";

const listQuery = ["api", undefined, `${adminTokensPath}?limit=100&offset=0`];
const token = (patch: Partial<AdminToken> = {}): AdminToken => ({
  id: "tok-1", owner_user_id: "me", owner_email: "me@example.invalid", owner_display_name: "Pat Doe",
  name: "CI publisher", scopes: ["catalog:write"], ip_allowlist: null,
  created_at: "2026-09-01T00:00:00Z", expires_at: "2027-01-01T00:00:00Z", revoked_at: null,
  last_used_at: null, last_used_address: null, ...patch,
});

function mount(rows: AdminToken[]) {
  const client = testClient();
  client.setQueryData(listQuery, { data: rows });
  const user = userEvent.setup();
  const mounted = render(<QueryClientProvider client={client}><ActionProvider><AdminTokensPage session={admin} /></ActionProvider></QueryClientProvider>);
  return { client, user, mounted };
}

beforeEach(() => {
  document.cookie = "omg_csrf=synthetic-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => {
  cleanup(); abortRequests();
  document.cookie = "omg_csrf=; Max-Age=0; Path=/";
  vi.unstubAllGlobals(); vi.restoreAllMocks();
});

describe("Admin API tokens list (#37 PR3)", () => {
  it("shows metadata only: owner, status, scopes, expiry and 'Never' for an unused token, never a plaintext secret column", async () => {
    mount([token()]);
    await screen.findByText("CI publisher");
    expect(screen.getByText("Pat Doe")).toBeDefined();
    expect(screen.getByText("me@example.invalid")).toBeDefined();
    expect(screen.getByText("Catalog")).toBeDefined();
    expect(screen.getByText("Never")).toBeDefined();
    expect(document.body.textContent).not.toMatch(/omg_admin_/);
  });
  it("offers Revoke for an active token and sends an idempotent DELETE that refreshes the list", async () => {
    const view = mount([token()]);
    const fetchSpy = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      if (input === `${adminTokensPath}/tok-1` && init?.method === "DELETE") return Promise.resolve(new Response(null, { status: 204 }));
      if (input === `${adminTokensPath}?limit=100&offset=0` && (init?.method ?? "GET") === "GET") return Promise.resolve(Response.json({ data: [token({ revoked_at: "2026-10-23T00:00:00Z" })] }));
      throw new Error("Unexpected synthetic fetch: no real network is permitted.");
    });
    vi.stubGlobal("fetch", fetchSpy);
    await screen.findByText("CI publisher");
    await view.user.click(screen.getByRole("button", { name: "Actions for CI publisher" }));
    await view.user.click(await screen.findByRole("menuitem", { name: "Revoke token…" }));
    await view.user.click(await screen.findByRole("button", { name: "Revoke token" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const deleteCall = fetchSpy.mock.calls.find(([, init]) => init?.method === "DELETE");
    expect(deleteCall?.[0]).toBe(`${adminTokensPath}/tok-1`);
    // The row refreshes from the server rather than an optimistic local edit.
    await waitFor(() => expect(screen.getByText("Revoked")).toBeDefined());
  });
  it("never offers Revoke for an already-revoked token", async () => {
    mount([token({ id: "tok-2", revoked_at: "2026-10-01T00:00:00Z" })]);
    await screen.findByText("CI publisher");
    await userEvent.setup().click(screen.getByRole("button", { name: "Actions for CI publisher" }));
    expect(screen.queryByRole("menuitem", { name: "Revoke token…" })).toBeNull();
    expect(screen.getByText("Revoked")).toBeDefined();
  });
  it("labels an expired-but-unrevoked token distinctly and still offers Revoke for a clean audit trail", async () => {
    mount([token({ id: "tok-3", expires_at: "2020-01-01T00:00:00Z" })]);
    await screen.findByText("CI publisher");
    expect(screen.getByText("Expired")).toBeDefined();
    await userEvent.setup().click(screen.getByRole("button", { name: "Actions for CI publisher" }));
    expect(await screen.findByRole("menuitem", { name: "Revoke token…" })).toBeDefined();
  });
  it("shows the immutable IP allowlist metadata in an optional column, never editable", async () => {
    mount([token({ id: "tok-allow", ip_allowlist: ["10.0.0.0/8", "192.168.1.0/24"] }), token({ id: "tok-open", name: "Open automation", ip_allowlist: null })]);
    await screen.findByText("CI publisher");
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Columns" }));
    await user.click(await screen.findByRole("menuitemcheckbox", { name: "IP allowlist" }));
    expect(await screen.findByText("2 addresses")).toBeDefined();
    expect(screen.getByText("Any")).toBeDefined();
    // Metadata only: no control anywhere in this row lets the allowlist be edited after creation.
    expect(screen.queryByRole("textbox", { name: /allowlist/i })).toBeNull();
  });

  it("opens the create dialog from the page action and returns focus to it on cancel", async () => {
    const view = mount([]);
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await screen.findByRole("dialog", { name: "Create admin API token" });
    await view.user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "Create token" })));
  });
});

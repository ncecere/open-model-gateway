// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { DeploymentConfigurationTabs, DeploymentHistory } from "./deployment-history";
import { testClient } from "../lib/test-fixtures";
import { ActionProvider } from "./ui";
const change = { id: "change", created_at: "2026-10-21T12:00:00Z", target: { kind: "deployment", id: "route" }, actor: { id: "actor", label: "Operator", admin_token_id: null }, action: "configuration.updated", schema_version: 1, revision: "2", values_available: true, before: { enabled: true }, after: { enabled: false } };
function view() { render(<QueryClientProvider client={testClient()}><DeploymentHistory id="route" /></QueryClientProvider>); return userEvent.setup(); }
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
describe("deployment History", () => {
  it("opens the route History tab lazily and supports keyboard navigation", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ data: [change], next_cursor: null }));
    vi.stubGlobal("fetch", fetch);
    render(<QueryClientProvider client={testClient()}><DeploymentConfigurationTabs id="route"><p>Route settings</p></DeploymentConfigurationTabs></QueryClientProvider>);
    expect(screen.getByRole("tab", { name: "Configuration" }).getAttribute("aria-selected")).toBe("true");
    expect(fetch).not.toHaveBeenCalled();
    const user = userEvent.setup();
    await user.click(screen.getByRole("tab", { name: "History" }));
    await screen.findByText("Enabled → Disabled");
    expect(screen.getByRole("tabpanel").getAttribute("aria-labelledby")).toBe(screen.getByRole("tab", { name: "History" }).id);
    await user.keyboard("{ArrowLeft}{Enter}");
    expect(screen.getByRole("tab", { name: "Configuration" }).getAttribute("aria-selected")).toBe("true");
  });
  it("shows readable diffs, actors and cursor pagination without writes", async () => {
    // Newest page for every request without a cursor (going back to "Newer" refetches it).
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(url.includes("cursor=")
      ? Response.json({ data: [{ ...change, id: "old", values_available: false, before: null, after: null, revision: null }], next_cursor: null })
      : Response.json({ data: [change], next_cursor: "cursor" })));
    vi.stubGlobal("fetch", fetch);
    const user = view();
    await screen.findByText("Enabled → Disabled");
    expect(screen.getByText("Operator")).toBeTruthy();
    expect(screen.getByText("Revision 2")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Older changes" }));
    await screen.findByText("Values unavailable");
    expect(fetch.mock.calls[1][0]).toContain("cursor=cursor");
    expect(screen.getByRole("button", { name: "Older changes" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("button", { name: "Newer changes" }));
    await screen.findByText("Enabled → Disabled");
    expect(fetch.mock.calls.every(([, options]) => options.method === undefined || options.method === "GET")).toBe(true);
  });
  it("never reconstructs legacy values and has no restore action", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ data: [{ ...change, values_available: false, before: null, after: null, actor: { id: null, label: "System", admin_token_id: null } }], next_cursor: null })));
    view();
    await screen.findByText("Values unavailable");
    expect(screen.queryByText("Enabled → Disabled")).toBeNull();
    expect(screen.queryByRole("button", { name: /Restore/i })).toBeNull();
  });
  it("shows refusal without stale history", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { message: "Not authorized" } }, { status: 403 })));
    view();
    await screen.findByText("Not authorized");
    expect(screen.queryByText("Operator")).toBeNull();
  });
  // #87: a restore is a new change through the guarded apply, offered only
  // to Platform Admins (the server authorizes every step).
  const restorePreview = { target: { kind: "deployment", id: "route" }, expected_revision: "3", diff: [{ field: "enabled", before: false, after: true }], serving_routes_gained: ["route"], serving_routes_lost: [], models_losing_last_serving_route: [], affected_workspaces: { data: [], total: 0, limit: 100, truncated: false }, observed_at: "2026-10-21T12:00:00Z", dependency_versions: {}, warnings: ["Estimate"], restore: { change_id: "created", revision: "1", created_at: "2026-10-20T12:00:00Z", enabled: true } };
  const created = { ...change, id: "created", action: "deployment.created", revision: "1", before: null, after: { enabled: true } };
  function restoreView(restore = true) {
    document.cookie = "omg_csrf=test; path=/";
    render(<QueryClientProvider client={testClient()}><ActionProvider><DeploymentHistory id="route" restore={restore ? { record: { id: "route", enabled: false }, name: "Upstream" } : undefined} /></ActionProvider></QueryClientProvider>);
    return userEvent.setup();
  }
  it("offers restore only to Platform Admins and only for recorded values", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => Promise.resolve(Response.json({ data: [created, { ...change, id: "legacy", values_available: false, before: null, after: null }], next_cursor: null }))));
    restoreView(false);
    await screen.findByText("Values unavailable");
    expect(screen.queryByRole("button", { name: "Restore this configuration" })).toBeNull();
    cleanup();
    restoreView();
    await screen.findByText("Values unavailable");
    expect(screen.getAllByRole("button", { name: "Restore this configuration" })).toHaveLength(1);
  });
  it("previews the restore, then applies at the previewed revision with its source", async () => {
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(
      url.endsWith("/restore-preview") ? Response.json(restorePreview)
        : url.endsWith("/configuration/apply") ? Response.json({ outcome: "applied", target: { kind: "deployment", id: "route" }, revision: "4", change_id: "new", restored_from: "created" })
          : Response.json({ data: [{ ...created, restored_from: null }, { ...change, id: "restored", restored_from: "created", bulk_operation_id: "bulk-op" }], next_cursor: null })));
    vi.stubGlobal("fetch", fetch);
    const user = restoreView();
    await screen.findByText(/Restored the configuration of change/);
    expect(screen.getByText("bulk-op")).toBeTruthy();
    await user.click(screen.getAllByRole("button", { name: "Restore this configuration" })[0]);
    await screen.findByText(/Restores the configuration recorded at revision 1/);
    const preview = fetch.mock.calls.find(([url]) => String(url).endsWith("/restore-preview"))!;
    expect(preview[1]).toMatchObject({ method: "POST", body: JSON.stringify({ change_id: "created" }) });
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Restore Upstream\?$/ });
    await user.click(screen.getByRole("button", { name: /^Restore configuration$/ }));
    await waitFor(() => expect(fetch.mock.calls.some(([url]) => String(url).endsWith("/configuration/apply"))).toBe(true));
    const apply = fetch.mock.calls.find(([url]) => String(url).endsWith("/configuration/apply"))!;
    expect(apply[1]).toMatchObject({ method: "POST", body: JSON.stringify({ enabled: true, expected_revision: "3", restore_of: "created" }) });
  });
  it("cannot confirm a restore that changes nothing, and a stale revision stays in the confirmation", async () => {
    let unchanged = true;
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(
      url.endsWith("/restore-preview") ? Response.json(unchanged ? { ...restorePreview, diff: [] } : restorePreview)
        : url.endsWith("/configuration/apply") ? Response.json({ error: { code: "409", reason: "revision_conflict", message: "This route's enablement changed since the preview; review it again." } }, { status: 409 })
          : Response.json({ data: [created], next_cursor: null })));
    vi.stubGlobal("fetch", fetch);
    const user = restoreView();
    await user.click(await screen.findByRole("button", { name: "Restore this configuration" }));
    await screen.findByText("The route already has this configuration. Nothing would be applied.");
    expect(screen.getByRole("button", { name: /^Continue to confirmation$/ }).hasAttribute("disabled")).toBe(true);
    unchanged = false;
    await user.click(screen.getByRole("button", { name: /^Refresh preview$/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /^Continue to confirmation$/ }).hasAttribute("disabled")).toBe(false));
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await user.click(await screen.findByRole("button", { name: /^Restore configuration$/ }));
    await screen.findByText("This route's enablement changed since the preview; review it again.");
    expect(screen.getByRole("dialog", { name: /^Restore Upstream\?$/ })).toBeTruthy();
  });
});

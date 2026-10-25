// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { DeploymentConfigurationTabs, DeploymentHistory } from "./deployment-history";
import { testClient } from "../lib/test-fixtures";
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
});

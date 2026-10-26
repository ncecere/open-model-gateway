// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { DeploymentReviewButton } from "./deployment-preview";
import { ActionProvider } from "./ui";
import { testClient } from "../lib/test-fixtures";
const record = { id: "deployment", enabled: true };
const impact = {
  target: { kind: "deployment", id: record.id }, expected_revision: "catalog:12",
  diff: [{ field: "enabled", before: true, after: false }],
  serving_routes_gained: [], serving_routes_lost: [record.id],
  models_losing_last_serving_route: [{ id: "model", public_name: "company/smart" }],
  affected_workspaces: { data: [{ id: "team", name: "Product", kind: "team" }], total: 101, limit: 100, truncated: true },
  observed_at: "2026-10-21T12:00:00Z", dependency_versions: { catalog: "12" },
  warnings: ["Configuration estimate, not a reservation.", "Personal workspace effects are omitted."],
};
function view(current = record) {
  document.cookie = "omg_csrf=test; path=/";
  const client = testClient();
  render(<QueryClientProvider client={client}><ActionProvider><DeploymentReviewButton record={current} name="Upstream" /></ActionProvider></QueryClientProvider>);
  return userEvent.setup();
}
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
describe("deployment configuration review", () => {
  it("previews without applying, shows bounded impact, then uses the existing confirmation", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json(impact)).mockResolvedValueOnce(Response.json({ ok: true }));
    vi.stubGlobal("fetch", fetch);
    const user = view();
    expect(fetch).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: /^Review changes$/ }));
    await screen.findByText("company/smart");
    expect(screen.getByText(/101 active shared workspaces/)).toBeTruthy();
    expect(screen.getByText(/Showing the first/)).toBeTruthy();
    expect(screen.getByText("Product")).toBeTruthy();
    expect(screen.getByText(/Configuration estimate, not a reservation/)).toBeTruthy();
    expect(screen.getByText(/Personal workspace effects are omitted/)).toBeTruthy();
    const details = screen.getByRole("region", { name: "Deployment change details" });
    expect(details.tabIndex).toBe(0);
    const warnings = screen.getByRole("list", { name: "Configuration estimate warnings" });
    expect(details.contains(warnings)).toBe(true);
    expect(within(warnings).getAllByRole("listitem").map(item => item.textContent)).toEqual(impact.warnings);
    const facts = screen.getByText("Serving routes gained / lost").closest("dl")!;
    expect([...facts.querySelectorAll("dt")].map(term => term.textContent)).toEqual(["Enabled", "Serving routes gained / lost", "Observed", "Expected revision (estimate only)"]);
    expect(facts.querySelector("code")?.textContent).toBe("catalog:12");
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe("/api/v1/platform/deployments/deployment/configuration/preview");
    expect(fetch.mock.calls[0][1]).toMatchObject({ method: "POST", body: JSON.stringify({ enabled: false }), headers: { "X-CSRF-Token": "test" } });
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Disable Upstream\?$/ });
    expect(fetch).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: /^Disable$/ }));
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
    expect(fetch.mock.calls[1][0]).toBe("/api/v1/platform/deployments/deployment");
    expect(fetch.mock.calls[1][1]).toMatchObject({ method: "PATCH", body: JSON.stringify({ enabled: false }) });
  });
  it("enable review sends true and a failed apply stays in the existing confirmation", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json({ ...impact, diff: [{ field: "enabled", before: false, after: true }], serving_routes_gained: [record.id], serving_routes_lost: [], models_losing_last_serving_route: [] })).mockResolvedValueOnce(Response.json({ error: { code: "403", message: "Live authority revoked" } }, { status: 403 }));
    vi.stubGlobal("fetch", fetch);
    const user = view({ ...record, enabled: false });
    await user.click(screen.getByRole("button", { name: /^Review changes$/ }));
    await screen.findByText("Disabled → Enabled");
    expect(fetch.mock.calls[0][1].body).toBe(JSON.stringify({ enabled: true }));
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Enable Upstream\?$/ });
    await user.click(screen.getByRole("button", { name: /^Enable$/ }));
    await screen.findByText("Live authority revoked");
    expect(screen.getByRole("dialog", { name: /^Enable Upstream\?$/ })).toBeTruthy();
    expect(fetch.mock.calls[1][1].body).toBe(JSON.stringify({ enabled: true }));
  });
  it("cancel never applies and reopening obtains a new observation", async () => {
    const fetch = vi.fn().mockImplementation(() => Promise.resolve(Response.json(impact)));
    vi.stubGlobal("fetch", fetch);
    const user = view();
    for (let n = 1; n <= 2; n++) {
      await user.click(screen.getByRole("button", { name: /^Review changes$/ }));
      await screen.findByText("company/smart");
      await user.click(screen.getByRole("button", { name: /^Cancel$/ }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(fetch).toHaveBeenCalledTimes(n);
    }
    expect(fetch.mock.calls.every(([, init]) => init.method === "POST")).toBe(true);
  });
  it.each([403, 404, 400])("refused preview %s cannot be confirmed", async status => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { code: String(status), message: "Preview refused" } }, { status })));
    const user = view();
    await user.click(screen.getByRole("button", { name: /^Review changes$/ }));
    await screen.findByText("Preview refused");
    expect(screen.getByRole("button", { name: /^Continue to confirmation$/ }).hasAttribute("disabled")).toBe(true);
  });
  it("does not accept a mismatched target", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ ...impact, target: { kind: "deployment", id: "other" } })));
    const user = view();
    await user.click(screen.getByRole("button", { name: /^Review changes$/ }));
    await screen.findByText(/different deployment/);
    expect(screen.getByRole("button", { name: /^Continue to confirmation$/ }).hasAttribute("disabled")).toBe(true);
  });
});

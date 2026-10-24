// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { DeploymentStatusActions } from "./deployment-preview";
import { ActionProvider, Button, useApi } from "./ui";
import { platformPath } from "../lib/api";
import { PageHeader } from "./templates/page-header";
import { HEADER_COLLAPSE_QUERY } from "./templates/header-actions";
import { RoutesTable, type RouteRow, type RouteSet } from "../pages/model-page";
import { routeTiers } from "../lib/model-setup";
import { model, testClient } from "../lib/test-fixtures";

const record = { id: "deployment", enabled: true };
const impact = {
  target: { kind: "deployment", id: record.id }, expected_revision: "catalog:12",
  diff: [{ field: "enabled", before: true, after: false }],
  serving_routes_gained: [], serving_routes_lost: [record.id],
  models_losing_last_serving_route: [],
  affected_workspaces: { data: [], total: 0, limit: 100, truncated: false },
  observed_at: "2026-10-21T12:00:00Z", dependency_versions: { catalog: "12" }, warnings: [],
};
function viewport(width: number) {
  vi.stubGlobal("matchMedia", (query: string) => ({ matches: query === HEADER_COLLAPSE_QUERY && width <= 640, media: query, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }));
}
function routeSet(enabled: boolean): RouteSet {
  const route = { ...record, enabled, model_id: model.id, provider_connection_id: "connection", provider_name: "Production", upstream_model: "Upstream" };
  const row: RouteRow = { id: record.id, route, enabled, connectionEnabled: true, priority: 0, weight: 1 };
  const rows = [row], tiers = routeTiers(rows, { strategy: "priority", max_attempts: 1 });
  return { list: { data: [route], isError: false, isPending: false, refetch() {} }, policy: { isError: false }, rows, tiers, ordered: [...tiers.primary, ...tiers.fallback, ...tiers.unknown, ...tiers.inactive] } as unknown as RouteSet;
}
beforeEach(() => {
  document.cookie = "omg_csrf=test; path=/";
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  viewport(1440);
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

describe("menu-originated deployment review focus", () => {
  for (const source of ["retire", "narrow-retire", "review-disable", "review-enable", "route-menu", "route-menu-enable"] as const) {
    it.each(["review-cancel", "cancel", "success"] as const)(`${source}: %s restores a connected trigger or fallback`, async outcome => {
      if (source === "narrow-retire") viewport(390);
      const routeMenu = source.startsWith("route-menu"), reviewButton = source.startsWith("review-"), initiallyEnabled = !source.endsWith("-enable");
      const path = `${platformPath}/deployments/${record.id}`, client = testClient();
      client.setQueryData(["api", undefined, path], { ...record, enabled: initiallyEnabled });
      let finishRefetch!: (response: Response) => void;
      const refetchResponse = new Promise<Response>(resolve => { finishRefetch = resolve; });
      function Controls() {
        const enabled = useApi<typeof record>(path).data!.enabled;
        return <><output aria-label="Deployment status">{enabled ? "Enabled" : "Disabled"}</output>{routeMenu
          ? <RoutesTable set={routeSet(enabled)} model={model} workload="generation" writable />
          : <DeploymentStatusActions record={{ ...record, enabled }} name="Upstream">{actions => <PageHeader title="Route" actions={actions} />}</DeploymentStatusActions>}</>;
      }
      const fetch = vi.fn().mockImplementation((_path: string, init: RequestInit) => {
        if (init.method === "POST") return Promise.resolve(Response.json({ ...impact, diff: [{ field: "enabled", before: initiallyEnabled, after: !initiallyEnabled }] }));
        if (init.method === "GET") return refetchResponse;
        return Promise.resolve(Response.json({ ok: true }));
      });
      vi.stubGlobal("fetch", fetch);
      render(<QueryClientProvider client={client}><ActionProvider><Controls /></ActionProvider></QueryClientProvider>);
      const user = userEvent.setup();
      const triggerName = routeMenu ? /^Actions for route Upstream$/ : reviewButton ? /^Review changes$/ : source === "narrow-retire" ? /^More actions$/ : /^Actions for Upstream$/;
      const origin = screen.getByRole("button", { name: triggerName });
      origin.focus();
      await user.keyboard("{Enter}");
      if (!reviewButton) {
        await screen.findByRole("menuitem", { name: routeMenu ? initiallyEnabled ? /^Disable route…$/ : /^Enable route…$/ : /^Retire resource…$/ });
        await user.keyboard("{End}{Enter}");
      }
      await screen.findByText(initiallyEnabled ? "Enabled → Disabled" : "Disabled → Enabled");
      if (outcome === "review-cancel") {
        screen.getByRole("button", { name: /^Cancel$/ }).focus();
        await user.keyboard("{Enter}");
        await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
        await waitFor(() => expect(document.activeElement).toBe(origin));
        expect(origin.isConnected).toBe(true);
        expect(fetch).toHaveBeenCalledTimes(1);
        return;
      }
      const proceed = screen.getByRole("button", { name: /^Continue to confirmation$/ });
      proceed.focus();
      await user.keyboard("{Enter}");
      await screen.findByRole("dialog", { name: routeMenu ? initiallyEnabled ? /^Disable route Upstream\?$/ : /^Enable route Upstream\?$/ : reviewButton ? initiallyEnabled ? /^Disable Upstream\?$/ : /^Enable Upstream\?$/ : /^Retire Upstream\?$/ });
      const finish = screen.getByRole("button", { name: outcome === "cancel" ? /^Cancel$/ : routeMenu || reviewButton ? initiallyEnabled ? /^Disable$/ : /^Enable$/ : /^Retire resource$/ });
      finish.focus();
      await user.keyboard("{Enter}");
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      if (outcome === "success") {
        // The mutation closes confirmation while the invalidated GET is still
        // pending. Only then deliver the refreshed render that can remove or
        // remount the originating trigger (retire menu / route regrouping).
        expect(screen.getByLabelText("Deployment status").textContent).toBe(initiallyEnabled ? "Enabled" : "Disabled");
        await waitFor(() => expect(document.activeElement).toBe(origin));
        expect(origin.isConnected).toBe(true);
        finishRefetch(Response.json({ ...record, enabled: !initiallyEnabled }));
        await waitFor(() => expect(screen.getByLabelText("Deployment status").textContent).toBe(initiallyEnabled ? "Disabled" : "Enabled"));
      }
      const destination = outcome === "success" ? screen.getByRole("button", { name: routeMenu ? triggerName : /^Review changes$/ }) : origin;
      await waitFor(() => expect(document.activeElement).toBe(destination));
      expect(destination.isConnected).toBe(true);
      expect(document.activeElement?.isConnected).toBe(true);
      expect(fetch).toHaveBeenCalledTimes(outcome === "cancel" ? 1 : 3);
      if (outcome === "success") expect(fetch.mock.calls[1][1].method).toBe(routeMenu || reviewButton ? "PATCH" : "DELETE");
    });
  }
  it("passes deployment actions through the header classifier at 390px, merging sibling actions into one menu", async () => {
    viewport(390);
    const other = vi.fn();
    render(<QueryClientProvider client={testClient()}><ActionProvider><DeploymentStatusActions record={record} name="Upstream">{actions => <PageHeader title="Route" actions={<>{actions}<Button variant="secondary" onClick={other}>Other action</Button></>} />}</DeploymentStatusActions></ActionProvider></QueryClientProvider>);
    expect(screen.getAllByRole("button").map(b => b.getAttribute("aria-label") ?? b.textContent)).toEqual(["Review changes", "More actions"]);
    const user = userEvent.setup();
    screen.getByRole("button", { name: /^More actions$/ }).focus();
    await user.keyboard("{Enter}");
    expect((await screen.findAllByRole("menuitem")).map(i => i.textContent)).toEqual(["Other action", "Retire resource…"]);
    await user.keyboard("{Home}{Enter}");
    expect(other).toHaveBeenCalledOnce();
  });
});

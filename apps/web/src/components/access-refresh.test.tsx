// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider, type Query } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dashboardRouteTree } from "../router";
import { abortRequests, api, type Session } from "../lib/api";
import { admin, auditor, testClient } from "../lib/test-fixtures";
import { adminTokensPath } from "../pages/admin-tokens";

const clients: ReturnType<typeof testClient>[] = [];
const unexpected: string[] = [];
const mePath = "/api/v1/me", modelsPath = "/api/v1/platform/models";
const summaryPath = "/api/v1/me/notifications/summary", jumpKeysPath = "/api/v1/me/keys?status=all&limit=200";
const privateJumpKey = "synthetic-private-jump-key", privateJumpEmail = "synthetic-private-jump-user@demo.invalid";
const denied = () => Response.json({ error: { code: "403", message: "This resource is not permitted." } }, { status: 403 });
const empty = () => Response.json({ data: [], has_more: false });

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  localStorage.clear(); sessionStorage.clear();
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => {
  cleanup(); abortRequests();
  for (const client of clients.splice(0)) client.clear();
  vi.unstubAllGlobals(); vi.restoreAllMocks();
  expect(unexpected.splice(0)).toEqual([]);
});

// Flush Query notifications and React remount tasks after a refused response.
// The transport budget below bounds the OLD loop independently of these turns.
async function flushNotifications() {
  for (let turn = 0; turn < 4; turn++) await act(async () => { await new Promise(resolve => setTimeout(resolve, 0)); });
}
function pendingResponse(options?: RequestInit) {
  let answer!: (response: Response) => void;
  const promise = new Promise<Response>((resolve, reject) => {
    const signal = options?.signal;
    const abort = () => reject(signal?.reason ?? new DOMException("Aborted", "AbortError"));
    answer = response => { signal?.removeEventListener("abort", abort); resolve(response); };
    if (signal?.aborted) abort();
    else signal?.addEventListener("abort", abort, { once: true });
  });
  return { promise, answer };
}
async function setup({ policy = "deny", refuseOnce = false, freezeRefresh = false, tokenPage = false } = {}) {
  const client = testClient();
  // Keep unobserved private rows alive, so their disappearance proves retirement,
  // not the fixture's default zero-time garbage collection.
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, gcTime: Infinity } });
  clients.push(client);
  let current: Session = admin, forbidden = true, unauthorized = false, summaryForbidden = false;
  const held: ReturnType<typeof pendingResponse>[] = [];
  const resourceCalls: string[] = [], sessionCalls: RequestInit[] = [], entityCalls: string[] = [], notificationCalls: string[] = [];
  const resourcePath = tokenPage ? adminTokensPath : modelsPath;
  let tokensCreated = 0;
  const fetch = vi.fn((input: RequestInfo | URL, options?: RequestInit): Promise<Response> => {
    const path = String(input), url = new URL(path, window.location.origin);
    if (options?.method === "GET") {
      if (path === mePath) {
        sessionCalls.push(options);
        // Initial probe succeeds. Hold revalidation when requested; also stop a
        // regressed implementation after four refreshes without returning a fake
        // 200/401 or suppressing its access-refresh events/counts.
        if ((sessionCalls.length > 1 && freezeRefresh) || sessionCalls.length > 5) {
          const response = pendingResponse(options); held.push(response); return response.promise;
        }
        return Promise.resolve(Response.json(current));
      }
      // The real JumpSearch's existing sources/row shapes (jump-search.test.tsx),
      // served by transport rather than seeding an unobserved cache entry. These
      // can succeed even when the current page has already reached its denied gate.
      if (path === jumpKeysPath) {
        entityCalls.push(path);
        return Promise.resolve(Response.json({ data: [{ id: "private-jump-key", name: privateJumpKey, status: "active", workspace: { id: "team", name: "Product", kind: "team" } }] }));
      }
      if ((url.pathname === modelsPath && url.searchParams.get("limit") === "8") || (["/api/v1/platform/users", "/api/v1/platform/workspaces"].includes(url.pathname) && url.searchParams.has("q"))) {
        entityCalls.push(path);
        return Promise.resolve(url.pathname === "/api/v1/platform/users"
          ? Response.json({ data: [{ id: "private-jump-user", email: privateJumpEmail, disabled_at: null }] })
          : empty());
      }
      if (path === "/api/v1/platform/providers?limit=200") { entityCalls.push(path); return Promise.resolve(empty()); }
      if (url.pathname === resourcePath) {
        resourceCalls.push(path);
        if (unauthorized) return Promise.resolve(Response.json({ error: { code: "401", message: "Sign in" } }, { status: 401 }));
        if (!forbidden) return Promise.resolve(empty());
        if (refuseOnce) forbidden = false;
        // Four real 403s are enough to expose the loop. Further fetches remain
        // abortable until cleanup: never weaken the expected ONE refresh count.
        if (resourceCalls.length > 4) return pendingResponse(options).promise;
        return Promise.resolve(denied());
      }
      if (url.pathname === "/api/v1/platform/providers") return Promise.resolve(empty());
      if (path === "/api/v1/platform/server-policy") return Promise.resolve(Response.json({ openrouter: { data_collection: "deny", free_models_available: false } }));
      if (url.pathname === summaryPath) {
        notificationCalls.push(path);
        return Promise.resolve(summaryForbidden ? denied() : Response.json({ unread: 0 }));
      }
      if (path === "/api/v1/auth/config") return Promise.resolve(Response.json({ enabled: true }));
    }
    if (tokenPage && path === adminTokensPath && options?.method === "POST") {
      tokensCreated++;
      return Promise.resolve(Response.json({ id: `issued-${tokensCreated}`, token: `synthetic-budget-token-${tokensCreated}`, scopes: ["platform:read"], created_at: "2026-10-23T00:00:00Z", expires_at: "2099-01-01T00:00:00Z" }));
    }
    unexpected.push(`${options?.method} ${path}`);
    throw new Error(`Unexpected synthetic request: ${options?.method} ${path}. No real network permitted.`);
  });
  vi.stubGlobal("fetch", fetch);
  const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [tokenPage ? "/admin/settings/admin-tokens" : `/admin/models?policy=${policy}`] }) });
  await router.load();
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return {
    client, router, resourceCalls, sessionCalls, entityCalls, notificationCalls,
    refuseSummary: () => { summaryForbidden = true; },
    authority: (next: Session) => { current = next; },
    expire: () => { unauthorized = true; },
    forbidAgain: () => { forbidden = true; refuseOnce = false; },
    allow: () => { forbidden = false; },
    freezeNextRefresh: () => { freezeRefresh = true; },
    resume: async () => {
      freezeRefresh = false;
      await act(async () => { for (const response of held.splice(0)) response.answer(Response.json(current)); });
    },
  };
}
type View = Awaited<ReturnType<typeof setup>>;
async function stableDenied(view: View, probes: number) {
  await waitFor(() => {
    expect(view.sessionCalls).toHaveLength(probes);
    expect(screen.queryByText("Checking current access…")).toBeNull();
    expect(screen.queryAllByText(/^(Access denied|Access not available|You don't have access)$/).length).toBeGreaterThan(0);
    expect(view.client.isFetching()).toBe(0);
  });
  const requests = view.resourceCalls.length;
  await flushNotifications();
  expect(view.sessionCalls).toHaveLength(probes);
  expect(view.resourceCalls).toHaveLength(requests);
  expect(screen.queryByText("Checking current access…")).toBeNull();
  expect(screen.queryAllByText(/^(Access denied|Access not available|You don't have access)$/).length).toBeGreaterThan(0);
}
async function recoveredOnce(view: View) {
  await waitFor(() => {
    expect(view.sessionCalls).toHaveLength(2); // initial probe + ONE automatic refresh
    expect(view.client.isFetching()).toBe(0);
    expect(screen.getByRole("link", { name: "Add model" })).toBeTruthy();
  });
  await flushNotifications();
  expect(view.sessionCalls).toHaveLength(2);
}
function seedPrivate(view: View) {
  view.client.setQueryData(["api", JSON.stringify(admin), "/api/v1/workspaces/team/keys"], { data: [{ name: "synthetic-private-key-metadata" }] });
  expect(JSON.stringify(view.client.getQueryData(["api", JSON.stringify(admin), "/api/v1/workspaces/team/keys"]))).toContain("synthetic-private-key-metadata");
}
function expectPrivateGone(view: View) {
  expect(JSON.stringify(view.client.getQueryCache().getAll().map(query => query.state.data))).not.toContain("synthetic-private-key-metadata");
}

describe("stable denied shell privacy on the actual dashboard router", () => {
  it("keeps the denied shell query-free when entity search is requested by keyboard", async () => {
    const user = userEvent.setup(), view = await setup();
    await stableDenied(view, 2);
    expect(view.entityCalls).toEqual([]);
    expect(screen.getByRole("link", { name: /^Overview$/ })).toBeTruthy();
    await user.keyboard("{Control>}k{/Control}");
    await flushNotifications();
    expect(view.entityCalls).toEqual([]);
    expect(screen.queryByRole("button", { name: "Search or jump to…" })).toBeNull();
    expect(screen.queryByRole("dialog", { name: "Search or jump to" })).toBeNull();
    expect(document.body.textContent).not.toContain(privateJumpKey);
    expect(document.body.textContent).not.toContain(privateJumpEmail);
    expect(view.client.getQueryCache().findAll({ predicate: query => query.queryKey[0] === "api" })).toEqual([]);
    expect(view.sessionCalls).toHaveLength(2);
  });

  it("retires completed private palette DOM and observers on a repeated tagged refusal", async () => {
    const user = userEvent.setup(), view = await setup();
    await stableDenied(view, 2);
    const navKey = view.router.state.location.state.__TSR_key;
    const navigationLink = screen.getByRole("link", { name: /^Overview$/ });
    // Exercise the old leak if the denied shell still exposes search. The fixed
    // shell removes it entirely, so do not require opening a removed widget.
    const trigger = screen.queryByRole("button", { name: "Search or jump to…" });
    const observedPrivate: Query[] = [];
    if (trigger) {
      await user.click(trigger);
      await user.type(screen.getByRole("combobox", { name: "Search or jump to" }), "synthetic");
      await screen.findByText(privateJumpKey);
      await screen.findByText(privateJumpEmail);
      await waitFor(() => expect(view.client.isFetching()).toBe(0));
      expect(view.entityCalls).toContain(jumpKeysPath);
      expect(view.entityCalls).toContain("/api/v1/platform/users?q=synthetic&limit=5");
      observedPrivate.push(...view.client.getQueryCache().getAll().filter(query => {
        const data = JSON.stringify(query.state.data) ?? "";
        return data.includes(privateJumpKey) || data.includes(privateJumpEmail);
      }));
      expect(observedPrivate).toHaveLength(2);
      expect(observedPrivate.every(query => query.getObserversCount() > 0)).toBe(true);
    }
    // A real api() response dispatches the tagged event. No hand-built event or
    // cache-only assertion: removeQueries alone leaves mounted observers' DOM.
    view.refuseSummary();
    await act(async () => { await expect(api(`${summaryPath}?repeated=true`)).rejects.toMatchObject({ status: 403 }); });
    await flushNotifications();
    expect(view.sessionCalls).toHaveLength(2);
    expect(view.router.state.location.state.__TSR_key).toBe(navKey);
    expect(navigationLink.isConnected).toBe(true); // do not remount the whole shell
    expect(view.client.getQueryCache().findAll({ predicate: query => query.queryKey[0] === "api" })).toEqual([]);
    expect(document.body.textContent).not.toContain(privateJumpKey);
    expect(document.body.textContent).not.toContain(privateJumpEmail);
    expect(screen.queryByRole("dialog", { name: "Search or jump to" })).toBeNull();
    expect(observedPrivate.every(query => query.getObserversCount() === 0)).toBe(true);
  });

  it("stops notification polling after stable denial across three virtual minutes", async () => {
    // Keep React/Query notification timeouts real. Fake only the interval used
    // by the real NotificationBell, installed before it mounts.
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    try {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
      const view = await setup();
      // waitFor's polling interval is faked too. Drain real Query notification
      // timeouts first: an unchanged unread count need not mutate the DOM and
      // therefore cannot wake waitFor after isFetching reaches zero.
      await flushNotifications();
      await stableDenied(view, 2);
      const calls = view.notificationCalls.length;
      for (let minute = 0; minute < 3; minute++) {
        await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
        await flushNotifications();
      }
      expect(view.notificationCalls).toHaveLength(calls);
      expect(view.sessionCalls).toHaveLength(2);
      expect(screen.queryByText("Checking current access…")).toBeNull();
      expect(screen.getByRole("link", { name: /^Overview$/ })).toBeTruthy();
    } finally {
      cleanup(); // dispose fake interval handles before restoring the real clock
      vi.useRealTimers();
    }
  });
});

describe("bounded automatic access refresh on the actual dashboard router", () => {
  it("refreshes unchanged /me exactly once, then leaves a stable resource permission notice", async () => {
    const view = await setup();
    await stableDenied(view, 2); // includes the initial /me probe, not two refreshes
    expect(view.client.getQueryData(["session"])).toEqual(admin);
    expect(view.router.state.location.pathname).toBe("/admin/models");
    expect(screen.queryByRole("link", { name: "Sign in with single sign-on" })).toBeNull();
  });

  it("singleflights concurrent resource 403s while /me is pending, then settles denied", async () => {
    const view = await setup({ policy: "deny,allow", freezeRefresh: true });
    await waitFor(() => {
      expect(view.resourceCalls.some(path => new URL(path, window.location.origin).searchParams.get("data_policy") === "deny")).toBe(true);
      expect(view.resourceCalls.some(path => new URL(path, window.location.origin).searchParams.get("data_policy") === "allow")).toBe(true);
      expect(view.sessionCalls.length).toBeGreaterThanOrEqual(2);
    });
    await flushNotifications();
    expect(view.sessionCalls).toHaveLength(2);
    expect(view.sessionCalls[1].signal?.aborted).toBe(false);
    expect(screen.getByText("Checking current access…")).toBeTruthy();
    await view.resume();
    await stableDenied(view, 2);
  });

  it("allows one fresh automatic refresh on a subsequent same-path router navigation", async () => {
    const view = await setup();
    await stableDenied(view, 2);
    // ParsedLocation.state is ParsedHistoryState: the published history 1.162.4
    // type exposes __TSR_key (not an invented location.key). Use real navigation.
    const key = view.router.state.location.state.__TSR_key;
    expect(key).toBeTypeOf("string");
    await act(async () => { await view.router.navigate({ to: "/admin/models", search: { policy: "allow" } }); });
    expect(view.router.state.location.pathname).toBe("/admin/models");
    expect(view.router.state.location.search.policy).toBe("allow");
    expect(view.router.state.location.state.__TSR_key).not.toBe(key);
    await stableDenied(view, 3); // initial probe + one refresh for EACH navigation
    expect(view.resourceCalls.some(path => new URL(path, window.location.origin).searchParams.get("data_policy") === "allow")).toBe(true);
  });

  it("fences a pending old-navigation probe before a same-path new entry revalidates", async () => {
    const view = await setup({ freezeRefresh: true });
    await waitFor(() => expect(view.sessionCalls).toHaveLength(2));
    const previousProbe = view.sessionCalls[1].signal;
    expect(previousProbe?.aborted).toBe(false);
    await act(async () => { await view.router.navigate({ to: "/admin/models", search: { policy: "allow" } }); });
    await waitFor(() => expect(view.sessionCalls).toHaveLength(3));
    expect(previousProbe?.aborted).toBe(true);
    expect(view.sessionCalls[2].signal?.aborted).toBe(false);
    await view.resume(); // also attempts to complete the abandoned old request
    await stableDenied(view, 3);
    expect(view.router.state.location.search.policy).toBe("allow");
    expect(view.client.getQueryData(["session"])).toEqual(admin);
  });

  it("keeps a later denied fence when an aborted background /me response completes", async () => {
    const view = await setup({ refuseOnce: true });
    await recoveredOnce(view);
    view.freezeNextRefresh();
    // Model a window-focus session probe, independent of automatic 403 handling.
    let probe!: Promise<void>;
    await act(async () => { probe = view.client.refetchQueries({ queryKey: ["session"], exact: true }); });
    await waitFor(() => expect(view.sessionCalls).toHaveLength(3));
    view.forbidAgain(); seedPrivate(view);
    await act(async () => { await view.client.invalidateQueries({ predicate: query => query.queryKey[0] === "api" && String(query.queryKey[2]).startsWith(modelsPath) }); });
    await stableDenied(view, 3); // no fourth /me for the repeated refusal
    expect(view.sessionCalls[2].signal?.aborted).toBe(true);
    view.authority(auditor);
    await view.resume();
    await probe;
    await stableDenied(view, 3);
    expect(view.client.getQueryData(["session"])).toEqual(admin);
    expectPrivateGone(view);
  });

  it("never revives an ended session when a superseded automatic /me completes", async () => {
    const view = await setup({ freezeRefresh: true });
    await waitFor(() => expect(view.sessionCalls).toHaveLength(2));
    seedPrivate(view); view.expire();
    await act(async () => { await expect(api(`${modelsPath}?late401=true`)).rejects.toMatchObject({ status: 401 }); });
    await screen.findByRole("link", { name: "Sign in with single sign-on" });
    expect(view.sessionCalls[1].signal?.aborted).toBe(true);
    await view.resume();
    await flushNotifications();
    expect(view.sessionCalls).toHaveLength(2);
    expect(view.client.getQueryData(["session"])).toBeUndefined();
    expectPrivateGone(view);
    expect(screen.getByRole("link", { name: "Sign in with single sign-on" })).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Add model" })).toBeNull();
  });

  it("still honors a bare manual authority event after automatic refresh, clearing private data", async () => {
    const view = await setup({ refuseOnce: true });
    await recoveredOnce(view);
    seedPrivate(view);
    view.authority(auditor);
    await act(async () => { window.dispatchEvent(new Event("omg:access-refresh")); });
    await screen.findByText("Read-only");
    await flushNotifications();
    expect(view.sessionCalls).toHaveLength(3);
    expect(view.client.getQueryData(["session"])).toEqual(auditor);
    expect(screen.queryByRole("link", { name: "Add model" })).toBeNull();
    expectPrivateGone(view);
  });

  it("retires later private data and credentials without another automatic refresh, then recovers by a bare manual event", async () => {
    const user = userEvent.setup(), view = await setup({ refuseOnce: true, tokenPage: true });
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    await waitFor(() => {
      expect(view.sessionCalls).toHaveLength(2);
      expect(view.client.isFetching()).toBe(0);
      expect(screen.getByRole("button", { name: "Create token" })).toBeTruthy();
    });
    const navKey = view.router.state.location.state.__TSR_key;
    async function createToken(name: string) {
      await user.click(screen.getByRole("button", { name: "Create token" }));
      const dialog = screen.getByRole("dialog");
      await user.type(within(dialog).getByRole("textbox", { name: "Name" }), name);
      await user.click(within(dialog).getByRole("button", { name: "Create token" }));
      const disclosure = await screen.findByRole("textbox", { name: "Admin API token" });
      if (!(disclosure instanceof HTMLTextAreaElement)) throw new Error("Expected the real one-time credential textarea.");
      await waitFor(() => expect(view.client.isFetching()).toBe(0));
      return disclosure;
    }
    const disclosure = await createToken("Before later refusal");
    expect(disclosure.value).toBe("synthetic-budget-token-1");
    // Retain the real React callback: clicking a detached DOM button would not
    // prove that the credential's lifetime guard still refuses old bindings.
    const copy = screen.getByRole("button", { name: "Copy token" });
    const binding = Object.keys(copy).find(key => key.startsWith("__reactProps$"));
    const props: unknown = binding ? Reflect.get(copy, binding) : undefined;
    const oldCopy: unknown = props && typeof props === "object" ? Reflect.get(props, "onClick") : undefined;
    if (typeof oldCopy !== "function") throw new Error("Expected the actual render-bound Copy callback.");
    seedPrivate(view);
    const lists = view.client.getQueryCache().findAll({ predicate: query => query.queryKey[0] === "api" && String(query.queryKey[2]).startsWith(adminTokensPath) });
    expect(lists).toHaveLength(1);
    const listKey = lists[0].queryKey, privateName = "synthetic-visible-private-token-metadata";
    await act(async () => {
      view.client.setQueryData(listKey, { data: [{ id: "private-token-row", name: privateName, owner_user_id: "me", owner_email: admin.user.email, owner_display_name: null, scopes: ["platform:read"], created_at: "2026-10-23T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null, last_used_at: null, last_used_address: null, ip_allowlist: null }], has_more: false });
    });
    await screen.findByText(privateName);
    expect(JSON.stringify(view.client.getQueryData(listKey))).toContain(privateName);
    expect(view.sessionCalls).toHaveLength(2); // creation did not consume a /me refresh
    const resources = view.resourceCalls.length, sameResource = view.resourceCalls.at(-1);
    view.forbidAgain();
    await act(async () => { await view.client.invalidateQueries({ queryKey: listKey, exact: true }); });
    await stableDenied(view, 2);
    expect(view.resourceCalls).toHaveLength(resources + 1);
    expect(view.resourceCalls.at(-1)).toBe(sameResource);
    expect(view.router.state.location.state.__TSR_key).toBe(navKey);
    expectPrivateGone(view);
    expect(JSON.stringify(view.client.getQueryCache().getAll().map(query => query.state.data))).not.toContain(privateName);
    expect(screen.queryByText(privateName)).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(disclosure.isConnected).toBe(false);
    expect(Array.from(document.querySelectorAll("textarea"), textarea => textarea.value)).not.toContain("synthetic-budget-token-1");
    expect(JSON.stringify(view.client.getQueryCache().getAll().map(query => query.state.data))).not.toContain("synthetic-budget-token-1");
    await act(async () => { oldCopy({}); });
    expect(write).not.toHaveBeenCalled();

    // Exhausting the automatic budget must not permanently disable refresh, or
    // turn a later bare authority-change event into a no-op. Hold /me so cache
    // retirement is required BEFORE the changed authority response can help.
    seedPrivate(view);
    const changed = { ...admin, workspaces: admin.workspaces.filter(workspace => workspace.id !== "team") };
    view.allow(); view.authority(changed); view.freezeNextRefresh();
    await act(async () => { window.dispatchEvent(new Event("omg:access-refresh")); });
    await waitFor(() => expect(view.sessionCalls).toHaveLength(3));
    expectPrivateGone(view);
    expect(screen.queryByRole("dialog")).toBeNull();
    await view.resume();
    await waitFor(() => {
      expect(view.client.getQueryData(["session"])).toEqual(changed);
      expect(view.client.isFetching()).toBe(0);
      expect(screen.queryAllByText(/^(Access denied|Access not available|You don't have access)$/)).toHaveLength(0);
      expect(screen.getByRole("button", { name: "Create token" })).toBeTruthy();
    });
    expect(view.router.state.location.state.__TSR_key).toBe(navKey);
    const fresh = await createToken("After manual recovery");
    expect(fresh.value).toBe("synthetic-budget-token-2");
    await act(async () => { oldCopy({}); });
    expect(write).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Copy token" }));
    expect(write).toHaveBeenCalledTimes(1);
    expect(write).toHaveBeenLastCalledWith("synthetic-budget-token-2");
    expect(view.sessionCalls).toHaveLength(3);
    expectPrivateGone(view);
    const persisted = JSON.stringify({ queries: view.client.getQueryCache().getAll().map(query => query.state.data), mutations: view.client.getMutationCache().getAll().map(mutation => mutation.state) });
    expect(persisted).not.toContain(privateName);
    expect(persisted).not.toContain("synthetic-budget-token-");
    expect(document.body.textContent).not.toContain("synthetic-budget-token-1");
    expect(Array.from(document.querySelectorAll("textarea"), textarea => textarea.value)).not.toContain("synthetic-budget-token-1");
  });

  it("still terminates the session on a real protected 401 after automatic refresh", async () => {
    const view = await setup({ refuseOnce: true });
    await recoveredOnce(view);
    seedPrivate(view); view.expire();
    const resources = view.resourceCalls.length;
    await act(async () => { await view.client.invalidateQueries({ predicate: query => query.queryKey[0] === "api" && String(query.queryKey[2]).startsWith(modelsPath) }); });
    await screen.findByRole("link", { name: "Sign in with single sign-on" });
    await flushNotifications();
    expect(view.sessionCalls).toHaveLength(2);
    expect(view.resourceCalls).toHaveLength(resources + 1);
    expect(view.client.getQueryData(["session"])).toBeUndefined();
    expectPrivateGone(view);
    expect(screen.queryByRole("link", { name: "Add model" })).toBeNull();
  });
});

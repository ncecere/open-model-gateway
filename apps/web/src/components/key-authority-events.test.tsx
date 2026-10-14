// @vitest-environment jsdom
import { StrictMode, type FormEvent, type MouseEvent as ReactMouseEvent } from "react";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dashboardRouteTree } from "../router";
import { grant, session, testClient } from "../lib/test-fixtures";
import { abortRequests, type Session } from "../lib/api";
import { toast } from "./ui/toast/toast";

type Flow = "creation" | "rotation";
type Boundary = "omg:unauthorized" | "omg:access-refresh";
const flows: Flow[] = ["creation", "rotation"];
const boundaries: Boundary[] = ["omg:unauthorized", "omg:access-refresh"];
const keysPath = "/api/v1/workspaces/team/keys";
const rotatePath = `${keysPath}/synthetic-key/rotate`;
const secret = "synthetic-authority-secret", freshSecret = "synthetic-authority-fresh-secret";
const key = { id: "synthetic-key", name: "CLI", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
const failure = "Synthetic admitted request failure.";
const clients: ReturnType<typeof testClient>[] = [];
const unexpected: string[] = [];

function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function reactHandler(element: HTMLElement, binding: "onSubmit" | "onClick") {
  const name = Object.keys(element).find(value => value.startsWith("__reactProps$"));
  if (!name) throw new Error("Expected React props on the real mounted native element.");
  const props: unknown = Reflect.get(element, name);
  if (!props || typeof props !== "object") throw new Error("Expected mounted React props.");
  const handler: unknown = Reflect.get(props, binding);
  if (typeof handler !== "function") throw new Error(`Expected the actual ${binding} binding.`);
  return handler;
}
function eventFields<T extends HTMLElement>(nativeEvent: Event, target: T) {
  return {
    nativeEvent, currentTarget: target, target, type: nativeEvent.type,
    bubbles: nativeEvent.bubbles, cancelable: nativeEvent.cancelable,
    eventPhase: nativeEvent.eventPhase, isTrusted: nativeEvent.isTrusted, timeStamp: nativeEvent.timeStamp,
    get defaultPrevented() { return nativeEvent.defaultPrevented; },
    preventDefault: () => nativeEvent.preventDefault(), stopPropagation: () => nativeEvent.stopPropagation(),
    isDefaultPrevented: () => nativeEvent.defaultPrevented,
    isPropagationStopped: () => nativeEvent.cancelBubble, persist() {},
  };
}
// Detached DOM events are not sufficient: retain the actual render-bound callbacks.
function retainedSubmit(form: HTMLFormElement) {
  const handler = reactHandler(form, "onSubmit");
  return () => {
    const event: FormEvent<HTMLFormElement> = eventFields(new Event("submit", { bubbles: true, cancelable: true }), form);
    handler(event);
    expect(event.isDefaultPrevented()).toBe(true);
  };
}
function retainedClick(element: HTMLElement) {
  if (!(element instanceof HTMLButtonElement)) throw new Error("Expected the real Copy button.");
  const handler = reactHandler(element, "onClick");
  return () => {
    const nativeEvent = new MouseEvent("click", { bubbles: true, cancelable: true });
    const event: ReactMouseEvent<HTMLButtonElement> = {
      ...eventFields(nativeEvent, element), nativeEvent,
      altKey: nativeEvent.altKey, button: nativeEvent.button, buttons: nativeEvent.buttons,
      clientX: nativeEvent.clientX, clientY: nativeEvent.clientY, ctrlKey: nativeEvent.ctrlKey,
      metaKey: nativeEvent.metaKey, movementX: nativeEvent.movementX, movementY: nativeEvent.movementY,
      pageX: nativeEvent.pageX, pageY: nativeEvent.pageY, relatedTarget: nativeEvent.relatedTarget,
      screenX: nativeEvent.screenX, screenY: nativeEvent.screenY, shiftKey: nativeEvent.shiftKey,
      detail: nativeEvent.detail, view: { document, styleMedia: { type: "screen", matchMedium: () => false } },
      get defaultPrevented() { return nativeEvent.defaultPrevented; },
      getModifierState: modifier => nativeEvent.getModifierState(modifier),
    };
    handler(event);
  };
}
function mountedForm() {
  const form = screen.getByRole("dialog").querySelector("form");
  if (!(form instanceof HTMLFormElement)) throw new Error("Expected the mounted native form.");
  expect(form.isConnected).toBe(true);
  return form;
}
async function mount(client: ReturnType<typeof testClient>, strict: boolean) {
  const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: ["/workspaces/team/keys"] }) });
  await router.load();
  const tree = <QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>;
  return { router, ...render(strict ? <StrictMode>{tree}</StrictMode> : tree) };
}
async function setup(strict = false) {
  const user = userEvent.setup(), client = testClient();
  clients.push(client);
  let nextMe: ReturnType<typeof deferred<Response>> | undefined;
  let postResponse = () => Promise.resolve(Response.json({ id: key.id, token: secret }));
  const lists = new Map<string, unknown[]>([
    [keysPath, [key]], ["/api/v1/workspaces/team/models", [grant]],
    ["/api/v1/workspaces/team/service-accounts", [{ id: "robot", name: "Automation", disabled_at: null }]],
    ["/api/v1/workspaces/team/members", []],
    ["/api/v1/notifications", []], ["/api/v1/workspaces/team/key-safety/findings", []],
  ]);
  const fetchSpy = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const path = String(input), url = new URL(path, window.location.origin);
    if (init?.method === "GET") {
      if (path === "/api/v1/me") return nextMe?.promise ?? Promise.resolve(Response.json(session));
      if (path === "/api/v1/auth/config") return Promise.resolve(Response.json({ enabled: true }));
      if (path === "/api/v1/me/notifications/summary") return Promise.resolve(Response.json({ unread: 0 }));
      if (path === "/api/v1/me/summary?totals_only=true") return Promise.resolve(Response.json({ totals: { current: null } }));
      if (path === "/api/v1/me/models") return Promise.resolve(Response.json({ models: [], usable_count: 0, not_serving: [], not_serving_count: 0, workspaces_without_models: [] }));
      if (url.pathname === "/api/v1/me/keys") return Promise.resolve(Response.json({ data: [], has_more: false }));
      if (path === "/api/v1/workspaces/team/policy") return Promise.resolve(Response.json({ policy: null, effective: null, provenance: null }));
      if (lists.has(url.pathname)) return Promise.resolve(Response.json({ data: lists.get(url.pathname), has_more: false }));
      if (path === "/api/v1/workspaces/team/key-safety") return Promise.resolve(Response.json({ data: [], has_more: false }));
    }
    if (init?.method === "POST" && [keysPath, rotatePath].includes(path)) return postResponse();
    if (init?.method === "PATCH" && path === `${keysPath}/${key.id}`) return Promise.resolve(Response.json({ saved: true }));
    unexpected.push(`${init?.method} ${path}`);
    throw new Error(`Unexpected synthetic fetch: ${init?.method} ${path}. No real network permitted.`);
  });
  vi.stubGlobal("fetch", fetchSpy);
  const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
  let mounted = await mount(client, strict);
  const writes = () => fetchSpy.mock.calls.filter(([, init]) => init?.method !== "GET");
  async function ready() {
    await screen.findByRole("button", { name: "Actions for CLI" });
    await waitFor(() => {
      const button = screen.getByRole("button", { name: "Create key" });
      expect(button.hasAttribute("disabled")).toBe(false);
      expect(client.isFetching({ queryKey: ["session"] })).toBe(0);
    });
  }
  await ready();
  async function open(flow: Flow, service = false) {
    if (flow === "creation") {
      await user.click(screen.getByRole("button", { name: "Create key" }));
      await user.type(await screen.findByRole("textbox", { name: /^Name/ }), "  CLI  ");
      if (service) await user.click(screen.getByRole("radio", { name: "A service account" }));
    } else {
      await user.click(screen.getByRole("button", { name: "Actions for CLI" }));
      await user.click(await screen.findByRole("menuitem", { name: "Rotate key…" }));
      const expiry = await screen.findByRole("spinbutton", { name: "Expires in (days)" });
      await user.clear(expiry); await user.type(expiry, "45");
    }
    return mountedForm();
  }
  function freezeAccess() {
    expect(client.isFetching({ queryKey: ["session"] })).toBe(0);
    nextMe = deferred<Response>();
    return nextMe;
  }
  async function transitioned(boundary: Boundary) {
    if (boundary === "omg:access-refresh") {
      await screen.findByText("Checking current access…");
      // Refetch starts only after Home awaits cancellation; do not settle it early.
      await waitFor(() => expect(client.isFetching({ queryKey: ["session"] })).toBe(1));
    } else await screen.findByRole("link", { name: "Sign in with single sign-on" });
    expect(screen.queryByRole("dialog")).toBeNull();
  }
  async function recover(boundary: Boundary, pending?: ReturnType<typeof deferred<Response>>, authority: Session = session) {
    if (boundary === "omg:access-refresh") {
      if (!pending) throw new Error("Expected deferred explicit access revalidation.");
      await act(async () => { pending.resolve(Response.json(authority)); });
      nextMe = undefined;
    } else {
      // Unauthorized ends Home permanently. Recovery is a fresh authenticated
      // Home/Router mount, not an invented sign-in button or state manipulation.
      mounted.unmount(); client.clear(); mounted = await mount(client, strict);
    }
    if (authority === session) await ready();
    else await waitFor(() => expect(mounted.router.state.location.pathname).toBe("/home"));
  }
  return { user, client, write, writes, open, ready, freezeAccess, transitioned, recover,
    location: () => mounted.router.state.location.href,
    respond: (response: typeof postResponse) => { postResponse = response; } };
}
type View = Awaited<ReturnType<typeof setup>>;
function expectRequest(view: View, flow: Flow, index = 0, service = false) {
  const [path, init] = view.writes()[index];
  expect(path).toBe(flow === "creation" ? keysPath : rotatePath);
  expect(init?.method).toBe("POST");
  expect(init?.body).toBe(JSON.stringify(flow === "creation"
    ? { name: "CLI", expires_in_days: 30, model_ids: null, ...(service ? { service_account_id: "robot" } : {}) }
    : { expires_in_days: 45 }));
  expect(init?.credentials).toBe("same-origin"); expect(init?.cache).toBe("no-store"); expect(init?.redirect).toBe("error");
  expect(init?.headers).toEqual({ Accept: "application/json", "X-CSRF-Token": "synthetic-csrf", "Content-Type": "application/json" });
}
async function disclosure(view: View, flow: Flow, value = secret) {
  const input = await screen.findByRole("textbox", { name: flow === "creation" ? "API key" : "One-time key" });
  if (!(input instanceof HTMLTextAreaElement)) throw new Error("Expected the real one-time disclosure textarea.");
  expect(input.value).toBe(value);
  // Rotation invalidates /me on completion. Let that ordinary refetch settle
  // before installing the deferred response for the explicit refresh boundary.
  // Background page controls are inert/aria-hidden while this dialog is open.
  await waitFor(() => expect(view.client.isFetching({ queryKey: ["session"] })).toBe(0));
  contained(view, false);
  return screen.getByRole("button", { name: "Copy key" });
}
function contained(view: View, removed = true) {
  const caches = JSON.stringify(view.client.getQueryCache().getAll().map(query => ({ key: query.queryKey, state: query.state })));
  const storage = JSON.stringify([localStorage, sessionStorage]);
  for (const value of [secret, freshSecret]) {
    expect(caches).not.toContain(value); expect(storage).not.toContain(value);
    expect(window.location.href).not.toContain(value); expect(view.location()).not.toContain(value);
    if (removed) expect(document.body.textContent).not.toContain(value);
  }
  expect(view.client.getMutationCache().getAll()).toEqual([]);
}
async function admissibleSubmit(view: View, flow: Flow) {
  view.respond(() => Promise.resolve(Response.json({ error: { code: "500", message: failure } }, { status: 500 })));
  const form = await view.open(flow), submit = retainedSubmit(form);
  // The very same valid binding admits a real request first. A server failure
  // releases busy/inFlight without changing its valid current draft values.
  await act(async () => { submit(); });
  await screen.findByText(failure);
  await waitFor(() => expect(form.getAttribute("aria-busy")).toBe("false"));
  expect(view.writes()).toHaveLength(1); expectRequest(view, flow);
  view.respond(() => Promise.resolve(Response.json({ id: key.id, token: freshSecret })));
  return { form, submit };
}
async function issue(view: View, flow: Flow) {
  const form = await view.open(flow), submit = retainedSubmit(form);
  await act(async () => { submit(); });
  expect(view.writes()).toHaveLength(1); expectRequest(view, flow);
  return disclosure(view, flow);
}

beforeEach(() => {
  document.cookie = "omg_csrf=synthetic-csrf; Path=/"; localStorage.clear(); sessionStorage.clear();
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
  vi.spyOn(toast, "success").mockReturnValue("synthetic-toast");
  vi.spyOn(toast, "error").mockReturnValue("synthetic-error-toast");
});
afterEach(() => {
  cleanup(); abortRequests(); for (const client of clients.splice(0)) client.clear();
  document.cookie = "omg_csrf=; Max-Age=0; Path=/";
  vi.unstubAllGlobals(); vi.restoreAllMocks();
  expect(unexpected.splice(0)).toEqual([]);
});

for (const boundary of boundaries) for (const flow of flows) {
  describe(`${flow}: ${boundary} authority retirement in actual Home`, () => {
    it("denies a previously admitted valid submit in the dispatch stack and never lends it recovered authority", async () => {
      const view = await setup(), { form, submit } = await admissibleSubmit(view, flow);
      const pending = boundary === "omg:access-refresh" ? view.freezeAccess() : undefined;
      await act(async () => {
        window.dispatchEvent(new Event(boundary));
        expect(form.isConnected).toBe(true); submit();
        expect(view.writes()).toHaveLength(1);
      });
      await view.transitioned(boundary); expect(form.isConnected).toBe(false);
      await act(async () => { submit(); }); expect(view.writes()).toHaveLength(1); contained(view);
      await view.recover(boundary, pending);
      const freshSubmit = retainedSubmit(await view.open(flow));
      await act(async () => { submit(); }); expect(view.writes()).toHaveLength(1);
      await act(async () => { freshSubmit(); });
      await disclosure(view, flow, freshSecret); expect(view.writes()).toHaveLength(2); expectRequest(view, flow, 1);
      await act(async () => { submit(); }); expect(view.writes()).toHaveLength(2);
    });

    it("retires the live disclosure Copy in the dispatch stack, clears it on commit, and keeps fresh Copy usable", async () => {
      const view = await setup(), button = await issue(view, flow), copy = retainedClick(button);
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      await screen.findByRole("button", { name: "Copied" });
      const pending = boundary === "omg:access-refresh" ? view.freezeAccess() : undefined;
      await act(async () => {
        window.dispatchEvent(new Event(boundary));
        expect(button.isConnected).toBe(true); copy();
        expect(view.write).toHaveBeenCalledTimes(1);
      });
      await view.transitioned(boundary); expect(button.isConnected).toBe(false); contained(view);
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledTimes(1);
      await view.recover(boundary, pending);
      view.respond(() => Promise.resolve(Response.json({ id: key.id, token: freshSecret })));
      const freshSubmit = retainedSubmit(await view.open(flow));
      await act(async () => { freshSubmit(); }); const freshButton = await disclosure(view, flow, freshSecret);
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledTimes(1);
      await view.user.click(freshButton);
      expect(view.write).toHaveBeenNthCalledWith(2, freshSecret);
      await screen.findByRole("button", { name: "Copied" });
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledTimes(2);
    });

    it(`contains delayed clipboard ${boundary === "omg:unauthorized" ? "success" : "failure"} feedback without replay after retirement`, async () => {
      const view = await setup(), button = await issue(view, flow), copy = retainedClick(button), clipboard = deferred<void>();
      view.write.mockReturnValueOnce(clipboard.promise);
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      const status = flow === "rotation" ? within(screen.getByRole("dialog")).getAllByRole("status").find(node => node.tagName === "P") : undefined;
      if (flow === "rotation") { expect(status).toBeDefined(); expect(status?.textContent).toBe(""); }
      const pending = boundary === "omg:access-refresh" ? view.freezeAccess() : undefined;
      await act(async () => { window.dispatchEvent(new Event(boundary)); });
      await view.transitioned(boundary);
      await act(async () => {
        if (boundary === "omg:unauthorized") clipboard.resolve();
        else clipboard.reject(new Error("Synthetic late clipboard denial."));
      });
      expect(toast.success).not.toHaveBeenCalled(); expect(toast.error).not.toHaveBeenCalled();
      if (status) { expect(status.isConnected).toBe(false); expect(status.textContent).toBe(""); }
      await view.recover(boundary, pending);
      await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledTimes(1); contained(view);
    });

    it("aborts an already-admitted POST and contains its late response without automatically replaying it", async () => {
      const view = await setup(), post = deferred<Response>(); view.respond(() => post.promise);
      const form = await view.open(flow), submit = retainedSubmit(form);
      await act(async () => { submit(); }); expect(view.writes()).toHaveLength(1); expectRequest(view, flow);
      const signal = view.writes()[0][1]?.signal;
      if (!signal) throw new Error("Expected the admitted transport signal.");
      expect(signal.aborted).toBe(false);
      const pending = boundary === "omg:access-refresh" ? view.freezeAccess() : undefined;
      await act(async () => {
        window.dispatchEvent(new Event(boundary));
        expect(form.isConnected).toBe(true); expect(signal.aborted).toBe(true);
      });
      await view.transitioned(boundary);
      await act(async () => { post.resolve(Response.json({ id: key.id, token: secret })); });
      expect(view.writes()).toHaveLength(1); contained(view);
      await view.recover(boundary, pending);
      await act(async () => { submit(); }); expect(view.writes()).toHaveLength(1); contained(view);
      view.respond(() => Promise.resolve(Response.json({ id: key.id, token: freshSecret })));
      const freshSubmit = retainedSubmit(await view.open(flow));
      await act(async () => { freshSubmit(); }); await disclosure(view, flow, freshSecret);
      expect(view.writes()).toHaveLength(2); expectRequest(view, flow, 1); expect(signal.aborted).toBe(true);
    });
  });
}

for (const flow of flows) for (const binding of ["submit", "Copy"] as const) {
  it(`keeps old ${flow} ${binding} denied when explicit access revalidation removes workspace permission`, async () => {
    const view = await setup();
    const retained = binding === "submit" ? await admissibleSubmit(view, flow) : undefined;
    const button = binding === "Copy" ? await issue(view, flow) : undefined;
    const copy = button ? retainedClick(button) : undefined;
    if (copy) { await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledExactlyOnceWith(secret); }
    const pending = view.freezeAccess();
    await act(async () => {
      window.dispatchEvent(new Event("omg:access-refresh"));
      expect((retained?.form ?? button)?.isConnected).toBe(true);
      if (retained) retained.submit(); else if (copy) copy();
    });
    expect(view.writes()).toHaveLength(1); expect(view.write).toHaveBeenCalledTimes(copy ? 1 : 0);
    await view.transitioned("omg:access-refresh"); contained(view);
    await view.recover("omg:access-refresh", pending, { ...session, workspaces: [] });
    await act(async () => { if (retained) retained.submit(); else if (copy) copy(); });
    expect(view.writes()).toHaveLength(1); expect(view.write).toHaveBeenCalledTimes(copy ? 1 : 0);
    expect(screen.queryByRole("button", { name: "Create key" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Actions for CLI" })).toBeNull(); contained(view);
  });
}

for (const flow of flows) {
  it(`admits untouched fresh ${flow} submit and Copy through actual Home under StrictMode`, async () => {
    const view = await setup(true), form = await view.open(flow, flow === "creation");
    const submit = retainedSubmit(form);
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1); expectRequest(view, flow, 0, flow === "creation");
    const copy = retainedClick(await disclosure(view, flow));
    await act(async () => { copy(); }); expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
    await screen.findByRole("button", { name: "Copied" });
  });
}

it("keeps the authorized generic Disable ActionDialog usable under StrictMode via the real key Actions menu", async () => {
  const view = await setup(true);
  await view.user.click(screen.getByRole("button", { name: "Actions for CLI" }));
  await view.user.click(await screen.findByRole("menuitem", { name: "Disable key…" }));
  await screen.findByRole("dialog", { name: "Disable CLI?" });
  const submit = retainedSubmit(mountedForm());
  await act(async () => { submit(); });
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(view.writes()).toHaveLength(1);
  const [path, init] = view.writes()[0];
  expect(path).toBe(`${keysPath}/${key.id}`); expect(init?.method).toBe("PATCH");
  expect(init?.body).toBe(JSON.stringify({ disabled: true }));
  expect(init?.headers).toEqual({ Accept: "application/json", "X-CSRF-Token": "synthetic-csrf", "Content-Type": "application/json" });
  expect(toast.success).toHaveBeenCalledWith("Key disabled."); expect(view.write).not.toHaveBeenCalled(); contained(view);
});

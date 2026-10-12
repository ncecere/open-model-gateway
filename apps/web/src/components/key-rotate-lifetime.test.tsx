// @vitest-environment jsdom
import { StrictMode, useEffect, useLayoutEffect, type FormEvent } from "react";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionProvider, Button, useAction, type Action } from "./ui";
import { keyActions } from "../pages/keys";
import { team, testClient } from "../lib/test-fixtures";
import { abortRequests, api } from "../lib/api";
import { nameField } from "../lib/forms";

const key = { id: "synthetic-key", name: "CLI", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
const rotatePath = "/api/v1/workspaces/team/keys/synthetic-key/rotate";
const resourcePath = "/api/v1/workspaces/team/synthetic-resource";
const keysQuery = ["api", undefined, "/api/v1/workspaces/team/keys", "choices"];
const secret = "synthetic-rotate-lifetime-secret", freshSecret = "synthetic-fresh-rotation-secret";
const failure = "Synthetic rotation failure.";
const clients: ReturnType<typeof testClient>[] = [];

function Open({ action }: { action: Action }) {
  const ask = useAction();
  return <Button onClick={() => ask(action)}>Open action</Button>;
}
// A removed sibling witnesses passive cleanup; the surviving observer runs in
// the layout phase after owner removal, without reaching into owner refs/state.
function PassiveWitness({ onCleanup }: { onCleanup?: () => void }) {
  useEffect(() => () => { onCleanup?.(); }, [onCleanup]);
  return null;
}
function LayoutObserver({ observe }: { observe?: () => void }) {
  useLayoutEffect(() => { observe?.(); }, [observe]);
  return null;
}
function mountedForm(nonSecret = false) {
  const input = screen.getByRole(nonSecret ? "textbox" : "spinbutton", { name: nonSecret ? "Name" : "Expires in (days)" });
  const form = input.closest("form");
  if (!(form instanceof HTMLFormElement)) throw new Error("Expected the mounted native ActionDialog form.");
  expect(form.isConnected).toBe(true);
  return form;
}
// Capture the real native binding through vendored Form, not a reconstructed
// submit or detached DOM event (which React normally refuses to dispatch).
function retainedSubmit(form: HTMLFormElement) {
  const name = Object.keys(form).find(value => value.startsWith("__reactProps$"));
  if (!name) throw new Error("Expected React props on the native form.");
  const props: unknown = Reflect.get(form, name);
  if (!props || typeof props !== "object" || !("onSubmit" in props) || typeof props.onSubmit !== "function") {
    throw new Error("Expected the actual render-bound onSubmit callback.");
  }
  const handler = props.onSubmit;
  return () => {
    const nativeEvent = new Event("submit", { bubbles: true, cancelable: true });
    const event: FormEvent<HTMLFormElement> = {
      nativeEvent, currentTarget: form, target: form, type: nativeEvent.type,
      bubbles: nativeEvent.bubbles, cancelable: nativeEvent.cancelable,
      eventPhase: nativeEvent.eventPhase, isTrusted: nativeEvent.isTrusted, timeStamp: nativeEvent.timeStamp,
      get defaultPrevented() { return nativeEvent.defaultPrevented; },
      preventDefault: () => nativeEvent.preventDefault(), stopPropagation: () => nativeEvent.stopPropagation(),
      isDefaultPrevented: () => nativeEvent.defaultPrevented, isPropagationStopped: () => nativeEvent.cancelBubble,
      persist() {},
    };
    handler(event);
    expect(event.isDefaultPrevented()).toBe(true);
  };
}
function deferred() {
  let resolve!: (value: Response) => void, reject!: (error: Error) => void;
  const promise = new Promise<Response>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function failedResponse() {
  return Response.json({ error: { code: "500", reason: "synthetic_failure", message: failure } }, { status: 500 });
}
async function setup(strict = false, action = keyActions.rotate(team.id, key), onPassive?: () => void) {
  const user = userEvent.setup(), client = testClient();
  clients.push(client);
  client.setQueryDefaults(keysQuery, { gcTime: Infinity });
  client.setQueryData(keysQuery, []);
  const fetchSpy = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    if (input === rotatePath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-replacement", token: secret }));
    if (input === resourcePath && init?.method === "PATCH") return Promise.resolve(Response.json({ saved: true }));
    throw new Error("Unexpected synthetic fetch: no real network is permitted.");
  });
  vi.stubGlobal("fetch", fetchSpy);
  function tree(owner: number | null = 0, observe?: () => void) {
    const content = <QueryClientProvider client={client}>
      {owner !== null && <ActionProvider key={owner}><Open action={action} /><PassiveWitness onCleanup={onPassive} /></ActionProvider>}
      <LayoutObserver observe={observe} />
    </QueryClientProvider>;
    return strict ? <StrictMode>{content}</StrictMode> : content;
  }
  const mounted = render(tree());
  async function open() {
    await user.click(screen.getByRole("button", { name: "Open action" }));
    await screen.findByRole("dialog", { name: action.title });
  }
  await open();
  const writes = () => fetchSpy.mock.calls.filter(([, init]) => init?.method !== "GET");
  async function expiry(value: string) {
    const input = screen.getByRole("spinbutton", { name: "Expires in (days)" });
    await user.clear(input);
    if (value) await user.type(input, value);
    return mountedForm();
  }
  return { user, client, fetchSpy, mounted, tree, open, writes, expiry };
}
type View = Awaited<ReturnType<typeof setup>>;
function expectRequest(view: View, index = 0, days = 30) {
  const write = view.writes()[index];
  expect(write).toBeDefined();
  const [input, init] = write;
  expect(input).toBe(rotatePath);
  expect(init?.method).toBe("POST");
  expect(init?.body).toBe(JSON.stringify({ expires_in_days: days }));
  expect(init?.credentials).toBe("same-origin");
  expect(init?.cache).toBe("no-store");
  expect(init?.redirect).toBe("error");
  expect(init?.headers).toEqual({ Accept: "application/json", "X-CSRF-Token": "synthetic-csrf", "Content-Type": "application/json" });
}
function requestSignal(view: View) {
  const signal = view.writes()[0]?.[1]?.signal;
  if (!signal) throw new Error("Expected the admitted request's abort signal.");
  return signal;
}
function expectExpiry(value: string) {
  const input = screen.getByRole("spinbutton", { name: "Expires in (days)" });
  if (!(input instanceof HTMLInputElement)) throw new Error("Expected the expiry input.");
  expect(input.value).toBe(value);
}
async function expectDisclosure(value = secret) {
  const input = await screen.findByRole("textbox", { name: "One-time key" });
  if (!(input instanceof HTMLTextAreaElement)) throw new Error("Expected the replacement key textarea.");
  expect(input.value).toBe(value);
  expect(screen.getByRole("dialog", { name: "Save your replacement key" }).textContent).toContain("Shown only once");
}
function expectNoSecretPersistence(view: View) {
  const cache = JSON.stringify({
    queries: view.client.getQueryCache().getAll().map(query => ({ key: query.queryKey, state: query.state })),
    mutations: view.client.getMutationCache().getAll().map(mutation => mutation.state),
  });
  for (const value of [secret, freshSecret]) {
    expect(cache).not.toContain(value);
    expect(window.location.href).not.toContain(value);
  }
  expect(view.client.getMutationCache().getAll()).toEqual([]);
  expect(localStorage.length + sessionStorage.length).toBe(0);
}
function deferPost(view: View) {
  const pending = deferred();
  view.fetchSpy.mockImplementation((input, init) => {
    if (input === rotatePath && init?.method === "POST") return pending.promise;
    throw new Error("Unexpected synthetic fetch: no real network is permitted.");
  });
  return pending;
}
function freshPosts(view: View) {
  view.fetchSpy.mockImplementation((input, init) => {
    if (input === rotatePath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-fresh-replacement", token: freshSecret }));
    throw new Error("Unexpected synthetic fetch: no real network is permitted.");
  });
}
async function confirmation(view: View) {
  await view.user.click(screen.getByRole("button", { name: "Cancel" }));
  return screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
}
async function expectRemoved(form: HTMLFormElement) {
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.queryByRole("alertdialog")).toBeNull();
  expect(form.isConnected).toBe(false);
}

beforeEach(() => {
  document.cookie = "omg_csrf=synthetic-csrf; Path=/";
  localStorage.clear(); sessionStorage.clear();
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => {
  cleanup(); abortRequests();
  for (const client of clients.splice(0)) client.clear();
  document.cookie = "omg_csrf=; Max-Age=0; Path=/";
  vi.unstubAllGlobals(); vi.restoreAllMocks();
});

describe("ActionDialog rotation submit lifetime", () => {
  for (const strict of [false, true]) {
    for (const via of ["ordinary", "retained"] as const) {
      it(`admits the valid untouched rotation default via ${via} submission${strict ? " under StrictMode" : ""}`, async () => {
        const view = await setup(strict), submit = retainedSubmit(mountedForm());
        expectExpiry("30");
        expect(view.writes()).toHaveLength(0);
        if (via === "ordinary") await view.user.click(screen.getByRole("button", { name: "Rotate key" }));
        else await act(async () => { submit(); });
        await expectDisclosure();
        expect(view.writes()).toHaveLength(1);
        expectRequest(view);
        expectNoSecretPersistence(view);
      });
    }
  }

  for (const retirement of ["cancel", "discard", "unmount"] as const) {
    it(`rejects a valid retained submit after ${retirement} and never borrows a replacement owner's authority`, async () => {
      const view = await setup();
      const form = retirement === "discard" ? await view.expiry("45") : mountedForm();
      const oldSubmit = retainedSubmit(form);
      if (retirement === "unmount") view.mounted.unmount();
      else {
        if (retirement === "discard") await view.user.click(within(await confirmation(view)).getByRole("button", { name: "Discard" }));
        else await view.user.click(screen.getByRole("button", { name: "Cancel" }));
        await expectRemoved(form);
      }
      expect(form.isConnected).toBe(false);
      await act(async () => { oldSubmit(); });
      expect(view.writes()).toHaveLength(0);
      expect(screen.queryByRole("dialog")).toBeNull();
      if (retirement === "unmount") render(view.tree(1));
      await view.open();
      const freshSubmit = retainedSubmit(mountedForm());
      await act(async () => { oldSubmit(); });
      expect(view.writes()).toHaveLength(0);
      expectExpiry("30");
      expect(screen.queryByRole("textbox", { name: "One-time key" })).toBeNull();
      await act(async () => { freshSubmit(); });
      await expectDisclosure();
      expect(view.writes()).toHaveLength(1);
      expectRequest(view);
      expectNoSecretPersistence(view);
    });
  }

  for (const dirty of [false, true]) {
    it(`revokes synchronously on ${dirty ? "confirmed Discard" : "Cancel"} before connected form removal`, async () => {
      const view = await setup(), form = dirty ? await view.expiry("45") : mountedForm();
      const submit = retainedSubmit(form);
      const button = dirty
        ? within(await confirmation(view)).getByRole("button", { name: "Discard" })
        : screen.getByRole("button", { name: "Cancel" });
      if (!(button instanceof HTMLButtonElement)) throw new Error("Expected the actual retirement button.");
      await act(async () => {
        button.click();
        expect(form.isConnected).toBe(true);
        submit();
      });
      expect(view.writes()).toHaveLength(0);
      await expectRemoved(form);
      expectNoSecretPersistence(view);
    });
  }

  it("rejects owner teardown in layout before passive cleanup, with a live layout-phase admission control", async () => {
    let passiveCleaned = false;
    const view = await setup(false, keyActions.rotate(team.id, key), () => { passiveCleaned = true; });
    const pending = deferPost(view), form = mountedForm(), submit = retainedSubmit(form);
    let liveObserved = false, retiredObserved = false;
    view.mounted.rerender(view.tree(0, () => {
      liveObserved = true;
      expect(passiveCleaned).toBe(false);
      expect(form.isConnected).toBe(true);
      submit();
    }));
    expect(liveObserved).toBe(true);
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    await act(async () => { pending.resolve(failedResponse()); });
    await screen.findByText(failure);
    // The old inFlight exclusion is now released: rejection here must be lifetime admission.
    freshPosts(view);
    view.mounted.rerender(view.tree(null, () => {
      retiredObserved = true;
      expect(form.isConnected).toBe(false);
      expect(passiveCleaned).toBe(false);
      submit();
    }));
    expect(retiredObserved).toBe(true);
    expect(passiveCleaned).toBe(true);
    expect(view.writes()).toHaveLength(1);
    expect(screen.queryByRole("dialog")).toBeNull();
    expectNoSecretPersistence(view);
  });

  it("keeps a dirty draft and its retained submit live after Cancel and Keep editing", async () => {
    const view = await setup(), form = await view.expiry("45"), submit = retainedSubmit(form);
    const confirm = await confirmation(view);
    expect(view.writes()).toHaveLength(0);
    expect(form.isConnected).toBe(true);
    await view.user.click(within(confirm).getByRole("button", { name: "Keep editing" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expectExpiry("45");
    await act(async () => { submit(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view, 0, 45);
    expectNoSecretPersistence(view);
  });

  it("validates the actual invalid expiry under StrictMode and admits explicit correction", async () => {
    const view = await setup(true);
    const invalid = retainedSubmit(await view.expiry("0"));
    await act(async () => { invalid(); });
    expect(view.writes()).toHaveLength(0);
    const input = screen.getByRole("spinbutton", { name: "Expires in (days)" });
    await waitFor(() => expect(input.getAttribute("aria-invalid")).toBe("true"));
    expect(screen.getByRole("dialog").textContent).toMatch(/whole number|greater than|range|Constraints not satisfied/i);
    const corrected = retainedSubmit(await view.expiry("365"));
    await act(async () => { corrected(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view, 0, 365);
    expectNoSecretPersistence(view);
  });

  for (const outcome of ["success", "failure"] as const) {
    it(`preserves live pending ${outcome}, inFlight exclusion and explicit retry without replay`, async () => {
      const view = await setup(), pending = deferPost(view), form = mountedForm(), submit = retainedSubmit(form);
      await act(async () => { submit(); submit(); });
      expect(view.writes()).toHaveLength(1);
      expectRequest(view);
      const signal = requestSignal(view);
      expect(signal.aborted).toBe(false);
      expect(form.getAttribute("aria-busy")).toBe("true");
      expect(screen.getByRole("button", { name: "Cancel" }).hasAttribute("disabled")).toBe(true);
      expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(false);
      await act(async () => { pending.resolve(outcome === "success" ? Response.json({ id: "synthetic-replacement", token: secret }) : failedResponse()); });
      expect(signal.aborted).toBe(false);
      expect(view.writes()).toHaveLength(1);
      if (outcome === "success") {
        await expectDisclosure();
        await waitFor(() => expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(true));
      } else {
        await screen.findByText(failure);
        expect(form.getAttribute("aria-busy")).toBe("false");
        expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(false);
        const retry = retainedSubmit(await view.expiry("45"));
        await act(async () => {});
        expect(view.writes()).toHaveLength(1);
        freshPosts(view);
        await act(async () => { retry(); });
        await expectDisclosure(freshSecret);
        expect(view.writes()).toHaveLength(2);
        expectRequest(view, 1, 45);
      }
      expectNoSecretPersistence(view);
    });
  }

  for (const outcome of ["resolve", "reject"] as const) {
    for (const lifecycle of ["unmount", "reopen", "discard"] as const) {
      it(`aborts admitted rotation on ${lifecycle}, contains late ${outcome}, and rejects old submits after settlement`, async () => {
        const view = await setup(), pending = deferPost(view);
        const form = lifecycle === "discard" ? await view.expiry("45") : mountedForm();
        const oldSubmit = retainedSubmit(form);
        // An already-open dirty confirmation remains able to discard an admitted request.
        const confirm = lifecycle === "discard" ? await confirmation(view) : undefined;
        await act(async () => { oldSubmit(); });
        expect(view.writes()).toHaveLength(1);
        expectRequest(view, 0, lifecycle === "discard" ? 45 : 30);
        const signal = requestSignal(view);
        expect(signal.aborted).toBe(false);
        if (confirm) {
          await view.user.click(within(confirm).getByRole("button", { name: "Discard" }));
          await expectRemoved(form);
          await view.open();
        } else if (lifecycle === "unmount") view.mounted.unmount();
        else { view.mounted.rerender(view.tree(1)); await view.open(); }
        expect(form.isConnected).toBe(false);
        expect(signal.aborted).toBe(true);
        await act(async () => {
          if (outcome === "resolve") pending.resolve(Response.json({ id: "synthetic-late-replacement", token: secret }));
          else pending.reject(new Error(failure));
        });
        expect(view.writes()).toHaveLength(1);
        expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(false);
        expect(screen.queryByRole("textbox", { name: "One-time key" })).toBeNull();
        expect(screen.queryByText(failure)).toBeNull();
        expect(screen.queryByRole("alert")).toBeNull();
        if (lifecycle === "unmount") {
          expect(screen.queryByRole("dialog")).toBeNull();
          render(view.tree(1));
          await view.open();
        }
        expectExpiry("30");
        const freshSubmit = retainedSubmit(mountedForm());
        freshPosts(view);
        // Settle first so inFlight cannot masquerade as a retirement guard.
        await act(async () => { oldSubmit(); });
        expect(view.writes()).toHaveLength(1);
        expectExpiry("30");
        expect(screen.queryByRole("textbox", { name: "One-time key" })).toBeNull();
        await act(async () => { freshSubmit(); });
        await expectDisclosure(freshSecret);
        expect(view.writes()).toHaveLength(2);
        expectRequest(view, 1);
        expect(signal.aborted).toBe(true);
        expectNoSecretPersistence(view);
      });
    }
  }
});

function resourceAction(after: Action["after"]): Action {
  return { title: "Rename resource", fields: [nameField], run: (values, signal) => api(resourcePath, { method: "PATCH", body: { name: values.name }, signal }), after };
}
describe("shared non-secret ActionDialog controls", () => {
  it("preserves untouched invalid validation and corrected completion/after under StrictMode", async () => {
    const after = vi.fn(), view = await setup(true, resourceAction(after));
    const untouched = retainedSubmit(mountedForm(true));
    await act(async () => { untouched(); });
    expect(view.writes()).toHaveLength(0);
    const input = screen.getByRole("textbox", { name: "Name" });
    await waitFor(() => expect(input.getAttribute("aria-invalid")).toBe("true"));
    expect(screen.getByRole("dialog").textContent).toMatch(/required|Constraints not satisfied/i);
    await view.user.type(input, "  Corrected resource  ");
    const corrected = retainedSubmit(mountedForm(true));
    await act(async () => { corrected(); });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(view.writes()).toHaveLength(1);
    expect(view.writes()[0][0]).toBe(resourcePath);
    expect(view.writes()[0][1]?.body).toBe(JSON.stringify({ name: "Corrected resource" }));
    expect(after).toHaveBeenCalledExactlyOnceWith({ saved: true });
    await act(async () => { untouched(); });
    expect(view.writes()).toHaveLength(1);
    expect(after).toHaveBeenCalledTimes(1);
    // A valid retired binding must be rejected by lifetime, not field validation.
    await act(async () => { corrected(); });
    expect(view.writes()).toHaveLength(1);
    expect(after).toHaveBeenCalledTimes(1);
    expectNoSecretPersistence(view);
  });

  it("revokes a valid non-secret draft on confirmed discard but keeps fresh ordinary completion live", async () => {
    const after = vi.fn(), view = await setup(false, resourceAction(after));
    await view.user.type(screen.getByRole("textbox", { name: "Name" }), "Retired resource");
    const form = mountedForm(true), oldSubmit = retainedSubmit(form);
    const confirm = await confirmation(view);
    await view.user.click(within(confirm).getByRole("button", { name: "Keep editing" }));
    expect(form.isConnected).toBe(true);
    await view.user.click(within(await confirmation(view)).getByRole("button", { name: "Discard" }));
    await expectRemoved(form);
    await view.open();
    await view.user.type(screen.getByRole("textbox", { name: "Name" }), "Fresh resource");
    await act(async () => { oldSubmit(); });
    expect(view.writes()).toHaveLength(0);
    expect(after).not.toHaveBeenCalled();
    await view.user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(view.writes()).toHaveLength(1);
    expect(view.writes()[0][1]?.method).toBe("PATCH");
    expect(view.writes()[0][1]?.body).toBe(JSON.stringify({ name: "Fresh resource" }));
    expect(after).toHaveBeenCalledExactlyOnceWith({ saved: true });
    expectNoSecretPersistence(view);
  });
});

// @vitest-environment jsdom
import { StrictMode, useState, type FormEvent } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionProvider } from "./ui";
import { CreateKeyDialog } from "../pages/keys";
import { session, team, testClient } from "../lib/test-fixtures";
import { abortRequests } from "../lib/api";
import * as apiModule from "../lib/api";

const keysPath = "/api/v1/workspaces/team/keys";
const policyPath = "/api/v1/workspaces/team/policy";
const keysQuery = ["api", undefined, keysPath, "choices"];
const emptyPolicy = { policy: null, effective: null, provenance: null };
const secret = "synthetic-create-lifetime-secret";
const freshSecret = "synthetic-fresh-create-secret";
const clients: ReturnType<typeof testClient>[] = [];

function CreateFlow() {
  const [open, setOpen] = useState(true);
  return open
    ? <CreateKeyDialog session={session} workspace={team} options={[{ value: "model", label: "Smart" }]} accounts={[]} onClose={() => setOpen(false)} />
    : <button type="button" onClick={() => setOpen(true)}>Create again</button>;
}

function mountedForm() {
  const form = screen.getByRole("textbox", { name: "Name" }).closest("form");
  if (!(form instanceof HTMLFormElement)) throw new Error("Expected the mounted native creation form.");
  expect(form.isConnected).toBe(true);
  return form;
}

// Capture the real render-bound React binding. A detached DOM submit would not
// challenge it: React ordinarily refuses to dispatch events on detached nodes.
// Only this transport-admission test reaches into React's DOM props; it neither
// rebuilds the handler nor inspects component state, refs or setters.
function retainedSubmit(form: HTMLFormElement) {
  const name = Object.keys(form).find(value => value.startsWith("__reactProps$"));
  if (!name) throw new Error("Expected React props on the mounted form.");
  const props: unknown = Reflect.get(form, name);
  if (!props || typeof props !== "object" || !("onSubmit" in props) || typeof props.onSubmit !== "function") {
    throw new Error("Expected the mounted form's actual onSubmit callback.");
  }
  const handler = props.onSubmit;
  return () => {
    const nativeEvent = new Event("submit", { bubbles: true, cancelable: true });
    const event: FormEvent<HTMLFormElement> = {
      nativeEvent, currentTarget: form, target: form, type: nativeEvent.type,
      bubbles: nativeEvent.bubbles, cancelable: nativeEvent.cancelable,
      eventPhase: nativeEvent.eventPhase, isTrusted: nativeEvent.isTrusted,
      timeStamp: nativeEvent.timeStamp,
      get defaultPrevented() { return nativeEvent.defaultPrevented; },
      preventDefault: () => nativeEvent.preventDefault(),
      stopPropagation: () => nativeEvent.stopPropagation(),
      isDefaultPrevented: () => nativeEvent.defaultPrevented,
      isPropagationStopped: () => nativeEvent.cancelBubble,
      persist() {},
    };
    handler(event);
    expect(event.isDefaultPrevented()).toBe(true);
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(yes => { resolve = yes; });
  return { promise, resolve };
}

async function setup(strict = false) {
  const user = userEvent.setup(), client = testClient();
  clients.push(client);
  client.setQueryData(["api", undefined, policyPath], emptyPolicy);
  // Keep this inactive, tokenless list beyond testClient's zero GC interval so
  // completion's existing invalidation remains observable after owner removal.
  client.setQueryDefaults(keysQuery, { gcTime: Infinity });
  client.setQueryData(keysQuery, []);
  const fetchSpy = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    if (input === policyPath && init?.method === "GET") return Promise.resolve(Response.json(emptyPolicy));
    if (input === keysPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-key", token: secret }));
    throw new Error("Unexpected synthetic fetch: no real network is permitted.");
  });
  vi.stubGlobal("fetch", fetchSpy);
  function tree(owner = 0) {
    const content = <QueryClientProvider client={client}><ActionProvider><CreateFlow key={owner} /></ActionProvider></QueryClientProvider>;
    return strict ? <StrictMode>{content}</StrictMode> : content;
  }
  const mounted = render(tree());
  await screen.findByRole("textbox", { name: "Name" });
  const writes = () => fetchSpy.mock.calls.filter(([, init]) => init?.method !== "GET");
  async function validDraft(value = "  CLI  ") {
    await user.type(screen.getByRole("textbox", { name: "Name" }), value);
    return mountedForm();
  }
  return { user, client, fetchSpy, mounted, tree, writes, validDraft };
}

type View = Awaited<ReturnType<typeof setup>>;

function expectRequest(view: View, index = 0, name = "CLI") {
  const write = view.writes()[index];
  expect(write).toBeDefined();
  const [input, init] = write;
  expect(input).toBe(keysPath);
  expect(init?.method).toBe("POST");
  expect(init?.body).toBe(JSON.stringify({ name, expires_in_days: 30, model_ids: null }));
  expect(init?.credentials).toBe("same-origin");
  expect(init?.cache).toBe("no-store");
  expect(init?.redirect).toBe("error");
  expect(init?.headers).toEqual({ Accept: "application/json", "X-CSRF-Token": "synthetic-csrf", "Content-Type": "application/json" });
}

function expectName(value: string) {
  const input = screen.getByRole("textbox", { name: "Name" });
  if (!(input instanceof HTMLInputElement)) throw new Error("Expected the creation name input.");
  expect(input.value).toBe(value);
}

async function expectDisclosure(value = secret) {
  const input = await screen.findByRole("textbox", { name: "API key" });
  if (!(input instanceof HTMLTextAreaElement)) throw new Error("Expected the one-time key textarea.");
  expect(input.value).toBe(value);
  expect(screen.getByRole("dialog").textContent).toContain("Shown only once");
}

function expectNoSecretPersistence(view: View) {
  const cache = JSON.stringify({
    queries: view.client.getQueryCache().getAll().map(query => ({ key: query.queryKey, state: query.state })),
    mutations: view.client.getMutationCache().getAll().map(mutation => mutation.state),
  });
  expect(cache).not.toContain(secret);
  expect(cache).not.toContain(freshSecret);
  expect(view.client.getMutationCache().getAll()).toEqual([]);
  expect(localStorage.length + sessionStorage.length).toBe(0);
  expect(window.location.href).not.toContain(secret);
  expect(window.location.href).not.toContain(freshSecret);
}

function deferPost(view: View) {
  const pending = deferred<Response>();
  view.fetchSpy.mockImplementation((input, init) => {
    if (input === policyPath && init?.method === "GET") return Promise.resolve(Response.json(emptyPolicy));
    if (input === keysPath && init?.method === "POST") return pending.promise;
    throw new Error("Unexpected synthetic fetch: no real network is permitted.");
  });
  return pending;
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

describe("CreateKeyDialog submit lifetime", () => {
  it("ignores an API completion handed off before retirement but resumed afterward", async () => {
    const view = await setup(), form = await view.validDraft(), submit = retainedSubmit(form);
    // Keep the real transport, but retire after api() has checked its signal and
    // returned the token, before the dialog's await continuation can disclose it.
    const realApi = apiModule.api;
    const apiSpy = vi.spyOn(apiModule, "api").mockImplementationOnce(async <T,>(path: string, options?: apiModule.RequestOptions) => {
      const result = await realApi<T>(path, options);
      window.dispatchEvent(new Event("omg:access-refresh"));
      expect(form.isConnected).toBe(true);
      return result;
    });
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    await waitFor(() => expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(true));
    await act(async () => { submit(); });
    expect(apiSpy).toHaveBeenCalledTimes(1);
    expect(view.writes()).toHaveLength(1);
    expectNoSecretPersistence(view);
  });

  it("shows a tokenless 2xx error without disclosure or automatic retry and allows an explicit creation retry", async () => {
    const view = await setup(), form = await view.validDraft();
    view.fetchSpy.mockResolvedValueOnce(Response.json({ id: "synthetic-tokenless-key" }));
    await act(async () => { retainedSubmit(form)(); });
    await screen.findByText("No token was returned. Check the list before trying again.");
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    expect(form.getAttribute("aria-busy")).toBe("false");
    await act(async () => {});
    expect(view.writes()).toHaveLength(1);
    await view.user.click(screen.getByRole("button", { name: "Create key" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(2);
    expectRequest(view, 1);
    expectNoSecretPersistence(view);
  });

  it("sends exactly one POST and one disclosure for two submits dispatched in the same React batch", async () => {
    const view = await setup(), pending = deferPost(view), form = await view.validDraft();
    // Both dispatches run before React re-renders, so each handler closes over busy === false:
    // only a synchronous in-flight guard can refuse the second.
    act(() => { fireEvent.submit(form); fireEvent.submit(form); });
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expect(form.getAttribute("aria-busy")).toBe("true");
    await act(async () => { pending.resolve(Response.json({ id: "synthetic-batch-key", token: secret })); });
    await expectDisclosure();
    expect(screen.getAllByRole("textbox", { name: "API key" })).toHaveLength(1);
    expect(view.writes()).toHaveLength(1);
    expectNoSecretPersistence(view);
  });

  it("blocks the live-captured submit binding retained after the first success from creating a second key", async () => {
    const view = await setup(), form = await view.validDraft(), submit = retainedSubmit(form);
    // Positive control: this very capture is a live handler that admits the first request.
    await act(async () => { submit(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expect(form.isConnected).toBe(false);
    // The owner is still mounted and not retired, so only the success seal can refuse it.
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1);
    await expectDisclosure();
    expectNoSecretPersistence(view);
  });

  it("releases the in-flight guard and busy state when authority is retired mid-flight, and creates nothing further", async () => {
    const view = await setup(), pending = deferPost(view), form = await view.validDraft(), submit = retainedSubmit(form);
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1);
    expect(form.getAttribute("aria-busy")).toBe("true");
    const signal = view.writes()[0][1]?.signal;
    if (!signal) throw new Error("Expected the admitted request's abort signal.");
    // What Home does on authority loss: retire the owner, then abort in-flight requests.
    await act(async () => {
      window.dispatchEvent(new Event("omg:access-refresh"));
      abortRequests();
      pending.resolve(Response.json({ id: "synthetic-late-key", token: secret }));
    });
    expect(signal.aborted).toBe(true);
    await waitFor(() => expect(form.getAttribute("aria-busy")).toBe("false"));
    expect(screen.getByRole("button", { name: "Cancel" }).hasAttribute("disabled")).toBe(false);
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1);
    expectNoSecretPersistence(view);
  });

  for (const submitVia of ["ordinary user", "retained mounted form"] as const) {
    it(`admits a fresh human-key draft through ${submitVia} submission with the exact current request`, async () => {
      const view = await setup(), form = await view.validDraft();
      expect(view.writes()).toHaveLength(0);
      if (submitVia === "ordinary user") await view.user.click(screen.getByRole("button", { name: "Create key" }));
      else {
        const submit = retainedSubmit(form);
        await act(async () => { submit(); });
      }
      await expectDisclosure();
      expect(view.writes()).toHaveLength(1);
      expectRequest(view);
      expectNoSecretPersistence(view);
    });
  }

  it("blocks a valid retained submit after explicit cancel", async () => {
    const view = await setup(), form = await view.validDraft(), submit = retainedSubmit(form);
    await view.user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(form.isConnected).toBe(false);
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(0);
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    expectNoSecretPersistence(view);
  });

  it("blocks a valid retained submit after its owner unmounts", async () => {
    const view = await setup(), form = await view.validDraft(), submit = retainedSubmit(form);
    view.mounted.unmount();
    expect(form.isConnected).toBe(false);
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(0);
    expect(screen.queryByRole("dialog")).toBeNull();
    expectNoSecretPersistence(view);
  });

  it("revokes synchronously on cancel while the form is still connected before commit", async () => {
    const view = await setup(), form = await view.validDraft(), submit = retainedSubmit(form);
    const cancel = screen.getByRole("button", { name: "Cancel" });
    if (!(cancel instanceof HTMLButtonElement)) throw new Error("Expected the actual Cancel button.");
    await act(async () => {
      // Native click dispatches the actual React Cancel binding. Keep both
      // operations in one batch and prove removal has not yet committed.
      cancel.click();
      expect(form.isConnected).toBe(true);
      submit();
    });
    expect(view.writes()).toHaveLength(0);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(form.isConnected).toBe(false);
    expectNoSecretPersistence(view);
  });

  for (const retirement of ["cancel", "unmount"] as const) {
    it(`never reactivates a valid old submit after ${retirement} and reopening a fresh owner`, async () => {
      const view = await setup(), form = await view.validDraft(), oldSubmit = retainedSubmit(form);
      if (retirement === "cancel") {
        await view.user.click(screen.getByRole("button", { name: "Cancel" }));
        await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
        await view.user.click(screen.getByRole("button", { name: "Create again" }));
      } else view.mounted.rerender(view.tree(1));
      expect(form.isConnected).toBe(false);
      const freshForm = await view.validDraft("Fresh CLI"), freshSubmit = retainedSubmit(freshForm);
      await act(async () => { oldSubmit(); });
      expect(view.writes()).toHaveLength(0);
      expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
      expectName("Fresh CLI");
      await act(async () => { freshSubmit(); });
      await expectDisclosure();
      expect(view.writes()).toHaveLength(1);
      expectRequest(view, 0, "Fresh CLI");
      expectNoSecretPersistence(view);
    });
  }

  it("blocks an invalid draft but admits an explicitly corrected draft in the same live owner", async () => {
    const view = await setup();
    const invalidSubmit = retainedSubmit(mountedForm());
    await act(async () => { invalidSubmit(); });
    expect(view.writes()).toHaveLength(0);
    await screen.findByText("Enter a name.");
    const correctedForm = await view.validDraft(), submit = retainedSubmit(correctedForm);
    await act(async () => { submit(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expectNoSecretPersistence(view);
  });

  it("does not replay a failed POST and permits an explicit corrected retry while the owner stays live", async () => {
    const view = await setup();
    view.fetchSpy.mockImplementation((input, init) => {
      if (input === policyPath && init?.method === "GET") return Promise.resolve(Response.json(emptyPolicy));
      if (input === keysPath && init?.method === "POST") return Promise.resolve(Response.json({ error: { code: "500", reason: "synthetic_failure", message: "Synthetic creation failure." } }, { status: 500 }));
      throw new Error("Unexpected synthetic fetch: no real network is permitted.");
    });
    const form = await view.validDraft(), submit = retainedSubmit(form);
    await act(async () => { submit(); });
    await screen.findByText("Synthetic creation failure.");
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(false);
    await view.user.clear(screen.getByRole("textbox", { name: "Name" }));
    await view.validDraft("Corrected CLI");
    // Editing and another settled act turn must not automatically replay it.
    await act(async () => {});
    expect(view.writes()).toHaveLength(1);
    view.fetchSpy.mockImplementation((input, init) => {
      if (input === policyPath && init?.method === "GET") return Promise.resolve(Response.json(emptyPolicy));
      if (input === keysPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-retry-key", token: secret }));
      throw new Error("Unexpected synthetic fetch: no real network is permitted.");
    });
    const retry = retainedSubmit(mountedForm());
    await act(async () => { retry(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(2);
    expectRequest(view, 1, "Corrected CLI");
    expectNoSecretPersistence(view);
  });

  it("preserves untouched invalid-form feedback and correction under StrictMode", async () => {
    const view = await setup(true);
    await view.user.click(screen.getByRole("button", { name: "Create key" }));
    expect(view.writes()).toHaveLength(0);
    await screen.findByText("Enter a name.");
    await view.validDraft();
    await view.user.click(screen.getByRole("button", { name: "Create key" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expectNoSecretPersistence(view);
  });

  it("keeps a fresh retained form submit live under StrictMode", async () => {
    const view = await setup(true), form = await view.validDraft(), submit = retainedSubmit(form);
    await act(async () => { submit(); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expectNoSecretPersistence(view);
  });

  it("allows an already-admitted deferred POST to complete and disclose while its owner remains live", async () => {
    const view = await setup(), pending = deferPost(view);
    const form = await view.validDraft(), submit = retainedSubmit(form);
    await act(async () => { submit(); });
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
    expect(form.getAttribute("aria-busy")).toBe("true");
    expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(false);
    const signal = view.writes()[0][1]?.signal;
    if (!signal) throw new Error("Expected the admitted request's abort signal.");
    expect(signal.aborted).toBe(false);
    await act(async () => { pending.resolve(Response.json({ id: "synthetic-deferred-key", token: secret })); });
    await expectDisclosure();
    await waitFor(() => expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(true));
    expect(view.writes()).toHaveLength(1);
    expect(signal.aborted).toBe(false);
    expectNoSecretPersistence(view);
  });

  for (const lifecycle of ["unmount", "reopen"] as const) {
    it(`completes an admitted deferred POST after ${lifecycle} without cancellation, replay or replacement disclosure`, async () => {
      const view = await setup(), pending = deferPost(view);
      const form = await view.validDraft(), submit = retainedSubmit(form);
      await act(async () => { submit(); });
      expect(view.writes()).toHaveLength(1);
      expectRequest(view);
      const signal = view.writes()[0][1]?.signal;
      if (!signal) throw new Error("Expected the admitted request's abort signal.");
      const aborted = vi.fn();
      signal.addEventListener("abort", aborted);
      if (lifecycle === "unmount") view.mounted.unmount();
      else view.mounted.rerender(view.tree(1));
      expect(form.isConnected).toBe(false);
      expect(signal.aborted).toBe(false);
      expect(view.writes()).toHaveLength(1);
      if (lifecycle === "reopen") await view.validDraft("Replacement CLI");
      await act(async () => { pending.resolve(Response.json({ id: "synthetic-retired-key", token: secret })); });
      // Completion still invalidates tokenless server-state queries; retirement
      // is not retroactive transport cancellation or a denial of that result.
      await waitFor(() => expect(view.client.getQueryState(keysQuery)?.isInvalidated).toBe(true));
      expect(signal.aborted).toBe(false);
      expect(aborted).not.toHaveBeenCalled();
      expect(view.writes()).toHaveLength(1);
      expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
      expectNoSecretPersistence(view);
      if (lifecycle === "unmount") expect(screen.queryByRole("dialog")).toBeNull();
      else {
        expectName("Replacement CLI");
        view.fetchSpy.mockImplementation((input, init) => {
          if (input === policyPath && init?.method === "GET") return Promise.resolve(Response.json(emptyPolicy));
          if (input === keysPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-fresh-key", token: freshSecret }));
          throw new Error("Unexpected synthetic fetch: no real network is permitted.");
        });
        const freshSubmit = retainedSubmit(mountedForm());
        await act(async () => { freshSubmit(); });
        await expectDisclosure(freshSecret);
        expect(view.writes()).toHaveLength(2);
        expectRequest(view, 1, "Replacement CLI");
        expectNoSecretPersistence(view);
      }
    });
  }
});

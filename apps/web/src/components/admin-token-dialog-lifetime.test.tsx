// @vitest-environment jsdom
import { StrictMode, useState } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionProvider } from "./ui";
import { CreateAdminTokenDialog, adminTokensPath } from "../pages/admin-tokens";
import { session, testClient } from "../lib/test-fixtures";
import { abortRequests } from "../lib/api";

const listQuery = ["api", undefined, `${adminTokensPath}?limit=100&offset=0`];
const secret = "synthetic-admin-token-secret";
const freshSecret = "synthetic-fresh-admin-token-secret";
const clients: ReturnType<typeof testClient>[] = [];

function CreateFlow() {
  const [open, setOpen] = useState(true);
  return open ? <CreateAdminTokenDialog session={session} onClose={() => setOpen(false)} /> : <button type="button" onClick={() => setOpen(true)}>Create again</button>;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(yes => { resolve = yes; });
  return { promise, resolve };
}

async function setup(strict = false) {
  const user = userEvent.setup(), client = testClient();
  clients.push(client);
  client.setQueryDefaults(listQuery, { gcTime: Infinity });
  client.setQueryData(listQuery, { data: [] });
  const fetchSpy = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    if (input === adminTokensPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-token", token: secret, scopes: ["platform:read"], created_at: "2026-10-23T00:00:00Z", expires_at: "2026-11-22T00:00:00Z" }));
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
  async function validDraft(name = "CI publisher") {
    await user.type(screen.getByRole("textbox", { name: "Name" }), name);
    // One scope is preselected (platform:read): a valid draft needs no further input.
  }
  return { user, client, fetchSpy, mounted, tree, writes, validDraft };
}

type View = Awaited<ReturnType<typeof setup>>;

function expectRequest(view: View, index = 0, name = "CI publisher") {
  const write = view.writes()[index];
  expect(write).toBeDefined();
  const [input, init] = write;
  expect(input).toBe(adminTokensPath);
  expect(init?.method).toBe("POST");
  expect(init?.body).toBe(JSON.stringify({ name, scopes: ["platform:read"], expires_in_days: 30 }));
  expect(init?.credentials).toBe("same-origin");
  expect(init?.cache).toBe("no-store");
  expect(init?.redirect).toBe("error");
}

async function expectDisclosure(value = secret) {
  const input = await screen.findByRole("textbox", { name: "Admin API token" });
  if (!(input instanceof HTMLTextAreaElement)) throw new Error("Expected the one-time token textarea.");
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
    if (input === adminTokensPath && init?.method === "POST") return pending.promise;
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

describe("CreateAdminTokenDialog submit lifetime", () => {
  it("admits a fresh draft with the default scope, the 30-day default expiry and no allowlist", async () => {
    const view = await setup();
    await view.validDraft();
    expect(view.writes()).toHaveLength(0);
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectRequest(view);
    expectNoSecretPersistence(view);
  });

  it("requires a name and at least one scope, and rejects an out-of-range expiry", async () => {
    const view = await setup();
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    expect(view.writes()).toHaveLength(0);
    await screen.findByText("Enter a name.");
    await view.user.type(screen.getByRole("textbox", { name: "Name" }), "X");
    await view.user.click(screen.getByRole("checkbox", { name: "Read" }));
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await screen.findByText("Select at least one scope.");
    expect(view.writes()).toHaveLength(0);
    await view.user.click(screen.getByRole("checkbox", { name: "Catalog" }));
    const expiry = screen.getByRole("textbox", { name: "Expires in (days)" });
    await view.user.clear(expiry); await view.user.type(expiry, "91");
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await screen.findByText("Tokens must expire within 1 to 90 days.");
    expect(view.writes()).toHaveLength(0);
    await view.user.clear(expiry); await view.user.type(expiry, "60");
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expect(JSON.parse(view.writes()[0]![1]!.body as string)).toEqual({ name: "X", scopes: ["catalog:write"], expires_in_days: 60 });
    expectNoSecretPersistence(view);
  });

  it("parses the optional IP allowlist textarea and omits it entirely when left blank", async () => {
    const view = await setup();
    await view.validDraft();
    await view.user.type(screen.getByRole("textbox", { name: "IP allowlist" }), "10.0.0.0/8\n192.168.1.0/24, 2001:db8::/32");
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    expect(JSON.parse(view.writes()[0]![1]!.body as string)).toEqual({ name: "CI publisher", scopes: ["platform:read"], expires_in_days: 30, ip_allowlist: ["10.0.0.0/8", "192.168.1.0/24", "2001:db8::/32"] });
    expectNoSecretPersistence(view);
  });

  it("blocks a valid retained submit after explicit cancel", async () => {
    const view = await setup();
    await view.validDraft();
    const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
    await view.user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(form.isConnected).toBe(false);
    await act(async () => { fireEvent.submit(form); });
    expect(view.writes()).toHaveLength(0);
    expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
    expectNoSecretPersistence(view);
  });

  it("blocks a valid retained submit after its owner unmounts", async () => {
    const view = await setup();
    await view.validDraft();
    const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
    view.mounted.unmount();
    expect(form.isConnected).toBe(false);
    await act(async () => { fireEvent.submit(form); });
    expect(view.writes()).toHaveLength(0);
    expectNoSecretPersistence(view);
  });

  for (const retirement of ["cancel", "unmount"] as const) {
    it(`never reactivates a valid old submit after ${retirement} and reopening a fresh owner`, async () => {
      const view = await setup();
      await view.validDraft();
      const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
      if (retirement === "cancel") { await view.user.click(screen.getByRole("button", { name: "Cancel" })); await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull()); await view.user.click(screen.getByRole("button", { name: "Create again" })); }
      else view.mounted.rerender(view.tree(1));
      expect(form.isConnected).toBe(false);
      await view.validDraft("Fresh CLI");
      await act(async () => { fireEvent.submit(form); });
      expect(view.writes()).toHaveLength(0);
      expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
      await view.user.click(screen.getByRole("button", { name: "Create token" }));
      await expectDisclosure();
      expect(view.writes()).toHaveLength(1);
      expectRequest(view, 0, "Fresh CLI");
      expectNoSecretPersistence(view);
    });
  }

  it("does not replay a failed POST and permits an explicit corrected retry while the owner stays live", async () => {
    const view = await setup();
    view.fetchSpy.mockImplementation((input, init) => {
      if (input === adminTokensPath && init?.method === "POST") return Promise.resolve(Response.json({ error: { code: "500", reason: "synthetic_failure", message: "Synthetic creation failure." } }, { status: 500 }));
      throw new Error("Unexpected synthetic fetch: no real network is permitted.");
    });
    await view.validDraft();
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await screen.findByText("Synthetic creation failure.");
    expect(view.writes()).toHaveLength(1);
    expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
    await act(async () => {});
    expect(view.writes()).toHaveLength(1);
    view.fetchSpy.mockImplementation((input, init) => {
      if (input === adminTokensPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-retry-token", token: secret, scopes: ["platform:read"] }));
      throw new Error("Unexpected synthetic fetch: no real network is permitted.");
    });
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(2);
    expectNoSecretPersistence(view);
  });

  it("keeps a fresh retained form submit live under StrictMode", async () => {
    const view = await setup(true);
    await view.validDraft();
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expectNoSecretPersistence(view);
  });

  it("allows an already-admitted deferred POST to complete and disclose while its owner remains live", async () => {
    const view = await setup(), pending = deferPost(view);
    await view.validDraft();
    const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
    await act(async () => { fireEvent.submit(form); });
    expect(view.writes()).toHaveLength(1);
    expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
    expect(form.getAttribute("aria-busy")).toBe("true");
    const signal = view.writes()[0]![1]?.signal;
    if (!signal) throw new Error("Expected the admitted request's abort signal.");
    expect(signal.aborted).toBe(false);
    await act(async () => { pending.resolve(Response.json({ id: "synthetic-deferred-token", token: secret, scopes: ["platform:read"] })); });
    await expectDisclosure();
    expect(view.writes()).toHaveLength(1);
    expect(signal.aborted).toBe(false);
    expectNoSecretPersistence(view);
  });

  for (const lifecycle of ["unmount", "reopen"] as const) {
    it(`aborts an admitted deferred POST on ${lifecycle} and never discloses, copies or auto-retries a late completion`, async () => {
      const view = await setup(), pending = deferPost(view);
      await view.validDraft();
      const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
      await act(async () => { fireEvent.submit(form); });
      expect(view.writes()).toHaveLength(1);
      const signal = view.writes()[0]![1]?.signal;
      if (!signal) throw new Error("Expected the admitted request's abort signal.");
      expect(signal.aborted).toBe(false);
      if (lifecycle === "unmount") view.mounted.unmount();
      else view.mounted.rerender(view.tree(1));
      expect(form.isConnected).toBe(false);
      // Retiring the lifetime aborts the in-flight request outright (stricter than key creation).
      expect(signal.aborted).toBe(true);
      if (lifecycle === "reopen") await view.validDraft("Replacement CLI");
      await act(async () => { pending.resolve(Response.json({ id: "synthetic-retired-token", token: secret, scopes: ["platform:read"] })); });
      await act(async () => {});
      expect(view.writes()).toHaveLength(1);
      expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
      expectNoSecretPersistence(view);
      if (lifecycle === "unmount") expect(screen.queryByRole("dialog")).toBeNull();
      else {
        view.fetchSpy.mockImplementation((input, init) => {
          if (input === adminTokensPath && init?.method === "POST") return Promise.resolve(Response.json({ id: "synthetic-fresh-token", token: freshSecret, scopes: ["platform:read"] }));
          throw new Error("Unexpected synthetic fetch: no real network is permitted.");
        });
        await view.user.click(screen.getByRole("button", { name: "Create token" }));
        await expectDisclosure(freshSecret);
        expect(view.writes()).toHaveLength(2);
        expectNoSecretPersistence(view);
      }
    });
  }

  for (const event of ["omg:unauthorized", "omg:access-refresh"] as const) {
    it(`retires the lifetime on a live ${event} event: a retained submit after it never posts`, async () => {
      const view = await setup();
      await view.validDraft();
      const form = screen.getByRole("textbox", { name: "Name" }).closest("form")!;
      await act(async () => { window.dispatchEvent(new Event(event)); });
      expect(form.isConnected).toBe(true);
      await act(async () => { fireEvent.submit(form); });
      expect(view.writes()).toHaveLength(0);
      expect(screen.queryByRole("textbox", { name: "Admin API token" })).toBeNull();
      expectNoSecretPersistence(view);
    });
  }
});

describe("CreateAdminTokenDialog Copy lifetime", () => {
  async function issue(view: View) {
    await view.validDraft();
    await view.user.click(screen.getByRole("button", { name: "Create token" }));
    await expectDisclosure();
    return screen.getByRole("button", { name: "Copy token" });
  }

  it("copies the currently displayed secret without putting it in caches or storage", async () => {
    const view = await setup();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    const button = await issue(view);
    await view.user.click(button);
    expect(write).toHaveBeenCalledExactlyOnceWith(secret);
    await screen.findByRole("button", { name: "Copied" });
    expectNoSecretPersistence(view);
  });

  it("keeps the manual-copy fallback when a live clipboard write fails", async () => {
    const view = await setup();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockRejectedValueOnce(new Error("synthetic clipboard denial"));
    const button = await issue(view);
    await view.user.click(button);
    expect(write).toHaveBeenCalledExactlyOnceWith(secret);
    expect(screen.getByRole("button", { name: "Copy token" })).toBeDefined();
    expect(screen.getByRole("textbox", { name: "Admin API token" })).toBeDefined();
  });

  it("rejects a retained Copy binding after explicit disclosure close, and never reactivates it after reopening", async () => {
    const view = await setup();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    const button = await issue(view);
    await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(button.isConnected).toBe(false);
    await act(async () => { button.click(); });
    expect(write).not.toHaveBeenCalled();
    await view.user.click(screen.getByRole("button", { name: "Create again" }));
    const freshButton = await issue(view);
    await act(async () => { button.click(); });
    expect(write).not.toHaveBeenCalled();
    await view.user.click(freshButton);
    expect(write).toHaveBeenCalledExactlyOnceWith(secret);
  });

  it("rejects a retained Copy binding after owner unmount", async () => {
    const view = await setup();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    const button = await issue(view);
    view.mounted.unmount();
    await act(async () => { button.click(); });
    expect(write).not.toHaveBeenCalled();
  });

  it("does not publish stale Copy feedback when an admitted write later resolves after close", async () => {
    const view = await setup();
    const pending = deferred<void>();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockReturnValueOnce(pending.promise);
    const button = await issue(view);
    await view.user.click(button);
    expect(write).toHaveBeenCalledExactlyOnceWith(secret);
    await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
    await act(async () => { pending.resolve(); });
    // An already-issued clipboard operation is not retroactively canceled or erased; it is
    // simply never surfaced again once retired.
    expect(write).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

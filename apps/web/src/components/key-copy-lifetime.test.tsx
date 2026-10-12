// @vitest-environment jsdom
import { StrictMode, useState, type MouseEvent as ReactMouseEvent } from "react";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionProvider, Button, useAction } from "./ui";
import { CreateKeyDialog, keyActions } from "../pages/keys";
import { session, team, testClient } from "../lib/test-fixtures";
import { abortRequests } from "../lib/api";
import { toast } from "./ui/toast/toast";

const clients: ReturnType<typeof testClient>[] = [];
const secret = "synthetic-copy-lifetime-secret";
const key = { id: "synthetic-key", name: "CLI", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
function CreateFlow() {
  const [open, setOpen] = useState(true);
  return open ? <CreateKeyDialog session={session} workspace={team} options={[{ value: "model", label: "Smart" }]} accounts={[]} onClose={() => setOpen(false)} /> : <Button onClick={() => setOpen(true)}>Create again</Button>;
}
function RotateFlow() {
  const ask = useAction();
  return <Button onClick={() => ask(keyActions.rotate(team.id, key))}>Open rotation</Button>;
}

// Capture the actual mounted React click binding, not a reconstructed copy function.
// This challenges retained callbacks; React does not normally dispatch detached-DOM clicks.
function retainedClick(button: HTMLElement) {
  const name = Object.keys(button).find(value => value.startsWith("__reactProps$"));
  expect(name).toBeDefined();
  const props = (button as unknown as Record<string, unknown>)[name!] as { onClick: (event: ReactMouseEvent<HTMLButtonElement>) => unknown };
  const handler = props.onClick;
  expect(typeof handler).toBe("function");
  if (!(button instanceof HTMLButtonElement)) throw new Error("Retained click requires a real button.");
  return () => {
    const nativeEvent = new MouseEvent("click", { bubbles: true, cancelable: true });
    const event: ReactMouseEvent<HTMLButtonElement> = {
      nativeEvent, currentTarget: button, target: button, type: "click",
      altKey: nativeEvent.altKey, button: nativeEvent.button, buttons: nativeEvent.buttons,
      clientX: nativeEvent.clientX, clientY: nativeEvent.clientY, ctrlKey: nativeEvent.ctrlKey,
      metaKey: nativeEvent.metaKey, movementX: nativeEvent.movementX, movementY: nativeEvent.movementY,
      pageX: nativeEvent.pageX, pageY: nativeEvent.pageY, relatedTarget: nativeEvent.relatedTarget,
      screenX: nativeEvent.screenX, screenY: nativeEvent.screenY, shiftKey: nativeEvent.shiftKey,
      bubbles: nativeEvent.bubbles, cancelable: nativeEvent.cancelable, detail: nativeEvent.detail,
      eventPhase: nativeEvent.eventPhase, isTrusted: nativeEvent.isTrusted, timeStamp: nativeEvent.timeStamp,
      view: { document, styleMedia: { type: "screen", matchMedium: () => false } },
      get defaultPrevented() { return nativeEvent.defaultPrevented; },
      getModifierState: modifier => nativeEvent.getModifierState(modifier),
      preventDefault: () => nativeEvent.preventDefault(), stopPropagation: () => nativeEvent.stopPropagation(),
      isDefaultPrevented: () => nativeEvent.defaultPrevented, isPropagationStopped: () => nativeEvent.cancelBubble,
      persist() {},
    };
    return handler(event);
  };
}
function rotationFeedback() {
  const feedback = within(screen.getByRole("dialog")).getAllByRole("status").filter(element => element.tagName === "P");
  expect(feedback).toHaveLength(1);
  return feedback[0];
}
function deferred() {
  let resolve!: () => void, reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function showSecret(flow: "creation" | "rotation", strict = false) {
  const user = userEvent.setup(), client = testClient();
  clients.push(client);
  client.setQueryData(["api", undefined, "/api/v1/workspaces/team/policy"], { policy: null, effective: null, provenance: null });
  vi.stubGlobal("fetch", vi.fn().mockImplementation((_input: unknown, init?: RequestInit) => Promise.resolve(Response.json(init?.method === "POST" ? { id: key.id, token: secret } : { policy: null, effective: null, provenance: null }))));
  const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
  const content = <QueryClientProvider client={client}><ActionProvider>{flow === "creation" ? <CreateFlow /> : <RotateFlow />}</ActionProvider></QueryClientProvider>;
  const mounted = render(strict ? <StrictMode>{content}</StrictMode> : content);
  async function issue() {
    if (flow === "creation") {
      const name = await screen.findByRole("textbox", { name: "Name" });
      await user.type(name, "CLI");
      await user.click(screen.getByRole("button", { name: "Create key" }));
    } else {
      await user.click(screen.getByRole("button", { name: "Open rotation" }));
      await user.click(await screen.findByRole("button", { name: "Rotate key" }));
    }
    const input = await screen.findByRole("textbox", { name: flow === "creation" ? "API key" : "One-time key" });
    expect((input as HTMLTextAreaElement).value).toBe(secret);
    return screen.getByRole("button", { name: "Copy key" });
  }
  const button = await issue();
  return { user, mounted, write, button, issue, client };
}

beforeEach(() => {
  document.cookie = "omg_csrf=synthetic-csrf; Path=/";
  localStorage.clear(); sessionStorage.clear();
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
  vi.spyOn(toast, "success").mockReturnValue("synthetic-toast");
  vi.spyOn(toast, "error").mockReturnValue("synthetic-error-toast");
});
afterEach(() => {
  cleanup(); abortRequests();
  for (const client of clients.splice(0)) client.clear();
  vi.unstubAllGlobals(); vi.restoreAllMocks();
});

for (const flow of ["creation", "rotation"] as const) {
  describe(`${flow} one-time Copy lifetime`, () => {
    it("copies the currently displayed secret without putting it in caches or storage", async () => {
      const view = await showSecret(flow);
      await view.user.click(view.button);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      await screen.findByRole("button", { name: "Copied" });
      expect(JSON.stringify(view.client.getQueryCache().getAll().map(query => query.state.data))).not.toContain(secret);
      expect(view.client.getMutationCache().getAll()).toEqual([]);
      expect(localStorage.length + sessionStorage.length).toBe(0);
    });
    it("reaches the live production Copy handler through the retained binding", async () => {
      const view = await showSecret(flow), copy = retainedClick(view.button);
      await act(async () => { copy(); });
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      await screen.findByRole("button", { name: "Copied" });
      if (flow === "creation") expect(toast.success).toHaveBeenCalledExactlyOnceWith("Copied", "Clear your clipboard after storing the key.");
      else expect(rotationFeedback().textContent).toBe("Copied. Clear your clipboard after storing the key.");
      await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
      await act(async () => { copy(); });
      expect(view.write).toHaveBeenCalledTimes(1);
    });
    it("keeps the existing feedback when a live clipboard write fails", async () => {
      const view = await showSecret(flow);
      view.write.mockRejectedValueOnce(new Error("synthetic clipboard denial"));
      await view.user.click(view.button);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      if (flow === "creation") expect(toast.error).toHaveBeenCalledWith("Clipboard unavailable", "Select and copy the key manually.");
      else await screen.findByText("Clipboard unavailable. Select and copy the key manually.");
    });
    it("rejects a retained Copy binding after explicit disclosure close", async () => {
      const view = await showSecret(flow), oldCopy = retainedClick(view.button);
      await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(view.button.isConnected).toBe(false);
      await act(async () => { oldCopy(); });
      expect(view.write).not.toHaveBeenCalled();
    });
    it("rejects a retained Copy binding after owner unmount", async () => {
      const view = await showSecret(flow), oldCopy = retainedClick(view.button);
      view.mounted.unmount();
      await act(async () => { oldCopy(); });
      expect(view.write).not.toHaveBeenCalled();
    });
    it("revokes synchronously on close before React removes the Copy button", async () => {
      const view = await showSecret(flow), oldCopy = retainedClick(view.button), close = retainedClick(screen.getByRole("button", { name: "I have saved it" }));
      await act(async () => {
        close();
        expect(view.button.isConnected).toBe(true);
        oldCopy();
      });
      expect(view.write).not.toHaveBeenCalled();
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(view.button.isConnected).toBe(false);
    });
    it("never reactivates an old binding when a fresh disclosure opens", async () => {
      const view = await showSecret(flow), oldCopy = retainedClick(view.button);
      await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      if (flow === "creation") await view.user.click(screen.getByRole("button", { name: "Create again" }));
      const freshButton = await view.issue();
      await act(async () => { oldCopy(); });
      expect(view.write).not.toHaveBeenCalled();
      await view.user.click(freshButton);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
    });
    it("keeps fresh Copy live under StrictMode while revoking the closed binding", async () => {
      const view = await showSecret(flow, true), oldCopy = retainedClick(view.button);
      await view.user.click(view.button);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
      await act(async () => { oldCopy(); });
      expect(view.write).toHaveBeenCalledTimes(1);
    });
  });
}

for (const outcome of ["resolve", "reject"] as const) {
  it(`does not publish stale creation Copy feedback when an admitted write later ${outcome}s`, async () => {
    const view = await showSecret("creation"), pending = deferred();
    view.write.mockReturnValueOnce(pending.promise);
    await view.user.click(view.button);
    expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
    await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
    await act(async () => { if (outcome === "resolve") pending.resolve(); else pending.reject(new Error("synthetic late clipboard denial")); });
    expect(toast.success).not.toHaveBeenCalled();
    expect(toast.error).not.toHaveBeenCalled();
    // An already-issued clipboard operation is not retroactively canceled or erased.
    expect(view.write).toHaveBeenCalledTimes(1);
  });
  for (const lifecycle of ["unmount", "reopen"] as const) {
    it(`keeps admitted creation Copy ${outcome} feedback revoked after ${lifecycle}`, async () => {
      const view = await showSecret("creation"), pending = deferred();
      view.write.mockReturnValueOnce(pending.promise);
      await view.user.click(view.button);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      let freshButton: HTMLElement | undefined;
      if (lifecycle === "unmount") view.mounted.unmount();
      else {
        await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
        await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
        await view.user.click(screen.getByRole("button", { name: "Create again" }));
        freshButton = await view.issue();
      }
      await act(async () => { if (outcome === "resolve") pending.resolve(); else pending.reject(new Error("synthetic retired clipboard denial")); });
      expect(toast.success).not.toHaveBeenCalled();
      expect(toast.error).not.toHaveBeenCalled();
      expect(view.write).toHaveBeenCalledTimes(1);
      if (freshButton) {
        expect(freshButton.textContent).toBe("Copy key");
        await view.user.click(freshButton);
        expect(view.write).toHaveBeenCalledTimes(2);
        expect(toast.success).toHaveBeenCalledExactlyOnceWith("Copied", "Clear your clipboard after storing the key.");
      }
    });
  }
}


for (const outcome of ["resolve", "reject"] as const) {
  it(`publishes live rotation feedback only after an admitted write ${outcome}s`, async () => {
    const view = await showSecret("rotation"), pending = deferred();
    const status = rotationFeedback();
    view.write.mockReturnValueOnce(pending.promise);
    await view.user.click(view.button);
    expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
    expect(status.textContent).toBe("");
    await act(async () => { if (outcome === "resolve") pending.resolve(); else pending.reject(new Error("synthetic pending rotation denial")); });
    expect(status.textContent).toBe(outcome === "resolve" ? "Copied. Clear your clipboard after storing the key." : "Clipboard unavailable. Select and copy the key manually.");
    expect(screen.getByRole("button", { name: outcome === "resolve" ? "Copied" : "Copy key" })).toBeDefined();
  });
  for (const lifecycle of ["close", "unmount", "reopen"] as const) {
    it(`does not publish retired rotation ${outcome} feedback after ${lifecycle}`, async () => {
      const view = await showSecret("rotation"), pending = deferred();
      const retiredStatus = rotationFeedback();
      view.write.mockReturnValueOnce(pending.promise);
      await view.user.click(view.button);
      expect(view.write).toHaveBeenCalledExactlyOnceWith(secret);
      expect(retiredStatus.textContent).toBe("");
      let freshButton: HTMLElement | undefined;
      if (lifecycle === "unmount") view.mounted.unmount();
      else {
        await view.user.click(screen.getByRole("button", { name: "I have saved it" }));
        await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
        if (lifecycle === "reopen") freshButton = await view.issue();
      }
      expect(retiredStatus.isConnected).toBe(false);
      await act(async () => { if (outcome === "resolve") pending.resolve(); else pending.reject(new Error("synthetic retired rotation denial")); });
      // Observable containment only: this does not prove cancellation or inspect
      // whether React ignored a setter on an already-unmounted owner.
      expect(retiredStatus.textContent).toBe("");
      expect(view.write).toHaveBeenCalledTimes(1);
      if (freshButton) {
        const freshStatus = rotationFeedback();
        expect(freshStatus.textContent).toBe("");
        expect(freshButton.textContent).toBe("Copy key");
        await view.user.click(freshButton);
        expect(view.write).toHaveBeenCalledTimes(2);
        expect(freshStatus.textContent).toBe("Copied. Clear your clipboard after storing the key.");
      } else expect(screen.queryByRole("dialog")).toBeNull();
    });
  }
}

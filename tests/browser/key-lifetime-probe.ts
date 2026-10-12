import type { Locator, Page } from "@playwright/test";
import { readFileSync } from "node:fs";

export type ProbeCounts = { posts: number; copies: number; exactCopies: number; credentialResponses: number };
type Probe = {
  capture(slot: string, element: HTMLElement, binding: "onSubmit" | "onClick"): void;
  invoke(slot: string, boundary?: "omg:unauthorized" | "omg:access-refresh"): boolean;
  counts(): ProbeCounts;
  disclosureMatches(element: HTMLTextAreaElement): boolean;
  contained(gatewayLog?: string): boolean;
  navigate(): void;
  idle(): boolean;
  holdNextResponse(): void;
  responseHeld(): boolean;
  releaseResponse(): void;
  heldResponseAborted(): boolean;
  mutationKeyNegativeControl(): boolean;
};
declare global { interface Window { __omgKeyLifetimeAcceptance: Probe } }

/** Only the clipboard is mocked. Fetch still reaches Rust; credentials stay in this closure. */
export async function installKeyProbe(page: Page) {
  await page.evaluate(() => {
    const tokens = new Set<string>(), consoleMessages: string[] = [];
    let consoleInspectable = true;
    for (const level of ["log", "info", "warn", "error", "debug"] as const) {
      const original = console[level].bind(console);
      console[level] = (...args: unknown[]) => {
        try { consoleMessages.push(args.map(value => typeof value === "string" ? value : value instanceof Error ? value.message : JSON.stringify(value)).join(" ")); }
        catch { consoleInspectable = false; }
        original(...args);
      };
    }
    const retained = new Map<string, { element: HTMLElement; binding: "onSubmit" | "onClick"; handler: Function }>();
    let posts = 0, copies = 0, exactCopies = 0, client: object | undefined, router: object | undefined;
    let holdNext = false, held = false, release: (() => void) | undefined, heldSignal: AbortSignal | null | undefined;
    const realFetch = window.fetch.bind(window);
    window.fetch = async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input), location.href);
      const method = init?.method ?? (input instanceof Request ? input.method : "GET");
      const keyPost = method === "POST" && /^\/api\/v1\/workspaces\/[^/]+\/keys(?:\/[^/]+\/rotate)?$/.test(url.pathname);
      const hold = keyPost && holdNext;
      if (hold) { holdNext = false; heldSignal = init?.signal; }
      if (keyPost) posts++;
      const response = await realFetch(input, init);
      if (keyPost && response.ok) {
        const body: unknown = await response.clone().json();
        if (body && typeof body === "object") {
          const token: unknown = Reflect.get(body, "token");
          if (typeof token === "string") tokens.add(token);
        }
      }
      if (hold) {
        held = true;
        await new Promise<void>(resolve => { release = resolve; });
        held = false;
      }
      return response;
    };
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: {
      writeText: async (value: string) => { copies++; if (tokens.has(value)) exactCopies++; },
    } });
    const call = (object: object | undefined, name: string, args: unknown[] = []) => {
      if (!object) throw new Error("Missing actual runtime owner.");
      const method: unknown = Reflect.get(object, name);
      if (typeof method !== "function") throw new Error("Missing actual runtime method.");
      return Reflect.apply(method, object, args);
    };
    const locateOwners = (element: HTMLElement) => {
      const key = Object.keys(element).find(name => name.startsWith("__reactFiber$"));
      if (!key) throw new Error("Expected the mounted React fiber.");
      let fiber: unknown = Reflect.get(element, key);
      for (let depth = 0; fiber && typeof fiber === "object" && depth < 100; depth++) {
        const props: unknown = Reflect.get(fiber, "memoizedProps");
        if (props && typeof props === "object") {
          const query: unknown = Reflect.get(props, "client"), route: unknown = Reflect.get(props, "router");
          if (query && typeof query === "object" && typeof Reflect.get(query, "getQueryCache") === "function") client = query;
          if (route && typeof route === "object" && typeof Reflect.get(route, "navigate") === "function") router = route;
        }
        fiber = Reflect.get(fiber, "return");
      }
      if (!client || !router) throw new Error("Expected actual QueryClient and Router owners.");
    };
    const main = document.querySelector("main");
    if (!(main instanceof HTMLElement)) throw new Error("Expected the actual mounted main element.");
    locateOwners(main);
    window.__omgKeyLifetimeAcceptance = {
      capture(slot, element, binding) {
        if (!element.isConnected) throw new Error("Capture requires a live mounted native element.");
        locateOwners(element);
        const key = Object.keys(element).find(name => name.startsWith("__reactProps$"));
        const props: unknown = key ? Reflect.get(element, key) : undefined;
        if (!props || typeof props !== "object") throw new Error("Expected actual React bindings.");
        const handler: unknown = Reflect.get(props, binding);
        if (typeof handler !== "function") throw new Error("Expected the actual render-bound callback.");
        retained.set(slot, { element, binding, handler });
      },
      invoke(slot, boundary) {
        const entry = retained.get(slot);
        if (!entry) throw new Error("Missing retained callback.");
        if (boundary) window.dispatchEvent(new Event(boundary));
        const connected = entry.element.isConnected;
        const nativeEvent = entry.binding === "onSubmit"
          ? new Event("submit", { bubbles: true, cancelable: true })
          : new MouseEvent("click", { bubbles: true, cancelable: true });
        const event = {
          nativeEvent, currentTarget: entry.element, target: entry.element, type: nativeEvent.type,
          bubbles: nativeEvent.bubbles, cancelable: nativeEvent.cancelable, eventPhase: nativeEvent.eventPhase,
          isTrusted: nativeEvent.isTrusted, timeStamp: nativeEvent.timeStamp,
          get defaultPrevented() { return nativeEvent.defaultPrevented; },
          preventDefault: () => nativeEvent.preventDefault(), stopPropagation: () => nativeEvent.stopPropagation(),
          isDefaultPrevented: () => nativeEvent.defaultPrevented,
          isPropagationStopped: () => nativeEvent.cancelBubble, persist() {},
          ...(nativeEvent instanceof MouseEvent ? {
            altKey: nativeEvent.altKey, button: nativeEvent.button, buttons: nativeEvent.buttons,
            clientX: nativeEvent.clientX, clientY: nativeEvent.clientY, ctrlKey: nativeEvent.ctrlKey,
            metaKey: nativeEvent.metaKey, movementX: nativeEvent.movementX, movementY: nativeEvent.movementY,
            pageX: nativeEvent.pageX, pageY: nativeEvent.pageY, relatedTarget: nativeEvent.relatedTarget,
            screenX: nativeEvent.screenX, screenY: nativeEvent.screenY, shiftKey: nativeEvent.shiftKey,
            detail: nativeEvent.detail, view: window, getModifierState: (modifier: string) => nativeEvent.getModifierState(modifier),
          } : {}),
        };
        entry.handler(event);
        if (entry.binding === "onSubmit" && !nativeEvent.defaultPrevented) throw new Error("Submit must prevent default.");
        return connected;
      },
      counts: () => ({ posts, copies, exactCopies, credentialResponses: tokens.size }),
      disclosureMatches: element => tokens.has(element.value),
      contained(gatewayLog = "") {
        const queryCache: unknown = call(client, "getQueryCache"), mutationCache: unknown = call(client, "getMutationCache");
        if (!queryCache || typeof queryCache !== "object" || !mutationCache || typeof mutationCache !== "object") throw new Error("Missing actual caches.");
        const entries = (cache: object) => {
          const values: unknown = call(cache, "getAll");
          if (!Array.isArray(values)) throw new Error("Expected actual cache entries.");
          return values.map((value: unknown) => {
            if (!value || typeof value !== "object") throw new Error("Expected an actual cache entry.");
            const options: unknown = Reflect.get(value, "options");
            return {
              queryKey: Reflect.get(value, "queryKey"),
              mutationKey: options && typeof options === "object" ? Reflect.get(options, "mutationKey") : undefined,
              state: Reflect.get(value, "state"),
            };
          });
        };
        const state = JSON.stringify([entries(queryCache), entries(mutationCache), { ...localStorage }, { ...sessionStorage }, location.href, consoleMessages, gatewayLog]);
        return consoleInspectable && [...tokens].every(token => !state.includes(token));
      },
      navigate: () => { void call(router, "navigate", [{ to: "/home" }]); },
      idle: () => call(client, "isFetching") === 0,
      holdNextResponse: () => { holdNext = true; },
      responseHeld: () => held,
      releaseResponse: () => {
        if (!release || !held) throw new Error("Expected a held actual Rust response.");
        release(); release = undefined;
      },
      heldResponseAborted: () => heldSignal?.aborted === true,
      mutationKeyNegativeControl: () => {
        const token = tokens.values().next().value;
        if (typeof token !== "string" || !token) throw new Error("Expected a privately tracked credential.");
        const cache: unknown = call(client, "getMutationCache");
        if (!cache || typeof cache !== "object") throw new Error("Expected the actual mutation cache.");
        // Fixture-only synchronous canary. Remove exactly this mutation before returning a boolean.
        const mutation: unknown = call(cache, "build", [client, { mutationKey: ["private-key-lifetime-canary", token] }]);
        try { return !window.__omgKeyLifetimeAcceptance.contained(); }
        finally { call(cache, "remove", [mutation]); }
      },
    };
  });
}

export async function captureCallback(page: Page, slot: string, element: Locator, binding: "onSubmit" | "onClick") {
  await element.evaluate((node, args) => {
    if (!(node instanceof HTMLElement)) throw new Error("Expected a native HTMLElement.");
    window.__omgKeyLifetimeAcceptance.capture(args.slot, node, args.binding);
  }, { slot, binding });
}
export const probeCounts = (page: Page) => page.evaluate(() => window.__omgKeyLifetimeAcceptance.counts());
export const invokeCallback = (page: Page, slot: string, boundary?: "omg:unauthorized" | "omg:access-refresh") =>
  page.evaluate(args => window.__omgKeyLifetimeAcceptance.invoke(args.slot, args.boundary), { slot, boundary });
export const waitForIdle = (page: Page) => page.waitForFunction(() => window.__omgKeyLifetimeAcceptance.idle());
// Compare privately inside the browser. Neither log contents nor credentials enter test output.
export const credentialsContained = (page: Page) => page.evaluate(
  gatewayLog => window.__omgKeyLifetimeAcceptance.contained(gatewayLog),
  readFileSync("target/browser-tests/gateway.log", "utf8"),
);

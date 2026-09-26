import { afterEach, describe, expect, it, vi } from "vitest";
import { getGatewayStatus } from "./gateway";

const signal = () => new AbortController().signal;

afterEach(() => vi.unstubAllGlobals());

describe("gateway readiness", () => {
  it("uses a same-origin URL and checks the response payload", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ status: "ready" }));
    vi.stubGlobal("fetch", fetch);
    await expect(getGatewayStatus(signal())).resolves.toBe("ready");
    expect(fetch).toHaveBeenCalledWith("/health/ready", {
      cache: "no-store",
      signal: expect.any(AbortSignal),
    });
  });

  it("distinguishes a reachable but unready gateway", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ status: "not_ready" }, { status: 503 })));
    await expect(getGatewayStatus(signal())).resolves.toBe("not ready");
  });

  it("does not treat an arbitrary successful response as readiness", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ status: "other" })));
    await expect(getGatewayStatus(signal())).rejects.toThrow("Unexpected gateway");
  });

  it("rejects HTML fallback responses", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("<!doctype html>")));
    await expect(getGatewayStatus(signal())).rejects.toThrow();
  });

  it("propagates network failures instead of showing cached success", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("Failed to fetch")));
    await expect(getGatewayStatus(signal())).rejects.toThrow("Failed to fetch");
  });

  it("preserves caller cancellation in the fetch signal", async () => {
    const controller = new AbortController();
    controller.abort();
    const fetch = vi.fn().mockImplementation((_url: string, init: RequestInit) => {
      init.signal?.throwIfAborted();
      return Promise.resolve(Response.json({ status: "ready" }));
    });
    vi.stubGlobal("fetch", fetch);
    await expect(getGatewayStatus(controller.signal)).rejects.toThrow();
  });
});

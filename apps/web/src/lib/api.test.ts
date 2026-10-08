import { afterEach, describe, expect, it, vi } from "vitest";
import { API, ApiError, api, csrfCookie } from "./api";

afterEach(() => vi.unstubAllGlobals());
const cookie = (value = "omg_csrf=csrf-value") => vi.stubGlobal("document", { cookie: value });
describe("CSRF cookie", () => {
  it("reads only the exact cookie and decodes it", () => {
    expect(csrfCookie("not_omg_csrf=wrong; omg_csrf=encoded%2Bvalue; another=1")).toBe("encoded+value");
    expect(csrfCookie("omg_session=private")).toBeUndefined();
    expect(csrfCookie("omg_csrf=")).toBeUndefined();
    expect(csrfCookie("omg_csrf=%oops")).toBeUndefined();
  });
});
describe("management API", () => {
  it("uses same-origin cookies, bypasses HTTP caching, and passes cancellation", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ user: { id: "me" } }));
    vi.stubGlobal("fetch", fetch);
    const controller = new AbortController(), signal = controller.signal;
    await api(`${API}/me`, { signal });
    expect(fetch).toHaveBeenCalledWith("/api/v1/me", expect.objectContaining({ method: "GET", credentials: "same-origin", cache: "no-store", signal: expect.any(AbortSignal) }));
    expect(fetch.mock.calls[0][1].headers).toEqual({ Accept: "application/json" });
    controller.abort(); expect(fetch.mock.calls[0][1].signal.aborted).toBe(true);
  });
  it.each(["POST", "PATCH", "DELETE"] as const)("sends readable CSRF cookie for %s without overriding browser Origin", async (method) => {
    cookie();
    const fetch = vi.fn().mockResolvedValue(Response.json({ ok: true }));
    vi.stubGlobal("fetch", fetch);
    await api(`${API}/workspaces/ws/keys`, { method, body: { name: "a" } });
    expect(fetch.mock.calls[0][1]).toMatchObject({ method, body: '{"name":"a"}', headers: { "X-CSRF-Token": "csrf-value", "Content-Type": "application/json" } });
    expect(fetch.mock.calls[0][1].headers).not.toHaveProperty("Origin");
    expect(fetch.mock.calls[0][1].headers).not.toHaveProperty("Authorization");
  });
  it("blocks mutation without CSRF before sending the request", async () => {
    cookie("");
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    await expect(api(`${API}/auth/logout`, { method: "POST" })).rejects.toMatchObject({ status: 403, code: "csrf_missing" });
    expect(fetch).not.toHaveBeenCalled();
  });
  it.each(["https://example.com/api/v1/me", "//example.com/api/v1/me", "/health/ready", "/api/v1/me#token"])("rejects non-management or non-relative URL %s", async (path) => {
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    await expect(api(path)).rejects.toThrow("Invalid API path");
    expect(fetch).not.toHaveBeenCalled();
  });
  it("returns one-time secrets directly without wrapping them in a cache", async () => {
    cookie(); vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ id: "key", token: "one-time" })));
    await expect(api(`${API}/workspaces/ws/keys`, { method: "POST", body: { name: "a", expires_in_days: 30 } })).resolves.toEqual({ id: "key", token: "one-time" });
  });
  it("preserves structured gateway errors", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { code: "forbidden", message: "Access denied" } }, { status: 403 })));
    await expect(api(`${API}/me`)).rejects.toMatchObject({ status: 403, code: "forbidden", message: "Access denied" });
  });
  it("renders Axum plain-text validation errors without requiring JSON", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("Failed to deserialize query string", { status: 400 })));
    await expect(api(`${API}/me`)).rejects.toThrow("Failed to deserialize query string");
  });
  it("does not expose proxy HTML errors as trusted markup or detailed errors", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("<html>proxy failure</html>", { status: 502, headers: { "content-type": "text/html" } })));
    await expect(api(`${API}/me`)).rejects.toThrow("Request failed (502)");
  });
  it("rejects a successful HTML SPA fallback", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("<!doctype html>")));
    await expect(api(`${API}/me`)).rejects.toMatchObject({ code: "invalid_response" });
  });
  it("supports empty 204 logout responses", async () => {
    cookie(); vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(null, { status: 204 })));
    await expect(api(`${API}/auth/logout`, { method: "POST" })).resolves.toBeUndefined();
  });
  it("signals expiration on a protected request, not on the initial session probe", async () => {
    const dispatchEvent = vi.fn(); vi.stubGlobal("window", { dispatchEvent });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => Promise.resolve(Response.json({ error: { message: "Sign in" } }, { status: 401 }))));
    await expect(api(`${API}/me`)).rejects.toBeInstanceOf(ApiError);
    expect(dispatchEvent).not.toHaveBeenCalled();
    await expect(api(`${API}/workspaces/id/models`)).rejects.toBeInstanceOf(ApiError);
    expect(dispatchEvent.mock.calls[0][0].type).toBe("omg:unauthorized");
  });
  it("sanitizes network errors while preserving cancellation", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("private details")));
    await expect(api(`${API}/me`)).rejects.toMatchObject({ code: "network_error" });
    const controller = new AbortController(); controller.abort();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(controller.signal.reason));
    await expect(api(`${API}/me`, { signal: controller.signal })).rejects.toBe(controller.signal.reason);
  });
});

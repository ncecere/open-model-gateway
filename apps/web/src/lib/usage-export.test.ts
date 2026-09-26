import { afterEach, describe, expect, it, vi } from "vitest";
import { EXPORT_MAX_BYTES, usageCsv } from "./usage-export";

afterEach(() => vi.unstubAllGlobals());
describe("bounded same-origin CSV export", () => {
  it("uses a fixed encoded workspace path, cookies, no cache, and forbids redirects", async () => {
    const fetcher = vi.fn().mockResolvedValue(new Response("id,cost\r\nrow,1\r\n", { headers: { "content-type": "text/csv; charset=utf-8" } }));
    vi.stubGlobal("fetch", fetcher);
    const controller = new AbortController();
    const blob = await usageCsv("team/name", 100, 100, controller.signal);
    expect(fetcher).toHaveBeenCalledWith("/api/v1/workspaces/team%2Fname/usage-export?limit=100&offset=100", expect.objectContaining({ credentials: "same-origin", cache: "no-store", redirect: "error", signal: controller.signal }));
    expect(await blob.text()).toContain("row,1");
  });
  it("rejects unbounded pagination without fetching", async () => {
    const fetcher = vi.fn(); vi.stubGlobal("fetch", fetcher);
    for (const [limit, offset] of [[0, 0], [1001, 0], [1, -1], [1, 100001], [1.5, 0]]) await expect(usageCsv("ws", limit, offset)).rejects.toThrow("Invalid export page");
    expect(fetcher).not.toHaveBeenCalled();
  });
  it("does not download an HTML login/error response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("<html>Login</html>", { headers: { "content-type": "text/html" } })));
    await expect(usageCsv("ws", 1, 0)).rejects.toThrow("did not return CSV");
  });
  it("caps streamed bytes even without Content-Length", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(new Uint8Array(EXPORT_MAX_BYTES + 1), { headers: { "content-type": "text/csv" } })));
    await expect(usageCsv("ws", 1000, 0)).rejects.toThrow("exceeds 10 MiB");
  });
  it("surfaces authorization failure without saving server content", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("private server contents", { status: 403 })));
    await expect(usageCsv("ws", 1, 0)).rejects.toThrow("CSV export failed (403)");
  });
  it("clears the session on expired authentication", async () => {
    const dispatchEvent = vi.fn(); vi.stubGlobal("window", { dispatchEvent });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 401 })));
    await expect(usageCsv("ws", 1, 0)).rejects.toThrow();
    expect(dispatchEvent).toHaveBeenCalledWith(expect.objectContaining({ type: "omg:unauthorized" }));
  });
});

import { describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import { addEach, batchFailureMessage } from "./grants";

describe("adding several models", () => {
  it("posts one model at a time, reports progress and keeps per-item failures", async () => {
    const post = vi.fn((id: string) => id === "b" ? Promise.reject(new ApiError(409, "409", "Model is no longer available")) : Promise.resolve({ ok: true }));
    const progress = vi.fn();
    const result = await addEach(["a", "b", "c"], post, undefined, progress);
    expect(post.mock.calls.map(c => c[0])).toEqual(["a", "b", "c"]);
    expect(result).toEqual({ added: ["a", "c"], failed: [{ id: "b", message: "Model is no longer available" }], stopped: false });
    expect(progress.mock.calls).toEqual([[1, 3], [2, 3], [3, 3]]);
    const names: Record<string, string> = { a: "Alpha", b: "Beta", c: "Gamma" };
    expect(batchFailureMessage(result, 3, id => names[id])).toBe("Added 2 of 3 models. Not added: Beta (Model is no longer available). Submitting again retries only the models not yet added.");
  });
  it("stops before the next request once aborted", async () => {
    const controller = new AbortController();
    const post = vi.fn(async () => { controller.abort(); });
    const result = await addEach(["a", "b"], post, controller.signal);
    expect(post).toHaveBeenCalledTimes(1);
    expect(result.stopped).toBe(true);
  });
});

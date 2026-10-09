import { describe, expect, it } from "vitest";
import { dismissedKey, readDismissed } from "./dismissed";

describe("dismissed notes", () => {
  it("is remembered per note and user", () => {
    const saved = new Map([[dismissedKey("sso-sync", "u1"), "1"]]);
    const storage = { getItem: (k: string) => saved.get(k) ?? null };
    expect(readDismissed(storage, "sso-sync", "u1")).toBe(true);
    expect(readDismissed(storage, "sso-sync", "u2")).toBe(false);
    expect(readDismissed(storage, "other", "u1")).toBe(false);
  });
  it("shows the note when storage is unavailable", () => {
    expect(readDismissed(undefined, "sso-sync", "u1")).toBe(false);
    expect(readDismissed({ getItem: () => { throw new Error("blocked"); } }, "sso-sync", "u1")).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { issuanceBlock, issuanceRefusal, issuanceUsageText, retryText, type KeyIssuance } from "./key-issuance";

const view = (holder: [number, number, string | null], workspace?: [number, number, string | null]): KeyIssuance => ({
  window_seconds: 86400,
  holder: { used: holder[0], limit: holder[1], next_available_at: holder[2] },
  ...(workspace ? { workspace: { used: workspace[0], limit: workspace[1], next_available_at: workspace[2] } } : {}),
});
const refusal = (scope: string, retry: number, operation = "create") => new ApiError(429, "429", "Key issuance limit reached", "key_issuance_limit_exceeded", { scope, operation }, retry);

describe("key issuance budget text", () => {
  it("reads usage in the last 24 hours", () => {
    expect(issuanceUsageText(view([3, 50, null]).holder)).toBe("3 of 50 issued in the last 24 h");
    expect(issuanceUsageText(view([1, 1, null]).holder)).toBe("1 of 1 issued in the last 24 h");
    expect(issuanceUsageText(view([1234, 10000, null]).holder)).toBe("1,234 of 10,000 issued in the last 24 h");
  });
  it("names the wait in plain units", () => {
    expect(retryText(1)).toBe("in 1 second");
    expect(retryText(45)).toBe("in 45 seconds");
    expect(retryText(60)).toBe("in 1 minute");
    expect(retryText(61)).toBe("in 2 minutes");
    expect(retryText(3600)).toBe("in 1 hour");
    expect(retryText(3 * 3600 + 5 * 60)).toBe("in 3 hours 5 minutes");
    expect(retryText(86400)).toBe("in 24 hours");
    expect(retryText(0)).toBe("in 1 second");
  });
  it("blocks Create only when a budget the caller can see is spent", () => {
    const now = Date.parse("2026-10-23T12:00:00Z");
    expect(issuanceBlock(view([2, 3, null]), now)).toBeUndefined();
    expect(issuanceBlock(view([3, 3, "2026-10-23T14:30:00Z"]), now)).toEqual({ scope: "holder", message: "You have created or rotated 3 keys in the last 24 hours, the most this workspace allows. You can issue another in 2 hours 30 minutes." });
    // Workspace admins also see the workspace's budget.
    expect(issuanceBlock(view([1, 3, null], [5, 5, "2026-10-23T12:00:30Z"]), now)).toEqual({ scope: "workspace", message: "This workspace has issued 5 keys in the last 24 hours, the most it allows. Another can be issued in 30 seconds." });
    // The holder's own limit is named first when both are spent.
    expect(issuanceBlock(view([3, 3, "2026-10-23T13:00:00Z"], [5, 5, "2026-10-23T12:00:30Z"]), now)?.scope).toBe("holder");
    // A past time still shows a (minimal) wait, never a negative one.
    expect(issuanceBlock(view([3, 3, "2026-10-23T11:00:00Z"]), now)?.message).toContain("in 1 second");
    expect(issuanceBlock(view([1, 1, "2026-10-23T12:00:30Z"]), now)?.message).toContain("created or rotated 1 key ");
  });
  it("says what was used when a lowered limit leaves the use above it", () => {
    const now = Date.parse("2026-10-23T12:00:00Z");
    // 5 used, limit lowered to 3: never "created 3 keys".
    expect(issuanceBlock(view([5, 3, "2026-10-23T14:00:00Z"]), now)?.message).toBe("You have created or rotated 5 keys in the last 24 hours, and this workspace now allows 3. You can issue another in 2 hours.");
    expect(issuanceBlock(view([1, 3, null], [7, 4, "2026-10-23T12:10:00Z"]), now)?.message).toBe("This workspace has issued 7 keys in the last 24 hours, and it now allows 4. Another can be issued in 10 minutes.");
    expect(issuanceBlock(view([1, 1, "2026-10-23T12:00:30Z"]), now)?.message).toContain("created or rotated 1 key ");
  });
  it("turns a 429 refusal into a named message with the retry time and nothing else", () => {
    expect(issuanceRefusal(refusal("holder", 7500))).toEqual({ scope: "holder", operation: "create", retrySeconds: 7500, message: "You've reached this workspace's limit on creating or rotating keys. You can try again in 2 hours 5 minutes." });
    expect(issuanceRefusal(refusal("workspace", 90, "rotate"))).toEqual({ scope: "workspace", operation: "rotate", retrySeconds: 90, message: "This workspace has reached its limit on creating or rotating keys across all members. Try again in 2 minutes." });
    // Other errors, and other 429s, are not issuance refusals.
    expect(issuanceRefusal(new ApiError(429, "429", "slow down"))).toBeUndefined();
    expect(issuanceRefusal(new ApiError(403, "403", "Access denied", "key_issuance_limit_exceeded"))).toBeUndefined();
    expect(issuanceRefusal(new Error("x"))).toBeUndefined();
    // A missing wait still gives a usable message.
    expect(issuanceRefusal(new ApiError(429, "429", "m", "key_issuance_limit_exceeded", { scope: "holder", operation: "create" }))?.message).toContain("try again later");
  });
});

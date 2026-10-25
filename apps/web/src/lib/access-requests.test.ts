import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { accessRequestRefusal, newIdempotencyKey, reasonError } from "./access-requests";

describe("access request reason (#83 slice A): bounded plain text, never HTML", () => {
  it("accepts empty (optional) and ordinary single-line text", () => {
    expect(reasonError("")).toBeUndefined();
    expect(reasonError("I need the staging catalog for an incident.")).toBeUndefined();
  });
  it("mirrors the server's 1,000-scalar-value bound, counting by code point not UTF-16 length", () => {
    expect(reasonError("a".repeat(1000))).toBeUndefined();
    expect(reasonError("a".repeat(1001))).toMatch(/1,000 characters/);
    // Each 😀 is one Unicode scalar value but two UTF-16 code units: length-based counting would
    // wrongly reject this where the server's chars().count() (and [...].length) accept it.
    expect(reasonError("😀".repeat(1000))).toBeUndefined();
  });
  it("rejects control characters, including line breaks, matching the server's char::is_control check", () => {
    expect(reasonError("line one\nline two")).toMatch(/control characters/);
    expect(reasonError("tabbed\ttext")).toMatch(/control characters/);
    expect(reasonError("a\u0080b")).toMatch(/control characters/);
  });
});

describe("newIdempotencyKey (#83 slice A)", () => {
  it("is visible ASCII within the server's 1-128 byte bound and unique per call", () => {
    const a = newIdempotencyKey(), b = newIdempotencyKey();
    expect(a).not.toBe(b);
    expect(a.length).toBeGreaterThan(0);
    expect(a.length).toBeLessThanOrEqual(128);
    expect(/^[\x21-\x7e]+$/.test(a)).toBe(true);
  });
});

describe("accessRequestRefusal (#83 slice A): fixed 429 message naming the wait", () => {
  it("names the wait from retry_after_seconds, never a generic message", () => {
    const error = new ApiError(429, "429", "Too many access requests", "access_request_rate_limited", undefined, 42);
    const refusal = accessRequestRefusal(error);
    expect(refusal?.title).toBe("Too many requests");
    expect(refusal?.message).toMatch(/Try again in 42 seconds/);
    expect(refusal?.tone).toBe("warning");
  });
  it("still shows a fixed message when the wait itself is unknown", () => {
    const error = new ApiError(429, "429", "Too many access requests", "access_request_rate_limited");
    expect(accessRequestRefusal(error)?.message).toMatch(/try again later/i);
  });
  it("ignores every other error, including an unrelated 429 and a 409 conflict, so refusals stay generic", () => {
    expect(accessRequestRefusal(new ApiError(429, "429", "Too many keys", "key_issuance_limit_exceeded", undefined, 5))).toBeUndefined();
    expect(accessRequestRefusal(new ApiError(409, "409", "Request is no longer applicable"))).toBeUndefined();
    expect(accessRequestRefusal(new Error("network"))).toBeUndefined();
  });
});

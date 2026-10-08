import { describe, expect, it } from "vitest";
import { cssPercent, formatBasisPoints, formatDurationMs, formatMicroUsd, formatShare, parseInteger, percentChange, shareBasisPoints } from "./kit-format";

describe("UI kit exact formatting", () => {
  it("parses only non-negative integer strings and never treats unknown as zero", () => {
    expect(parseInteger("9007199254740993")).toBe(9007199254740993n);
    expect(parseInteger("1.5")).toBeNull();
    expect(parseInteger("-1")).toBeNull();
    expect(parseInteger(null)).toBeNull();
    expect(parseInteger(1.5)).toBeNull();
  });

  it("computes shares exactly; tiny real shares are never 0%", () => {
    expect(formatShare("1", "1000000")).toBe("<0.1%");
    expect(formatShare("1", "3")).toBe("33.3%");
    expect(formatShare("0", "3")).toBe("0%");
    expect(formatShare("5", "5")).toBe("100%");
    expect(formatShare("1", "0")).toBe("—");
    expect(formatShare(null, "5")).toBe("Unknown");
    expect(formatShare("8100", "50000000", true)).toBe("<1%");
    expect(shareBasisPoints("9007199254740993", "9007199254740993")).toBe(10000n);
    expect(formatBasisPoints(null)).toBe("Unknown");
    expect(cssPercent(25000n)).toBe("100%");
    expect(cssPercent(1250n)).toBe("12.5%");
  });

  it("computes period changes exactly for integer strings and only when both sides are known", () => {
    expect(percentChange("150", "100")).toEqual({ direction: "up", text: "+50%" });
    expect(percentChange("50", "100")).toEqual({ direction: "down", text: "−50%" });
    expect(percentChange("100", "100")).toEqual({ direction: "flat", text: "0%" });
    expect(percentChange("100", "0")).toEqual({ direction: "up", text: "New" });
    expect(percentChange("99999", "100000")).toEqual({ direction: "down", text: "<−0.1%" });
    expect(percentChange(null, "100")).toBeNull();
    expect(percentChange("100", undefined)).toBeNull();
    expect(percentChange(0.5, 0.25)).toEqual({ direction: "up", text: "+100%" });
    expect(percentChange(Number.NaN, 0.25)).toBeNull();
  });

  it("formats durations and money without rounding sub-cent to $0", () => {
    expect(formatDurationMs(41)).toBe("41 ms");
    expect(formatDurationMs(1234)).toBe("1.2 s");
    expect(formatDurationMs(125_000)).toBe("2 min 5 s");
    expect(formatDurationMs(null)).toBe("Unknown");
    expect(formatMicroUsd("8100")).toBe("$0.0081");
    expect(formatMicroUsd("1")).toBe("$0.000001");
    expect(formatMicroUsd("0")).toBe("$0.00");
    expect(formatMicroUsd(null)).toBe("Unknown");
  });
});

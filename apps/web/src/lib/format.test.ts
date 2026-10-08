import { describe, expect, it } from "vitest";
import { detailTime, priceText, readinessText, tableTime, tokensText, utcDay, utcPeriod } from "./format";

describe("shared formatters (review rules 3–6)", () => {
  const now = new Date("2026-10-08T12:00:00Z");
  it("writes dates one way: tables, detail pages, UTC periods and days", () => {
    expect(tableTime("2026-10-08T14:30:00Z", now, "UTC")).toBe("Oct 8, 2:30 PM");
    expect(tableTime("2025-01-02T00:05:00Z", now, "UTC")).toBe("Jan 2, 2025, 12:05 AM");
    expect(detailTime("2026-10-08T06:30:00Z", "America/New_York")).toBe("Oct 8, 2026, 2:30 AM EDT");
    expect(utcPeriod("2026-10-01", "2026-10-09")).toBe("Oct 1 – Oct 8, 2026 (UTC)");
    expect(utcPeriod("2025-12-20", "2026-01-05")).toBe("Dec 20, 2025 – Jan 4, 2026 (UTC)");
    expect(utcDay("2026-10-01")).toBe("Oct 1, 2026");
    expect(tableTime("nope")).toBe("nope");
  });
  it("writes tokens, prices and readiness one way; unknown is never zero", () => {
    expect(tokensText("145", "0")).toBe("145 in · 0 out");
    expect(tokensText(null, "0")).toBe("Unknown");
    expect(tokensText("0", "0", "audio_speech")).toBe("Not applicable");
    expect(priceText("100000", "M input tokens")).toBe("$0.10 / M input tokens");
    expect(priceText("8100", "/M tokens")).toBe("$0.0081 / M tokens");
    expect(priceText(null, "M tokens")).toBe("Unknown");
    expect(readinessText.not_serving).toBe("Not serving");
  });
});

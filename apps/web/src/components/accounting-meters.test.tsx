// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { CacheAccounting, MeterAccounting, NO_METER_CHARGE, meterRows } from "../pages/usage/accounting";
import { knownSpendText } from "../lib/reports";
import { markup } from "../lib/test-fixtures";
import type { MeterCostComponents, UnitMeter } from "../lib/governance";

const keys: UnitMeter[] = ["output_images", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "search_units", "requests"];
const counts = (values: Partial<Record<UnitMeter, string>>) => Object.fromEntries(keys.map(k => [k, values[k] ?? "0"])) as Record<UnitMeter, string>;
const zeros: MeterCostComponents = { output_images_microusd: "0", input_characters_microusd: "0", input_audio_microusd: "0", output_audio_microusd: "0", search_units_microusd: "0", requests_microusd: "0" };

describe("accounting meter table: unknown is not zero, sub-cent is not $0", () => {
  // The live demo's report: an image priced per output token (meter charge 0), characters at 330 µUSD, and meters
  // that some attempts never reported.
  const usage = { output_images: "1", input_characters: "22", input_audio_seconds_ms: "1000", output_audio_seconds_ms: "0", search_units: "0", requests: "5" };
  const relevant = counts({ output_images: "1", input_characters: "2", input_audio_seconds_ms: "1", output_audio_seconds_ms: "1", search_units: "1", requests: "6" });
  const unknown = counts({ output_audio_seconds_ms: "1", search_units: "1", requests: "1" });
  const components = { ...zeros, input_characters_microusd: "330", input_audio_microusd: "100" };

  it("shows exact micro-USD, Unknown spend beside unknown usage and no $0.00 next to real usage", () => {
    const rows = meterRows(usage, components, relevant, unknown);
    expect(rows.map(r => [r.label, r.usage, r.spent])).toEqual([
      ["Images generated", "1", "—"],
      ["Input characters", "22", "$0.00033"],
      ["Audio input", "1 s", "$0.0001"],
      ["Audio output", "Unknown", "Unknown"],
      ["Search units", "Unknown", "Unknown"],
      ["Metered requests", "Unknown · partial (at least 5)", "Unknown"],
    ]);
    expect(rows[0]!.note).toBe(NO_METER_CHARGE);
    const html = markup(<MeterAccounting usage={usage} components={components} providerCost={null} relevant={relevant} unknown={unknown} />);
    expect(html).not.toContain("$0.00<");
    expect(html).toContain(NO_METER_CHARGE);
  });

  it("gives a partial meter's known charge as a lower bound", () => {
    expect(meterRows(usage, { ...components, requests_microusd: "2500" }, relevant, unknown).find(r => r.key === "requests")?.spent).toBe("At least $0.0025");
  });

  it("hides meters with zero usage and zero spend, and missing spend is Unknown", () => {
    const quiet = { ...usage, input_audio_seconds_ms: "0" };
    const rows = meterRows(quiet, { ...components, input_audio_microusd: "0", input_characters_microusd: undefined as unknown as string }, counts({ input_audio_seconds_ms: "1", input_characters: "1" }), counts({}));
    expect(rows.map(r => [r.key, r.spent])).toEqual([["input_characters", "Unknown"]]);
    // Older gateways (no per-meter counts): known zero rows are hidden; unknown usage keeps unknown spend.
    const legacy = meterRows({ ...usage, output_audio_seconds_ms: null, search_units: "0", requests: "0" }, zeros);
    expect(legacy.map(r => [r.key, r.usage, r.spent])).toEqual([["output_images", "1", "—"], ["input_characters", "22", "—"], ["input_audio_seconds_ms", "1 s", "—"], ["output_audio_seconds_ms", "Unknown", "Unknown"]]);
    expect(markup(<MeterAccounting usage={{ ...usage, output_images: "0", input_characters: "0", input_audio_seconds_ms: "0", requests: "0" }} components={zeros} providerCost={null} relevant={counts({ output_images: "1" })} unknown={counts({})} />)).toBe("");
  });

  it("never shows a zero cache charge beside unknown tokens", () => {
    const html = markup(<CacheAccounting billing={null} components={{ uncached_input_microusd: "0", cache_read_microusd: "0", cache_write_default_microusd: "0", cache_write_5m_microusd: "0", cache_write_1h_microusd: "0", output_microusd: "2266" }} attempts="3" />);
    expect(html).not.toContain("$0.00<");
    expect(html).toContain("$0.002266");
  });
});

describe("knownSpendText (Explore, By workspace)", () => {
  it("is Unknown for a zero known spend with unresolved attempts, exact otherwise", () => {
    expect(knownSpendText("0", "1")).toBe("Unknown");
    expect(knownSpendText("0", "0")).toBe("$0.00");
    expect(knownSpendText("2753", "5")).toBe("$0.0028"); // rounded for reading, never $0.00
    expect(knownSpendText("8271628", "0")).toBe("$8.27");
    expect(knownSpendText(null, "0")).toBe("Unknown");
    expect(knownSpendText("14", null)).toBe("$0.000014");
  });
});

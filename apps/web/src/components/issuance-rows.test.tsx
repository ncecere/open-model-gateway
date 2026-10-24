import { describe, expect, it } from "vitest";
import { LimitsTable } from "./scope-limits";
import { draftOf, noLimits, type Limits } from "../lib/limits";
import { markup } from "../lib/test-fixtures";

const base: Limits = { ...noLimits };
const table = (limits: Limits, extra: Partial<Parameters<typeof LimitsTable>[0]> = {}) => markup(<LimitsTable caption="Limits" scopeLabel="This workspace" draft={draftOf(limits)} editing storage {...extra} />);

describe("key issuance rows (#253)", () => {
  it("shows both rows, with the 10,000 ceiling for a blank value, when the gateway reports the fields", () => {
    const html = table({ ...base, key_issuances_per_holder: 50, key_issuances_per_workspace: null }, { inherited: { label: "Inherited", limits: { ...base, key_issuances_per_holder: 50, key_issuances_per_workspace: 500 } }, effective: { limits: { ...base, key_issuances_per_holder: 50, key_issuances_per_workspace: 500 } }, editing: false, emptyText: "No limit" });
    expect(html).toContain("Keys per person per 24 h");
    expect(html).toContain("Keys per workspace per 24 h");
    expect(html).toContain("10,000 (maximum)");
  });
  it("hides them when the gateway's response lacks the fields (an older gateway), rather than showing a ceiling it does not enforce", () => {
    const html = table(base, { inherited: { label: "Inherited", limits: base }, effective: { limits: base } });
    expect(html).not.toContain("Keys per person per 24 h");
    expect(html).not.toContain("Keys per workspace per 24 h");
    expect(html).not.toContain("10,000 (maximum)");
    expect(html).toContain("Storage");
  });
});

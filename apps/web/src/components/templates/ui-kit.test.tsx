// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { Table, Td } from "../ui/table/table";
import { Button } from "../ui/button/button";
import { BudgetRing } from "./budget-ring";
import { CopyId, shortId } from "./copy-id";
import { DataPolicyBadge, toDataPolicy } from "./data-policy-badge";
import { FilterToolbar, type ToolbarFacet, type ToolbarValues, activeToolbarCount, rangeText, toolbarValuesFromSearch, toolbarValuesToSearch } from "./filter-toolbar";
import { LayerTable } from "./layer-table";
import { DangerAction, DangerZone, InfoBanner } from "./notices";
import { PercentBarCell } from "./percent-bar-cell";
import { PeriodBadge, PeriodPills } from "./period-pills";
import { PIVOT_NONE, PivotControls, type PivotValue, pivotFromSearch, pivotToSearch } from "./pivot-controls";
import { PresetChoice, type PresetValue, presetChoiceText } from "./preset-choice";
import { PrevNext } from "./prev-next";
import { PriceLine, PriceLines } from "./price-line";
import { SectionNav } from "./section-nav";
import { StatTile, StatTileGrid } from "./stat-tile";
import { StatusPill, isInactiveStatus, toLifecycleStatus } from "./status-pill";
import { ColumnChooser, DensityToggle, ViewDataTable, tableViewFromSearch, tableViewToSearch } from "./table-view";
import { ExpandableRow, TableDividerRow, expandColumn } from "./table-rows";
import { Timeline } from "./timeline";
import { TypeTabs } from "./type-tabs";
import { UsageBar, usageText } from "./usage-bar";

beforeEach(() => {
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const LONG = "An exceptionally long label that keeps going to check wrapping on a 390 pixel phone screen without overflow";

describe("StatTile", () => {
  it("renders label, value, a labelled sparkline and a configurable delta", () => {
    const { container } = render(<StatTile label="Spend" value="$12.40" series={[1, 2, null, 4]} delta={{ current: "150", previous: "100", increaseIs: "bad", label: "vs previous 7 days" }} />);
    expect(screen.getByText("Spend")).toBeTruthy();
    expect(screen.getByText("$12.40")).toBeTruthy();
    expect(screen.getByRole("img", { name: /Spend trend over 4 points, from 1 to 4/ })).toBeTruthy();
    expect(screen.getByText("+50%")).toBeTruthy();
    expect(container.querySelector("[data-sentiment='negative']")).not.toBeNull();
    expect(screen.getByText("Increased by")).toBeTruthy();
  });

  it("good and neutral semantics", () => {
    const { container, rerender } = render(<StatTile label="Cache hit" value="40%" delta={{ current: 0.4, previous: 0.2, increaseIs: "good" }} />);
    expect(container.querySelector("[data-sentiment='positive']")).not.toBeNull();
    rerender(<StatTile label="Requests" value="12" delta={{ current: "12", previous: "10", increaseIs: "neutral" }} />);
    expect(container.querySelector("[data-sentiment='neutral']")).not.toBeNull();
  });

  it("keeps unknown values and comparisons unknown, never zero", () => {
    render(<StatTile label="Spend" value={null} series={[5]} delta={{ current: null, previous: "100" }} hint="2 unpriced" />);
    expect(screen.getByText("Unknown")).toBeTruthy();
    expect(screen.getByText(/No comparison/)).toBeTruthy();
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.queryByText("0%")).toBeNull();
  });

  it("is a single link or button named by its label", async () => {
    const onClick = vi.fn();
    const { rerender } = render(<StatTile label="Tokens" value="1,204" onClick={onClick} />);
    await userEvent.click(screen.getByRole("button", { name: "Tokens" }));
    expect(onClick).toHaveBeenCalledOnce();
    rerender(<StatTile label={LONG} value="1" href="/usage/tokens" />);
    expect(screen.getByRole("link", { name: LONG }).getAttribute("href")).toBe("/usage/tokens");
  });

  it("pins every tile's sparkline slot to the bottom at a fixed size, whatever the text above (review #12)", () => {
    const { container } = render(<StatTileGrid columns={3} label="Usage">
      <StatTile label="Spend" value="$10.61" series={[1, 2, 3]} delta={{ current: "3", previous: "2", label: "vs previous 9 days" }} hint="+$5.45 on hold · View unresolved" />
      <StatTile label="Requests" value="1,945" series={[4, 5, 6]} delta={{ current: "6", previous: "4", label: "vs previous 9 days" }} hint="including 1 retry" />
      <StatTile label="Tokens" value="4,989,243" series={[7, null]} delta={{ current: "7", previous: "5" }} />
    </StatTileGrid>);
    const tiles = [...container.querySelectorAll("dl")].map(dl => dl.parentElement!);
    expect(tiles).toHaveLength(3);
    for (const tile of tiles) {
      // Every tile in the row has exactly one chart slot, the last block of its list (or just before details).
      const slots = tile.querySelectorAll("dd > span[class*='chart']");
      expect(slots).toHaveLength(1);
      expect(slots[0]!.parentElement!.tagName).toBe("DD");
      expect(slots[0]!.parentElement!.nextElementSibling).toBeNull();
    }
    // A series that can't be drawn keeps an empty, hidden slot of the same size (no fabricated chart).
    const empty = tiles[2]!.querySelector("[data-empty]")!;
    expect(empty.getAttribute("aria-hidden")).toBe("true");
    expect(empty.querySelector("svg")).toBeNull();
    // No placeholder hint line any more: alignment is structural.
    expect(tiles[2]!.textContent).not.toContain("\u00a0");
    // A tile without a series has no chart slot at all.
    const { container: plain } = render(<StatTile label="Teams" value="4" />);
    expect(plain.querySelector("span[class*='chart']")).toBeNull();
    // The layout contract lives in CSS (jsdom has no layout): flex column, list fills it, chart pinned with a fixed height.
    const css = readFileSync(resolve(__dirname, "stat-tile.module.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
    expect(css).toMatch(/\.tile\s*\{[^}]*display:\s*flex;[^}]*flex-direction:\s*column;/);
    expect(css).toMatch(/\.tile > dl\s*\{[^}]*flex:\s*1 1 auto;/);
    expect(css).toMatch(/\.tile dd:has\(> \.chart\)\s*\{[^}]*margin-top:\s*auto;/);
    expect(css).toMatch(/\.chart\s*\{[^}]*width:\s*100%;[^}]*height:\s*1\.5rem;/);
  });

  it("groups tiles", () => {
    render(<StatTileGrid label="Usage summary"><StatTile label="A" value="1" /><StatTile label="B" value="2" /></StatTileGrid>);
    expect(screen.getByRole("group", { name: "Usage summary" })).toBeTruthy();
  });
});

describe("PivotControls", () => {
  const options = { metrics: [{ value: "spend", label: "Spend" }, { value: "requests", label: "Requests" }], dimensions: [{ value: "model", label: "Model" }, { value: "key", label: "API key" }, { value: "member", label: "Member" }] };
  const defaults: PivotValue = { metric: "spend", groupBy: "model", thenBy: PIVOT_NONE, topN: "10" };

  it("renders labelled controlled selects and never repeats the group dimension", () => {
    const onChange = vi.fn();
    render(<PivotControls {...options} value={{ ...defaults, thenBy: "key" }} onChange={onChange} />);
    expect(screen.getByRole("group", { name: "Pivot" })).toBeTruthy();
    for (const name of ["Metric", "Group by", "Then by", "Top"]) expect(screen.getByLabelText(name)).toBeTruthy();
    const thenBy = screen.getByLabelText("Then by") as HTMLSelectElement;
    expect((within(thenBy).getByRole("option", { name: "Model" }) as HTMLOptionElement).disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Group by"), { target: { value: "key" } });
    expect(onChange).toHaveBeenLastCalledWith({ metric: "spend", groupBy: "key", thenBy: PIVOT_NONE, topN: "10" });
    fireEvent.change(screen.getByLabelText("Top"), { target: { value: "25" } });
    expect(onChange).toHaveBeenLastCalledWith({ metric: "spend", groupBy: "model", thenBy: "key", topN: "25" });
  });

  it("round-trips through URL search values and rejects unknown values", () => {
    const value: PivotValue = { metric: "requests", groupBy: "key", thenBy: "member", topN: "25" };
    expect(pivotFromSearch(pivotToSearch(value), defaults, options)).toEqual(value);
    expect(pivotToSearch(defaults).then).toBeUndefined();
    expect(pivotFromSearch({ metric: "evil", group: "model", then: "model", top: "999" }, defaults, options)).toEqual(defaults);
    expect(pivotFromSearch({ top: 5 }, defaults, options).topN).toBe("5");
  });
});

describe("PercentBarCell", () => {
  it("shows exact sub-cent value and a non-zero share", () => {
    const { container } = render(<PercentBarCell value="$0.0081" part="8100" total="50000000000" />);
    expect(screen.getByText("$0.0081")).toBeTruthy();
    expect(screen.getByText("<0.1%")).toBeTruthy();
    expect(container.querySelector("[aria-hidden] span")?.getAttribute("style")).toContain("--percent");
  });

  it("unknown and zero totals have no bar", () => {
    const { container, rerender } = render(<PercentBarCell part={null} total="100" />);
    expect(screen.getByText("Unknown")).toBeTruthy();
    expect(container.querySelector("[aria-hidden]")).toBeNull();
    rerender(<PercentBarCell part="0" total="0" />);
    expect(screen.getByText("No total")).toBeTruthy();
  });
});

describe("ColumnChooser + Density", () => {
  type Row = { id: string; name: string; model: string; cost: string };
  const rows: Row[] = [{ id: "1", name: "req-1", model: "gpt", cost: "$0.0081" }];
  const columns = [{ id: "name", header: "Request", accessor: "name" as const, rowHeader: true }, { id: "model", header: "Model", accessor: "model" as const }, { id: "cost", header: "Cost", accessor: "cost" as const, defaultHidden: true }];

  it("adds a density toggle and the Columns menu to Bitop DataTable", async () => {
    const onViewChange = vi.fn();
    const { container } = render(<ViewDataTable<Row> caption="Requests" columns={columns} data={rows} getRowId={r => r.id} onViewChange={onViewChange} />);
    expect(screen.getByRole("button", { name: /Columns/ })).toBeTruthy();
    expect(screen.queryByText("Cost")).toBeNull();
    expect(container.querySelector("table")?.getAttribute("data-density")).toBe("comfortable");
    await userEvent.click(screen.getByRole("button", { name: "Compact rows" }));
    expect(onViewChange).toHaveBeenLastCalledWith({ hidden: ["cost"], density: "compact" });
    expect(container.querySelector("table")?.getAttribute("data-density")).toBe("compact");
  });

  it("DensityToggle reports only real changes", async () => {
    const onChange = vi.fn();
    render(<DensityToggle value="compact" onChange={onChange} />);
    expect(screen.getByRole("group", { name: "Row density" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Compact rows" }).getAttribute("aria-pressed")).toBe("true");
    await userEvent.click(screen.getByRole("button", { name: "Compact rows" }));
    expect(onChange).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Comfortable rows" }));
    expect(onChange).toHaveBeenCalledWith("comfortable");
  });

  it("standalone ColumnChooser hides columns and keeps one visible", async () => {
    function Harness() {
      const [hidden, setHidden] = useState<string[]>(["b"]);
      return <><ColumnChooser columns={[{ id: "a", label: "Alpha", hideable: false }, { id: "b", label: "Beta" }, { id: "c", label: "Gamma" }]} hidden={hidden} onHiddenChange={setHidden} defaultHidden={["b"]} /><output>{hidden.join(",")}</output></>;
    }
    render(<Harness />);
    await userEvent.click(screen.getByRole("button", { name: /Columns/ }));
    const gamma = await screen.findByRole("menuitemcheckbox", { name: "Gamma" });
    expect(screen.queryByRole("menuitemcheckbox", { name: "Alpha" })).toBeNull();
    await userEvent.click(gamma);
    expect(screen.getByText("b,c")).toBeTruthy();
  });

  it("round-trips the view through URL values", () => {
    expect(tableViewToSearch({ hidden: ["model", "cost"], density: "compact" })).toEqual({ cols: "cost,model", density: "compact" });
    expect(tableViewToSearch({ hidden: [], density: "comfortable" })).toEqual({ cols: undefined, density: undefined });
    expect(tableViewFromSearch({ cols: "cost,evil", density: "huge" }, ["name", "cost"])).toEqual({ hidden: ["cost"], density: "comfortable" });
  });
});

describe("UsageBar", () => {
  it("shows exact amounts with the period and a meter", () => {
    render(<UsageBar label="Key budget" used="12400000" limit="50000000" period="month" />);
    expect(screen.getByText("$12.40 / $50.00 · Monthly")).toBeTruthy();
    const meter = screen.getByRole("meter", { name: "Key budget" });
    expect(meter.getAttribute("aria-valuetext")).toBe("$12.40 / $50.00 · Monthly");
  });

  it("unlimited, sub-cent, zero, unknown and over limit", () => {
    const { rerender } = render(<UsageBar label="Spend" used="12400000" limit={null} />);
    expect(screen.getByText("$12.40 / ∞")).toBeTruthy();
    expect(screen.getByRole("img", { name: "Spend: $12.40, no limit" })).toBeTruthy();
    expect(screen.queryByRole("meter")).toBeNull();
    rerender(<UsageBar label="Spend" used="8100" limit="50000000" />);
    expect(screen.getByText("$0.0081 / $50.00")).toBeTruthy();
    rerender(<UsageBar label="Spend" used="0" limit="50000000" />);
    expect(screen.getByText("$0.00 / $50.00")).toBeTruthy();
    rerender(<UsageBar label="Spend" used={null} limit="50000000" />);
    expect(screen.getByText("Unknown / $50.00")).toBeTruthy();
    expect(screen.queryByRole("meter")).toBeNull();
    rerender(<UsageBar label="Spend" used="60000000" limit="50000000" />);
    expect(screen.getByRole("meter").getAttribute("aria-valuetext")).toContain("over limit");
    expect(usageText("1", undefined, "lifetime")).toBe("$0.000001 / ∞ · Lifetime");
  });
});

describe("BudgetRing", () => {
  it("is a meter whose text never rounds a tiny share to 0%", () => {
    render(<BudgetRing label="Key limit" used="8100" limit="50000000" period="month" />);
    const meter = screen.getByRole("meter", { name: "Key limit" });
    expect(meter.getAttribute("aria-valuetext")).toBe("$0.0081 of $50.00 · Monthly, <1% used");
    expect(screen.getAllByText("<1%").length).toBeGreaterThan(0);
    expect(screen.getByText("$0.0081 / $50.00 · Monthly")).toBeTruthy();
  });

  it("states unlimited, unknown, at and over limit", () => {
    const { rerender, container } = render(<BudgetRing label="Key limit" used="100" limit={null} />);
    expect(screen.getByRole("img", { name: "Key limit: $0.00010, no limit" })).toBeTruthy();
    expect(screen.getByText("∞")).toBeTruthy();
    rerender(<BudgetRing label="Key limit" used={null} limit="50000000" />);
    expect(screen.getByText("?")).toBeTruthy();
    expect(screen.getByRole("img", { name: /Unknown of \$50\.00/ })).toBeTruthy();
    rerender(<BudgetRing label="Key limit" used="50000000" limit="50000000" />);
    expect(screen.getByText("At limit")).toBeTruthy();
    rerender(<BudgetRing label="Key limit" used="75000000" limit="50000000" ringOnly />);
    expect(screen.getByRole("meter").getAttribute("aria-valuetext")).toContain("over limit");
    expect(container.querySelector("[data-level='over']")).not.toBeNull();
    expect(screen.getByRole("meter").getAttribute("aria-valuenow")).toBe("100");
  });
});

describe("PeriodPills", () => {
  it("offers Daily/Weekly/Monthly/Lifetime with one pressed and reset copy", async () => {
    const onChange = vi.fn();
    render(<PeriodPills label="Reset period" value="month" onChange={onChange} showReset />);
    const group = screen.getByRole("group", { name: "Reset period" });
    expect(within(group).getAllByRole("button").map(b => b.textContent)).toEqual(["Daily", "Weekly", "Monthly", "Lifetime"]);
    expect(screen.getByRole("button", { name: "Monthly" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText("Resets monthly on the 1st at 00:00 UTC")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Monthly" }));
    expect(onChange).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Weekly" }));
    expect(onChange).toHaveBeenCalledWith("week");
  });

  it("supports subsets, a disabled reason and the read-only badge", () => {
    render(<><PeriodPills label="Period" value="day" onChange={() => {}} periods={["day", "week"]} disabled disabledReason="Set a limit first" /><PeriodBadge period="lifetime" /></>);
    expect(screen.getAllByRole("button")).toHaveLength(2);
    expect(screen.getByText("Set a limit first")).toBeTruthy();
    expect(screen.getByText("Lifetime")).toBeTruthy();
  });
});

describe("PresetChoice", () => {
  const presets = [{ value: "", label: "No limit" }, { value: "10", label: "$10" }, { value: "50", label: "$50" }];
  function Harness({ initial, onChange }: { initial: PresetValue; onChange?: (v: PresetValue) => void }) {
    const [value, setValue] = useState(initial);
    return <PresetChoice legend="Spending limit" presets={presets} value={value} onChange={v => { setValue(v); onChange?.(v); }} custom={{ label: "Custom limit (USD)", prefix: "$", inputMode: "decimal" }} />;
  }

  it("selects presets and reveals the custom input in place", async () => {
    const onChange = vi.fn();
    render(<Harness initial={{ kind: "preset", preset: "" }} onChange={onChange} />);
    expect(screen.getByRole("radio", { name: "No limit" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.queryByLabelText("Custom limit (USD)")).toBeNull();
    await userEvent.click(screen.getByRole("radio", { name: "Custom" }));
    const input = screen.getByLabelText("Custom limit (USD)");
    await userEvent.type(input, "10");
    expect(onChange).toHaveBeenLastCalledWith({ kind: "custom", text: "10" });
    // A custom value equal to a preset stays custom.
    expect(screen.getByRole("radio", { name: "Custom" }).getAttribute("aria-checked")).toBe("true");
    expect(presetChoiceText({ kind: "custom", text: "10" })).toBe("10");
  });
});

describe("StatusPill", () => {
  it("labels every state distinctly, Disabled vs Revoked included", () => {
    const { container } = render(<>{(["active", "disabled", "revoked", "unknown", "pending"] as const).map(s => <StatusPill key={s} status={s} />)}</>);
    for (const label of ["Active", "Disabled", "Revoked", "Unknown", "Pending"]) expect(screen.getByText(label)).toBeTruthy();
    const tone = (s: string) => container.querySelector(`[data-status='${s}']`)?.getAttribute("data-tone");
    expect(tone("disabled")).not.toBe(tone("revoked"));
    expect(container.querySelector("[data-status='unknown']")?.getAttribute("data-variant")).toBe("outline");
  });

  it("normalises server values; unrecognised is unknown", () => {
    expect(toLifecycleStatus("ENABLED")).toBe("active");
    expect(toLifecycleStatus(null)).toBe("unknown");
    expect(toLifecycleStatus("weird")).toBe("unknown");
    expect(isInactiveStatus("revoked")).toBe(true);
    expect(isInactiveStatus("active")).toBe(false);
  });
});

describe("Timeline", () => {
  it("lists attempts in order with status words, markers and an honest total", () => {
    render(<Timeline label="Upstream attempts" showTotal showDurationBars items={[
      { id: "1", status: "failed", title: "Azure east", meta: "HTTP 503", durationMs: 41 },
      { id: "2", status: "success", title: "OpenAI", durationMs: 1200, marker: "Fallback" },
      { id: "3", status: "unknown", title: LONG, durationMs: null },
    ]} />);
    const list = screen.getByRole("list", { name: "Upstream attempts" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(3);
    expect(items[0]!.textContent).toContain("Step 1, Failed");
    expect(items[1]!.textContent).toContain("Fallback");
    expect(items[2]!.textContent).toContain("Unknown");
    expect(screen.getByText("Total: at least 1.2 s")).toBeTruthy();
  });

  it("empty and all-unknown states", () => {
    const { rerender } = render(<Timeline label="Attempts" items={[]} />);
    expect(screen.getByText("No attempts recorded.")).toBeTruthy();
    rerender(<Timeline label="Attempts" showTotal items={[{ id: "1", status: "pending", title: "Waiting" }]} />);
    expect(screen.getByText("Total: Unknown")).toBeTruthy();
  });
});

describe("CopyId", () => {
  it("shows a short id, keeps the full value in title/clipboard and names the button", async () => {
    const user = userEvent.setup();
    const id = "req_8f3a2b1c4d5e6f7a8b9c91c2";
    render(<CopyId value={id} label="request ID" />);
    const code = screen.getByText("req_8f3a…91c2");
    expect(code.getAttribute("title")).toBe(id);
    expect(document.body.textContent).not.toContain(id);
    await user.click(screen.getByRole("button", { name: "Copy request ID" }));
    expect(await navigator.clipboard.readText()).toBe(id);
    expect(await screen.findByText("Copied request ID to clipboard")).toBeTruthy();
  });

  it("short values in full; missing values are Unknown without a button", () => {
    expect(shortId("abc")).toBe("abc");
    const { rerender } = render(<CopyId value="abc" label="key ID" />);
    expect(screen.getByText("abc")).toBeTruthy();
    rerender(<CopyId value={null} label="key ID" />);
    expect(screen.getByText("Unknown")).toBeTruthy();
    expect(screen.queryByRole("button")).toBeNull();
  });
});

describe("LayerTable", () => {
  it("orders layers, shows unknown counts and expands details in place", async () => {
    render(<LayerTable caption="Effective limits" valueHeader="Budget" showCounts effective={{ value: "$50.00 / month", counts: { available: 10, partial: 0, unavailable: 2 } }} layers={[
      { id: "p", kind: "platform", name: "Platform", source: "Type default", value: "$500.00 / month", counts: { available: 12, partial: 0, unavailable: 0 } },
      { id: "t", kind: "team", name: "Team Research", value: "Inherited", state: "inherited", counts: { available: null, partial: null, unavailable: null } },
      { id: "k", kind: "key", name: "Key ci-runner", value: "$50.00 / month", details: <p>Model x: not in catalog</p> },
    ]} />);
    const table = screen.getByRole("table", { name: "Effective limits" });
    expect(within(table).getByRole("columnheader", { name: "Budget" })).toBeTruthy();
    expect(within(table).getByText("Layer 1 of 3:")).toBeTruthy();
    expect(within(table).getAllByText("Unknown")).toHaveLength(3);
    expect(within(table).getByText("Effective")).toBeTruthy();
    const toggle = screen.getByRole("button", { name: "Details for Key ci-runner" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await userEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(document.getElementById(toggle.getAttribute("aria-controls")!)?.textContent).toContain("not in catalog");
  });
});

describe("PriceLine", () => {
  it("shows amount with its unit and never shows unknown as free", () => {
    const { rerender } = render(<PriceLine amount="500000" unit="M tokens" />);
    expect(screen.getByText("$0.50")).toBeTruthy();
    expect(document.body.textContent).toContain("/ per M tokens");
    rerender(<PriceLine amount={null} unit="M tokens" />);
    expect(screen.getByText("Unknown · not free")).toBeTruthy();
    rerender(<PriceLine amount="0" unit="image" />);
    expect(screen.getByText("$0.00")).toBeTruthy();
    rerender(<PriceLine amount="8100" unit="1K calls" />);
    expect(screen.getByText("$0.0081")).toBeTruthy();
    rerender(<PriceLine unit="M tokens" notApplicable />);
    expect(screen.getByText("Not applicable")).toBeTruthy();
  });

  it("strikes a higher list price and renders tiers", () => {
    const { container, rerender } = render(<PriceLine amount="400000" listAmount="600000" unit="M tokens" />);
    expect(container.querySelector("s")?.textContent).toBe("List price $0.60");
    rerender(<PriceLine amount="400000" listAmount="100000" unit="M tokens" />);
    expect(container.querySelector("s")).toBeNull();
    rerender(<PriceLine unit="M tokens" tiers={[{ label: "≤128K", amount: "500000" }, { label: ">128K", amount: null }]} />);
    const items = screen.getAllByRole("listitem");
    expect(items[0]!.textContent).toContain("≤128K$0.50");
    expect(items[1]!.textContent).toContain("Unknown · not free");
  });

  it("lays out labelled lines", () => {
    render(<PriceLines items={[{ label: "Input", price: { amount: "100000", unit: "M tokens" } }, { label: "Web search", price: { amount: null, unit: "1K calls" } }]} />);
    expect(screen.getByText("Input").tagName).toBe("DT");
    expect(screen.getByText("Unknown · not free")).toBeTruthy();
  });
});

describe("FilterToolbar", () => {
  const facets: ToolbarFacet[] = [
    { id: "status", label: "Status", type: "toggle", allLabel: "All", options: [{ value: "on", label: "Enabled" }, { value: "off", label: "Disabled" }] },
    { id: "provider", label: "Provider", type: "select", multiple: true, placeholder: "Any provider", options: [{ value: "a", label: "Anthropic" }, { value: "o", label: "OpenAI" }] },
    { id: "price", label: "Input price", type: "range", prefix: "$", validate: v => /^\d+(\.\d+)?$/.test(v) ? undefined : "Enter a number" },
  ];
  function Harness({ initial = {}, q = "", onChange, onSearch }: { initial?: ToolbarValues; q?: string; onChange?: (v: ToolbarValues) => void; onSearch?: (q: string) => void }) {
    const [values, setValues] = useState(initial), [text, setText] = useState(q);
    return <FilterToolbar facets={facets} values={values} onChange={v => { setValues(v); onChange?.(v); }} counts={{ provider: { a: 3, o: 1 } }}
      search={{ label: "Search models", placeholder: "Name", value: text, onChange: next => { setText(next); onSearch?.(next); }, debounceMs: 0 }} end={<Button size="sm">Export</Button>} />;
  }

  it("is one labelled filter row: search first, facets with small labels above sm controls, actions at the end", () => {
    const { container } = render(<Harness />);
    const group = screen.getByRole("group", { name: "Filters" });
    const before = (x: Element, y: Element) => !!(x.compareDocumentPosition(y) & Node.DOCUMENT_POSITION_FOLLOWING);
    const search = within(group).getByRole("searchbox", { name: "Search models" }), exportButton = within(group).getByRole("button", { name: "Export" });
    for (const label of ["Status", "Provider", "Input price"]) {
      // A small visible label directly above its control, in the same column, between the search box and the actions.
      const el = within(group).getAllByText(label).find(n => n.tagName === "SPAN" && n.nextElementSibling)!;
      expect(el.parentElement!.contains(el.nextElementSibling)).toBe(true);
      expect(before(search, el) && before(el, exportButton)).toBe(true);
    }
    // One control height everywhere: every sized control is "sm".
    const sized = [...container.querySelectorAll("[data-size]")];
    expect(sized.length).toBeGreaterThan(3);
    for (const control of sized) expect(control.getAttribute("data-size")).toBe("sm");
    expect(screen.queryByRole("list", { name: "Active filters" })).toBeNull();
  });

  it("changes facets, commits valid ranges, and shows chips with Clear all", async () => {
    const onChange = vi.fn(), onSearch = vi.fn();
    render(<Harness onChange={onChange} onSearch={onSearch} />);
    await userEvent.click(screen.getByRole("button", { name: "Disabled" }));
    expect(onChange).toHaveBeenLastCalledWith({ status: ["off"] });
    await userEvent.type(screen.getByRole("textbox", { name: "Input price, Max" }), "abc");
    fireEvent.blur(screen.getByRole("textbox", { name: "Input price, Max" }));
    expect(screen.getByRole("alert").textContent).toBe("Enter a number");
    expect(onChange).toHaveBeenCalledTimes(1); // invalid text is never committed
    await userEvent.clear(screen.getByRole("textbox", { name: "Input price, Max" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Input price, Max" }), "5");
    fireEvent.blur(screen.getByRole("textbox", { name: "Input price, Max" }));
    expect(onChange).toHaveBeenLastCalledWith({ status: ["off"], price: ["", "5"] });
    await userEvent.type(screen.getByRole("searchbox", { name: "Search models" }), "gpt");
    expect(onSearch).toHaveBeenLastCalledWith("gpt");
    const chips = screen.getByRole("list", { name: "Active filters" });
    expect(within(chips).getByRole("button", { name: "Remove filter Input price: ≤ $5" })).toBeTruthy();
    expect(within(chips).getByRole("button", { name: "Remove filter Search: gpt" })).toBeTruthy();
    // A single-choice toggle shows its choice by being pressed, so it has no chip (like Bitop's FilterBar).
    expect(within(chips).queryByRole("button", { name: /Status/ })).toBeNull();
    await userEvent.click(within(chips).getByRole("button", { name: "Remove filter Input price: ≤ $5" }));
    expect(onChange).toHaveBeenLastCalledWith({ status: ["off"] });
    await userEvent.click(screen.getByRole("button", { name: "Clear all" }));
    expect(onChange).toHaveBeenLastCalledWith({});
    expect(onSearch).toHaveBeenLastCalledWith("");
  });

  it("collapses in place behind 'Filters (n)' on a phone: a disclosure button, never a sheet", async () => {
    const { container } = render(<Harness initial={{ status: ["on"], provider: ["a", "o"] }} q="x" />);
    const toggle = screen.getByRole("button", { name: /^Filters/ });
    expect(toggle.textContent).toBe("Filters (4 active)");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    const panel = document.getElementById(toggle.getAttribute("aria-controls")!)!;
    expect(panel.contains(screen.getByRole("group", { name: "Filters" }))).toBe(true);
    await userEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(panel.parentElement!.hasAttribute("data-open")).toBe(true);
    expect(container.querySelector("[role='dialog']")).toBeNull();
  });

  it("moves the least-used facets behind 'More filters (n)' and keeps their chips under the row", async () => {
    const onChange = vi.fn();
    function More() {
      const [values, setValues] = useState<ToolbarValues>({ provider: ["a"] });
      return <FilterToolbar facets={facets} more={["provider", "price"]} values={values} onChange={v => { setValues(v); onChange(v); }} end={<Button size="sm">Export</Button>} />;
    }
    render(<More />);
    const group = screen.getByRole("group", { name: "Filters" });
    expect(within(group).queryByText("Provider")).toBeNull();
    expect(within(group).queryByRole("textbox", { name: "Input price, Min" })).toBeNull();
    const more = within(group).getByRole("button", { name: "More filters, 1 active" });
    // The button follows the facets and comes before the page actions.
    expect(!!(within(group).getByRole("button", { name: "Disabled" }).compareDocumentPosition(more) & Node.DOCUMENT_POSITION_FOLLOWING)).toBe(true);
    expect(!!(more.compareDocumentPosition(within(group).getByRole("button", { name: "Export" })) & Node.DOCUMENT_POSITION_FOLLOWING)).toBe(true);
    expect(within(screen.getByRole("list", { name: "Active filters" })).getByRole("button", { name: "Remove filter Provider: Anthropic" })).toBeTruthy();
    await userEvent.click(more);
    const popup = await screen.findByRole("dialog", { name: "More filters" });
    expect(within(popup).getByRole("group", { name: "More filters" })).toBeTruthy();
    await userEvent.type(within(popup).getByRole("textbox", { name: "Input price, Max" }), "5");
    fireEvent.blur(within(popup).getByRole("textbox", { name: "Input price, Max" }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: ["a"], price: ["", "5"] });
    expect(screen.getByRole("button", { name: "More filters, 2 active" })).toBeTruthy();
  });

  it("keeps Grounded's metrics in its stylesheet: 640px breakpoint, xs labels, sm controls", () => {
    const css = readFileSync(resolve(__dirname, "filter-toolbar.module.css"), "utf8");
    expect(css).toContain("@media (max-width: 40rem)");
    expect(css).toMatch(/\.label \{[^}]*font-size: var\(--font-size-xs\)/);
    expect(css).toMatch(/\.search \{[^}]*flex: 1 1 12rem;[^}]*max-width: 18rem/);
    // Sort / View / Columns sit at the end of the same row (pushed right), not on a row of their own.
    expect(css).toMatch(/\.end \{[^}]*margin-inline-start: auto/);
    expect(css).not.toMatch(/position: fixed|translateX/); // no drawer
  });

  it("round-trips URL values, dropping options a facet doesn't offer", () => {
    const values = { status: ["off"], provider: ["o", "a"], price: ["0.1", ""] };
    const search = toolbarValuesToSearch(facets, values);
    expect(search).toEqual({ status: "off", provider: "a,o", price: "0.1.." });
    expect(toolbarValuesFromSearch(search, facets)).toEqual({ status: ["off"], provider: ["a", "o"], price: ["0.1", ""] });
    expect(toolbarValuesFromSearch({ status: "off,on", provider: "evil" }, facets)).toEqual({ status: ["off"] });
    expect(activeToolbarCount(facets, values, "q")).toBe(5);
    expect(rangeText("", "5", "$")).toBe("≤ $5");
    expect(rangeText("1", "5", "$")).toBe("$1 – $5");
  });
});

describe("TypeTabs", () => {
  it("renders Bitop pill tabs with counts; unknown counts show no number", async () => {
    const onChange = vi.fn();
    const { container } = render(<TypeTabs label="Model type" value="all" onChange={onChange} items={[{ value: "all", label: "All", count: 42 }, { value: "embeddings", label: "Embeddings", count: 0 }, { value: "audio", label: "Audio", count: null }]}><p>Panel body</p></TypeTabs>);
    expect(screen.getByRole("tablist", { name: "Model type" }).getAttribute("data-variant")).toBe("pills");
    expect(screen.getByRole("tab", { name: /^All,\s?42$/ }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("tab", { name: /^Embeddings,\s?0$/ })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Audio" })).toBeTruthy();
    expect(screen.getByRole("tabpanel").textContent).toBe("Panel body");
    await userEvent.click(screen.getByRole("tab", { name: "Audio" }));
    expect(onChange).toHaveBeenCalledWith("audio");
    expect(container.textContent).not.toContain("null");
  });
});

describe("SectionNav", () => {
  it("is a landmark of anchors with the current section marked; clicking focuses the section", async () => {
    render(<><SectionNav items={[{ id: "routes", label: "Routes" }, { id: "pricing", label: "Pricing" }]} /><section id="routes">R</section><section id="pricing">P</section></>);
    const nav = screen.getByRole("navigation", { name: "On this page" });
    const routes = within(nav).getByRole("link", { name: "Routes" }), pricing = within(nav).getByRole("link", { name: "Pricing" });
    expect(pricing.getAttribute("href")).toBe("#pricing");
    expect(routes.getAttribute("aria-current")).toBe("location");
    await userEvent.click(pricing);
    expect(pricing.getAttribute("aria-current")).toBe("location");
    expect(routes.getAttribute("aria-current")).toBeNull();
    expect(document.activeElement?.id).toBe("pricing");
  });
});

describe("TableDividerRow and ExpandableRow", () => {
  it("divides a table body with visible reason text", () => {
    render(<Table caption="Routes" columns={["Route", "Price"]}><TableDividerRow colSpan={2} label="Fallback only · not used by default" reason="Used only when earlier routes fail." /></Table>);
    const cell = screen.getByText("Fallback only · not used by default").closest("td")!;
    expect(cell.getAttribute("colspan")).toBe("2");
    expect(screen.getByText("Used only when earlier routes fail.")).toBeTruthy();
  });

  it("controlled expandable row", async () => {
    const onExpandedChange = vi.fn();
    const { rerender } = render(<Table caption="Keys" columns={[expandColumn, "Key"]}><ExpandableRow label="ci" colSpan={2} expanded={false} onExpandedChange={onExpandedChange} details="Detail text"><Td>ci</Td></ExpandableRow></Table>);
    expect(screen.getByRole("columnheader", { name: "Details" })).toBeTruthy();
    expect(screen.queryByText("Detail text")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Details for ci" }));
    expect(onExpandedChange).toHaveBeenCalledWith(true);
    rerender(<Table caption="Keys" columns={[expandColumn, "Key"]}><ExpandableRow label="ci" colSpan={2} expanded details="Detail text"><Td>ci</Td></ExpandableRow></Table>);
    expect(screen.getByText("Detail text").closest("td")?.getAttribute("colspan")).toBe("2");
  });
});

describe("DataPolicyBadge", () => {
  it("distinguishes keeps / doesn't keep / unknown (amber)", () => {
    const { container } = render(<><DataPolicyBadge policy="no_keep" /><DataPolicyBadge policy="keeps" detail="30-day retention" /><DataPolicyBadge policy="unknown" /></>);
    expect(screen.getByText("Doesn't keep data")).toBeTruthy();
    expect(screen.getByText("· 30-day retention")).toBeTruthy();
    expect(container.querySelector("[data-policy='unknown']")?.getAttribute("data-tone")).toBe("warning");
    expect(toDataPolicy(null)).toBe("unknown");
    expect(toDataPolicy("ZDR")).toBe("no_keep");
  });
});

describe("PrevNext", () => {
  it("names targets, disables a missing side and supports j/k outside fields", async () => {
    const onNext = vi.fn();
    render(<><PrevNext noun="request" position={{ index: 3, total: 10 }} shortcuts prev={null} next={{ label: "req_2", onSelect: onNext }} /><input aria-label="Search" /></>);
    expect(screen.getByRole("navigation", { name: "Request navigation" })).toBeTruthy();
    expect(screen.getByText("4 / 10")).toBeTruthy();
    expect((screen.getByRole("button", { name: "No previous request" }) as HTMLButtonElement).disabled).toBe(true);
    const next = screen.getByRole("button", { name: "Next request: req_2" });
    expect(next.getAttribute("aria-keyshortcuts")).toBe("j");
    await userEvent.keyboard("j");
    expect(onNext).toHaveBeenCalledOnce();
    await userEvent.click(screen.getByLabelText("Search"));
    await userEvent.keyboard("j");
    expect(onNext).toHaveBeenCalledOnce();
  });

  it("renders links and an unknown total", () => {
    render(<PrevNext noun="route" position={{ index: 0, total: null }} prev={{ label: "A", href: "/routes/a" }} next={{ label: "B", href: "/routes/b" }} />);
    expect(screen.getByRole("link", { name: "Previous route: A" }).getAttribute("href")).toBe("/routes/a");
    expect(screen.getByText("1 / ?")).toBeTruthy();
  });
});

describe("InfoBanner and DangerZone", () => {
  it("reuses Bitop Alert and Card", () => {
    render(<>
      <InfoBanner title="Read-only">Personal limits are set by the platform.</InfoBanner>
      <DangerZone>
        <DangerAction title="Disable key" description="Requests fail until re-enabled." action={<Button variant="danger">Disable key</Button>} />
        <DangerAction title="Enable key" description="Not available." disabledReason="Revoked keys can't be re-enabled." action={<Button variant="secondary" disabled>Enable key</Button>} />
      </DangerZone>
    </>);
    expect(screen.getByRole("status").textContent).toContain("Personal limits are set by the platform.");
    const zone = screen.getByRole("region", { name: "Danger zone" });
    expect(within(zone).getAllByRole("heading", { level: 3 })).toHaveLength(2);
    expect(within(zone).getByText("Revoked keys can't be re-enabled.")).toBeTruthy();
  });
});

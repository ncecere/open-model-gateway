// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { WhyUnavailable } from "./effective-access";
import { reasonLabel, type AccessModel } from "../lib/effective-access";

afterEach(() => cleanup());

const models: AccessModel[] = [
  { model_id: "a", public_name: "company/a", display_name: "Model A", status: "unavailable", reasons: [{ code: "some_routes_unavailable", layer: "platform" }, { code: "not_in_catalog", layer: "workspace" }, { code: "budget_exhausted", layer: "workspace", period: "month" }] },
  { model_id: "b", public_name: "company/b", display_name: "Model B", status: "unavailable", reasons: [{ code: "key_restriction", layer: "key" }] },
  { model_id: "c", public_name: "company/c", display_name: "Model C", status: "available", reasons: [] },
];

describe("Why can't I use this model?", () => {
  it("shows the first reason in plain words inline, with +N more opening the row", async () => {
    const user = userEvent.setup();
    render(<WhyUnavailable models={models} kind="team" />);
    const table = screen.getByRole("table", { name: "Why can't I use this model?" });
    const rowA = within(table).getByRole("rowheader", { name: /Model A/ }).closest("tr")!;
    // Blocking reasons first; the count text is gone.
    expect(rowA.textContent).toContain("Not in an available catalog");
    expect(rowA.textContent).not.toMatch(/\d reasons?/);
    expect(rowA.textContent).not.toContain("Some routes turned off");
    const more = within(rowA).getByRole("button", { name: "Show 2 more reasons for Model A" });
    expect(more.textContent).toBe("+2 more");
    expect(more.getAttribute("aria-expanded")).toBe("false");
    // One reason: no "+N more".
    const rowB = within(table).getByRole("rowheader", { name: /Model B/ }).closest("tr")!;
    expect(rowB.textContent).toContain("Key limited to other models");
    expect(within(rowB).queryByRole("button", { name: /more reason/ })).toBeNull();
    await user.click(more);
    const list = screen.getByRole("list", { name: "Reasons for Model A" });
    expect(within(list).getAllByRole("listitem")).toHaveLength(3);
    expect(list.textContent).toContain("monthly budget of this workspace is used up");
    expect(within(rowA).getByRole("button", { name: "Details for Model A" }).getAttribute("aria-expanded")).toBe("true");
    await user.click(within(rowA).getByRole("button", { name: "Hide 2 more reasons for Model A" }));
    expect(screen.queryByRole("list", { name: "Reasons for Model A" })).toBeNull();
  });
  it("labels every known reason briefly and keeps unknown codes visible", () => {
    expect(reasonLabel({ code: "not_selected", layer: "workspace" }, "project")).toBe("Not added to this project");
    expect(reasonLabel({ code: "workspace_disabled", layer: "platform" }, "personal")).toBe("This workspace is disabled");
    expect(reasonLabel({ code: "budget_exhausted", layer: "key", period: "day" }, "team")).toBe("Daily budget used up");
    expect(reasonLabel({ code: "brand_new", layer: "key" }, "team")).toBe("Unavailable (brand_new)");
  });
});

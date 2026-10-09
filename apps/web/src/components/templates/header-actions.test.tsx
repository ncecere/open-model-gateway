// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Plus } from "lucide-react";
import { Button } from "../ui/button/button";
import { ActionMenu } from "./action-menu";
import { PrevNext } from "./prev-next";
import { PageHeader } from "./page-header";
import { HEADER_COLLAPSE_QUERY, collapseActions } from "./header-actions";
import { TypeTabs } from "./type-tabs";
import { Heading } from "../ui";

/** A viewport `width` px wide: matchMedia answers the collapse query like a browser would. */
function viewport(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: (query: string) => ({ matches: query === HEADER_COLLAPSE_QUERY && width <= 640, media: query, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
}

beforeEach(() => {
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const three = (onCreate = () => {}, onDelete = () => {}) => <>
  <Button variant="secondary" render={<a href="/settings" />}>Settings</Button>
  <Button onClick={onCreate}><Plus aria-hidden />Create key</Button>
  <Button variant="danger" onClick={onDelete}>Delete workspace</Button>
</>;

describe("HeaderActions (mobile header: primary + ⋯)", () => {
  it("collapses 2+ actions to the primary and a ⋯ menu at 390px; destructive items stay red and last", async () => {
    viewport(390);
    const user = userEvent.setup(), create = vi.fn(), remove = vi.fn();
    render(<Heading title="Product" actions={three(create, remove)} />);
    const visible = screen.getAllByRole("button").map(b => b.getAttribute("aria-label") ?? b.textContent);
    expect(visible).toEqual(["Create key", "More actions"]);
    expect(screen.queryByRole("link", { name: "Settings" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Create key" }));
    expect(create).toHaveBeenCalledOnce();
    await user.click(screen.getByRole("button", { name: "More actions" }));
    const items = await screen.findAllByRole("menuitem");
    expect(items.map(i => i.textContent)).toEqual(["Settings", "Delete workspace"]);
    expect(items[0]!.getAttribute("href")).toBe("/settings");
    expect(items[1]!.className).toMatch(/itemDanger/);
    expect(items[0]!.className).not.toMatch(/itemDanger/);
    await user.click(items[1]!);
    expect(remove).toHaveBeenCalledOnce();
  });

  it("keeps the desktop header unchanged", () => {
    viewport(1440);
    render(<Heading title="Product" actions={three()} />);
    expect(screen.getByRole("link", { name: "Settings" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Create key" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Delete workspace" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "More actions" })).toBeNull();
  });

  it("leaves a single action alone on a phone", () => {
    viewport(390);
    render(<Heading title="Models" actions={<Button><Plus aria-hidden />Add model</Button>} />);
    expect(screen.getByRole("button", { name: "Add model" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "More actions" })).toBeNull();
  });

  it("merges an existing ⋯ menu into one, keeps record navigation inline and carries disabled reasons", async () => {
    viewport(390);
    const user = userEvent.setup(), retire = vi.fn();
    render(<PageHeader title="gpt-6-luna" actions={<>
      <PrevNext noun="route" />
      <Button variant="secondary" disabled title="Add a model first">Assign</Button>
      <Button variant="secondary">Disable</Button>
      <ActionMenu label="Actions for gpt-6-luna" actions={[{ label: "Retire resource…", danger: true, onSelect: retire }, { label: "Hidden", hidden: true }]} />
    </>} />);
    expect(screen.getByRole("navigation", { name: "Route navigation" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "Actions for gpt-6-luna" })).toBeNull();
    // No primary-variant button: the first one leads, the rest go in the menu.
    expect(screen.getByRole("button", { name: "Assign" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "Disable" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "More actions" }));
    const items = await screen.findAllByRole("menuitem");
    expect(items.map(i => i.textContent)).toEqual(["Disable", "Retire resource…"]);
    expect(items[1]!.className).toMatch(/itemDanger/);
    await user.click(items[1]!);
    expect(retire).toHaveBeenCalledOnce();
  });

  it("turns type tabs into one scrolling strip on a phone, wrapping rows on desktop", () => {
    const items = ["All", "Text", "Embeddings", "Images", "Speech to text", "Text to speech", "Rerank", "System One"].map((label, i) => ({ value: label, label, count: i + 1 }));
    viewport(390);
    render(<TypeTabs label="Model type" items={items} value="All" onChange={() => {}} end="14 models" />);
    expect(screen.getByRole("tablist", { name: "Model type" }).getAttribute("data-overflow-mode")).toBe("scroll");
    cleanup();
    viewport(1440);
    render(<TypeTabs label="Model type" items={items} value="All" onChange={() => {}} />);
    expect(screen.getByRole("tablist", { name: "Model type" }).getAttribute("data-overflow-mode")).toBe("wrap");
  });

  it("plans the collapse from fragments and skips empty slots", () => {
    const plan = collapseActions(<>{false}{null}<><Button variant="secondary">A</Button></><Button type="submit">Save</Button><Button>B</Button></>);
    expect(plan?.primary?.props).toMatchObject({ children: "B" });
    expect(plan?.more.map(i => i.label)).toEqual(["A"]);
    // A submit button can't be a menu item: it stays inline.
    expect(plan?.inline).toHaveLength(1);
    expect(collapseActions(<><Button>Only</Button>{false}</>)).toBeNull();
    const reason = collapseActions(<><Button disabled title="Add a model first">Create key</Button><Button variant="secondary">Settings</Button></>);
    expect(reason?.primary?.props).toMatchObject({ children: "Create key" });
    expect(reason?.more).toEqual([expect.objectContaining({ label: "Settings", disabled: false })]);
    const disabled = collapseActions(<><Button>Go</Button><Button variant="secondary" disabled title="Loading">Assign</Button></>);
    expect(disabled?.more[0]).toMatchObject({ label: "Assign", disabled: true, disabledReason: "Loading" });
  });
});

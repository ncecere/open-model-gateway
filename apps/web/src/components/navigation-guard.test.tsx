// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DiscardChangesDialog, discardCopy } from "./navigation-guard";

afterEach(() => cleanup());

describe("Unsaved changes confirmation", () => {
  it("uses one wording: Discard unsaved changes? / Keep editing / Discard", async () => {
    Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
    const user = userEvent.setup(), discard = vi.fn(), change = vi.fn();
    render(<DiscardChangesDialog open onOpenChange={change} onDiscard={discard} />);
    const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
    expect(dialog.textContent).toContain("Your changes haven't been saved.");
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(change).toHaveBeenCalledWith(false); expect(discard).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(discard).toHaveBeenCalledOnce();
  });
  it("is the only unsaved-changes wording in the app (no older variants left)", () => {
    expect(discardCopy).toMatchObject({ title: "Discard unsaved changes?", confirm: "Discard", cancel: "Keep editing" });
    const sources = import.meta.glob(["../**/*.tsx", "!../**/*.test.tsx"], { query: "?raw", import: "default", eager: true }) as Record<string, string>;
    expect(Object.keys(sources).length).toBeGreaterThan(20);
    const stale = Object.entries(sources).filter(([, text]) => /Leave without saving|Discard changes"|Switch tabs without saving|Discard and switch/.test(text)).map(([path]) => path);
    expect(stale).toEqual([]);
  });
});

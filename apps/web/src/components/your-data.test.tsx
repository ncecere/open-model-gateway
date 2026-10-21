// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { YourData } from "./your-data";
import { downloadExport, rangeDays } from "../lib/subject-export";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("Your data (#74)", () => {
  it("counts inclusive days", () => {
    expect(rangeDays("2026-10-01", "2026-10-01")).toBe(1);
    expect(rangeDays("2026-07-14", "2026-10-14")).toBe(93);
    expect(rangeDays("bad", "2026-10-14")).toBeNaN();
  });
  it("lays the dates out in the shared Stack/Inline layout, not as bare siblings (#189)", () => {
    render(<YourData />);
    const fromField = screen.getByLabelText("From").parentElement as HTMLElement;
    const toField = screen.getByLabelText("To").parentElement as HTMLElement;
    const button = screen.getByRole("button", { name: /Download your data/ });
    const dates = fromField.parentElement as HTMLElement;
    expect(toField.parentElement).toBe(dates);
    expect(dates.className).toMatch(/inline/);
    const stack = dates.parentElement as HTMLElement;
    expect(stack.className).toMatch(/stack/);
    expect(button.parentElement).toBe(stack);
  });
  it("refuses a range over 93 days before asking the server", () => {
    render(<YourData />);
    const [from] = screen.getAllByLabelText(/From/);
    fireEvent.change(from, { target: { value: "2026-01-01" } });
    expect(screen.getByText(/Choose up to 93 days/)).toBeTruthy();
    expect((screen.getByRole("button", { name: /Download your data/ }) as HTMLButtonElement).disabled).toBe(true);
  });
  it("downloads only same-origin JSON and explains the hourly limit", async () => {
    await expect(downloadExport("https://evil.example/x", "x.json")).rejects.toThrow("Invalid export path");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 429 })));
    await expect(downloadExport("/api/v1/me/export", "x.json")).rejects.toThrow("3 times an hour");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("<html>", { status: 200, headers: { "content-type": "text/html" } })));
    await expect(downloadExport("/api/v1/me/export", "x.json")).rejects.toThrow("did not return an export");
  });
});

// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SettingsForm } from "./templates/settings-form";

beforeEach(() => {
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
describe("authoritative settings refresh", () => {
  const fields = (value: string) => [{ name: "amount", label: "Amount", value }];
  const client = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });
  it("refreshes pristine controls after reset/invalidation", async () => {
    const c = client();
    const view = (value: string) => <QueryClientProvider client={c}><SettingsForm fields={fields(value)} writable onSave={vi.fn()} /></QueryClientProvider>;
    const { rerender } = render(view("12.34"));
    rerender(view("100.00"));
    await waitFor(() => expect((screen.getByLabelText("Amount") as HTMLInputElement).value).toBe("100.00"));
    expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
  });
  it("preserves an edited draft but discard adopts the latest server values", async () => {
    const c = client();
    const view = (value: string) => <QueryClientProvider client={c}><SettingsForm fields={fields(value)} writable onSave={vi.fn()} /></QueryClientProvider>;
    const { rerender } = render(view("12.34"));
    fireEvent.change(screen.getByLabelText("Amount"), { target: { value: "11.00" } });
    rerender(view("100.00"));
    expect((screen.getByLabelText("Amount") as HTMLInputElement).value).toBe("11.00");
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect((screen.getByLabelText("Amount") as HTMLInputElement).value).toBe("100.00"));
  });
  it("refreshes readonly controls without exposing submission", async () => {
    const c = client();
    const view = (value: string) => <QueryClientProvider client={c}><SettingsForm fields={fields(value)} writable={false} onSave={vi.fn()} /></QueryClientProvider>;
    const { rerender } = render(view("12.34"));
    rerender(view("100.00"));
    // Read-only settings are a description list, not disabled inputs (review rule 12).
    await waitFor(() => expect(screen.getByText("100.00")).toBeTruthy());
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("button", { name: /Save/ })).toBeNull();
  });
});

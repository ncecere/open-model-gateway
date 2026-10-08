// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { ActionProvider, Button, useAction, type Action } from "./ui";
import { SettingsForm } from "./templates/settings-form";
import { type Field } from "../lib/forms";
import { priceFields } from "../lib/governance";
import { testClient } from "../lib/test-fixtures";

function Open({ action }: { action: Action }) {
  const ask = useAction();
  return <Button onClick={() => ask(action)}>Open action</Button>;
}
const modes = ["dialog", "settings"] as const;
type Mode = typeof modes[number];
async function mountForm(mode: Mode, fields: Field[], run = vi.fn().mockResolvedValue({ ok: true }), writable = true) {
  const user = userEvent.setup(), client = testClient();
  const result = render(<QueryClientProvider client={client}><ActionProvider>{mode === "dialog"
    ? <Open action={{ title: "Publish version", submitLabel: "Submit", fields, run }} />
    : <SettingsForm fields={fields} writable={writable} onSave={run} />}</ActionProvider></QueryClientProvider>);
  if (mode === "dialog") await user.click(screen.getByRole("button", { name: "Open action" }));
  return { user, run, client, ...result };
}
function submitButton(mode: Mode) {
  return screen.getByRole("button", { name: mode === "dialog" ? "Submit" : "Save settings" });
}
async function expectSaved(mode: Mode) {
  if (mode === "dialog") await waitFor(() => expect(screen.queryByRole("dialog", { name: "Publish version" })).toBeNull());
  else await screen.findByText("Settings saved.");
}
beforeEach(() => {
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe.each(modes)("%s controlled validation retry", mode => {
  it("submits once after correcting a custom pricing error despite native validity being true", async () => {
    const values = { input_usd_per_million: "0.05", output_usd_per_million: "0.20", input_token_limit: "100000", output_token_limit: "10000", cache_read_status: "priced", cache_read_usd: "0.01", cache_write_status: "priced", cache_write_usd: "0.20", cache_write_5m_status: "priced", cache_write_5m_usd: "0.20", cache_write_1h_status: "priced", cache_write_1h_usd: "0.40" };
    const fields = priceFields().map(field => ({ ...field, value: values[field.name as keyof typeof values] ?? field.value }));
    const { user, run, client } = await mountForm(mode, fields);
    const input = screen.getByRole("textbox", { name: /^Input rate/ }) as HTMLInputElement;
    await user.clear(input); await user.type(input, "0.1000001");
    await user.click(submitButton(mode));
    expect(run).not.toHaveBeenCalled();
    expect(input.validity.valid).toBe(true);
    expect(input.getAttribute("aria-invalid")).toBe("true");
    await screen.findByText(/Enter a non-negative USD amount with up to 6 decimal places/);
    await user.clear(input); await user.type(input, "0.10");
    expect(input.validity.valid).toBe(true);
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledOnce());
    expect(run.mock.calls[0][0]).toMatchObject({ ...values, input_usd_per_million: "0.10" });
    await expectSaved(mode);
    client.clear();
  });

  it.each([
    { type: "textarea" as const, bad: "bad", good: "valid" },
    { type: "select" as const, bad: "bad", good: "valid" },
    { type: "number" as const, bad: "1", good: "2" },
  ])("retries custom validation through native $type edits", async ({ type, bad, good }) => {
    const field: Field = { name: "value", label: "Value", type, value: bad, options: [{ value: bad, label: "Bad" }, { value: good, label: "Good" }], validate: value => value === good ? undefined : "Choose a valid value." };
    const { user, run, client } = await mountForm(mode, [field, { name: "note", label: "Note", value: "before" }]);
    await user.type(screen.getByRole("textbox", { name: /^Note/ }), " changed");
    await user.click(submitButton(mode));
    await screen.findByText("Choose a valid value.");
    expect(run).not.toHaveBeenCalled();
    const input = screen.getByRole(type === "select" ? "combobox" : type === "number" ? "spinbutton" : "textbox", { name: /^Value/ });
    if (type === "select") await user.selectOptions(input, good);
    else { await user.clear(input); await user.type(input, good); }
    expect(input.getAttribute("aria-invalid")).not.toBe("true");
    expect(screen.queryByText("Choose a valid value.")).toBeNull();
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledOnce());
    expect(run.mock.calls[0][0].value).toBe(good);
    await expectSaved(mode); client.clear();
  });

  it("retries validation through real checkbox-group edits", async () => {
    const { user, run, client } = await mountForm(mode, [
      { name: "models", label: "Models", type: "checkboxes", required: true, options: [{ value: "model", label: "Allow inference" }] },
      { name: "note", label: "Note", value: "before" },
    ]);
    await user.type(screen.getByRole("textbox", { name: /^Note/ }), " changed");
    await user.click(submitButton(mode));
    await screen.findByText("Select at least one option for models.");
    expect(run).not.toHaveBeenCalled();
    await user.click(screen.getByRole("checkbox", { name: "Allow inference" }));
    expect(screen.queryByText("Select at least one option for models.")).toBeNull();
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledOnce());
    expect(run.mock.calls[0][0].models).toBe('["model"]');
    await expectSaved(mode); client.clear();
  });

  it("retries custom validation through the real date picker", async () => {
    const { user, run, client } = await mountForm(mode, [
      { name: "day", label: "Start date", type: "date", value: "2026-01-01", validate: value => value === "2026-01-02" ? undefined : "Choose a later day." },
      { name: "note", label: "Note", value: "before" },
    ]);
    await user.type(screen.getByRole("textbox", { name: /^Note/ }), " changed");
    await user.click(submitButton(mode));
    await screen.findByText("Choose a later day.");
    expect(run).not.toHaveBeenCalled();
    const trigger = screen.getByRole("button", { name: /^Start date/ });
    expect(trigger.getAttribute("aria-invalid")).toBe("true");
    await user.click(trigger);
    const calendar = await screen.findByRole("dialog", { name: "Choose date" });
    await user.click(calendar.querySelector<HTMLButtonElement>('[data-date="2026-01-02"]')!);
    expect(screen.queryByText("Choose a later day.")).toBeNull();
    expect(trigger.getAttribute("aria-invalid")).not.toBe("true");
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledOnce());
    expect(run.mock.calls[0][0].day).toBe("2026-01-02");
    await expectSaved(mode); client.clear();
  });

  it("revalidates after edits and retains generic request errors until a valid retry", async () => {
    const run = vi.fn().mockRejectedValueOnce(new Error("Pinned scope changed")).mockResolvedValue({ ok: true });
    const field: Field = { name: "name", label: "Name", value: "before", validate: value => value === "bad" ? "Invalid name." : undefined };
    const { user, client } = await mountForm(mode, [field], run);
    const input = screen.getByRole("textbox", { name: /^Name/ });
    await user.clear(input); await user.type(input, "valid");
    await user.click(submitButton(mode));
    await screen.findByText("Pinned scope changed");
    await user.clear(input); await user.type(input, "bad");
    expect(screen.getByText("Pinned scope changed")).toBeDefined();
    await user.click(submitButton(mode));
    await screen.findByText("Invalid name.");
    expect(run).toHaveBeenCalledOnce();
    // A no-op event must not erase the controlled error.
    fireEvent.change(input, { target: { value: "bad" } });
    expect(screen.getByText("Invalid name.")).toBeDefined();
    await user.type(input, " again");
    expect(screen.queryByText("Invalid name.")).toBeNull();
    expect(screen.getByText("Pinned scope changed")).toBeDefined();
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledTimes(2));
    await expectSaved(mode); client.clear();
  });

  it("clears stale dependent-field errors on edits and validates dependencies again on retry", async () => {
    const { user, run, client } = await mountForm(mode, [
      { name: "mode", label: "Mode", type: "select", value: "limited", options: [{ value: "limited", label: "Limited" }, { value: "open", label: "Open" }] },
      { name: "cap", label: "Cap", value: "100", validate: (_, values) => values.mode === "limited" ? "Cap exceeds selected mode." : undefined },
      { name: "note", label: "Note", value: "before" },
    ]);
    await user.type(screen.getByRole("textbox", { name: /^Note/ }), " changed");
    await user.click(submitButton(mode));
    await screen.findByText("Cap exceeds selected mode.");
    await user.type(screen.getByRole("textbox", { name: /^Note/ }), " more");
    expect(screen.queryByText("Cap exceeds selected mode.")).toBeNull();
    await user.click(submitButton(mode));
    await screen.findByText("Cap exceeds selected mode.");
    expect(run).not.toHaveBeenCalled();
    await user.selectOptions(screen.getByRole("combobox", { name: /^Mode/ }), "open");
    await user.click(submitButton(mode));
    await waitFor(() => expect(run).toHaveBeenCalledOnce());
    await expectSaved(mode); client.clear();
  });

  it("suppresses double submission after recovering from validation", async () => {
    let resolve!: (value: unknown) => void;
    const run = vi.fn(() => new Promise(result => { resolve = result; }));
    const { user, client } = await mountForm(mode, [{ name: "value", label: "Value", value: "before", validate: value => value === "bad" ? "Invalid value." : undefined }], run);
    const input = screen.getByRole("textbox", { name: /^Value/ });
    await user.clear(input); await user.type(input, "bad");
    await user.click(submitButton(mode));
    await screen.findByText("Invalid value.");
    await user.clear(input); await user.type(input, "valid");
    await user.dblClick(submitButton(mode));
    expect(run).toHaveBeenCalledOnce();
    expect((input as HTMLInputElement).disabled).toBe(true);
    resolve({ ok: true });
    await expectSaved(mode); client.clear();
  });
});

it("renders readonly settings as values, with no controls or saving", async () => {
  const { run, client } = await mountForm("settings", [
    { name: "name", label: "Name", value: "before" },
    { name: "models", label: "Models", type: "checkboxes", options: [{ value: "model", label: "Allow inference" }] },
    { name: "day", label: "Start date", type: "date", value: "2026-01-01" },
  ], undefined, false);
  expect(screen.getByText("before")).toBeTruthy(); expect(screen.getByText("None")).toBeTruthy(); expect(screen.getByText("2026-01-01")).toBeTruthy();
  expect(screen.queryByRole("textbox")).toBeNull(); expect(screen.queryByRole("checkbox")).toBeNull();
  expect(screen.queryByRole("button", { name: /^Start date/ })).toBeNull();
  expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
  expect(run).not.toHaveBeenCalled(); client.clear();
});

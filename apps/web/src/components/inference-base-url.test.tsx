// @vitest-environment jsdom
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, expect, it, vi } from "vitest";
import { ApiScopeProvider } from "./ui";
import { session, team, testClient } from "../lib/test-fixtures";
import { CreateKeyDialog } from "../pages/keys";
import { ProtocolsPanel } from "../pages/model-page";
import { WorkspaceModelPage } from "../pages/workspace-model";
import { KeyDetail } from "../pages/key-detail";
import type { Session } from "../lib/api";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); document.cookie = "omg_csrf=; Max-Age=0; path=/"; });

it.each(["https://api.bitop.dev/v1", null])("shows and copies the effective URL in the key-created dialog (%s)", async configured => {
  const user = userEvent.setup(), client = testClient();
  const actor: Session = { ...session, installation: { ...session.installation, inference_base_url: configured } };
  const fetch = vi.fn().mockImplementation((_path: string, init?: RequestInit) => Promise.resolve(Response.json(init?.method === "POST" ? { id: "key", token: "synthetic-key" } : { policy: null, effective: null, provenance: null })));
  vi.stubGlobal("fetch", fetch); document.cookie = "omg_csrf=test; path=/";
  render(<QueryClientProvider client={client}><ApiScopeProvider session={actor}><CreateKeyDialog session={actor} workspace={team} options={[]} accounts={[]} onClose={() => {}} /></ApiScopeProvider></QueryClientProvider>);
  await user.type(await screen.findByRole("textbox", { name: "Name" }), "CLI");
  await user.click(screen.getByRole("button", { name: "Create key" }));
  await screen.findByRole("heading", { name: "Save your API key" });
  const base = configured ?? `${window.location.origin}/v1`;
  expect(screen.getByText(base)).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "Copy OpenAI-compatible base URL" }));
  expect(await navigator.clipboard.readText()).toBe(base);
  expect(fetch.mock.calls.every(([path]) => String(path).startsWith("/api/v1/"))).toBe(true);
  client.clear();
});

it.each(["https://api.bitop.dev:8443/prefix/v1/blue/v1", null])("shows SDK base URLs on key details for a regular user (%s)", async configured => {
  const user = userEvent.setup(), client = testClient();
  const actor: Session = { ...session, capabilities: { ...session.capabilities, platform_write: false, platform_read: false }, installation: { ...session.installation, inference_base_url: configured } };
  const scope = JSON.stringify(actor);
  client.setQueryData(["api", scope, "/api/v1/workspaces/team/keys/key"], { id: "key", name: "CLI", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null });
  render(<QueryClientProvider client={client}><ApiScopeProvider session={actor}><KeyDetail session={actor} workspace={team} id="key" /></ApiScopeProvider></QueryClientProvider>);
  expect(screen.getByRole("heading", { name: "Calling the API" })).toBeTruthy();
  const base = configured ?? `${window.location.origin}/v1`;
  await user.click(screen.getByRole("button", { name: "Copy OpenAI-compatible base URL" }));
  expect(await navigator.clipboard.readText()).toBe(base);
  await user.click(screen.getByRole("button", { name: "Copy Anthropic SDK base URL" }));
  expect(await navigator.clipboard.readText()).toBe(base.replace(/\/v1$/, ""));
  expect(screen.getByText(/Anthropic SDK appends/)).toBeTruthy();
  client.clear();
});

it.each(["https://api.bitop.dev:8443/prefix@blue/v1", "http://127.0.0.1:18187/prefix/v1", null])("copies protocol-aware endpoints on both model pages (%s)", async configured => {
  const user = userEvent.setup(), client = testClient();
  const actor: Session = { ...session, installation: { ...session.installation, inference_base_url: configured } };
  client.setQueryData(["api", JSON.stringify(actor), "/api/v1/workspaces/team/catalog", "choices"], [{ model_id: "m", public_name: "smart", display_name: "Smart", workload: "generation", protocols: ["chat_completions", "batches", "realtime"], eligibility: "direct", routes: "1" }]);
  render(<QueryClientProvider client={client}><ApiScopeProvider session={actor}><ProtocolsPanel protocols={["chat_completions", "batches", "realtime"]} routes={[]} /><WorkspaceModelPage session={actor} workspace={team} id="m" /></ApiScopeProvider></QueryClientProvider>);
  const base = configured ?? `${window.location.origin}/v1`;
  expect(screen.getAllByText(`${base}/chat/completions`).length).toBe(2);
  const realtimeSections = screen.getAllByRole("region", { name: "Realtime audio" });
  expect(realtimeSections).toHaveLength(2);
  for (const section of realtimeSections) {
    await user.click(within(section).getByRole("button", { name: "Copy Realtime audio endpoint" }));
    expect(await navigator.clipboard.readText()).toBe(`${base.replace(/^https:/, "wss:").replace(/^http:/, "ws:")}/realtime?model=<API model name>`);
  }
  const chatSections = screen.getAllByRole("region", { name: "Chat Completions" });
  expect(chatSections).toHaveLength(2);
  for (const section of chatSections) {
    const maximum = within(section).getByText("max_completion_tokens").closest("li");
    expect(maximum?.textContent).toContain("Optional; defaults to the pinned price's output ceiling");
    expect(maximum?.textContent).toMatch(/max_tokens.*accepted alias/);
    expect(maximum?.textContent).toMatch(/if both are supplied, values must be equal.*400/);
    expect(section.textContent).not.toContain("legacy max_tokens");
    await user.click(within(section).getByRole("button", { name: "Copy Chat Completions endpoint" }));
    expect(await navigator.clipboard.readText()).toBe(`${base}/chat/completions`);
  }
  const batchSections = screen.getAllByRole("region", { name: "Batch chat completions" });
  expect(batchSections).toHaveLength(2);
  for (const section of batchSections) {
    await user.click(within(section).getByRole("button", { name: "Copy Files endpoint" }));
    expect(await navigator.clipboard.readText()).toBe(`${base}/files`);
    await user.click(within(section).getByRole("button", { name: "Copy Batch chat completions endpoint" }));
    expect(await navigator.clipboard.readText()).toBe(`${base}/batches`);
    expect(section.textContent).toContain("purpose=batch");
  }
  expect(document.body.textContent).toContain("endpoint must be the literal relative path /v1/chat/completions; completion_window is 24h only");
  client.clear();
});

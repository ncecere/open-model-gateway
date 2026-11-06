// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests } from "../lib/api";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { WorkspaceTemplateDetail, WorkspaceTemplatesPage } from "../pages/workspace-templates";
import { templatePath, templatesPath, type TemplateVersion, type WorkspaceTemplate } from "../lib/workspace-templates";
import { admin, auditor, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const content = { kind: "team" as const, catalogs: { mode: "inherit" as const }, model_ids: [], limits: null, owner: { name: "lead" }, slots: [{ name: "crew", role: "member" as const, required: false, multiple: true }] };
const version = (patch: Partial<TemplateVersion>): TemplateVersion => ({ version: 1, state: "pending_review", content, content_hash: "a".repeat(64), author: "other-admin", created_at: "2026-11-01T00:00:00Z", reviewed_by: null, reviewed_at: null, published_at: null, retired_at: null, ...patch });
const template = (versions: TemplateVersion[]): WorkspaceTemplate => ({ id: "t1", name: "Team starter", created_by: "other-admin", created_at: "2026-11-01T00:00:00Z", versions });
function serve(routes: Record<string, (init?: RequestInit) => unknown>) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    const key = Object.keys(routes).find(r => path === r || path.startsWith(`${r}?`));
    if (key) return Promise.resolve(Response.json(routes[key]!(init)));
    return Promise.reject(new Error(`Unexpected ${init?.method ?? "GET"} ${path}`));
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}
const writes = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === "POST").map(([path, init]) => [path, JSON.parse(String((init as RequestInit).body ?? "null"))]);
function mount(node: React.ReactElement, navigate = vi.fn()) {
  render(<QueryClientProvider client={testClient()}><DashboardNavigationProvider search={{ page: "workspace-templates" }} navigate={navigate}><ActionProvider>{node}</ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
  return { user: userEvent.setup(), navigate };
}

describe("Workspace templates (#84)", () => {
  it("lists templates and creates one with the content a Platform Admin writes", async () => {
    const fetch = serve({ [templatesPath]: init => init?.method === "POST" ? template([version({})]) : { data: [{ id: "t1", name: "Team starter", created_at: "2026-11-01T00:00:00Z", latest_version: 2, published_version: 1 }] } });
    const { user } = mount(<WorkspaceTemplatesPage session={admin} />);
    await screen.findByText("Team starter");
    expect(screen.getByText("Version 1")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /Create template/ }));
    const dialog = await screen.findByRole("dialog", { name: "Create a workspace template" });
    await user.type(within(dialog).getByRole("textbox", { name: "Name" }), "Project starter");
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));
    await waitFor(() => expect(writes(fetch)).toHaveLength(1));
    const [path, body] = writes(fetch)[0]!;
    expect(path).toBe(templatesPath);
    expect(body.name).toBe("Project starter");
    expect(body.content.owner.name).toBeTruthy();
  });
  it("is read-only for an Auditor", async () => {
    serve({ [templatesPath]: () => ({ data: [] }) });
    mount(<WorkspaceTemplatesPage session={auditor} />);
    await screen.findByText("No templates yet.");
    expect(screen.queryByRole("button", { name: /Create template/ })).toBeNull();
  });
  it("offers each version only the steps the viewer can take; the author cannot approve", async () => {
    serve({ [templatePath("t1")]: () => template([version({ version: 2, author: "me" }), version({ version: 1, state: "published", reviewed_by: "x", reviewed_at: "2026-11-02T00:00:00Z", published_at: "2026-11-02T00:00:00Z" })]) });
    const { user } = mount(<WorkspaceTemplateDetail session={admin} id="t1" />);
    await screen.findByText("Waiting for review");
    expect(screen.getByText("Published")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Actions for Version 2" }));
    const approve = await screen.findByRole("menuitem", { name: /^Approve…/ });
    expect(approve.getAttribute("aria-disabled")).toBe("true");
    expect(screen.queryByRole("menuitem", { name: "Create workspace…" })).toBeNull();
    await user.keyboard("{Escape}");
    await user.click(screen.getByRole("button", { name: "Actions for Version 1" }));
    expect(await screen.findByRole("menuitem", { name: "Create workspace…" })).toBeTruthy();
    expect(screen.queryByRole("menuitem", { name: "Approve…" })).toBeNull();
  });
  it("reviews another admin's version", async () => {
    const fetch = serve({ [templatePath("t1")]: () => template([version({})]), [`${templatePath("t1")}/versions/1/review`]: () => template([version({ state: "reviewed" })]) });
    const { user } = mount(<WorkspaceTemplateDetail session={admin} id="t1" />);
    await screen.findByText("Waiting for review");
    await user.click(screen.getByRole("button", { name: "Actions for Version 1" }));
    await user.click(await screen.findByRole("menuitem", { name: "Approve…" }));
    await user.click(await screen.findByRole("button", { name: "Approve" }));
    await waitFor(() => expect(writes(fetch).map(([p]) => p)).toEqual([`${templatePath("t1")}/versions/1/review`]));
  });
  it("shows no steps to an Auditor", async () => {
    serve({ [templatePath("t1")]: () => template([version({})]) });
    mount(<WorkspaceTemplateDetail session={auditor} id="t1" />);
    await screen.findByText("Waiting for review");
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Add version" })).toBeNull();
  });
});

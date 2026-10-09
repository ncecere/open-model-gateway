// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests, type Session } from "../lib/api";
import { admin, auditor, testClient } from "../lib/test-fixtures";
import { logoFileError, type GeneralSettings } from "../lib/settings";
import { logoSrc } from "./layout/installation-logo";
import { AuthRequired } from "../pages/home";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const logo = { url: "/api/v1/branding/logo?v=0123456789ab", updated_at: "2026-10-09T00:00:00Z" };
const upload = { available: true, max_bytes: 524288, content_types: ["image/png", "image/jpeg", "image/webp"], min_side: 16, max_side: 4096, recommended_side: 64 };
const general: GeneralSettings = { display_name: "Example gateway", support_url: null, logo_url: null, human_key_max_lifetime_days: 365, timezone: "UTC", updated_at: "2026-10-08T00:00:00Z", updated_by: null, logo: null, logo_upload: upload };
const withLogo = (s: Session): Session => ({ ...s, installation: { ...s.installation, logo } });

function serve(session: Session, settings: GeneralSettings = general) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") return Promise.resolve(Response.json(session));
    if (path.endsWith("/settings/general/logo")) return Promise.resolve(Response.json(init?.method === "DELETE" ? { ...settings, logo: null } : { ...settings, logo: { ...logo, width: 64, height: 64 } }));
    if (path.endsWith("/settings/general")) return Promise.resolve(Response.json(settings));
    return Promise.resolve(Response.json({ data: [], has_more: false }));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
async function mount(href: string) {
  const client = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
  await router.load();
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { client };
}
const brand = async () => (await screen.findByRole("link", { name: "Example gateway" }));
const calls = (fetch: ReturnType<typeof vi.fn>, method: string) => fetch.mock.calls.filter(c => (c[1] as RequestInit | undefined)?.method === method);

describe("Portal mark and installation logo", () => {
  it("shows the Portal mark in the sidebar by default", async () => {
    serve(admin);
    const { client } = await mount("/admin/settings/general");
    const link = await brand();
    expect(link.querySelector('svg[data-mark="portal"]')).toBeTruthy();
    expect(link.querySelector("img")).toBeNull();
    expect(link.querySelector('svg[data-mark="portal"]')!.getAttribute("aria-hidden")).toBe("true");
    client.clear();
  });
  it("replaces the Portal mark with the installation's logo, falling back if it fails to load", async () => {
    serve(withLogo(admin));
    const { client } = await mount("/admin/settings/general");
    const link = await brand();
    const img = link.querySelector<HTMLImageElement>('img[data-mark="custom"]')!;
    expect(img.getAttribute("src")).toBe(logo.url);
    // Decorative inside Bitop's aria-hidden 20 px box: the name is the link text.
    expect(img.getAttribute("alt")).toBe("");
    expect(img.closest("[aria-hidden]")).toBeTruthy();
    expect(link.querySelector('svg[data-mark="portal"]')).toBeNull();
    fireEvent.error(img);
    await waitFor(() => expect(link.querySelector('svg[data-mark="portal"]')).toBeTruthy());
    client.clear();
  });
  it("draws only same-origin gateway logo paths", () => {
    expect(logoSrc(logo)).toBe(logo.url);
    expect(logoSrc({ ...logo, url: "/api/v1/branding/logo" })).toBe("/api/v1/branding/logo");
    for (const url of ["https://cdn.example.com/logo.png", "//evil.example/logo.png", "/api/v1/branding/logo?v=x\"", "javascript:alert(1)"]) expect(logoSrc({ ...logo, url })).toBeUndefined();
    expect(logoSrc(null)).toBeUndefined();
  });
  it("shows the Portal mark or the logo (alt: installation name) on the sign-in page", () => {
    const portal = renderToStaticMarkup(<AuthRequired refresh={() => {}} />);
    expect(portal).toContain('data-mark="portal"');
    expect(portal).not.toContain("<img");
    const custom = renderToStaticMarkup(<AuthRequired refresh={() => {}} branding={{ enabled: true, logo, installation_name: "Acme AI" }} />);
    expect(custom).toContain(`src="${logo.url.replace("&", "&amp;")}"`);
    expect(custom).toContain('alt="Acme AI"');
    expect(custom).toContain("Open Model Gateway</h1>");
  });
  it("pre-checks logo files: PNG, JPEG or WebP up to 512 KiB, never SVG", () => {
    expect(logoFileError({ type: "image/png", size: 1000 })).toBeUndefined();
    expect(logoFileError({ type: "image/webp", size: 524288 })).toBeUndefined();
    expect(logoFileError({ type: "image/svg+xml", size: 100 })).toMatch(/PNG, JPEG or WebP/);
    expect(logoFileError({ type: "image/png", size: 524289 })).toMatch(/512 KiB/);
  });
});

describe("Admin › Settings › General › Logo", () => {
  const row = async () => (await screen.findByRole("group", { name: "Logo" }));
  it("explains in one line and links to Storage when the file store is off", async () => {
    serve(admin, { ...general, logo_upload: { ...upload, available: false } });
    const { client } = await mount("/admin/settings/general");
    const r = await row();
    expect(within(r).getByRole("link", { name: "Data & privacy › Storage" }).getAttribute("href")).toBe("/admin/settings/privacy");
    expect(within(r).queryByRole("button", { name: "Upload" })).toBeNull();
    expect(within(r).getByText("Open Model Gateway (default)")).toBeTruthy();
    expect(screen.queryByRole("textbox", { name: /Logo URL/ })).toBeNull();
    client.clear();
  });
  it("uploads a logo as the raw image, rejecting other types before sending", async () => {
    const user = userEvent.setup({ applyAccept: false }), fetch = serve(admin);
    const { client } = await mount("/admin/settings/general");
    const r = await row();
    expect(within(r).queryByRole("button", { name: "Remove" })).toBeNull();
    const input = r.querySelector<HTMLInputElement>('input[type="file"]')!;
    expect(input.accept).toBe("image/png,image/jpeg,image/webp");
    await user.upload(input, new File(["<svg/>"], "logo.svg", { type: "image/svg+xml" }));
    expect(await within(r).findByRole("alert")).toHaveProperty("textContent", "Use a PNG, JPEG or WebP image.");
    expect(calls(fetch, "PUT")).toEqual([]);
    const png = new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], "logo.png", { type: "image/png" });
    await user.upload(input, png);
    await waitFor(() => expect(calls(fetch, "PUT")).toHaveLength(1));
    const [path, init] = calls(fetch, "PUT")[0]! as [string, RequestInit];
    expect(path).toBe("/api/v1/platform/settings/general/logo");
    expect((init.headers as Record<string, string>)["Content-Type"]).toBe("image/png");
    expect((init.headers as Record<string, string>)["X-CSRF-Token"]).toBe("test-csrf");
    expect(init.body).toBe(png);
    client.clear();
  });
  it("previews a custom logo with its size and removes it", async () => {
    const user = userEvent.setup(), fetch = serve(withLogo(admin), { ...general, logo: { ...logo, width: 128, height: 96 } });
    const { client } = await mount("/admin/settings/general");
    const r = await row();
    expect(within(r).getByText("Custom · 128 × 96 px")).toBeTruthy();
    expect(within(r).getByRole("img", { name: "Example gateway" }).getAttribute("src")).toBe(logo.url);
    await user.click(within(r).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(calls(fetch, "DELETE").map(c => c[0])).toEqual(["/api/v1/platform/settings/general/logo"]));
    client.clear();
  });
  it("notes that a deprecated logo URL is no longer shown, and keeps it on save", async () => {
    const user = userEvent.setup(), fetch = serve(admin, { ...general, logo_url: "https://cdn.example.com/logo.png" });
    const { client } = await mount("/admin/settings/general");
    const r = await row();
    expect(within(r).getByText("Logo URL is no longer shown; upload a logo.")).toBeTruthy();
    expect(document.querySelector('img[src^="https://"]')).toBeNull();
    const name = screen.getByRole("textbox", { name: /Display name/ });
    await user.clear(name); await user.type(name, "Acme AI");
    await user.click(await screen.findByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(calls(fetch, "PUT")).toHaveLength(1));
    expect(JSON.parse(String((calls(fetch, "PUT")[0]![1] as RequestInit).body))).toMatchObject({ display_name: "Acme AI", logo_url: "https://cdn.example.com/logo.png" });
    client.clear();
  });
  it("shows Auditors the logo read-only", async () => {
    serve(withLogo(auditor), { ...general, logo: { ...logo, width: 64, height: 64 } });
    const { client } = await mount("/admin/settings/general");
    expect(await screen.findByText("Custom · 64 × 64 px")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Upload" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Remove" })).toBeNull();
    expect(document.querySelector("main input")).toBeNull();
    client.clear();
  });
});

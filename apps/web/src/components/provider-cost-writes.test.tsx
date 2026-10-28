// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { ProviderReconciliations } from "../pages/usage/provider-reconciliations";
import { admin, auditor, session, testClient } from "../lib/test-fixtures";

const root = "/api/v1/platform", imports = `${root}/provider-cost-imports`, mappings = `${root}/provider-cost-mappings`;
const report = { id: "import", account_id: "account", provider: "anthropic", period_start: "2026-10-01T00:00:00Z", period_end: "2026-10-02T00:00:00Z", supersedes: "previous", line_count: 2 };
const binding = { provider_workspace_id: "already-mapped", connection_id: "connection", deployment_id: null, workspace_id: null, effective_start: report.period_start, effective_end: report.period_end, exclusive: false, non_omg_traffic: true, attested: true, evidence_reference: "old-proof", model_mappings: [] };
function serve(options: { refusal?: number; conflict?: number } = {}) {
  let latest = "mapping-old";
  const fetch = vi.fn().mockImplementation((url: string, init: RequestInit) => {
    if (init.method === "POST") {
      if (url === imports && options.refusal) return Promise.resolve(Response.json({ error: { message: '<script>parse refusal</script> Exact refusal.' } }, { status: options.refusal }));
      if (url === mappings && options.conflict) { latest = "mapping-concurrent"; return Promise.resolve(Response.json({ error: { message: "Overlapping coverage or invalid supersession; select the current exact revision" } }, { status: options.conflict })); }
      if (url === mappings) latest = "mapping-new";
      return Promise.resolve(Response.json({ id: url === imports ? "import" : url === mappings ? "mapping-new" : "frozen-new" }));
    }
    let data: unknown = { data: [] };
    if (url.startsWith(`${imports}?`)) data = { data: [report] };
    if (url === `${imports}/import/lines?limit=200&offset=0`) data = { data: [
      { amount_microusd: "-9007199254740993", dimensions: { workspace_id: null, model: "claude" } },
      { amount_microusd: "-2", dimensions: { workspace_id: "already-mapped" } },
    ] };
    if (url.startsWith(`${mappings}?`)) data = { data: [{ id: latest, account_id: "account", supersedes: latest === "mapping-concurrent" ? "mapping-old" : null }] };
    if (url === `${mappings}/${latest}`) data = { id: latest, account_id: "account", evidence_reference: "old-revision-proof", bindings: [{ ...binding, binding_number: 1 }] };
    if (url.startsWith(`${root}/providers?`)) data = { data: [{ id: "connection", name: "Anthropic connection", provider: "anthropic" }, { id: "other", name: "Other provider", provider: "openai" }] };
    return Promise.resolve(Response.json(data));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
function renderView(viewer = admin) {
  document.cookie = "omg_csrf=test-csrf; path=/";
  render(<QueryClientProvider client={testClient()}><ProviderReconciliations session={viewer} /></QueryClientProvider>);
  return userEvent.setup({ applyAccept: false });
}
async function open() { const user = renderView(); await user.click(screen.getByRole("button", { name: "Upload & manage" })); return user; }
async function upload(user: ReturnType<typeof userEvent.setup>) {
  const file = new File([' {"pages":[], "pages":[]}\r\n'], "report.json", { type: "application/json" });
  await user.upload(screen.getByLabelText("Anthropic report JSON"), file);
  await user.click(screen.getByRole("button", { name: "Upload report" }));
  await screen.findByText("Import resolved: import"); return file;
}
afterEach(() => { cleanup(); vi.unstubAllGlobals(); document.cookie = "omg_csrf=; max-age=0; path=/"; });
describe("Admin provider-cost writes", () => {
  it.each([auditor, session])("renders no writes or private evidence reads for $user.platform_role", async viewer => {
    const fetch = serve(); renderView(viewer);
    expect(screen.queryByRole("button", { name: "Upload & manage" })).toBeNull();
    expect(fetch.mock.calls.some(([url, init]) => init.method === "POST" || url.includes("/lines") || url.includes("provider-cost-mappings"))).toBe(false);
  });
  it("sends untouched File bytes, resolves signed totals and supersession without claiming a new import", async () => {
    const fetch = serve(), user = await open(), file = await upload(user);
    const call = fetch.mock.calls.find(([url, init]) => url === imports && init.method === "POST")!;
    expect(call[1].body).toBe(file);
    expect(call[1]).toMatchObject({ headers: { "X-CSRF-Token": "test-csrf", "Content-Type": "application/json" }, credentials: "same-origin", redirect: "error", cache: "no-store" });
    expect(await screen.findByText("-$9,007,199,254.740995")).toBeTruthy();
    expect(screen.getByText(/Supersedes import: previous/)).toBeTruthy();
    expect(screen.getByText(/Identical reuploads return the retained import/)).toBeTruthy();
    expect(within(screen.getByRole("region", { name: "Selected import result" })).getByText(/UTC coverage \[2026-10-01T00:00:00Z/)).toBeTruthy();
    expect(fetch.mock.calls.some(([url]) => url === `${imports}/import`)).toBe(false); // never fetch evidence_json
  });
  it.each([
    { name: "empty.json", content: "", type: "application/json", message: /empty/i },
    { name: "bad.txt", content: "{}", type: "text/plain", message: /JSON file type/i },
    { name: "untyped.json", content: "{}", type: "", message: /JSON file type/i },
    { name: "large.json", content: "x".repeat(5 * 1024 * 1024 + 1), type: "application/json", message: /5 MiB/i },
  ])("refuses $name locally with an announced error and focus", async ({ name, content, type, message }) => {
    const fetch = serve(), user = await open();
    await user.upload(screen.getByLabelText("Anthropic report JSON"), new File([content], name, { type }));
    await user.click(screen.getByRole("button", { name: "Upload report" }));
    expect(screen.getByRole("alert").textContent).toMatch(message);
    expect(document.activeElement).toBe(screen.getByRole("alert"));
    expect(fetch.mock.calls.some(([, init]) => init.method === "POST")).toBe(false);
  });
  it.each([400, 409, 422])("displays server upload refusal %s verbatim as text, without success", async refusal => {
    const fetch = serve({ refusal }), user = await open();
    await user.upload(screen.getByLabelText("Anthropic report JSON"), new File(["{}"], "report.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Upload report" }));
    expect((await screen.findByRole("alert")).textContent).toContain('<script>parse refusal</script> Exact refusal.');
    expect(document.querySelector("script")).toBeNull(); expect(screen.queryByText("Import resolved: import")).toBeNull();
    expect(fetch.mock.calls.filter(([, init]) => init.method === "POST")).toHaveLength(1);
  });
  it("lists default/unmapped dimensions, preserves existing bindings, attests a revision and freezes explicit IDs", async () => {
    const fetch = serve(), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    const table = await screen.findByRole("table", { name: "Provider dimensions" });
    expect(table.textContent).toContain("Provider default / missing identifier"); expect(table.textContent).toContain("Unmapped"); expect(table.textContent).toContain("Mapped");
    await user.selectOptions(screen.getByLabelText("Provider workspace dimension"), "0");
    await user.selectOptions(screen.getByLabelText("Gateway connection"), "connection");
    expect(screen.queryByRole("option", { name: "Other provider" })).toBeNull();
    await user.type(screen.getByLabelText("Binding evidence reference"), "finance:attestation");
    const stage = screen.getByRole("button", { name: "Apply binding to draft" });
    expect(stage.hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("checkbox", { name: "I attest this binding and its effective coverage" }));
    await user.click(stage);
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "finance:revision");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await screen.findByText("Saved mapping revision: mapping-new");
    const body = JSON.parse(fetch.mock.calls.find(([url, init]) => url === mappings && init.method === "POST")![1].body);
    expect(body).toMatchObject({ account_id: "account", supersedes: "mapping-old", evidence_reference: "finance:revision" });
    expect(body.bindings).toHaveLength(2); expect(body.bindings[0]).toEqual(binding);
    expect(body.bindings[1]).toEqual({ ...binding, provider_workspace_id: null, non_omg_traffic: false, evidence_reference: "finance:attestation" });
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    expect(screen.getByText(/Mapping revision: mapping-new/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    expect(await screen.findByText("Snapshot frozen: frozen-new")).toBeTruthy();
    expect(fetch.mock.calls.find(([url, init]) => url === `${root}/provider-reconciliations` && init.method === "POST")![1].body).toBe('{"import_id":"import","mapping_id":"mapping-new"}');
    expect(screen.getByRole("button", { name: "Open snapshot frozen-new" })).toBeTruthy();
  });
  it("edits and removes bindings explicitly and retains the saved revision after changing tabs", async () => {
    const fetch = serve(), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.click(await screen.findByRole("button", { name: "Edit binding 1" }));
    await user.clear(screen.getByLabelText("Binding evidence reference"));
    await user.type(screen.getByLabelText("Binding evidence reference"), "revised-proof");
    await user.click(screen.getByRole("checkbox", { name: "I attest this binding and its effective coverage" }));
    await user.click(screen.getByRole("button", { name: "Apply binding to draft" }));
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "revision-proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await screen.findByText("Saved mapping revision: mapping-new");
    const body = JSON.parse(fetch.mock.calls.find(([url, init]) => url === mappings && init.method === "POST")![1].body);
    expect(body.bindings).toEqual([{ ...binding, evidence_reference: "revised-proof" }]);
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await screen.findByText("Base mapping revision: mapping-new");
    await user.click(screen.getByRole("button", { name: "Remove binding 1" }));
    expect(within(screen.getByRole("table", { name: "Whole mapping draft" })).queryByText("already-mapped")).toBeNull();
    expect(fetch.mock.calls.filter(([url, init]) => url === mappings && init.method === "POST")).toHaveLength(1);
  });
  it("requires renewed attestation after a scope change and refuses contradictory exclusivity", async () => {
    const fetch = serve(), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await screen.findByRole("table", { name: "Provider dimensions" });
    await user.selectOptions(screen.getByLabelText("Gateway connection"), "connection");
    await user.type(screen.getByLabelText("Binding evidence reference"), "proof");
    await user.click(screen.getByRole("checkbox", { name: "I attest this binding and its effective coverage" }));
    await user.click(screen.getByRole("checkbox", { name: "Exclusive OMG workspace use for this interval" }));
    expect(screen.getByRole("button", { name: "Apply binding to draft" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("checkbox", { name: "I attest this binding and its effective coverage" }));
    await user.click(screen.getByRole("button", { name: "Apply binding to draft" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Exclusive use needs an OMG workspace and no non-OMG traffic");
    expect(fetch.mock.calls.filter(([url, init]) => url === mappings && init.method === "POST")).toHaveLength(0);
  });
  it("hides write controls immediately on demotion and aborts an in-flight upload", async () => {
    let signal: AbortSignal | undefined;
    const fetch = serve(), original = fetch.getMockImplementation()!;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (init.method === "POST") { signal = init.signal as AbortSignal; return new Promise(() => {}); }
      return original(url, init);
    });
    const client = testClient(), user = userEvent.setup();
    const renderWith = (viewer: typeof admin) => <QueryClientProvider client={client}><ProviderReconciliations session={viewer} /></QueryClientProvider>;
    document.cookie = "omg_csrf=test-csrf; path=/";
    const view = render(renderWith(admin));
    await user.click(screen.getByRole("button", { name: "Upload & manage" }));
    await user.upload(screen.getByLabelText("Anthropic report JSON"), new File(["{}"], "report.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Upload report" }));
    view.rerender(renderWith(auditor));
    expect(screen.queryByRole("button", { name: "Upload report" })).toBeNull();
    expect(signal?.aborted).toBe(true);
  });
  it("opens the frozen snapshot in the read-only detail view, and shows a freeze refusal without success", async () => {
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let refuse = true;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (url === `${root}/provider-reconciliations` && init.method === "POST" && refuse) return Promise.resolve(Response.json({ error: { message: "Too many attempts for this scope" } }, { status: 422 }));
      if (url === `${root}/provider-reconciliations/frozen-new`) return Promise.resolve(Response.json({ error: { message: "Snapshot read denied" } }, { status: 403 }));
      return original(url, init);
    });
    const user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    await screen.findByText(/Mapping revision: mapping-old/);
    await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Too many attempts for this scope");
    expect(screen.queryByText(/Snapshot frozen:/)).toBeNull();
    refuse = false; await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    await user.click(await screen.findByRole("button", { name: "Open snapshot frozen-new" }));
    await screen.findByText("Snapshot read denied");
    expect(screen.getByRole("heading", { name: "Reconciliation frozen-new" })).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByRole("heading", { name: "Reconciliation frozen-new" }));
  });
  it.each([409, 412])("locks mapping save on conflict %s until explicit reload, without overwriting", async conflict => {
    const fetch = serve({ conflict }), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await screen.findByRole("table", { name: "Provider dimensions" });
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "finance:new");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Overlapping coverage");
    expect(screen.getByRole("button", { name: "Save mapping revision" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("button", { name: "Reload latest mapping and discard draft" }));
    await screen.findByText(/Base mapping revision: mapping-concurrent/);
    expect(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }).getAttribute("aria-checked")).toBe("false");
    expect(fetch.mock.calls.filter(([url, init]) => url === mappings && init.method === "POST")).toHaveLength(1);
  });
});

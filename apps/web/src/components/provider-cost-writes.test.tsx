// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { createBrowserHistory, createRootRoute, createRoute, createRouter, Link, Outlet, RouterProvider } from "@tanstack/react-router";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ProviderReconciliations } from "../pages/usage/provider-reconciliations";
import { admin, auditor, session, testClient } from "../lib/test-fixtures";

const root = "/api/v1/platform", imports = `${root}/provider-cost-imports`, mappings = `${root}/provider-cost-mappings`;
const report = { id: "import", account_id: "account", provider: "anthropic", period_start: "2026-10-01T00:00:00Z", period_end: "2026-10-02T00:00:00Z", supersedes: "previous", line_count: 2 };
const binding = { provider_workspace_id: "already-mapped", connection_id: "connection", deployment_id: null, workspace_id: null, effective_start: report.period_start, effective_end: report.period_end, exclusive: false, non_omg_traffic: true, attested: true, evidence_reference: "old-proof", model_mappings: [] };
function serve(options: { refusal?: number; conflict?: number; modelMappings?: { provider_model: string; upstream_model: string }[] } = {}) {
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
    if (url === `${mappings}/${latest}`) data = { id: latest, account_id: "account", evidence_reference: "old-revision-proof", bindings: [{ ...binding, model_mappings: options.modelMappings ?? [], binding_number: 1 }] };
    if (url.startsWith(`${root}/providers?`)) data = { data: [{ id: "connection", name: "Anthropic connection", provider: "anthropic" }, { id: "other", name: "Other provider", provider: "openai" }] };
    return Promise.resolve(Response.json(data));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
function renderView(viewer = admin, client = testClient()) {
  document.cookie = "omg_csrf=test-csrf; path=/";
  render(<QueryClientProvider client={client}><ProviderReconciliations session={viewer} /></QueryClientProvider>);
  return userEvent.setup({ applyAccept: false });
}
async function open(client = testClient()) { const user = renderView(admin, client); await user.click(screen.getByRole("button", { name: "Upload & manage" })); return user; }
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
  it("retains an unsaved staged draft and warns before leaving it", async () => {
    serve(); const user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.click(await screen.findByRole("button", { name: "Remove binding 1" }));
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "unsaved-proof");
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    expect(within(await screen.findByRole("table", { name: "Whole mapping draft" })).queryByText("already-mapped")).toBeNull();
    expect((screen.getByLabelText("Mapping revision evidence reference") as HTMLInputElement).value).toBe("unsaved-proof");
    await user.click(screen.getByRole("button", { name: "Back to comparisons" }));
    await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(screen.getByRole("table", { name: "Whole mapping draft" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Back to comparisons" }));
    await user.click(screen.getByRole("button", { name: /^Discard$/ }));
    expect(screen.queryByRole("heading", { name: "Upload & manage provider costs" })).toBeNull();
  });
  it("exposes unsaved staged mappings to the shell sign-out confirmation until saved", async () => {
    serve(); const user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.click(await screen.findByRole("button", { name: "Remove binding 1" }));
    await user.click(screen.getByRole("tab", { name: "Upload report" }));
    // Home's sign-out flow checks this shared unsaved-changes contract, even
    // when the mapping panel is hidden, before cancelling requests/signing out.
    expect(document.querySelector('[data-dirty="true"]')).not.toBeNull();
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await screen.findByText("Saved mapping revision: mapping-new");
    expect(document.querySelector('[data-dirty="true"]')).toBeNull();
  });
  it("shows retained explicit model pairs read-only and preserves them on save", async () => {
    const modelMappings = [{ provider_model: "claude-provider", upstream_model: "claude-historical" }];
    const fetch = serve({ modelMappings }), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    const draft = await screen.findByRole("table", { name: "Whole mapping draft" });
    expect(draft.textContent).toContain("claude-provider → claude-historical");
    expect(screen.getByText(/Editing explicit model mappings is API-only/)).toBeTruthy();
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await screen.findByText("Saved mapping revision: mapping-new");
    expect(JSON.parse(fetch.mock.calls.find(([url, init]) => url === mappings && init.method === "POST")![1].body).bindings[0].model_mappings).toEqual(modelMappings);
  });
  it.each(["Mappings", "Freeze snapshot"])("records pending %s success after a guarded tab switch and retains the result", async step => {
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let finish!: (response: Response) => void, signal: AbortSignal | undefined;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (init.method === "POST" && url !== imports) { signal = init.signal as AbortSignal; return new Promise<Response>(resolve => { finish = resolve; }); }
      return original(url, init);
    });
    const user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: step }));
    if (step === "Mappings") {
      await user.type(await screen.findByLabelText("Mapping revision evidence reference"), "proof");
      await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
      await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    } else {
      await screen.findByText(/Mapping revision: mapping-old/);
      await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    }
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("tab", { name: "Upload report" }));
    await screen.findByRole("alertdialog", { name: "Write still in progress" });
    expect(screen.getByRole("tab", { name: step, hidden: true }).getAttribute("aria-selected")).toBe("true");
    await user.click(screen.getByRole("button", { name: "Switch view" }));
    expect(signal?.aborted).toBe(false);
    expect(screen.queryByText(/Snapshot frozen:|Saved mapping revision:/)).toBeNull();
    await act(async () => finish(Response.json({ id: "retained-result" })));
    await user.click(screen.getByRole("tab", { name: step }));
    expect(await screen.findByText(step === "Mappings" ? "Saved mapping revision: retained-result" : "Snapshot frozen: retained-result")).toBeTruthy();
    await user.click(screen.getByRole("tab", { name: "Upload report" }));
    await user.click(screen.getByRole("tab", { name: step }));
    expect(screen.getByText(step === "Mappings" ? "Saved mapping revision: retained-result" : "Snapshot frozen: retained-result")).toBeTruthy();
  });
  it.each([
    { step: "Mappings", source: "providers" }, { step: "Freeze snapshot", source: "providers" },
    { step: "Mappings", source: "summary" }, { step: "Freeze snapshot", source: "summary" },
  ])("records pending $step success through a $source refetch failure and unblocks navigation", async ({ step, source }) => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let failRead = false, finish!: (response: Response) => void, signal: AbortSignal | undefined;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (init.method === "POST" && url !== imports) { signal = init.signal as AbortSignal; return new Promise<Response>(resolve => { finish = resolve; }); }
      if (failRead && (source === "providers" ? url.startsWith(`${root}/providers?`) : url.includes("/lines?"))) return Promise.resolve(Response.json({ error: { message: "Background read failed" } }, { status: 503 }));
      return original(url, init);
    });
    const user = await open(client); await upload(user);
    await user.click(screen.getByRole("tab", { name: step }));
    if (step === "Mappings") {
      await user.type(await screen.findByLabelText("Mapping revision evidence reference"), "proof");
      await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
      await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    } else {
      await screen.findByText(/Mapping revision: mapping-old/);
      await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    }
    failRead = true;
    const predicate = (q: { queryKey: readonly unknown[] }) => source === "providers" ? q.queryKey.includes(`${root}/providers`) : q.queryKey.includes("summary");
    expect(client.getQueryCache().findAll({ predicate }).every(q => q.isStale())).toBe(true);
    await act(async () => { await client.refetchQueries({ predicate }); });
    if (source !== "providers" || step === "Mappings") await screen.findByText("Background read failed");
    else await waitFor(() => expect(client.getQueryCache().findAll({ predicate })[0].state.error).toBeTruthy());
    expect(signal?.aborted).toBe(false);
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(true);
    await act(async () => finish(Response.json({ id: "retained-after-refetch" })));
    expect(await screen.findByText(step === "Mappings" ? "Saved mapping revision: retained-after-refetch" : "Snapshot frozen: retained-after-refetch")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(false);
    if (source === "providers" && step === "Freeze snapshot") await user.click(screen.getByRole("tab", { name: "Mappings" }));
    expect(screen.getByRole("alert").textContent).toContain("Background read failed");
    await user.click(screen.getByRole("button", { name: "Back to comparisons" }));
    expect(screen.queryByRole("heading", { name: "Upload & manage provider costs" })).toBeNull();
  });
  it("keeps the draft and conflict through a summary refetch failure and retry", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
    const fetch = serve({ conflict: 409 }), original = fetch.getMockImplementation()!;
    let failRead = false;
    fetch.mockImplementation((url: string, init: RequestInit) => failRead && url.includes("/lines?") ? Promise.resolve(Response.json({ error: { message: "Summary unavailable" } }, { status: 503 })) : original(url, init));
    const user = await open(client); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.click(await screen.findByRole("button", { name: "Remove binding 1" }));
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "unsaved-conflict-proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await screen.findByText(/Overlapping coverage or invalid supersession/);
    failRead = true;
    await act(async () => { await client.refetchQueries({ predicate: q => q.queryKey.includes("summary") }); });
    expect(await screen.findByText("Summary unavailable")).toBeTruthy();
    expect(screen.getByText(/Overlapping coverage or invalid supersession/)).toBeTruthy();
    expect((screen.getByLabelText("Mapping revision evidence reference") as HTMLInputElement).value).toBe("unsaved-conflict-proof");
    expect(within(screen.getByRole("table", { name: "Whole mapping draft" })).queryByText("already-mapped")).toBeNull();
    expect(screen.getByRole("button", { name: "Save mapping revision" }).hasAttribute("disabled")).toBe(true);
    failRead = false; await user.click(screen.getByRole("button", { name: "Reload import lines" }));
    await waitFor(() => expect(screen.queryByText("Summary unavailable")).toBeNull());
    expect(screen.getByText(/Overlapping coverage or invalid supersession/)).toBeTruthy();
    expect((screen.getByLabelText("Mapping revision evidence reference") as HTMLInputElement).value).toBe("unsaved-conflict-proof");
  });
  it("releases pending ownership on editor unmount without waiting for abort settlement or clearing a newer write", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let finishOld!: (response: Response) => void, finishNew!: (response: Response) => void, signal: AbortSignal | undefined;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (init.method === "POST" && url === mappings) { signal = init.signal as AbortSignal; return new Promise<Response>(resolve => { finishOld = resolve; }); }
      if (init.method === "POST" && url === `${root}/provider-reconciliations`) return new Promise<Response>(resolve => { finishNew = resolve; });
      return original(url, init);
    });
    const user = await open(client); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.type(await screen.findByLabelText("Mapping revision evidence reference"), "proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    // A replacement query revision remounts the editor while the workflow lives.
    act(() => { client.setQueryData(["api", undefined, mappings, "account", "latest"], { id: "replacement", account_id: "account", bindings: [binding] }); });
    await screen.findByText("Base mapping revision: replacement");
    expect(signal?.aborted).toBe(true);
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(false);
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(true);
    await act(async () => finishOld(Response.json({ id: "cancelled-result" })));
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(true);
    await act(async () => finishNew(Response.json({ id: "new-result" })));
    await screen.findByText("Snapshot frozen: new-result");
    expect(screen.getByRole("button", { name: "Back to comparisons" }).hasAttribute("disabled")).toBe(false);
    expect(client.getQueryCache().findAll().some(q => (q.state.data as { id?: string } | undefined)?.id === "cancelled-result")).toBe(false);
  });
  it("retains unapplied binding fields across tabs and warns before discarding them", async () => {
    serve(); const user = await open(new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } })); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.selectOptions(await screen.findByLabelText("Gateway connection"), "connection");
    await user.type(screen.getByLabelText("Binding evidence reference"), "partial-evidence");
    await user.type(screen.getByLabelText("Deployment ID (optional)"), "partial-deployment");
    await user.click(screen.getByRole("tab", { name: "Freeze snapshot" }));
    expect(document.querySelector('[data-dirty="true"]')).not.toBeNull();
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    expect((screen.getByLabelText("Binding evidence reference") as HTMLInputElement).value).toBe("partial-evidence");
    expect((screen.getByLabelText("Deployment ID (optional)") as HTMLInputElement).value).toBe("partial-deployment");
    expect((screen.getByLabelText("Gateway connection") as HTMLSelectElement).value).toBe("connection");
    await user.click(screen.getByRole("button", { name: "Back to comparisons" }));
    await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect((screen.getByLabelText("Binding evidence reference") as HTMLInputElement).value).toBe("partial-evidence");
  });
  it("records a pending refusal on another tab without fabricating success", async () => {
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let finish!: (response: Response) => void;
    fetch.mockImplementation((url: string, init: RequestInit) => init.method === "POST" && url === mappings ? new Promise<Response>(resolve => { finish = resolve; }) : original(url, init));
    const user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await user.type(await screen.findByLabelText("Mapping revision evidence reference"), "proof");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    await user.click(screen.getByRole("tab", { name: "Upload report" }));
    await user.click(await screen.findByRole("button", { name: "Switch view" }));
    await act(async () => finish(Response.json({ error: { message: "Stale revision" } }, { status: 412 })));
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Stale revision");
    expect(screen.queryByText(/Saved mapping revision:/)).toBeNull();
    expect(screen.getByRole("button", { name: "Save mapping revision" }).hasAttribute("disabled")).toBe(true);
  });
  it("blocks route navigation and unload during a pending write, then permits navigation after resolution", async () => {
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let finish!: (response: Response) => void;
    fetch.mockImplementation((url: string, init: RequestInit) => init.method === "POST" ? new Promise<Response>(resolve => { finish = resolve; }) : original(url, init));
    vi.stubGlobal("scrollTo", vi.fn());
    const rootRoute = createRootRoute({ component: () => <><Link to="/profile">Leave page</Link><Outlet /></> });
    const home = createRoute({ getParentRoute: () => rootRoute, path: "/", component: () => <ProviderReconciliations session={admin} /> });
    const away = createRoute({ getParentRoute: () => rootRoute, path: "/profile", component: () => <h2>Away</h2> });
    window.history.replaceState(null, "", "/");
    const router = createRouter({ routeTree: rootRoute.addChildren([home, away]), history: createBrowserHistory() });
    document.cookie = "omg_csrf=test-csrf; path=/";
    render(<QueryClientProvider client={testClient()}><RouterProvider router={router} /></QueryClientProvider>);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Upload & manage" }));
    await user.upload(screen.getByLabelText("Anthropic report JSON"), new File(["{}"], "report.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Upload report" }));
    await user.click(screen.getByRole("link", { name: "Leave page" }));
    expect(router.state.location.pathname).toBe("/");
    expect(screen.queryByRole("button", { name: /^Discard$/ })).toBeNull();
    const unload = new Event("beforeunload", { cancelable: true }); window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(true);
    await act(async () => finish(Response.json({ id: "import" })));
    await screen.findByText("Import resolved: import");
    await user.click(screen.getByRole("link", { name: "Leave page" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/profile"));
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
  it.each([
    { step: "Mappings", cancellation: "demotion" }, { step: "Freeze snapshot", cancellation: "demotion" },
    { step: "Mappings", cancellation: "sign-out unmount" }, { step: "Freeze snapshot", cancellation: "sign-out unmount" },
  ])("cancels pending $step on $cancellation and ignores a late success", async ({ step, cancellation }) => {
    const fetch = serve(), original = fetch.getMockImplementation()!;
    let finish!: (response: Response) => void, signal: AbortSignal | undefined;
    fetch.mockImplementation((url: string, init: RequestInit) => {
      if (init.method === "POST" && url !== imports) { signal = init.signal as AbortSignal; return new Promise<Response>(resolve => { finish = resolve; }); }
      return original(url, init);
    });
    document.cookie = "omg_csrf=test-csrf; path=/";
    const client = testClient(), user = userEvent.setup();
    const content = (viewer: typeof admin) => <QueryClientProvider client={client}><ProviderReconciliations session={viewer} /></QueryClientProvider>;
    const view = render(content(admin));
    await user.click(screen.getByRole("button", { name: "Upload & manage" })); await upload(user);
    await user.click(screen.getByRole("tab", { name: step }));
    if (step === "Mappings") {
      await user.type(await screen.findByLabelText("Mapping revision evidence reference"), "proof");
      await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
      await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    } else {
      await screen.findByText(/Mapping revision: mapping-old/);
      await user.click(screen.getByRole("button", { name: "Freeze reconciliation" }));
    }
    if (cancellation === "demotion") view.rerender(content(auditor)); else view.unmount();
    expect(signal?.aborted).toBe(true);
    await act(async () => finish(Response.json({ id: "late-success" })));
    expect(screen.queryByText(/late-success/)).toBeNull();
    expect(client.getQueryCache().findAll().some(q => (q.state.data as { id?: string } | undefined)?.id === "late-success")).toBe(false);
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
  it.each([409, 412])("retains conflict %s lock across tabs until explicit reload, without overwriting", async conflict => {
    const fetch = serve({ conflict }), user = await open(); await upload(user);
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    await screen.findByRole("table", { name: "Provider dimensions" });
    await user.type(screen.getByLabelText("Mapping revision evidence reference"), "finance:new");
    await user.click(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }));
    await user.click(screen.getByRole("button", { name: "Save mapping revision" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Overlapping coverage");
    await user.click(screen.getByRole("tab", { name: "Upload report" }));
    await user.click(screen.getByRole("tab", { name: "Mappings" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Overlapping coverage");
    expect(screen.getByRole("button", { name: "Save mapping revision" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("button", { name: "Reload latest mapping and discard draft" }));
    await screen.findByText(/Base mapping revision: mapping-concurrent/);
    expect(screen.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision" }).getAttribute("aria-checked")).toBe("false");
    expect(fetch.mock.calls.filter(([url, init]) => url === mappings && init.method === "POST")).toHaveLength(1);
  });
});

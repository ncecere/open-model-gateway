// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { abortRequests, platformPath, type Deployment } from "../lib/api";
import type { Price, WorkloadKind } from "../lib/governance";
import type { PriceSuggestion } from "../lib/pricing";
import { PriceEditorDialog } from "./price-editor";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { AddModel } from "../pages/model-setup";
import { ModelDetail } from "../pages/catalog-details";
import { meterSummary, meterUsageText, tokenUsageText } from "../pages/governance";
import { AccountingReport } from "../pages/usage/accounting";
import { admin, auditor, member, model, provider, report, session, markup, testClient } from "../lib/test-fixtures";

const route: Deployment = { id: "d1", model_id: model.id, provider_connection_id: "or", provider_name: "OpenRouter", upstream_model: "openai/gpt-6-luna", enabled: true };
const pricesPath = `${platformPath}/deployments/d1/prices`, suggestionPath = `${platformPath}/deployments/d1/price-suggestion`;
beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
});
afterEach(() => { abortRequests(); cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function mountEditor(fetch: ReturnType<typeof vi.fn>, workload: WorkloadKind = "generation", profile = "openai") {
  vi.stubGlobal("fetch", fetch);
  const onClose = vi.fn(), user = userEvent.setup(), client = testClient();
  render(<QueryClientProvider client={client}><PriceEditorDialog deployment={route} workload={workload} profile={profile} onClose={onClose} /></QueryClientProvider>);
  return { onClose, user, client };
}
const posts = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(call => call[1]?.method === "POST");
const publish = () => screen.getByRole("button", { name: "Publish price" });

describe("v3 price editor dialog", () => {
  it("rejects sub-micro-dollar input, then retries after a correction and a server error with the same exact body", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json({ error: { code: "400", message: "Invalid price version" } }, { status: 400 })).mockResolvedValueOnce(Response.json({ id: "price" }, { status: 201 }));
    const { user, onClose } = mountEditor(fetch);
    await user.type(screen.getByRole("textbox", { name: /input token ceiling/ }), "1000");
    await user.type(screen.getByRole("textbox", { name: /output token ceiling/ }), "100");
    await user.selectOptions(screen.getByRole("combobox", { name: "Input tokens price" }), "priced");
    const input = screen.getByRole("textbox", { name: "$ per M input tokens" }) as HTMLInputElement;
    await user.type(input, "0.1000001");
    await user.click(publish());
    expect(fetch).not.toHaveBeenCalled();
    expect(input.getAttribute("aria-invalid")).toBe("true");
    await screen.findByText(/cannot be stored exactly/);
    await waitFor(() => expect(document.activeElement).toBe(input));
    await user.clear(input); await user.type(input, "0.10");
    expect(input.getAttribute("aria-invalid")).toBeNull();
    expect(screen.getByText("Shown as $0.10/M input tokens")).toBeTruthy();
    await user.click(publish());
    await screen.findByText("Invalid price version");
    expect(input.value).toBe("0.10");
    await user.click(publish());
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    const [first, second] = posts(fetch);
    expect(first[0]).toBe(pricesPath); expect(second[1].body).toBe(first[1].body);
    const body = JSON.parse(first[1].body);
    expect(body).toMatchObject({ pricing_version: 3, input_token_limit: 1000, output_token_limit: 100, max_units: {} });
    expect(body.price_lines).toContainEqual({ meter: "input_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" });
    expect(body.price_lines.some((l: { meter: string }) => l.meter === "output_tokens")).toBe(false); // Unknown, never free
    expect(body.price_lines).toContainEqual({ meter: "output_images", not_applicable: true });
  });

  it("offers the exact larger unit for $0.00000333/second instead of rounding", async () => {
    const { user } = mountEditor(vi.fn(), "audio_transcriptions");
    await user.selectOptions(screen.getByRole("combobox", { name: "Audio input price" }), "priced");
    const unit = screen.getByRole("combobox", { name: "Billing unit" }) as HTMLSelectElement;
    expect(unit.value).toBe("60000");
    await user.selectOptions(unit, "1000");
    await user.type(screen.getByRole("textbox", { name: "$ per second of audio input" }), "0.00000333");
    await user.click(publish());
    await screen.findByText(/Use per hour instead \(\$0\.011988\/hour\)/);
    await user.click(screen.getByRole("button", { name: "Use per hour ($0.011988/hour)" }));
    expect((screen.getByRole("textbox", { name: "$ per hour of audio input" }) as HTMLInputElement).value).toBe("0.011988");
    expect(unit.value).toBe("3600000");
    for (const meter of ["Audio input", "Input tokens", "Output tokens", "Requests"]) expect(screen.getByRole("group", { name: meter })).toBeTruthy();
    expect(screen.queryByRole("group", { name: "Cache read" })).toBeNull();
  });

  it("imports the current OpenRouter price as a draft, highlights needs_review lines and never auto-publishes", async () => {
    const suggestion: PriceSuggestion = { deployment_id: "d1", upstream_model: "openai/gpt-6-luna", source: "openrouter_public_catalog", catalog_model_id: "openai/gpt-6-luna", workload: "generation", needs_review: true, warnings: ["internal_reasoning is priced separately and has no gateway meter; review the output rate"], display_lines: null,
      lines: [
        { meter: "input_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input", needs_review: false },
        { meter: "input_tokens", microusd_per_batch: "200000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input", min_prompt_tokens: 272000, needs_review: false },
        { meter: "output_tokens", microusd_per_batch: null, batch: 1000000, unit_label: "/M tokens", sku_label: "Output", needs_review: true, note: "variable or unparseable catalog price; enter manually" },
        { meter: "cache_read_tokens", microusd_per_batch: "10000", batch: 1000000, unit_label: "/M tokens", sku_label: "Cache read", needs_review: false },
        ...(["cache_write_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens"] as const).map(meter => ({ meter, not_applicable: true, needs_review: true, note: "not listed in the catalog; confirm the model cannot produce it" })),
        { meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request", needs_review: false },
        ...(["output_images", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "search_units"] as const).map(meter => ({ meter, not_applicable: true, needs_review: false })),
      ],
      draft: { pricing_version: 3, input_token_limit: 400000, output_token_limit: 128000, price_lines: [], max_units: {} } };
    const fetch = vi.fn().mockImplementation((path: string, init: RequestInit) => Promise.resolve(path === suggestionPath && init.method === "GET" ? Response.json(suggestion) : Response.json({ id: "price" }, { status: 201 })));
    const { user, onClose } = mountEditor(fetch, "generation", "openrouter");
    await user.click(screen.getByRole("button", { name: "Import current OpenRouter price" }));
    await screen.findByText("Draft from OpenRouter's public catalog");
    expect(fetch.mock.calls[0][0]).toBe(suggestionPath);
    expect(posts(fetch)).toHaveLength(0);
    expect(screen.getByText(/internal_reasoning is priced separately/)).toBeTruthy();
    expect((screen.getByRole("textbox", { name: "$ per M input tokens" }) as HTMLInputElement).value).toBe("0.10");
    expect((screen.getByRole("textbox", { name: "$ per M input tokens (prompt > 272,000 tokens)" }) as HTMLInputElement).value).toBe("0.20");
    const output = screen.getByRole("group", { name: "Output tokens" });
    expect(output.getAttribute("data-review")).toBe("true"); expect(within(output).getByText("Needs review")).toBeTruthy();
    expect(screen.getByRole("group", { name: "Cache write (1-hour)" }).getAttribute("data-review")).toBe("true");
    expect(screen.getByRole("group", { name: "Input tokens" }).getAttribute("data-review")).toBeNull();
    // The draft is incomplete until the unknown output price is entered; nothing is posted.
    await user.click(publish());
    expect(posts(fetch)).toHaveLength(0);
    const outputPrice = within(output).getByRole("textbox", { name: "$ per M output tokens" });
    await waitFor(() => expect(document.activeElement).toBe(outputPrice));
    await user.type(outputPrice, "0.50");
    expect(output.getAttribute("data-review")).toBeNull();
    await user.click(publish());
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    expect(posts(fetch)).toHaveLength(1);
    const body = JSON.parse(posts(fetch)[0][1].body);
    expect(body).toMatchObject({ pricing_version: 3, input_token_limit: 400000, output_token_limit: 128000 });
    expect(body.price_lines).toContainEqual({ meter: "input_tokens", microusd_per_batch: "200000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input", min_prompt_tokens: 272000 });
    expect(body.price_lines).toContainEqual({ meter: "output_tokens", microusd_per_batch: "500000", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" });
    expect(body.price_lines).toContainEqual({ meter: "cache_write_1h_tokens", not_applicable: true });
  });

  it("only offers the OpenRouter import on OpenRouter connections and keeps import errors retryable", async () => {
    mountEditor(vi.fn(), "generation", "openai");
    expect(screen.queryByRole("button", { name: "Import current OpenRouter price" })).toBeNull();
    cleanup();
    const fetch = vi.fn().mockResolvedValue(Response.json({ error: { code: "502", message: "OpenRouter catalog unavailable" } }, { status: 502 }));
    const { user } = mountEditor(fetch, "generation", "openrouter");
    await user.click(screen.getByRole("button", { name: "Import current OpenRouter price" }));
    await screen.findByText("OpenRouter catalog unavailable");
    expect(posts(fetch)).toHaveLength(0);
  });
});

describe("token ceilings, free shortcut and re-import", () => {
  it("hides token ceilings for text to speech, marks every meter free and publishes zero ceilings", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ id: "price" }, { status: 201 }));
    const { user, onClose } = mountEditor(fetch, "audio_speech");
    expect(screen.queryByRole("textbox", { name: /token ceiling/ })).toBeNull();
    expect(screen.getByText(/No token meters apply/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Mark all free" }));
    for (const meter of ["Input characters", "Audio output", "Requests"]) expect((screen.getByRole("combobox", { name: `${meter} price` }) as HTMLSelectElement).value).toBe("free");
    expect(screen.queryByRole("button", { name: "Mark all free" })).toBeNull();
    expect(screen.getByText(/Input characters: Free · Output audio: Free · Requests: Free/)).toBeTruthy();
    await user.click(publish());
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    const body = JSON.parse(posts(fetch)[0][1].body);
    expect(body).toMatchObject({ input_token_limit: 0, output_token_limit: 0 });
    expect(body.price_lines).toContainEqual({ meter: "input_characters", microusd_per_batch: "0", batch: 1000000, unit_label: "/M characters", sku_label: "Characters" });
    expect(body.price_lines).toContainEqual({ meter: "input_tokens", not_applicable: true });
  });
  it("re-importing keeps the admin's token ceilings, shows the endpoint used and asks before replacing them", async () => {
    const current: Price = { id: "p", deployment_id: "d1", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 8000, output_token_limit: 64, cache_pricing: null, created_at: "2026-01-01T00:00:00Z", price_lines: [{ meter: "input_tokens", microusd_per_batch: "90000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" }, { meter: "output_tokens", microusd_per_batch: "0", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" }, { meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" }] };
    const suggestion: PriceSuggestion = { deployment_id: "d1", upstream_model: "cloudflare/clef-flash", source: "openrouter_public_catalog", catalog_model_id: "cloudflare/clef-flash", workload: "systemone", needs_review: false, warnings: ["Priced from the PrimeIntellect endpoint ($0.021/M input tokens), which matches the provider-reported cost of 1 recent request."], display_lines: null,
      endpoint: { selection: "evidence", provider: "PrimeIntellect", matched_attempts: 1, endpoint_count: 2, input_rate: "$0.021/M input tokens" }, ceilings: { input_token_limit: 8192, output_token_limit: 1024, context_length: 16384, max_completion_tokens: null },
      lines: [{ meter: "input_tokens", microusd_per_batch: "21000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input", needs_review: false }, { meter: "output_tokens", microusd_per_batch: "0", batch: 1000000, unit_label: "/M tokens", sku_label: "Output", needs_review: false }, { meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request", needs_review: false }],
      draft: { pricing_version: 3, input_token_limit: 8192, output_token_limit: 1024, price_lines: [], max_units: {} } };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json(suggestion)));
    const user = userEvent.setup();
    render(<QueryClientProvider client={testClient()}><PriceEditorDialog deployment={route} workload="systemone" profile="openrouter" current={current} onClose={vi.fn()} /></QueryClientProvider>);
    await user.click(screen.getByRole("button", { name: "Import current OpenRouter price" }));
    await screen.findByText("Draft from OpenRouter's public catalog");
    expect(screen.getByText("PrimeIntellect")).toBeTruthy();
    expect(screen.getByText(/matches the provider-reported cost of recent requests \(1 request\)/)).toBeTruthy();
    expect((screen.getByRole("textbox", { name: "$ per M input tokens" }) as HTMLInputElement).value).toBe("0.021");
    const input = screen.getByRole("textbox", { name: /input token ceiling/ }) as HTMLInputElement, output = screen.getByRole("textbox", { name: /output token ceiling/ }) as HTMLInputElement;
    expect([input.value, output.value]).toEqual(["8000", "64"]);
    expect(screen.getByText(/Your token ceilings \(8,000 input \/ 64 output\) were kept/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Use imported ceilings" }));
    expect([input.value, output.value]).toEqual(["8192", "1024"]);
    expect(screen.queryByRole("button", { name: "Use imported ceilings" })).toBeNull();
  });
});

describe("price display and read-only access", () => {
  const v3: Price = { id: "v3", deployment_id: "d1", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 4000, output_token_limit: 0, cache_pricing: null, created_at: "2026-01-01T00:00:00Z", max_units: { output_images: "4" },
    price_lines: [{ meter: "input_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" }, { meter: "output_images", microusd_per_batch: "20500", batch: 1, unit_label: "/image", sku_label: "Image output", variant: "768" }, { meter: "search_units", not_applicable: true }],
    display_lines: ["$0.10/M input tokens", "$0.0205/image (768)", "Search units: not applicable"], display_summary: "$0.10/M input tokens · $0.0205/image (768) · Search units: not applicable" };
  const imageModel = { ...model, supported_protocols: ["images" as const] };
  function page(session: typeof admin, tab: string) {
    const client = testClient(), path = `${platformPath}/models/${model.id}`;
    client.setQueryData(["api", undefined, `${platformPath}/deployments?model_id=${model.id}`, "choices"], [route]);
    client.setQueryData(["api", undefined, `${platformPath}/providers`, "choices"], [{ ...provider, id: "or", name: "OpenRouter", provider: "openrouter" }]);
    return markup(<ModelDetail session={session} id={model.id} tab={tab} onTabChange={() => {}} />, [[path, imageModel], [`${platformPath}/deployments/d1`, { ...route, provider: "openrouter", connection_enabled: true, workload: "images", protocols: ["images"], data_policy: { data_collection: "deny", basis: "current_configuration" }, price: v3 }], [`${pricesPath}?limit=1&offset=0`, { data: [v3] }], [`${platformPath}/deployments/d1/routing`, { routing: { priority: 0, weight: 1, residency: null, failure_threshold: 3, cooldown_seconds: 30 }, health: { consecutive_failures: 0, open_until: null } }], [`${path}/routing`, { policy: { strategy: "priority", max_attempts: 1, allow_ambiguous_failover: false, required_residency: null } }]], client);
  }
  it("shows OpenRouter-style lines from server display strings on the Pricing tab and route rows", () => {
    const pricing = page(admin, "pricing");
    for (const text of ["$0.10", "M input tokens", "$0.0205", "Image output (768)", "Not applicable: search units", "Unknown · not free", "4 output images", "Pricing v3", "Data collection denied", "Text → Image"]) expect(pricing).toContain(text);
    expect(pricing).not.toContain("$0.00"); expect(pricing).toContain("Import current OpenRouter price"); expect(pricing).toContain("Publish new price");
    const routes = page(admin, "routes");
    expect(routes).toContain('id="routes"'); expect(routes).toContain("Default route");
  });
  it("gives Auditors the same lines without publish or import controls", () => {
    const html = page(auditor, "pricing") + page(auditor, "routes");
    expect(html).toContain("$0.0205"); expect(html).toContain("Image output (768)");
    for (const control of ["Import current OpenRouter price", "Publish new price", "Set price", "Publish price"]) expect(html).not.toContain(control);
  });
  it("shows meter totals and provider-reported cost as evidence, never as the charge", () => {
    const withMeters = { ...report, meter_usage: { output_images: "12", input_characters: "1234567", input_audio_seconds_ms: "61500", output_audio_seconds_ms: null, search_units: "3", requests: "40" }, provider_reported_cost_microusd: "420000", cost_components: { ...report.cost_components, output_images_microusd: "246000" } };
    const html = markup(<AccountingReport report={withMeters} />);
    for (const text of ["Media and unit meters", "Images generated", "1,234,567", "1 min 1.5 s", "Provider-reported (for checking only)", "$0.42", "never billed", "$0.246"]) expect(html).toContain(text);
    expect(html).toContain("Unknown"); // unobserved audio output
    expect(markup(<AccountingReport report={report} />)).not.toContain("Provider-reported");
    expect(meterSummary({ meter_usage: { output_images: "2", input_characters: null, input_audio_seconds_ms: null, output_audio_seconds_ms: null, search_units: null, requests: "1" }, output_image_variant: "1024x1024" })).toBe("2 images (1024x1024) · 1 request");
    expect(meterSummary({ meter_usage: null, output_image_variant: null })).toBe("Not reported");
  });
  it("distinguishes known zero, unknown and partial meter totals and hides irrelevant meters", () => {
    const keys = ["output_images", "input_characters", "input_audio_seconds_ms", "output_audio_seconds_ms", "search_units", "requests"] as const;
    const counts = (values: Partial<Record<typeof keys[number], string>>) => Object.fromEntries(keys.map(k => [k, values[k] ?? "0"])) as Record<typeof keys[number], string>;
    const usage = { output_images: null, input_characters: "22", input_audio_seconds_ms: "1000", output_audio_seconds_ms: "0", search_units: null, requests: "3" };
    const relevant = counts({ input_characters: "2", input_audio_seconds_ms: "1", output_audio_seconds_ms: "2", search_units: "1", requests: "4" });
    const unknown = counts({ output_audio_seconds_ms: "1", search_units: "1", requests: "1" });
    expect(meterUsageText("output_images", usage, relevant, unknown)).toBeUndefined();
    expect(meterUsageText("input_characters", usage, relevant, unknown)).toBe("22");
    expect(meterUsageText("input_audio_seconds_ms", usage, relevant, unknown)).toBe("1 s");
    expect(meterUsageText("output_audio_seconds_ms", usage, relevant, unknown)).toBe("Unknown · partial (at least 0 s)");
    expect(meterUsageText("search_units", usage, relevant, unknown)).toBe("Unknown");
    expect(meterUsageText("requests", usage, relevant, unknown)).toBe("Unknown · partial (at least 3)");
    // Relevant attempts that all used nothing are a known zero.
    expect(meterUsageText("search_units", { ...usage, search_units: null }, counts({ search_units: "1" }), counts({}))).toBe("0");
    // Older gateways without per-meter counts keep the previous unknown display.
    expect(meterUsageText("search_units", usage)).toBe("Unknown");
    const none = { ...report, meter_usage: { ...usage, output_audio_seconds_ms: null, input_characters: null, input_audio_seconds_ms: null, requests: null }, meter_relevant_attempts: counts({}), meter_unknown_attempts: counts({}), provider_reported_cost_microusd: null };
    const html = markup(<AccountingReport report={none} />);
    // No workload produced a meter and the provider reported nothing: no meters section at all (zero rows hidden).
    expect(html).not.toContain("Media and unit meters");
    expect(html).not.toContain("Images generated");
    expect(tokenUsageText({ workload_kind: "audio_speech", input_tokens: "0", output_tokens: "0" })).toBe("Not applicable");
    expect(tokenUsageText({ workload_kind: "generation", input_tokens: "0", output_tokens: "0" })).toBe("0 in · 0 out");
  });
});

describe("Add model type and protocols", () => {
  const conn = (id: string, profile: string, name: string) => ({ ...provider, id, name, provider: profile, enabled: true });
  const mount = (list: ReturnType<typeof conn>[], fetch = vi.fn()) => {
    vi.stubGlobal("fetch", fetch);
    const client = testClient(), user = userEvent.setup();
    client.setQueryData(["api", undefined, `${platformPath}/providers`, "choices"], list);
    client.setQueryData(["api", undefined, `${platformPath}/catalogs`, "choices"], []);
    render(<QueryClientProvider client={client}><DashboardNavigationProvider search={{ page: "model-new" }} navigate={vi.fn()}><ActionProvider><AddModel session={admin} /></ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
    return user;
  };
  const type = () => screen.getByRole("combobox", { name: "Type" });
  /** Opens the Type list: [label, disabled] per option. */
  const typeOptions = async (user: ReturnType<typeof userEvent.setup>) => {
    await user.click(type());
    return within(await screen.findByRole("listbox")).getAllByRole("option").map(o => [o.textContent, o.getAttribute("aria-disabled") === "true"] as const);
  };
  const protocols = () => within(screen.getByRole("group", { name: "Protocols" })).getAllByRole("button").map(b => [b.textContent, b.getAttribute("aria-pressed") === "true"]);
  const types = ["Text", "Embeddings", "Images", "Speech to text", "Text to speech", "Rerank", "System One"];

  it("is one compact Type select with short labels; types the connection can't serve are disabled with a reason, not red sentences", async () => {
    const user = mount([conn("a", "anthropic", "Claude")]);
    expect(type().textContent).toBe("Text");
    expect(screen.queryByRole("radiogroup")).toBeNull();
    const options = await typeOptions(user);
    expect(options.map(([label]) => label?.replace(/\(.*\)/, ""))).toEqual(types);
    expect(options.filter(([, disabled]) => !disabled).map(([label]) => label)).toEqual(["Text"]);
    expect(screen.getByRole("option", { name: "Rerank (Not available on Anthropic)" }).getAttribute("aria-disabled")).toBe("true");
    expect(document.body.textContent).not.toMatch(/can't serve|POST \/v1/);
  });
  it("defaults protocols per profile and hides the ones a connection can't serve", async () => {
    mount([conn("a", "anthropic", "Claude")]);
    expect(protocols()).toEqual([["Chat Completions", true], ["Messages", true]]);
    expect((screen.getByRole("textbox", { name: /Upstream model ID/ }) as HTMLInputElement).placeholder).toBe("claude-sonnet-4-5");
    cleanup(); mount([conn("o", "openai", "OpenAI")]);
    expect(protocols()).toEqual([["Chat Completions", true], ["Responses", true]]);
    cleanup(); mount([conn("r", "openrouter", "OpenRouter")]);
    expect(protocols()).toEqual([["Chat Completions", true]]);
    cleanup(); const user = mount([conn("v", "vllm", "Local")]);
    expect(protocols()).toEqual([["Chat Completions", true]]);
    expect((await typeOptions(user)).filter(([, disabled]) => disabled).map(([label]) => label?.replace(/\(.*\)/, ""))).toEqual(["Images", "Speech to text", "Text to speech", "Rerank", "System One"]);
  });
  it("sends the chosen protocols; switching connection follows the new profile and drops a type it can't serve", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json({ model_id: "m", deployment_id: "d", price_id: null }, { status: 201 }));
    const user = mount([conn("o", "openai", "OpenAI"), conn("a", "anthropic", "Claude")], fetch);
    await user.click(screen.getByRole("button", { name: "Responses" }));
    expect(protocols()).toEqual([["Chat Completions", true], ["Responses", false]]);
    await user.click(type()); await user.click(await screen.findByRole("option", { name: "Images" }));
    await waitFor(() => expect(type().textContent).toBe("Images"));
    expect(screen.queryByRole("group", { name: "Protocols" })).toBeNull();
    expect((screen.getByRole("textbox", { name: /Upstream model ID/ }) as HTMLInputElement).placeholder).toBe("gpt-image-1");
    await user.selectOptions(screen.getByRole("combobox", { name: /Connection/ }), "a");
    expect(type().textContent).toBe("Text");
    expect(protocols()).toEqual([["Chat Completions", true], ["Messages", true]]);
    await user.type(screen.getByRole("textbox", { name: /Upstream model ID/ }), "claude-x");
    await user.type(screen.getByRole("textbox", { name: /Display name/ }), "Claude X");
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(fetch).toHaveBeenCalled());
    expect(JSON.parse(fetch.mock.calls[0][1].body).model.supported_protocols).toEqual(["chat_completions", "messages"]);
  });
  it("asks for a protocol when every chip is off", async () => {
    const fetch = vi.fn(), user = mount([conn("r", "openrouter", "OpenRouter")], fetch);
    await user.click(screen.getByRole("button", { name: "Chat Completions" }));
    await user.type(screen.getByRole("textbox", { name: /Upstream model ID/ }), "x");
    await user.type(screen.getByRole("textbox", { name: /Display name/ }), "X");
    await user.click(screen.getByRole("button", { name: "Add model" }));
    expect(await screen.findByText(/at least one option for protocols/)).toBeTruthy();
    expect(fetch).not.toHaveBeenCalled();
  });
  it("prices the chosen type's meters inside the disclosure", async () => {
    const user = mount([conn("r", "openrouter", "OpenRouter")]);
    await user.click(screen.getByRole("button", { name: "Add price now (optional)" }));
    expect(screen.getByRole("combobox", { name: "Cache read price" })).toBeTruthy();
    expect(screen.getByText("Or import OpenRouter's price later from the model's Pricing tab.")).toBeTruthy();
    await user.click(type()); await user.click(await screen.findByRole("option", { name: "Images" }));
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Image output price" })).toBeTruthy());
    expect(screen.queryByRole("combobox", { name: "Cache read price" })).toBeNull();
    await user.selectOptions(screen.getByRole("combobox", { name: "Image output price" }), "priced");
    await user.type(screen.getByRole("textbox", { name: "$ per image" }), "0.0205");
    expect(screen.getByText(/\$0\.0205\/image/, { selector: "span" })).toBeTruthy();
  });
});

import { describe, expect, it } from "vitest";
import { validateFields } from "./forms";
import { readinessText, apiNameFrom, chosenConnection, defaultProtocols, protocolSupported, setupIdentityFields, setupSourceFields, workloadSupported, initialSetupValues, modelReadiness, protocolLabel, protocolOptions, protocolSetError, setupBody, setupFields, setupSteps, workloadDisabledReason, workloadGroups, type SetupChoices } from "./model-setup";
import type { ModelReadiness, PlatformOverviewData } from "./api";
import { draftBody, emptyDraft, validateDraft } from "./pricing";

const counts: ModelReadiness = { routes: 2, enabled_routes: 1, priced_enabled_routes: 1, catalogs: 1, direct_workspaces: 0, connections: [{ id: "c1", name: "Local" }] };
const choices: SetupChoices = { connections: [{ id: "c1", name: "Local", provider: "vllm", enabled: true }, { id: "c2", name: "Cloud", provider: "openai", enabled: false }], catalogs: [{ id: "cat1", name: "Approved" }, { id: "cat2", name: "Beta" }] };

describe("model readiness derivation (contract §2)", () => {
  it("is ready when enabled, routed and offered", () => expect(modelReadiness({ enabled: true, readiness: counts })).toEqual({ state: "ready", warnings: [] }));
  it("treats direct assignment as offered", () => expect(modelReadiness({ enabled: true, readiness: { ...counts, catalogs: 0, direct_workspaces: 2 } }).state).toBe("ready"));
  it("warns about unpriced routes without blocking readiness", () => expect(modelReadiness({ enabled: true, readiness: { ...counts, enabled_routes: 2 } })).toEqual({ state: "ready", warnings: ["unpriced"] }));
  it("lists every blocking reason", () => expect(modelReadiness({ enabled: false, readiness: { ...counts, enabled_routes: 0, priced_enabled_routes: 0, catalogs: 0 } })).toEqual({ state: "needs_setup", warnings: ["disabled", "no_route", "not_offered"] }));
  it("needs attention when ceilings exceed a tokens-per-minute default or free OpenRouter routes are blocked", () => {
    const deny = { openrouter: { data_collection: "deny" as const, free_models_available: false } }, allow = { openrouter: { data_collection: "allow" as const, free_models_available: true } };
    expect(modelReadiness({ enabled: true, readiness: { ...counts, routes_over_token_limit: 1, type_tokens_per_minute: 100000 } })).toEqual({ state: "needs_attention", warnings: ["token_ceiling"] });
    expect(modelReadiness({ enabled: true, readiness: { ...counts, openrouter_free_routes: 1 } }, deny)).toEqual({ state: "needs_attention", warnings: ["free_blocked"] });
    expect(modelReadiness({ enabled: true, readiness: { ...counts, openrouter_free_routes: 1 } }, allow).state).toBe("ready");
    // Unknown policy is not a finding.
    expect(modelReadiness({ enabled: true, readiness: { ...counts, openrouter_free_routes: 1 } }).state).toBe("ready");
    // Setup gaps still win over configuration checks.
    expect(modelReadiness({ enabled: false, readiness: { ...counts, routes_over_token_limit: 1 } }).state).toBe("needs_setup");
  });
  it("calls an enabled model with no enabled route Not serving (one vocabulary everywhere)", () => { expect(modelReadiness({ enabled: true, readiness: { ...counts, enabled_routes: 0, priced_enabled_routes: 0 } }).state).toBe("not_serving"); expect(readinessText.not_serving).toBe("Not serving"); expect(Object.values(readinessText)).toEqual(["Ready", "Needs setup", "Needs attention", "Not serving", "Provider API retired", "Readiness unknown"]); });
  it("shows video models as Provider API retired, whatever their routes (OpenAI Videos API shut down 2026-09-24)", () => {
    expect(modelReadiness({ enabled: true, supported_protocols: ["videos"], readiness: counts })).toEqual({ state: "retired", warnings: ["provider_retired"] });
    expect(modelReadiness({ enabled: true, supported_protocols: ["videos"] }).state).toBe("retired");
    expect(readinessText.retired).toBe("Provider API retired");
    expect(modelReadiness({ enabled: true, supported_protocols: ["batches"], readiness: counts }).state).toBe("ready");
  });
  it("never infers readiness from a missing server value", () => { expect(modelReadiness({ enabled: true })).toEqual({ state: "unknown", warnings: [] }); expect(modelReadiness({ enabled: true, readiness: null })).toEqual({ state: "unknown", warnings: [] }); });
});

describe("Add model form", () => {
  it("derives API names within the alias character set", () => { expect(apiNameFrom("Llama 3.1 70B Instruct")).toBe("llama-3.1-70b-instruct"); expect(apiNameFrom("  openai/GPT-4.1 mini ")).toBe("openai/gpt-4.1-mini"); expect(apiNameFrom("--Ünïcode!!")).toBe("n-code"); expect(apiNameFrom("x".repeat(300))).toHaveLength(200); });
  it("chooses the picked, then requested, then first connection", () => { expect(chosenConnection("c2", choices.connections, "c1")).toBe("c2"); expect(chosenConnection("", choices.connections, "c2")).toBe("c2"); expect(chosenConnection("", choices.connections, "gone")).toBe("c1"); expect(chosenConnection("", [], "c1")).toBe(""); });
  const filled = { ...initialSetupValues(), provider_connection_id: "c1", upstream_model: " meta/llama ", display_name: "Llama", public_name: "llama", catalog_ids: '["cat2"]' };
  it("defaults to disabled, unpriced and unoffered", () => { const v = initialSetupValues(); expect(v).toMatchObject({ enabled: "false", pricing: "unpriced", catalog_ids: "[]", supported_protocols: '["chat_completions"]' }); });
  it("builds the one-transaction body without pricing", () => { expect(validateFields(setupFields(choices, filled), filled)).toEqual({}); expect(setupBody(filled, choices)).toEqual({ model: { public_name: "llama", display_name: "Llama", description: null, supported_protocols: ["chat_completions"], enabled: false }, route: { provider_connection_id: "c1", upstream_model: "meta/llama", enabled: false }, price: null, catalog_ids: ["cat2"] }); });
  it("enables model and route together when switched on", () => { const body = setupBody({ ...filled, enabled: "true" }, choices); expect(body.model.enabled).toBe(true); expect(body.route.enabled).toBe(true); });
  it("sends exact micro-USD v3 price lines, never floats", () => {
    const draft = emptyDraft("generation"), priced = { ...filled, pricing: "priced" };
    draft.inputTokenLimit = "1000"; draft.outputTokenLimit = "100";
    draft.meters.input_tokens = { ...draft.meters.input_tokens, mode: "priced", rows: [{ ...draft.meters.input_tokens.rows[0], usd: "9007199254.740993" }] };
    draft.meters.output_tokens = { ...draft.meters.output_tokens, mode: "priced", rows: [{ ...draft.meters.output_tokens.rows[0], usd: "0.000001" }] };
    draft.meters.cache_read_tokens = { ...draft.meters.cache_read_tokens, mode: "priced", rows: [{ ...draft.meters.cache_read_tokens.rows[0], usd: "0.1" }] };
    expect(validateFields(setupFields(choices, priced), priced)).toEqual({}); expect(validateDraft(draft)).toEqual({});
    const price = setupBody(priced, choices, draftBody(draft)).price as ReturnType<typeof draftBody>;
    expect(price).toMatchObject({ pricing_version: 3, input_token_limit: 1000, output_token_limit: 100 });
    expect(price.price_lines).toContainEqual({ meter: "input_tokens", microusd_per_batch: "9007199254740993", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" });
    expect(price.price_lines).toContainEqual({ meter: "output_tokens", microusd_per_batch: "1", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" });
    expect(price.price_lines).toContainEqual({ meter: "cache_read_tokens", microusd_per_batch: "100000", batch: 1000000, unit_label: "/M tokens", sku_label: "Cache read" });
    expect(price.price_lines.some(l => l.meter === "cache_write_tokens")).toBe(false); // unknown, never free
    expect(price.price_lines).toContainEqual({ meter: "output_images", not_applicable: true });
  });
  it("sends a price only when setting one, and validates the draft separately", () => { expect(Object.keys(validateFields(setupFields(choices, filled), filled))).toEqual([]); expect(setupBody(filled, choices, draftBody({ ...emptyDraft("generation"), inputTokenLimit: "1", outputTokenLimit: "1" })).price).toBeNull(); expect(validateDraft(emptyDraft("generation"))).toHaveProperty(["limits.input"]); });
  it("defaults text protocols per connection profile, in protocol order, and one protocol for other types", () => {
    expect(defaultProtocols("generation", "anthropic")).toEqual(["chat_completions", "messages"]);
    expect(defaultProtocols("generation", "bedrock")).toEqual(["chat_completions", "messages"]);
    expect(defaultProtocols("generation", "openai")).toEqual(["chat_completions", "responses"]);
    expect(defaultProtocols("generation", "openrouter")).toEqual(["chat_completions"]);
    expect(defaultProtocols("generation", "vllm")).toEqual(["chat_completions"]);
    expect(defaultProtocols("generation")).toEqual(["chat_completions"]);
    expect(defaultProtocols("embeddings", "openai")).toEqual(["embeddings"]);
  });
  it("knows which types and protocols a profile serves, assuming support for an unknown profile", () => {
    expect(protocolSupported("responses", "anthropic")).toBe(false); expect(protocolSupported("messages", "anthropic")).toBe(true);
    expect(protocolSupported("responses", undefined)).toBe(true); expect(protocolSupported("responses", "future-profile")).toBe(true);
    expect(workloadGroups.filter(g => workloadSupported(g.workload, "anthropic")).map(g => g.label)).toEqual(["Text"]);
    expect(workloadGroups.filter(g => !workloadSupported(g.workload, "openai")).map(g => g.label)).toEqual(["Rerank", "System One", "Video"]);
    // Video has no supported provider on any connection, even an unknown profile.
    expect(protocolSupported("videos", "future-profile")).toBe(false); expect(protocolSupported("videos", undefined)).toBe(false);
    for (const profile of ["openai", "openrouter", "anthropic", undefined]) expect(workloadDisabledReason("videos", profile, "OpenAI")).toMatch(/^No supported provider yet\./);
    expect(workloadDisabledReason("rerank", "openai", "OpenAI")).toBe("Not available on OpenAI"); expect(workloadDisabledReason("batches", "openai", "OpenAI")).toBeUndefined();
    expect(workloadGroups.filter(g => !workloadSupported(g.workload, "openrouter")).map(g => g.label)).toEqual(["Realtime audio", "Video", "Batch"]);
  });
  it("keeps help to the API-name hint and uses examples in the provider's format", () => {
    const fields = [...setupSourceFields(choices, "anthropic"), ...setupIdentityFields("anthropic")];
    expect(fields.filter(f => f.help).map(f => [f.name, f.help])).toEqual([["public_name", "What clients send as model."]]);
    expect(Object.fromEntries(fields.filter(f => f.placeholder).map(f => [f.name, f.placeholder]))).toEqual({ upstream_model: "claude-sonnet-4-5", display_name: "Claude Sonnet 4.5", public_name: "claude-sonnet-4-5", description: "What it's good for" });
    expect(JSON.stringify(workloadGroups)).not.toMatch(/POST|\/v1\/|priced/);
  });
  it("rejects unknown connections, catalogs and invalid API names", () => { const errors = validateFields(setupFields(choices, filled), { ...filled, provider_connection_id: "forged", catalog_ids: '["forged"]', public_name: "bad name", supported_protocols: "[]" }); expect(Object.keys(errors).sort()).toEqual(["catalog_ids", "provider_connection_id", "public_name", "supported_protocols"]); });
});

describe("setup checklist from server counts (contract §4)", () => {
  const setup: PlatformOverviewData["setup"] = { connections: 1, enabled_connections: 1, models: 2, ready_models: 0, enabled_routes: 2, priced_enabled_routes: 1, catalogs: 1, type_defaults: { personal: 0, team: 1, project: 0 }, entitled_users: 1, oidc_mappings: 0 };
  it("marks steps done from real state only", () => { const steps = setupSteps(setup); expect(steps.map(s => [s.id, s.done])).toEqual([["connection", true], ["model", true], ["pricing", false], ["offer", false], ["defaults", false], ["access", false]]); expect(steps[4].description).toContain("Personal, Project");
    // Each workspace type needs a default; the installation budget is an optional extra step (only when the overview reports budgets).
    expect(setupSteps({ ...setup, type_defaults: { personal: 1, team: 1, project: 1 } })[4].done).toBe(true);
    expect(setupSteps(setup, []).at(-1)).toMatchObject({ id: "budget", done: false, optional: true }); expect(setupSteps(setup, [{}]).at(-1)?.done).toBe(true); expect(steps[2].description).toContain("1 enabled route is unpriced"); });
  it("explains a disabled connection and an empty install", () => { const empty = setupSteps({ ...setup, enabled_connections: 0, enabled_routes: 0, priced_enabled_routes: 0, type_defaults: { personal: 0, team: 0, project: 0 } }); expect(empty[0]).toMatchObject({ done: false, description: "1 connection, none enabled." }); expect(empty[2].done).toBe(false); expect(empty[4].done).toBe(false); });
  it("counts access from SSO mappings or another entitled user", () => { expect(setupSteps({ ...setup, oidc_mappings: 1 })[5].done).toBe(true); expect(setupSteps({ ...setup, entitled_users: 2 })[5].done).toBe(true); });
});

describe("workload-grouped protocols", () => {
  it("offers every server protocol grouped by workload, none labelled as planned (all are served)", () => {
    expect(protocolOptions.map(o => o.value)).toEqual(["chat_completions", "responses", "messages", "embeddings", "images", "audio_transcriptions", "audio_speech", "rerank", "systemone", "realtime", "videos", "batches"]);
    expect(new Set(protocolOptions.map(o => o.group)).size).toBe(10);
    expect(protocolOptions.find(o => o.value === "rerank")?.label).toBe("Rerank");
    expect(JSON.stringify([protocolOptions, workloadGroups])).not.toMatch(/planned|not served/i);
    expect(protocolLabel("audio_transcriptions")).toBe("Speech to text");
    expect(protocolLabel("audio_speech")).toBe("Text to speech");
    expect(protocolLabel("responses")).toBe("Responses");
  });
  it("mirrors the server's workload-compatibility rule", () => {
    expect(protocolSetError(JSON.stringify(["chat_completions", "responses", "messages"]))).toBeUndefined();
    expect(protocolSetError(JSON.stringify(["images"]))).toBeUndefined();
    expect(protocolSetError(JSON.stringify(["chat_completions", "embeddings"]))).toMatch(/own model/);
    expect(protocolSetError(JSON.stringify(["rerank", "systemone"]))).toMatch(/own model/);
  });
});

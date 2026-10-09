import type { Model, ModelProtocol, ModelReadiness, ModelSetupBody, PlatformOverviewData, ServerPolicy } from "./api";
import { checkboxValues, parseCheckboxValues, type Field, type Values } from "./forms";
import type { WorkloadKind } from "./governance";
import { compareDecimal, workloadOf, type PriceBody } from "./pricing";
import type { CatalogType } from "./permissions";

// ---------------------------------------------------------------------------
// Readiness (contract §2). The server returns counts; this derivation is UI-only.
// Missing readiness is unknown, never "not ready" or "ready".
// ---------------------------------------------------------------------------
export type ReadinessWarning = "disabled" | "no_route" | "unpriced" | "not_offered" | "token_ceiling" | "free_blocked" | "provider_retired";
export const readinessLabels: Record<ReadinessWarning, string> = { disabled: "Disabled", no_route: "No enabled route", unpriced: "Unpriced route", not_offered: "Not offered", token_ceiling: "Token ceilings exceed a tokens-per-minute default", free_blocked: "Free OpenRouter route blocked by the data-collection policy", provider_retired: "Provider API retired" };
export type Readiness = { state: "ready" | "needs_attention" | "needs_setup" | "not_serving" | "retired" | "unknown"; warnings: ReadinessWarning[] };
/**
 * Workloads whose provider API no longer exists, with the reason. OpenAI shut down the Sora 2 models and the Videos
 * API on 2026-09-24 with no replacement; the gateway refuses new video jobs (unsupported_capability). An OpenRouter
 * video adapter is planned.
 */
export const retiredWorkloads: Partial<Record<WorkloadKind, string>> = {
  videos: "No supported provider yet. OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24; an OpenRouter video adapter is planned.",
};
/** Why a workload type can't be chosen on a connection, or undefined when it can. */
export function workloadDisabledReason(workload: WorkloadKind, profile?: string, providerName = "this connection"): string | undefined {
  return retiredWorkloads[workload] ?? (workloadSupported(workload, profile) ? undefined : `Not available on ${providerName}`);
}
/**
 * The one readiness vocabulary (review rule 3): Ready / Needs setup / Needs attention / Not serving.
 * "Not serving" = an enabled model with no enabled route (requests fail); workspaces show only Ready / Not serving + a reason.
 */
export const readinessText: Record<Readiness["state"], string> = { ready: "Ready", needs_setup: "Needs setup", needs_attention: "Needs attention", not_serving: "Not serving", retired: "Provider API retired", unknown: "Readiness unknown" };
/**
 * `policy` is the server's provider policy when known. Configuration checks are best effort: a route whose token
 * ceilings exceed the smallest applicable type-default tokens-per-minute limit is refused for those workspaces, and
 * OpenRouter `:free` endpoints may train on prompts, so they are unavailable when data collection is denied.
 */
export function modelReadiness(model: Pick<Model, "enabled"> & { supported_protocols?: readonly string[]; readiness?: ModelReadiness | null }, policy?: ServerPolicy): Readiness {
  // A retired provider API can never serve, whatever the routes say (video: OpenAI Videos API, shut down 2026-09-24).
  if (model.supported_protocols?.length && retiredWorkloads[workloadOf(model.supported_protocols as ModelProtocol[])]) return { state: "retired", warnings: ["provider_retired"] };
  const r = model.readiness;
  if (!r) return { state: "unknown", warnings: [] };
  const warnings: ReadinessWarning[] = [];
  if (!model.enabled) warnings.push("disabled");
  if (r.enabled_routes <= 0) warnings.push("no_route");
  else if (r.priced_enabled_routes < r.enabled_routes) warnings.push("unpriced");
  const offered = r.catalogs > 0 || r.direct_workspaces > 0;
  if (!offered) warnings.push("not_offered");
  if ((r.routes_over_token_limit ?? 0) > 0) warnings.push("token_ceiling");
  if (policy?.openrouter.data_collection === "deny" && (r.openrouter_free_routes ?? 0) > 0) warnings.push("free_blocked");
  // Pricing is a warning, not a readiness requirement: unpriced usage is recorded as unknown cost.
  const ready = model.enabled && r.enabled_routes > 0 && offered;
  return { state: model.enabled && r.enabled_routes <= 0 ? "not_serving" : !ready ? "needs_setup" : warnings.some(w => w === "token_ceiling" || w === "free_blocked") ? "needs_attention" : "ready", warnings };
}

// ---------------------------------------------------------------------------
// Add model form (contract "Frontend": /admin/models/new).
// ---------------------------------------------------------------------------
export const API_NAME_PATTERN = /^[A-Za-z0-9/_.:-]+$/;
/** An API model name from free text (Grounded's slugKey, widened to OMG alias characters). */
export function apiNameFrom(text: string): string {
  return text.trim().toLowerCase().replace(/[^a-z0-9/_.:-]+/g, "-").replace(/-{2,}/g, "-").replace(/^[-/._:]+|[-/._:]+$/g, "").slice(0, 200);
}
/**
 * The connection the form uses: the one chosen, else the one asked for (?connection=), else the first.
 * Derived rather than stored once, so a form opened by its address renders correctly before options load
 * (pattern from Grounded pages/admin/models/model-dialog.tsx `chosenConnection`).
 */
export function chosenConnection(chosen: string, connections: { id: string }[], wanted?: string): string {
  if (connections.some(c => c.id === chosen)) return chosen;
  if (wanted && connections.some(c => c.id === wanted)) return wanted;
  return connections[0]?.id ?? "";
}
// Workload groups: generation protocols combine; every other kind stands alone.
export const protocolOptions: { value: ModelProtocol; label: string; group: string }[] = [
  { value: "chat_completions", label: "Chat Completions", group: "Text generation" },
  { value: "responses", label: "Responses", group: "Text generation" },
  { value: "messages", label: "Messages", group: "Text generation" },
  { value: "embeddings", label: "Embeddings", group: "Embeddings" },
  { value: "images", label: "Image generation", group: "Images" },
  { value: "audio_transcriptions", label: "Speech to text", group: "Speech to text" },
  { value: "audio_speech", label: "Text to speech", group: "Text to speech" },
  { value: "rerank", label: "Rerank", group: "Rerank" },
  { value: "systemone", label: "System One decisions", group: "System One decisions" },
  { value: "realtime", label: "Realtime (WebSocket)", group: "Realtime audio" },
  { value: "videos", label: "Video generation (async job)", group: "Video" },
  { value: "batches", label: "Batch chat completions (async job)", group: "Batch" },
];
/**
 * The Add model Type choices, short labels only (API paths and units live in docs/protocol-matrix.md and on the
 * model page). Text protocols combine; every other type is one protocol.
 */
export const workloadGroups: { workload: WorkloadKind; label: string; protocols: typeof protocolOptions }[] = ([
  ["generation", "Text"], ["embeddings", "Embeddings"], ["images", "Images"], ["audio_transcriptions", "Speech to text"],
  ["audio_speech", "Text to speech"], ["rerank", "Rerank"], ["systemone", "System One"], ["realtime", "Realtime audio"],
  ["videos", "Video"], ["batches", "Batch"],
] as const).map(([workload, label]) => ({ workload, label, protocols: protocolOptions.filter(o => workloadOf([o.value]) === workload) }));
const protocolNames: Record<string, string> = { images: "Image generation", audio_transcriptions: "Speech to text", audio_speech: "Text to speech", rerank: "Rerank", systemone: "System One decisions", realtime: "Realtime audio", videos: "Video generation", batches: "Batch chat completions" };
export const protocolLabel = (p: string) => protocolNames[p] ?? protocolOptions.find(o => o.value === p)?.label ?? p;
const generationProtocols = new Set(["chat_completions", "responses", "messages"]);
/** Mirrors the server: one workload per model; only text-generation protocols combine. */
export function protocolSetError(value: string): string | undefined {
  let selected: unknown;
  try { selected = JSON.parse(value); } catch { return; }
  if (!Array.isArray(selected) || selected.length < 2) return;
  if (!selected.every(p => generationProtocols.has(String(p)))) return "Only Chat Completions, Responses and Messages can be combined; each other workload needs its own model.";
}
/**
 * Checking a protocol from another workload replaces the selection (workloads are mutually exclusive);
 * text-generation protocols add to each other. Unchecking only removes.
 */
export function toggleProtocol(selected: string[], protocol: string, checked: boolean): string[] {
  if (!checked) return selected.filter(p => p !== protocol);
  if (selected.includes(protocol)) return selected;
  return selected.length && workloadOf(selected) === workloadOf([protocol]) ? [...selected, protocol] : [protocol];
}
export type SetupChoices = { connections: { id: string; name: string; provider?: string; enabled?: boolean }[]; catalogs: { id: string; name: string }[] };
/**
 * Add model defaults that follow the connection's profile (review #23): the protocols a new text model starts with
 * (Anthropic and Bedrock: Messages + Chat Completions; OpenAI: Chat Completions + Responses; others: Chat Completions),
 * or the single protocol of any other workload. Always in protocolOptions order.
 */
export function defaultProtocols(workload: WorkloadKind, profile?: string): ModelProtocol[] {
  if (workload !== "generation") return [workloadGroups.find(g => g.workload === workload)?.protocols[0]?.value ?? "chat_completions"];
  const wanted: ModelProtocol[] = profile === "anthropic" || profile === "bedrock" ? ["chat_completions", "messages"] : profile === "openai" ? ["chat_completions", "responses"] : ["chat_completions"];
  return wanted.filter(p => protocolSupported(p, profile));
}
/** An example upstream model ID in the connection provider's own format, per workload. */
export function upstreamPlaceholder(profile?: string, workload: WorkloadKind = "generation"): string {
  const byWorkload: Partial<Record<WorkloadKind, Record<string, string>>> = {
    embeddings: { openai: "text-embedding-3-small", openrouter: "openai/text-embedding-3-small", ollama: "nomic-embed-text", default: "BAAI/bge-m3" },
    images: { openai: "gpt-image-1", default: "openai/gpt-image-1" },
    audio_transcriptions: { openai: "gpt-4o-transcribe", default: "openai/whisper-1" },
    audio_speech: { openai: "gpt-4o-mini-tts", default: "openai/gpt-4o-mini-tts" },
    rerank: { default: "cohere/rerank-v3.5" },
    systemone: { default: "typesafe/jev" },
    realtime: { default: "gpt-realtime" },
    videos: { default: "sora-2" },
    batches: { default: "gpt-4.1-mini" },
  };
  const text: Record<string, string> = { openai: "gpt-4.1-mini", anthropic: "claude-sonnet-4-5", bedrock: "anthropic.claude-3-5-sonnet-20240620-v1:0", openrouter: "openai/gpt-4.1-mini", ollama: "llama3.1:8b", vllm: "meta-llama/Llama-3.1-8B-Instruct", sglang: "meta-llama/Llama-3.1-8B-Instruct", openai_compatible: "meta-llama/Llama-3.1-8B-Instruct", default: "gpt-4.1-mini" };
  const table = workload === "generation" ? text : byWorkload[workload] ?? text;
  return table[profile ?? "default"] ?? table.default ?? "gpt-4.1-mini";
}
/** An example display name matching upstreamPlaceholder, per workload. */
export function displayPlaceholder(profile?: string, workload: WorkloadKind = "generation"): string {
  const byWorkload: Partial<Record<WorkloadKind, Record<string, string>>> = {
    embeddings: { ollama: "Nomic Embed Text", openai: "Text Embedding 3 Small", openrouter: "Text Embedding 3 Small", default: "BGE-M3" },
    images: { default: "GPT Image 1" },
    audio_transcriptions: { openai: "GPT-4o Transcribe", default: "Whisper" },
    audio_speech: { default: "GPT-4o mini TTS" },
    rerank: { default: "Rerank 3.5" },
    systemone: { default: "Jev" },
    realtime: { default: "GPT Realtime" },
    videos: { default: "Sora 2" },
    batches: { default: "GPT-4.1 mini (batch)" },
  };
  const text: Record<string, string> = { openai: "GPT-4.1 mini", openrouter: "GPT-4.1 mini", anthropic: "Claude Sonnet 4.5", bedrock: "Claude 3.5 Sonnet", ollama: "Llama 3.1 8B", default: "Llama 3.1 8B Instruct" };
  const table = workload === "generation" ? text : byWorkload[workload] ?? text;
  return table[profile ?? "default"] ?? table.default;
}
/** Whether a connection profile's adapter implements a client protocol (no or unrecognised profile: assume yes, unless no adapter does). */
export function protocolSupported(protocol: ModelProtocol, profile?: string): boolean {
  if (!protocolProfiles[protocol].length) return false;
  if (!profile || !Object.values(protocolProfiles).some(list => list.includes(profile))) return true;
  return protocolProfiles[protocol].includes(profile);
}
/** Whether a connection profile's adapter can carry any protocol of the workload (unknown profile: assume yes). */
export const workloadSupported = (workload: WorkloadKind, profile?: string) => (workloadGroups.find(g => g.workload === workload)?.protocols ?? []).some(p => protocolSupported(p.value, profile));
export function setupSourceFields(choices: SetupChoices, profile?: string, workload: WorkloadKind = "generation"): Field[] {
  return [
    { name: "provider_connection_id", label: "Connection", type: "select", required: true, options: choices.connections.map(c => ({ value: c.id, label: `${c.name}${c.enabled === false ? " · disabled" : ""}` })) },
    { name: "upstream_model", label: "Upstream model ID", required: true, maxLength: 512, placeholder: upstreamPlaceholder(profile, workload) },
    { name: "supported_protocols", label: "Protocols", type: "checkboxes", required: true, maxSelections: 3, options: protocolOptions, validate: protocolSetError },
  ];
}
export function setupIdentityFields(profile?: string, workload: WorkloadKind = "generation"): Field[] {
  return [
    { name: "display_name", label: "Display name", required: true, maxLength: 120, placeholder: displayPlaceholder(profile, workload) },
    // The one hint on the page: the name is easy to confuse with the display name or the upstream ID.
    { name: "public_name", label: "API model name", required: true, maxLength: 200, placeholder: apiNameFrom(upstreamPlaceholder(profile, workload)), help: "What clients send as model.", validate: v => API_NAME_PATTERN.test(v) ? undefined : "Use letters, digits, slash, hyphen, underscore, dot or colon." },
    { name: "description", label: "Description", type: "textarea", maxLength: 2000, placeholder: "What it's good for" },
  ];
}
export function setupAvailabilityFields(choices: SetupChoices): Field[] {
  return [{ name: "catalog_ids", label: "Catalogs", type: "checkboxes", maxSelections: 200, options: choices.catalogs.map(c => ({ value: c.id, label: c.name })) }];
}
/** Every Field-based input. Pricing is a v3 line draft (lib/pricing.ts), validated separately. */
export function setupFields(choices: SetupChoices, _values?: Values): Field[] {
  return [...setupSourceFields(choices), ...setupIdentityFields(), ...setupAvailabilityFields(choices)];
}
export function initialSetupValues(): Values {
  return { provider_connection_id: "", upstream_model: "", supported_protocols: JSON.stringify(["chat_completions"]), display_name: "", public_name: "", description: "", catalog_ids: "[]", pricing: "unpriced", enabled: "false" };
}
/** The request body (contract §1). `price` is sent only when "Set a price now" is chosen. Throws on malformed checkbox values; validate first. */
export function setupBody(values: Values, choices: SetupChoices, price: PriceBody | null = null): ModelSetupBody {
  const [, , protocols] = setupSourceFields(choices);
  const enabled = values.enabled === "true";
  return {
    model: { public_name: values.public_name.trim(), display_name: values.display_name.trim(), description: values.description.trim() || null, supported_protocols: checkboxValues(protocols, values.supported_protocols) as ModelProtocol[], enabled },
    route: { provider_connection_id: values.provider_connection_id, upstream_model: values.upstream_model.trim(), enabled },
    price: values.pricing === "priced" ? price : null,
    catalog_ids: checkboxValues(setupAvailabilityFields(choices)[0], values.catalog_ids),
  };
}
export const selectedIds = (value: string) => { try { return parseCheckboxValues(value); } catch { return []; } };

// ---------------------------------------------------------------------------
// Models catalog (ux-api-contract §10). Filters stay in the URL; rows come from
// GET /platform/models (admin) or /workspaces/{ws}/catalog (workspace).
// ---------------------------------------------------------------------------
/** Catalog row: a model plus the list-only fields (absent from older gateways: unknown, never zero). */
export type CatalogModel = Model & { workload?: WorkloadKind; created_at?: string; min_input_microusd_per_million?: string | null };
export type Eligibility = "selected" | "direct" | "available_from_catalog";
/** Workspace catalog row (contract §10). */
export type WorkspaceCatalogModel = { model_id: string; public_name: string; display_name: string; description: string | null; protocols: ModelProtocol[]; workload: WorkloadKind; eligibility: Eligibility; reason: string; min_input_microusd_per_million: string | null; min_output_microusd_per_million: string | null; routes: number | string; /** When the model was added (wave 2; absent from older gateways). */ created_at?: string };
export const catalogTypeTabs: { value: CatalogType | "all"; label: string }[] = [{ value: "all", label: "All" }, { value: "generation", label: "Text" }, { value: "embeddings", label: "Embeddings" }, { value: "images", label: "Images" }, { value: "audio_transcriptions", label: "Speech to text" }, { value: "audio_speech", label: "Text to speech" }, { value: "rerank", label: "Rerank" }, { value: "systemone", label: "System One" }, { value: "realtime", label: "Realtime" }, { value: "videos", label: "Video" }, { value: "batches", label: "Batch" }];
/** Input → output modalities of a workload, for cards and header tiles. */
export const workloadModalities: Record<WorkloadKind, string> = { generation: "Text → Text", embeddings: "Text → Vectors", images: "Text → Image", audio_transcriptions: "Audio → Text", audio_speech: "Text → Audio", rerank: "Text → Scores", systemone: "Text → Decisions", realtime: "Audio ⇄ Audio", videos: "Text → Video (async)", batches: "JSONL → JSONL (async)" };
export const catalogSorts = [{ value: "name", label: "Name (A–Z)" }, { value: "price", label: "Input price: low to high" }, { value: "newest", label: "Newest" }] as const;
export type CatalogSort = typeof catalogSorts[number]["value"];
export const modelWorkload = (m: Pick<CatalogModel, "workload" | "supported_protocols">): WorkloadKind => m.workload ?? workloadOf(m.supported_protocols);
/** Per-tab counts from the rows already filtered by everything except the type (as the server counts). */
export function typeCounts(rows: { workload: WorkloadKind }[]): Record<CatalogType | "all", number> {
  const counts = Object.fromEntries(catalogTypeTabs.map(t => [t.value, 0])) as Record<CatalogType | "all", number>;
  for (const r of rows) { counts.all += 1; counts[r.workload] += 1; }
  return counts;
}
export type CatalogFilters = { connections?: string[]; enabled?: "true" | "false"; readiness?: Readiness["state"][]; minPrice?: string; maxPrice?: string };
/**
 * Client-side facets (the server filters q, data policy, deprecated and the price ceiling). Connection matches any
 * route on it, like the server's provider_connection_id filter. A price bound excludes models whose price is unknown.
 */
export function filterCatalog<T extends CatalogModel>(rows: T[], f: CatalogFilters, policy?: ServerPolicy): T[] {
  return rows.filter(m => {
    if (f.connections?.length && !m.readiness?.connections.some(c => f.connections!.includes(c.id))) return false;
    if (f.enabled && String(m.enabled) !== f.enabled) return false;
    if (f.readiness?.length && !f.readiness.includes(modelReadiness(m, policy).state)) return false;
    if (f.minPrice !== undefined && (m.min_input_microusd_per_million == null || compareDecimal(m.min_input_microusd_per_million, f.minPrice) < 0)) return false;
    if (f.maxPrice !== undefined && (m.min_input_microusd_per_million == null || compareDecimal(m.min_input_microusd_per_million, f.maxPrice) > 0)) return false;
    return true;
  });
}
/** Name, cheapest input price (unknown last) or newest first; ties by API name. Stable across merged queries. */
/** Name order is the display name people read ("GLM 5.3 Flash" under G, not under its API name z-ai/…), then the API name. */
export function sortCatalog<T extends { public_name: string; display_name?: string | null; min_input_microusd_per_million?: string | null; created_at?: string }>(rows: T[], sort: CatalogSort = "name"): T[] {
  const byName = (a: T, b: T) => (a.display_name || a.public_name).localeCompare(b.display_name || b.public_name, undefined, { sensitivity: "base" }) || a.public_name.localeCompare(b.public_name);
  return [...rows].sort((a, b) => sort === "price" ? compareDecimal(a.min_input_microusd_per_million, b.min_input_microusd_per_million) || byName(a, b) : sort === "newest" ? (b.created_at ?? "").localeCompare(a.created_at ?? "") || byName(a, b) : byName(a, b));
}
export const eligibilityLabels: Record<Eligibility, string> = { selected: "Added", direct: "Assigned by admin", available_from_catalog: "Available to add" };

// ---------------------------------------------------------------------------
// Routes: which are used by default, which only as fallback (routing.rs order).
// ---------------------------------------------------------------------------
export type RouteState = { id: string; enabled: boolean; connectionEnabled?: boolean; priority?: number; weight?: number };
export type RoutePolicy = { strategy: "priority" | "weighted"; max_attempts: number };
export type RouteTiers<T> = { primary: T[]; fallback: T[]; /** Serving routes whose order is unknown (routing not loaded). */ unknown: T[]; inactive: T[]; fallbackReason: string; tieNote?: string };
/**
 * Mirrors the gateway's candidate order: enabled routes on enabled connections sorted by priority (lower first).
 * Priority strategy uses the first route; weighted splits traffic within the lowest-priority tier. The rest are
 * fallback only: tried on an allowed failure when max attempts > 1 (with matching asserted residency), otherwise
 * only while the routes above are disabled or cooling down. A route whose routing (or the model policy) is unknown
 * is listed apart, never presented as default or fallback.
 */
export function routeTiers<T extends RouteState>(routes: T[], policy?: RoutePolicy): RouteTiers<T> {
  const active = routes.filter(r => r.enabled && r.connectionEnabled !== false), inactive = routes.filter(r => !active.includes(r));
  if (!policy) return { primary: [], fallback: [], unknown: active, inactive, fallbackReason: "" };
  const known = active.filter(r => r.priority !== undefined).sort((a, b) => a.priority! - b.priority!), unknown = active.filter(r => r.priority === undefined);
  const top = known[0]?.priority, tier = known.filter(r => r.priority === top);
  const primary = !tier.length ? [] : policy.strategy === "weighted" ? tier : [tier[0]];
  const fallback = known.filter(r => !primary.includes(r));
  const fallbackReason = policy.max_attempts > 1 ? `Tried only after an allowed failure of an earlier route (up to ${policy.max_attempts} attempts; fallback routes must assert the same residency), or while earlier routes are disabled or cooling down.` : "This model allows one attempt, so these routes serve only while the routes above are disabled or cooling down.";
  const tieNote = policy?.strategy === "priority" && tier.length > 1 ? `${tier.length} routes share priority ${top}; the gateway uses its stored order among them, so set distinct priorities to choose.` : undefined;
  return { primary, fallback, unknown, inactive, fallbackReason, tieNote };
}
/** Server data policy (contract §10) to the DataPolicyBadge vocabulary, with plain-language labels. */
export function routeDataPolicy(value: { data_collection?: string; basis?: string } | null | undefined): { policy: "keeps" | "no_keep" | "unknown"; label: string; detail: string } {
  if (value?.data_collection === "deny") return { policy: "no_keep", label: "Data collection denied", detail: "Server setting for this provider" };
  if (value?.data_collection === "allow") return { policy: "keeps", label: "Data collection allowed", detail: "Prompts may be stored or used for training" };
  return { policy: "unknown", label: "Data policy unknown", detail: value?.basis === "not_configured" ? "Not configured for this provider" : "Not reported" };
}
/** Old ?tab= values on the model page, as section anchors. */
export function modelSectionFor(tab?: string): string | undefined {
  return tab === "deployments" ? "routes" : tab && ["routes", "pricing", "routing", "availability"].includes(tab) ? tab : undefined;
}
/** Old ?tab= values on the route page, as section anchors. */
export function routeSectionFor(tab?: string): string | undefined {
  return tab === "pricing" ? "price-history" : tab === "routing" ? "routing" : undefined;
}

// ---------------------------------------------------------------------------
// Client protocols a model serves (docs/protocol-matrix.md). Static facts about
// the gateway's inference surface; serving also needs a route whose adapter
// supports the protocol.
// ---------------------------------------------------------------------------
export type ProtocolEndpoint = { method: "POST" | "GET"; path: string; contentType: string; headers: { name: string; value: string }[]; params: { name: string; note: string; required?: boolean }[]; unsupported: string; adapters: string };
const bearer = { name: "Authorization", value: "Bearer <inference key>" }, json = { name: "Content-Type", value: "application/json" };
export const protocolEndpoints: Record<ModelProtocol, ProtocolEndpoint> = {
  chat_completions: { method: "POST", path: "/v1/chat/completions", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "messages", note: "Text-string messages: system, developer, user, assistant, tool", required: true }, { name: "max_completion_tokens", note: "Generation bound (needed when the price requires one)" }, { name: "temperature", note: "Sampling temperature" }, { name: "tools / tool_choice", note: "Function tools and tool results" }, { name: "stream", note: "With optional stream_options.include_usage" }], unsupported: "n > 1, legacy max_tokens, multimodal content arrays, structured output, reasoning, log probabilities and other extensions are rejected.", adapters: "OpenAI (native), Anthropic and Bedrock (representable subset), OpenRouter and local profiles (compatible subset)." },
  responses: { method: "POST", path: "/v1/responses", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "input", note: "String or item list", required: true }, { name: "instructions", note: "System text" }, { name: "max_output_tokens", note: "Generation bound" }, { name: "temperature", note: "Sampling temperature" }, { name: "tools / tool_choice", note: "Flat function tools" }, { name: "stream", note: "Server-sent events" }, { name: "store", note: "Only false" }], unsupported: "Stateful use (store:true, previous_response_id, background), hosted tools, structured output, reasoning items, images and audio are rejected.", adapters: "OpenAI only (native)." },
  messages: { method: "POST", path: "/v1/messages", contentType: "application/json", headers: [{ name: "Authorization or x-api-key", value: "<inference key> (one, never both)" }, { name: "anthropic-version", value: "2023-06-01" }, json], params: [{ name: "model", note: "API model name", required: true }, { name: "messages", note: "Text blocks, tool_use and tool_result", required: true }, { name: "max_tokens", note: "Positive", required: true }, { name: "system", note: "System text" }, { name: "temperature", note: "0–1" }, { name: "tools / tool_choice", note: "auto, none, any or a named tool" }, { name: "stream", note: "Buffered: events are sent after validated completion" }], unsupported: "Beta headers, thinking, images and audio, cache controls, metadata and error-marked tool results are rejected.", adapters: "Anthropic (native) and Bedrock (Converse)." },
  embeddings: { method: "POST", path: "/v1/embeddings", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "input", note: "String or up to 128 strings (1 MiB total)", required: true }, { name: "encoding_format", note: "Only float" }, { name: "dimensions", note: "Where the profile supports it" }], unsupported: "Token-ID arrays, base64 output and streaming are rejected. Input-only: no output tokens are billed.", adapters: "OpenAI, OpenRouter, Ollama and OpenAI-compatible local profiles." },
  images: { method: "POST", path: "/v1/images/generations", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "prompt", note: "Up to 32,000 characters", required: true }, { name: "n", note: "1–4 images" }, { name: "size", note: "Adapter-specific sizes or tiers" }, { name: "quality", note: "Adapter-specific" }, { name: "response_format", note: "Only b64_json" }], unsupported: "URL responses, streaming and fields such as background, style or output_format are rejected; the gateway never hosts files.", adapters: "OpenAI (gpt-image models) and OpenRouter." },
  audio_transcriptions: { method: "POST", path: "/v1/audio/transcriptions", contentType: "multipart/form-data", headers: [bearer, { name: "Content-Type", value: "multipart/form-data; boundary=…" }], params: [{ name: "file", note: "flac, mp3, mp4, mpeg, mpga, m4a, ogg, wav or webm", required: true }, { name: "model", note: "API model name", required: true }, { name: "language", note: "2–3 letter code" }, { name: "prompt", note: "Up to 4096 bytes" }, { name: "response_format", note: "json or text" }, { name: "temperature", note: "0–1" }], unsupported: "verbose_json, srt, vtt, timestamps, chunking and streaming are rejected.", adapters: "OpenAI and OpenRouter." },
  audio_speech: { method: "POST", path: "/v1/audio/speech", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "input", note: "Text to speak", required: true }, { name: "voice", note: "Voice name", required: true }, { name: "response_format", note: "mp3, wav, opus or pcm (adapter-dependent)" }, { name: "speed", note: "0.25–4" }], unsupported: "aac and flac output, instructions and SSE streaming are rejected.", adapters: "OpenAI and OpenRouter (mp3 and pcm only)." },
  rerank: { method: "POST", path: "/v1/rerank", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "query", note: "Up to 32 KiB", required: true }, { name: "documents", note: "Up to 1000 strings", required: true }, { name: "top_n", note: "At least 1" }], unsupported: "return_documents, object documents and provider options are rejected; documents are never echoed.", adapters: "OpenRouter." },
  systemone: { method: "POST", path: "/v1/systemone", contentType: "application/json", headers: [bearer, json], params: [{ name: "model", note: "API model name", required: true }, { name: "state", note: "String, object or array", required: true }, { name: "questions", note: "1–64 noul, choice or score questions", required: true }], unsupported: "Image parts, provider and user fields are rejected (422 for validation failures).", adapters: "OpenRouter." },
  realtime: { method: "GET", path: "/v1/realtime?model=<API model name>", contentType: "WebSocket upgrade; JSON text events", headers: [{ name: "Authorization or subprotocol", value: "Bearer <inference key>, or openai-insecure-api-key.<inference key> (one, never both)" }], params: [{ name: "model", note: "API model name (query parameter)", required: true }, { name: "session.update", note: "Realtime sessions; VAD only with create_response false" }, { name: "response.create", note: "One response at a time; max_output_tokens within the gateway ceiling" }, { name: "input_audio_buffer.*, conversation.item.*", note: "Audio and text input; no image input" }], unsupported: "Beta interface, client secrets, WebRTC/SIP, input transcription, automatic VAD responses, out-of-band responses, MCP tools and image input are rejected with an error event.", adapters: "OpenAI (GA interface)." },
  videos: { method: "POST", path: "/v1/videos", contentType: "multipart/form-data or application/json", headers: [bearer], params: [{ name: "model", note: "API model name", required: true }, { name: "prompt", note: "Up to 32 KiB", required: true }, { name: "seconds", note: "4, 8 or 12 (default 4, always sent)" }, { name: "size", note: "720x1280 (default), 1280x720, 1024x1792 or 1792x1024" }, { name: "GET /v1/videos/{id}[/content], DELETE", note: "Same workspace's keys only" }], unsupported: "Every request is refused (unsupported_capability): OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24. input_reference, remix, edits, extensions and characters were never supported.", adapters: "None yet. An OpenRouter video adapter is planned." },
  batches: { method: "POST", path: "/v1/files (purpose=batch), then /v1/batches", contentType: "multipart/form-data (JSONL file); application/json", headers: [bearer], params: [{ name: "file", note: "JSONL, one /v1/chat/completions request per line, ≤ 50,000 lines", required: true }, { name: "body.model", note: "This model's API name on every line", required: true }, { name: "body.max_completion_tokens", note: "Required per line (bounds the hold)", required: true }, { name: "input_file_id, endpoint, completion_window", note: "/v1/chat/completions and 24h only", required: true }], unsupported: "Other batch endpoints, n > 1, streaming, audio output, web search, predicted outputs and expires_after are rejected.", adapters: "OpenAI." },
};
/** Connection profiles whose adapter implements each client protocol (docs/protocol-matrix.md). */
export const protocolProfiles: Record<ModelProtocol, string[]> = {
  chat_completions: ["openai", "anthropic", "bedrock", "openai_compatible", "vllm", "sglang", "ollama", "openrouter"],
  responses: ["openai"],
  messages: ["anthropic", "bedrock"],
  embeddings: ["openai", "openai_compatible", "vllm", "sglang", "ollama", "openrouter"],
  images: ["openai", "openrouter"],
  audio_transcriptions: ["openai", "openrouter"],
  audio_speech: ["openai", "openrouter"],
  rerank: ["openrouter"],
  systemone: ["openrouter"],
  realtime: ["openai"],
  // Async jobs (docs/async-jobs.md). Video: no adapter since OpenAI shut down its Videos API (2026-09-24);
  // an OpenRouter video adapter (another API shape) is planned.
  videos: [],
  batches: ["openai"],
};

// ---------------------------------------------------------------------------
// Admin overview checklist (contract §4). Done states come only from server counts.
// ---------------------------------------------------------------------------
export type SetupStepId = "connection" | "model" | "pricing" | "offer" | "defaults" | "access" | "budget";
/** `optional` steps are offered but never block "Setup complete". */
export type SetupStepState = { id: SetupStepId; title: string; description: string; done: boolean; optional?: boolean };
/** Required steps first; an installation budget is the optional last step (known only when the overview reports budgets). */
export function setupSteps(setup: PlatformOverviewData["setup"], installationBudgets?: unknown[] | null): SetupStepState[] {
  const types = [["personal", "Personal"], ["team", "Team"], ["project", "Project"]] as const;
  const missing = types.filter(([k]) => !setup.type_defaults[k]).map(([, label]) => label);
  const unpriced = Math.max(0, setup.enabled_routes - setup.priced_enabled_routes);
  return [
    { id: "connection", title: "Connect a model provider", description: setup.connections && !setup.enabled_connections ? `${setup.connections} connection${setup.connections === 1 ? "" : "s"}, none enabled.` : "A provider endpoint with a server-side credential reference.", done: setup.enabled_connections > 0 },
    { id: "model", title: "Add a model with an enabled route", description: setup.models && !setup.enabled_routes ? "Models exist, but none has an enabled route." : "A model sends its requests through a route on a connection.", done: setup.enabled_routes > 0 },
    { id: "pricing", title: "Price every enabled route", description: unpriced ? `${unpriced} enabled route${unpriced === 1 ? " is" : "s are"} unpriced; their cost is recorded as unknown.` : "Configured rates estimate cost and enforce budgets.", done: setup.enabled_routes > 0 && unpriced === 0 },
    { id: "offer", title: "Offer a model in a catalog", description: "Workspaces use models from their available catalogs, or by direct assignment.", done: setup.ready_models > 0 },
    { id: "defaults", title: "Choose default catalogs for each workspace type", description: missing.length ? `No default catalog yet for: ${missing.join(", ")}. New workspaces of that type start with no models.` : "Personal, Team and Project workspaces each get models from their default catalogs.", done: missing.length === 0 },
    { id: "access", title: "Give people access", description: "Map an SSO group or add users. Signing in alone doesn't give access.", done: setup.oidc_mappings > 0 || setup.entitled_users > 1 },
    ...(installationBudgets === undefined ? [] : [{ id: "budget" as const, title: "Set an installation-wide budget (optional)", description: "A spending ceiling across every workspace, on top of team and project limits.", done: !!installationBudgets?.length, optional: true }]),
  ];
}

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ApiError, platformPath, type Deployment, type Model, type Provider, type Session } from "../lib/api";
import { validateFields } from "../lib/forms";
import { ActionProvider } from "./ui";
import { DashboardNavigationProvider } from "./navigation-link";
import { DeploymentDetail, ModelDetail, ProviderDetail } from "../pages/catalog-details";
import { Deployments, Models, Providers, deploymentCreateAction } from "../pages/catalog";

const session: Session = { user: { id: "operator", email: "operator@example.invalid", platform_admin: true }, organizations: [], workspaces: [] };
const model: Model = { id: "model-beyond-page-one", public_name: "company/smart", display_name: "Smart model", enabled: true, personal_enabled: false };
const provider: Provider = { id: "provider", name: "Production provider", provider: "openai", endpoint: null, region: null, enabled: false };
const deployment: Deployment = { id: "deployment", model_id: model.id, provider_connection_id: provider.id, upstream_model: "upstream-v2", enabled: false };
const go = () => {};
const clients: QueryClient[] = [];
function client() {
  const result = new QueryClient({ defaultOptions: { queries: { retry: false, retryOnMount: false, staleTime: Infinity } } });
  clients.push(result);
  return result;
}
function render(node: ReactNode, entries: [string, unknown][] = [], errors: [string, Error][] = [], cache = client()) {
  for (const [path, data] of entries) cache.setQueryData(["api", path], data);
  for (const [path, error] of errors) cache.getQueryCache().build(cache, { queryKey: ["api", path] }).setState({ status: "error", error, fetchStatus: "idle" });
  return renderToStaticMarkup(<QueryClientProvider client={cache}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
afterEach(() => { clients.forEach(cache => cache.clear()); clients.length = 0; vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("catalog detail records", () => {
  it("looks up a model beyond page one directly and never scans catalog choices", async () => {
    const cache = client();
    cache.setQueryData(["api", `${platformPath}/models?limit=100&offset=0`], { data: Array.from({ length: 100 }, (_, i) => ({ ...model, id: `first-page-${i}`, display_name: "Other model" })) });
    const path = `${platformPath}/models/${model.id}`;
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify(model), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const page = <ModelDetail session={session} id={model.id} onTabChange={go} />;
    expect(render(page, [], [], cache)).toContain("Loading model");
    await cache.getQueryCache().find({ queryKey: ["api", path], exact: true })!.fetch();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][0]).toBe(path);
    const html = render(page, [], [], cache);
    expect(html).toContain("Smart model");
    expect(html).toContain("Manage deployments");
    expect(html).toContain("Configure model routing");
    expect(html).toContain('data-variant="pills"');
    expect(html).not.toContain("Create deployment</button>");
    expect(cache.getQueryCache().getAll().some(query => query.queryKey.includes("choices"))).toBe(false);
    expect(cache.getQueryCache().getAll()).toHaveLength(2);
  });
  it.each([
    ["models", ModelDetail, "routing", "Edit model routing"],
    ["providers", ProviderDetail, "deployments", "Create deployment"],
    ["deployments", DeploymentDetail, "pricing", "Publish price version"],
  ] as const)("keeps %s writes unmounted for loading, missing, denied and mismatched reads", (kind, Detail, tab, control) => {
    const path = `${platformPath}/${kind}/requested`;
    const page = <Detail session={session} id="requested" tab={tab} onTabChange={go} />;
    const cache = client();
    expect(render(page, [], [], cache)).toContain("Loading");
    expect(cache.getQueryCache().getAll().map(query => query.queryKey)).toEqual([["api", path]]);
    for (const status of [403, 404]) {
      const failed = render(page, [], [[path, new ApiError(status, "unavailable", "No resource access")]]);
      expect(failed).toContain(status === 403 ? "Access denied" : "Not found");
      expect(failed).not.toContain(control);
    }
    const mismatch = render(page, [[path, { ...model, ...provider, ...deployment, id: "another-resource" }]]);
    expect(mismatch).toContain("different resource");
    expect(mismatch).not.toContain(control);
    expect(mismatch).not.toContain("Enable</button>");
  });
  it("denies cached data to a non-operator and hides stale writes after a failed refresh", () => {
    const path = `${platformPath}/deployments/${deployment.id}`;
    const denied = render(<DeploymentDetail session={{ ...session, user: { ...session.user, platform_admin: false } }} id={deployment.id} tab="pricing" onTabChange={go} />, [[path, deployment]]);
    expect(denied).toContain("Access not available");
    expect(denied).not.toContain("Publish price version");
    const stale = render(<DeploymentDetail session={session} id={deployment.id} tab="pricing" onTabChange={go} />, [[path, deployment]], [[path, new ApiError(403, "denied", "Authorization changed")]]);
    expect(stale).toContain("Access denied");
    expect(stale).not.toContain("Publish price version");
  });
  it("shows safe provider fields, not credentials even if a malformed response includes them", () => {
    const html = render(<ProviderDetail session={session} id={provider.id} onTabChange={go} />, [[`${platformPath}/providers/${provider.id}`, { ...provider, credential_ref: "env:DO_NOT_RENDER", api_key: "DO_NOT_RENDER_SECRET" }]]);
    expect(html).toContain("Production provider");
    expect(html).toContain("Secret reference (hidden)");
    expect(html).toContain("Rotate reference");
    expect(html).not.toContain("DO_NOT_RENDER");
    const workload = render(<ProviderDetail session={session} id={provider.id} onTabChange={go} />, [[`${platformPath}/providers/${provider.id}`, { ...provider, provider: "bedrock" }]]);
    expect(workload).toContain("rotate through AWS");
    expect(workload).not.toContain("Rotate reference</button>");
  });
  it("uses existing routing panels for the matched model and deployment", () => {
    const modelPath = `${platformPath}/models/${model.id}`;
    const modelHtml = render(<ModelDetail session={session} id={model.id} tab="routing" onTabChange={go} />, [[modelPath, model], [`${modelPath}/routing`, { policy: { strategy: "priority", max_attempts: 1, allow_ambiguous_failover: false, failure_threshold: 3, cooldown_seconds: 30, required_residency: "us-east" } }]]);
    expect(modelHtml).toContain("Edit model routing");
    expect(modelHtml).toContain("us-east");
    const deploymentPath = `${platformPath}/deployments/${deployment.id}`;
    const html = render(<DeploymentDetail session={session} id={deployment.id} tab="routing" onTabChange={go} />, [[deploymentPath, deployment], [`${deploymentPath}/routing`, { routing: { priority: 0, weight: 1, residency: "us-east" }, health: { consecutive_failures: 0, open_until: null } }]]);
    expect(html).toContain("Edit deployment routing");
    expect(html).toContain("Unknown · no recorded observation");
    expect(html).not.toContain("Healthy");
  });
  it("uses immutable pricing with exact USD and leaves inactive writable panels unmounted", () => {
    const path = `${platformPath}/deployments/${deployment.id}`;
    const price = { id: "price", input_microusd_per_million: "9007199254740993", output_microusd_per_million: "1", input_token_limit: 128000, output_token_limit: 4096, created_at: "2026-01-01T00:00:00Z" };
    const html = render(<DeploymentDetail session={session} id={deployment.id} tab="pricing" onTabChange={go} />, [[path, deployment], [`${path}/prices?limit=100&offset=0`, { data: [price] }]]);
    expect(html).toContain("$9,007,199,254.740993");
    expect(html).toContain("$0.000001");
    expect(html).toContain("Publish price version");
    expect(html).toContain("Versions are immutable");
    expect(html).not.toContain("Edit deployment routing");
    expect(html).not.toContain("Delete");
    const overview = render(<DeploymentDetail session={session} id={deployment.id} onTabChange={go} />, [[path, deployment]]);
    expect(overview).toContain(`href="/admin/models/${model.id}"`);
    expect(overview).toContain(`href="/admin/providers/${provider.id}"`);
    expect(overview).not.toContain("Publish price version</button>");
  });
});

describe("catalog collections and contextual creation", () => {
  it("links catalog names with native fallback links", () => {
    const models = render(<Models session={session} />, [[`${platformPath}/models?limit=100&offset=0`, { data: [model] }]]);
    const providers = render(<Providers session={session} />, [[`${platformPath}/providers?limit=100&offset=0`, { data: [provider] }]]);
    const deployments = render(<Deployments session={session} />, [[`${platformPath}/deployments?limit=100&offset=0`, { data: [deployment] }]]);
    expect(models).toContain(`href="/admin/models/${model.id}"`);
    expect(providers).toContain(`href="/admin/providers/${provider.id}"`);
    expect(deployments).toContain(`href="/admin/deployments/${deployment.id}"`);
    expect(models).toContain("Smart model");
    expect(providers).toContain("Production provider");
    expect(deployments).toContain("upstream-v2");
  });
  it.each(["model", "provider"] as const)("filters deployments on the server for a fixed %s", kind => {
    const cache = client();
    const isModel = kind === "model";
    const filter = isModel ? `model_id=${model.id}` : `provider_connection_id=${provider.id}`;
    const path = `${platformPath}/deployments?${filter}&limit=100&offset=0`;
    const html = render(<Deployments session={session} modelId={isModel ? model.id : undefined} providerId={isModel ? undefined : provider.id} embedded />, [[`${platformPath}/${isModel ? "models" : "providers"}/${isModel ? model.id : provider.id}`, isModel ? model : provider], [path, { data: [deployment] }]], [], cache);
    expect(html).toContain("Fixed creation context (read-only)");
    expect(html).toContain(isModel ? model.public_name : provider.name);
    expect(html).toContain("Create deployment</button>");
    expect(html).toContain("upstream-v2");
    expect(cache.getQueryCache().getAll().some(query => query.queryKey.includes("choices"))).toBe(false);
    expect(cache.getQueryCache().find({ queryKey: ["api", path], exact: true })).toBeDefined();
    expect(cache.getQueryCache().find({ queryKey: ["api", `${platformPath}/deployments?limit=100&offset=0`], exact: true })).toBeUndefined();
  });
  it("combines URL-backed search and status with the fixed deployment parent before pagination", () => {
    const cache = client();
    const path = `${platformPath}/deployments?model_id=${model.id}&limit=100&offset=100&q=upstream%25_&enabled=false`;
    const html = render(<DashboardNavigationProvider search={{ page: "model-detail", record: model.id, tab: "deployments", q: "upstream%_", enabled: "false", offset: 100 }} navigate={go}><Deployments session={session} modelId={model.id} embedded /></DashboardNavigationProvider>, [[`${platformPath}/models/${model.id}`, model], [path, { data: [deployment] }]], [], cache);
    expect(html).toContain("upstream-v2");
    expect(html).toContain("Page 2");
    expect(html).toContain('value="upstream%_"');
    expect(cache.getQueryCache().find({ queryKey: ["api", path], exact: true })).toBeDefined();
  });
  it("does not mount deployment creation for a mismatched or failed fixed parent", () => {
    const path = `${platformPath}/models/${model.id}`;
    const page = <Deployments session={session} modelId={model.id} embedded />;
    const mismatch = render(page, [[path, { ...model, id: "other-model" }]]);
    expect(mismatch).toContain("different parent");
    expect(mismatch).not.toContain("Create deployment");
    const failed = render(page, [], [[path, new ApiError(403, "denied", "No access")]]);
    expect(failed).toContain("Access denied");
    expect(failed).not.toContain("Create deployment");
  });
  it.each(["model", "provider", "both"] as const)("locks the %s creation context in the body rather than trusting form values", async kind => {
    const action = deploymentCreateAction({ model: kind !== "provider" ? model : undefined, provider: kind !== "model" ? provider : undefined, models: [model], providers: [provider] });
    const fields = action.fields!;
    expect(fields.find(field => field.name === "enabled")?.value).toBe("false");
    if (kind !== "provider") {
      expect(action.description).toContain(`Model (read-only): ${model.public_name}`);
      expect(fields.some(field => field.name === "model_id")).toBe(false);
    }
    if (kind !== "model") {
      expect(action.description).toContain(`Provider (read-only): ${provider.name}`);
      expect(fields.some(field => field.name === "provider_connection_id")).toBe(false);
    }
    expect(action.successNotice).toContain("Review routing and publish a price version");
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ id: "created-deployment" }), { status: 201 }));
    vi.stubGlobal("fetch", fetchMock);
    vi.stubGlobal("document", { cookie: "omg_csrf=test-csrf" });
    await action.run({ model_id: kind !== "provider" ? "wrong-model" : model.id, provider_connection_id: kind !== "model" ? "wrong-provider" : provider.id, upstream_model: "upstream-v2", enabled: "false" });
    expect(fetchMock.mock.calls[0][0]).toBe(`${platformPath}/deployments`);
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual({ model_id: model.id, provider_connection_id: provider.id, upstream_model: "upstream-v2", enabled: false });
    expect(fetchMock.mock.calls[0][1].method).toBe("POST");
  });
  it("keeps unscoped deployment creation usable and validates choice fields", () => {
    const action = deploymentCreateAction({ models: [model], providers: [provider] });
    const fields = action.fields!;
    expect(fields.find(field => field.name === "model_id")?.options?.[0].value).toBe(model.id);
    expect(fields.find(field => field.name === "provider_connection_id")?.options?.[0].label).toContain("disabled");
    expect(validateFields(fields, { model_id: model.id, provider_connection_id: provider.id, upstream_model: "upstream-v2", enabled: "false" })).toEqual({});
    expect(validateFields(fields, { model_id: "wrong", provider_connection_id: provider.id, upstream_model: "upstream-v2", enabled: "false" }).model_id).toBeDefined();
  });
});

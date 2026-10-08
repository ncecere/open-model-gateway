// @vitest-environment jsdom
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { BrandIcon, LabBadge, LabIcon, ProviderBadge, ProviderIcon, WithIcon, inferLab, inferLabFrom, profileBrands } from "./provider-icon";
import { ActionProvider, Button, useAction } from "./ui";
import { connectionCreateAction, providerOptions } from "../pages/catalog";
import { testClient } from "../lib/test-fixtures";

afterEach(cleanup);

const html = (node: React.ReactElement) => renderToStaticMarkup(node);

describe("inferLab", () => {
  it.each([
    ["z-ai/glm-4.6", "zai", "zai"], ["anthropic/claude-sonnet-4.5", "anthropic", "claude"], ["black-forest-labs/flux-1.1-pro", "bfl", "flux"],
    ["nvidia/llama-3.1-nemotron-70b-instruct", "nvidia", "nvidia"], ["microsoft/phi-4", "microsoft", "microsoft"], ["cloudflare/some-model", "cloudflare", "cloudflare"],
    ["qwen/qwen3-235b-a22b", "qwen", "qwen"], ["meta-llama/Llama-3.1-8B-Instruct", "meta", "meta"], ["mistralai/mistral-large", "mistral", "mistral"],
    ["deepseek-ai/DeepSeek-R1", "deepseek", "deepseek"], ["x-ai/grok-4", "xai", "grok"], ["google/gemma-3-27b-it", "google", "google"], ["openrouter/auto", "openrouter", "openrouter"],
    ["gpt-4.1-mini", "openai", "openai"], ["o3", "openai", "openai"], ["o4-mini", "openai", "openai"], ["gpt-image-1", "openai", "openai"], ["whisper-1", "openai", "openai"], ["tts-1-hd", "openai", "openai"], ["text-embedding-3-small", "openai", "openai"],
    ["claude-haiku-4-5", "anthropic", "claude"], ["gemini-2.5-pro", "google", "gemini"], ["llama3.2:3b", "meta", "meta"], ["mistral-small-latest", "mistral", "mistral"], ["glm-4.5-air", "zai", "zai"],
    ["flux-schnell", "bfl", "flux"], ["qwen2.5-coder:7b", "qwen", "qwen"], ["deepseek-r1:14b", "deepseek", "deepseek"], ["grok-3-mini", "xai", "grok"], ["command-r-plus", "cohere", "cohere"],
    ["voyage-3-large", "voyage", "voyage"], ["lfm-40b", "liquid", "liquid"], ["sonar-pro", "perplexity", "perplexity"],
    ["us.anthropic.claude-3-5-sonnet-20240620-v1:0", "anthropic", "claude"], ["meta.llama3-1-70b-instruct-v1:0", "meta", "meta"], ["amazon.nova-pro-v1:0", "amazon", "aws"], ["cohere.command-r-v1:0", "cohere", "cohere"],
    ["@cf/meta/llama-3.1-8b-instruct", "meta", "meta"], ["@cf/baai/bge-base-en-v1.5", "cloudflare", "cloudflare"],
  ])("%s → %s", (id, key, icon) => {
    expect(inferLab(id)).toMatchObject({ key, icon });
  });

  it("leaves unknown models unknown", () => {
    for (const id of ["", null, undefined, "my-private-model", "acme/secret-v2", "ollama-thing"]) expect(inferLab(id)).toBeUndefined();
    expect(inferLabFrom(["my-alias", "anthropic/claude-opus-4"])).toMatchObject({ key: "anthropic" });
  });
});

describe("ProviderIcon", () => {
  it("covers every connection profile the form offers", () => {
    for (const option of providerOptions) expect(option.value in profileBrands).toBe(true);
  });

  it("renders a decorative SVG without inline styles, titles or HTML injection", () => {
    const markup = html(<ProviderIcon profile="openai" size="lg" />);
    expect(markup).toMatch(/^<span aria-hidden="true"[^>]*data-brand="openai"><svg /);
    expect(markup).toContain('width="20"');
    expect(markup).toContain('fill="currentColor"');
    expect(markup).not.toMatch(/style=|<title|xmlns:xlink|href=/);
  });

  it("uses the neutral glyph for generic and unknown profiles", () => {
    for (const profile of ["openai_compatible", "sglang", "unknown", null]) expect(html(<ProviderIcon profile={profile} />)).toContain('data-brand="none"');
  });

  it("prefixes gradient IDs per instance so repeated icons do not collide", () => {
    const markup = html(<><BrandIcon brand="gemini" /><BrandIcon brand="gemini" /></>);
    const ids = [...markup.matchAll(/ id="([^"]+)"/g)].map(m => m[1]);
    expect(ids.length).toBe(6);
    expect(new Set(ids).size).toBe(6);
    for (const [, ref] of markup.matchAll(/url\(#([^)]+)\)/g)) expect(ids).toContain(ref);
  });

  it("shows the mono mark in dark mode for brands whose color mark needs a light surface", () => {
    const markup = html(<BrandIcon brand="cohere" />);
    expect(markup.match(/<svg /g)).toHaveLength(2);
  });
});

describe("Add connection profile picker", () => {
  function Open() { const ask = useAction(); return <Button onClick={() => ask(connectionCreateAction())}>Open</Button>; }
  async function open() {
    const user = userEvent.setup();
    render(<QueryClientProvider client={testClient()}><ActionProvider><Open /></ActionProvider></QueryClientProvider>);
    await user.click(screen.getByRole("button", { name: "Open" }));
    return user;
  }
  async function choose(user: ReturnType<typeof userEvent.setup>, label: string, option: string) {
    await user.click(screen.getByRole("combobox", { name: label }));
    await user.click(within(await screen.findByRole("listbox")).getByRole("option", { name: option }));
  }
  it("is one dropdown with provider logos in the trigger and every option", async () => {
    const user = await open();
    expect(screen.queryByRole("radiogroup")).toBeNull();
    const trigger = screen.getByRole("combobox", { name: "Provider profile" });
    expect(trigger.textContent).toContain("OpenAI");
    expect(trigger.querySelector('[aria-hidden="true"] svg')).toBeTruthy();
    expect(screen.queryByLabelText(/^Endpoint/)).toBeNull();
    await user.click(trigger);
    const options = within(await screen.findByRole("listbox")).getAllByRole("option");
    expect(options).toHaveLength(providerOptions.length);
    for (const option of options) expect(option.querySelector('[aria-hidden="true"] svg')).toBeTruthy();
    await user.click(within(screen.getByRole("listbox")).getByRole("option", { name: "vLLM" }));
    expect(screen.getByRole("combobox", { name: "Provider profile" }).textContent).toContain("vLLM");
    expect(screen.getByLabelText(/^Endpoint/)).toBeTruthy();
  });
  it("explains the chosen profile in one line and enables the connection with a switch, on by default (review #46)", async () => {
    const user = await open();
    expect(screen.getByText("Uses api.openai.com.")).toBeTruthy(); expect(screen.queryByText("Uses openrouter.ai.")).toBeNull();
    expect(screen.getByRole("combobox", { name: "Provider profile" }).getAttribute("aria-describedby")).toBeTruthy();
    await choose(user, "Provider profile", "Anthropic");
    expect(screen.getByText("Uses api.anthropic.com.")).toBeTruthy(); expect(screen.queryByText("Uses api.openai.com.")).toBeNull();
    expect(screen.queryByRole("combobox", { name: /Status/ })).toBeNull();
    const enable = screen.getByRole("switch", { name: "Enable now" });
    expect(enable.getAttribute("aria-checked")).toBe("true");
    await user.click(enable);
    expect(enable.getAttribute("aria-checked")).toBe("false"); expect(screen.getByText("Nothing is sent until you enable it.")).toBeTruthy();
  });
  it("asks Bedrock for a listed region, an AWS identity mode and an optional VPC endpoint", async () => {
    const user = await open();
    await choose(user, "Provider profile", "Amazon Bedrock");
    expect(screen.queryByLabelText(/Credential environment variable/)).toBeNull();
    const region = screen.getByRole("combobox", { name: "AWS region" }) as HTMLSelectElement;
    expect(region.value).toBe("us-east-1");
    await user.selectOptions(region, "other");
    expect(screen.getByLabelText(/^Region code/)).toBeTruthy();
    const identity = screen.getByRole("combobox", { name: "AWS identity" }) as HTMLSelectElement;
    expect([...identity.options].map(o => o.text)).toEqual(expect.arrayContaining(["Server AWS identity (default credential chain)", "Named AWS profile", "Assume IAM role"]));
    expect(screen.queryByLabelText(/^Role ARN/)).toBeNull();
    await user.selectOptions(identity, "role");
    for (const label of [/^Role ARN/, /^External ID/, /^Session name/]) expect(screen.getByLabelText(label)).toBeTruthy();
    await user.selectOptions(identity, "profile");
    expect(screen.getByLabelText(/^Profile name/)).toBeTruthy(); expect(screen.queryByLabelText(/^Role ARN/)).toBeNull();
    expect(screen.getByLabelText(/^VPC endpoint/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Add connection" }));
    expect(screen.getByLabelText(/^Profile name/).getAttribute("aria-invalid")).toBe("true");
  });
});


describe("LabIcon and labels", () => {
  it("falls back to a neutral glyph or nothing", () => {
    expect(html(<LabIcon model="private-model" />)).toContain('data-brand="none"');
    expect(html(<LabIcon model="private-model" fallback={false} />)).toBe("");
    expect(html(<LabIcon model={["alias", "gpt-4o"]} />)).toContain('data-brand="openai"');
  });

  it("keeps the text label beside the decorative icon", () => {
    expect(html(<WithIcon icon={<ProviderIcon profile="anthropic" />}>Anthropic</WithIcon>)).toMatch(/aria-hidden="true".*>Anthropic<\/span><\/span>$/);
    expect(html(<ProviderBadge profile="vllm" label="vLLM" />)).toContain("vLLM");
    expect(html(<LabBadge model={["z-ai/glm-4.6"]} />)).toContain("Z.ai");
    expect(html(<LabBadge model={["private"]} />)).toBe("");
  });
});

/*
 * Provider and model-lab logos (LobeHub Lobe Icons, MIT; see
 * src/assets/provider-icons/PROVENANCE.md). The SVGs are bundled at build
 * time and parsed into React elements here: no HTML injection, no runtime
 * fetch, no inline style attributes. Icons are decorative (aria-hidden); the
 * caller keeps the provider or lab name as visible text.
 *
 * Mono files use currentColor and follow the theme's text colour. Colored
 * variants are used where they read well on both themes; a few brands use
 * color in light mode and mono in dark mode (see `brands`).
 */
import { createElement, useId, type ReactNode } from "react";
import { Box, Server } from "lucide-react";
import { Badge } from "./ui/badge/badge";
import st from "./provider-icon.module.css";

const sources = import.meta.glob<string>("../assets/provider-icons/*.svg", { query: "?raw", import: "default", eager: true });
const fileSource = (file: string) => sources[`../assets/provider-icons/${file}`];

// ---------------------------------------------------------------------------
// A strict parser for the vendored files. Anything outside the allowlist throws
// (provenance.test.ts parses every file), so a refreshed file with scripts,
// event handlers, links or <foreignObject> can never render.
// ---------------------------------------------------------------------------
export type IconNode = { tag: string; attrs: Record<string, string>; children: IconNode[]; text?: string };
export const allowedElements = new Set(["svg", "title", "defs", "linearGradient", "stop", "path"]);
export const allowedAttributes = new Set(["xmlns", "viewBox", "width", "height", "style", "fill", "fill-rule", "clip-rule", "d", "id", "x1", "x2", "y1", "y2", "offset", "stop-color", "stop-opacity", "gradientUnits"]);
const SAFE_URL = /^url\(#[A-Za-z0-9_-]+\)$/;
const TOKEN = /<(\/?)([A-Za-z]+)((?:\s+[A-Za-z][A-Za-z0-9:-]*="[^"<>]*")*)\s*(\/?)>|([^<]+)/y;
const ATTRIBUTE = /\s+([A-Za-z][A-Za-z0-9:-]*)="([^"<>]*)"/g;

/** Parse one vendored SVG. Throws on any element, attribute or value outside the allowlist. */
export function parseIconSvg(source: string): IconNode {
  const root: IconNode = { tag: "#root", attrs: {}, children: [] }, stack = [root];
  TOKEN.lastIndex = 0;
  while (TOKEN.lastIndex < source.length) {
    const at = TOKEN.lastIndex, match = TOKEN.exec(source);
    if (!match) throw new Error(`Unexpected SVG content at ${at}`);
    const [, closing, tag, attributes, selfClosing, text] = match, parent = stack[stack.length - 1];
    if (text !== undefined) {
      if (parent.tag === "title") parent.text = (parent.text ?? "") + text;
      else if (text.trim()) throw new Error("Unexpected SVG text");
      continue;
    }
    if (!allowedElements.has(tag)) throw new Error(`Disallowed SVG element <${tag}>`);
    if (closing) {
      if (attributes || selfClosing || parent.tag !== tag) throw new Error(`Unbalanced </${tag}>`);
      stack.pop(); continue;
    }
    const attrs: Record<string, string> = {};
    for (const [, name, value] of attributes.matchAll(ATTRIBUTE)) {
      if (!allowedAttributes.has(name)) throw new Error(`Disallowed SVG attribute ${name}`);
      if (name === "xmlns" ? value !== "http://www.w3.org/2000/svg" : /url\(/i.test(value) ? !SAFE_URL.test(value) : /[:\\&]/.test(value) && name !== "style") throw new Error(`Disallowed SVG value for ${name}`);
      attrs[name] = value;
    }
    if (tag === "svg" ? stack.length !== 1 || root.children.length : stack.length === 1) throw new Error("An icon must be exactly one root <svg>");
    const node: IconNode = { tag, attrs, children: [] };
    parent.children.push(node);
    if (!selfClosing) stack.push(node);
  }
  if (stack.length !== 1 || root.children.length !== 1) throw new Error("Unbalanced SVG");
  return root.children[0];
}

const parsed = new Map<string, IconNode>();
function iconTree(file: string): IconNode | undefined {
  if (!parsed.has(file)) { const source = fileSource(file); if (source === undefined) return; parsed.set(file, parseIconSvg(source)); }
  return parsed.get(file);
}
const camel = (name: string) => name.replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
/** React props for one element: no style, IDs and url(#…) references prefixed per instance. */
function props(attrs: Record<string, string>, prefix: string) {
  const out: Record<string, string> = {};
  for (const [name, value] of Object.entries(attrs)) {
    if (name === "style" || name === "xmlns" || name === "width" || name === "height") continue;
    out[camel(name)] = name === "id" ? `${prefix}${value}` : value.replace(/^url\(#(.+)\)$/, `url(#${prefix}$1)`);
  }
  return out;
}
function children(node: IconNode, prefix: string): ReactNode[] {
  return node.children.filter(child => child.tag !== "title").map((child, i) => createElement(child.tag, { key: i, ...props(child.attrs, prefix) }, ...children(child, prefix)));
}

// ---------------------------------------------------------------------------
// Brands, connection profiles and model labs.
// ---------------------------------------------------------------------------
type Variant = "mono" | "color";
/** `light`/`dark`: which file to show on each theme. Mono uses currentColor. */
type Brand = { slug: string; light: Variant; dark: Variant };
const brand = (slug: string, light: Variant = "mono", dark: Variant = light): Brand => ({ slug, light, dark });
export const brands = {
  openai: brand("openai"), anthropic: brand("anthropic"), claude: brand("claude", "color"), openrouter: brand("openrouter"),
  aws: brand("aws", "color"), bedrock: brand("bedrock", "color"), ollama: brand("ollama"), vllm: brand("vllm", "color"),
  google: brand("google", "color"), gemini: brand("gemini", "color"), meta: brand("meta", "color"), mistral: brand("mistral", "color"),
  nvidia: brand("nvidia", "color"), zhipu: brand("zhipu", "color"), zai: brand("zai"), bfl: brand("bfl"), flux: brand("flux"),
  microsoft: brand("microsoft", "color"), cloudflare: brand("cloudflare", "color"), qwen: brand("qwen", "color"), deepseek: brand("deepseek", "color"),
  xai: brand("xai"), grok: brand("grok"), cohere: brand("cohere", "color", "mono"), voyage: brand("voyage", "color", "mono"), liquid: brand("liquid"),
  perplexity: brand("perplexity", "color"), huggingface: brand("huggingface", "color"),
} satisfies Record<string, Brand>;
export type BrandKey = keyof typeof brands;
const fileOf = (b: Brand, variant: Variant) => `${b.slug}${variant === "color" ? "-color" : ""}.svg`;

/** Connection profiles (Provider.provider). SGLang has no upstream icon and the generic profile is not a brand. */
export const profileBrands: Record<string, BrandKey | undefined> = { openai: "openai", anthropic: "anthropic", openrouter: "openrouter", bedrock: "bedrock", vllm: "vllm", ollama: "ollama", sglang: undefined, openai_compatible: undefined };

export type Lab = { key: string; label: string; icon: BrandKey };
const labs = {
  openai: { label: "OpenAI", icon: "openai" }, anthropic: { label: "Anthropic", icon: "claude" }, google: { label: "Google", icon: "gemini" },
  meta: { label: "Meta", icon: "meta" }, mistral: { label: "Mistral AI", icon: "mistral" }, nvidia: { label: "NVIDIA", icon: "nvidia" },
  zai: { label: "Z.ai", icon: "zai" }, zhipu: { label: "Zhipu AI", icon: "zhipu" }, bfl: { label: "Black Forest Labs", icon: "bfl" },
  microsoft: { label: "Microsoft", icon: "microsoft" }, cloudflare: { label: "Cloudflare", icon: "cloudflare" }, qwen: { label: "Qwen", icon: "qwen" },
  deepseek: { label: "DeepSeek", icon: "deepseek" }, xai: { label: "xAI", icon: "xai" }, cohere: { label: "Cohere", icon: "cohere" },
  voyage: { label: "Voyage AI", icon: "voyage" }, liquid: { label: "Liquid AI", icon: "liquid" }, perplexity: { label: "Perplexity", icon: "perplexity" },
  huggingface: { label: "Hugging Face", icon: "huggingface" }, amazon: { label: "Amazon", icon: "aws" }, openrouter: { label: "OpenRouter", icon: "openrouter" },
} satisfies Record<string, { label: string; icon: BrandKey }>;
type LabKey = keyof typeof labs;
const lab = (key: LabKey, icon?: BrandKey): Lab => ({ key, label: labs[key].label, icon: icon ?? labs[key].icon });

/** `vendor/model` prefixes (OpenRouter, Hugging Face, Workers AI) and Bedrock `vendor.model` IDs. */
const vendors: Record<string, LabKey> = {
  openai: "openai", anthropic: "anthropic", google: "google", "google-ai": "google", meta: "meta", "meta-llama": "meta", facebook: "meta",
  mistral: "mistral", mistralai: "mistral", nvidia: "nvidia", "z-ai": "zai", zai: "zai", "zai-org": "zai", thudm: "zhipu", zhipu: "zhipu", zhipuai: "zhipu",
  "black-forest-labs": "bfl", bfl: "bfl", microsoft: "microsoft", cloudflare: "cloudflare", qwen: "qwen", alibaba: "qwen", deepseek: "deepseek",
  "deepseek-ai": "deepseek", "x-ai": "xai", xai: "xai", cohere: "cohere", coherelabs: "cohere", cohereforai: "cohere", voyage: "voyage", voyageai: "voyage",
  liquid: "liquid", liquidai: "liquid", perplexity: "perplexity", "perplexity-ai": "perplexity", huggingface: "huggingface", huggingfaceh4: "huggingface",
  amazon: "amazon", aws: "amazon", openrouter: "openrouter",
};
/** Known model families, matched at the start of the model name. Order matters. */
const families: [RegExp, LabKey, BrandKey?][] = [
  [/^(gpt-image|gpt-|gpt\d|chatgpt|o\d(?:$|[-.:])|dall-e|whisper|tts-|text-embedding-|text-moderation|omni-moderation|davinci|babbage|codex|sora)/, "openai"],
  [/^claude/, "anthropic"],
  [/^gemini/, "google"],
  [/^(gemma|imagen|veo|palm|learnlm)/, "google", "google"],
  [/^(nemotron|nv-)/, "nvidia"],
  [/^(llama|codellama)/, "meta"],
  [/^(mistral|mixtral|codestral|ministral|pixtral|magistral|devstral|voxtral|open-mistral|open-mixtral)/, "mistral"],
  [/^(glm|chatglm|cogview|cogvideo)/, "zai"],
  [/^flux/, "bfl", "flux"],
  [/^phi(?:-?\d|$)/, "microsoft"],
  [/^(qwen|qwq|qvq)/, "qwen"],
  [/^deepseek/, "deepseek"],
  [/^grok/, "xai", "grok"],
  [/^(command|embed-(?:english|multilingual|v\d)|rerank-(?:english|multilingual|v\d)|c4ai|aya)/, "cohere"],
  [/^(voyage|rerank-\d|rerank-lite)/, "voyage"],
  [/^lfm/, "liquid"],
  [/^(sonar|pplx)/, "perplexity"],
  [/^(nova|titan)/, "amazon"],
];
const BEDROCK = /^(?:(?:us|eu|apac|global|us-gov|ca|jp|au)\.)?([a-z]+(?:-[a-z]+)*)\.(.+)$/;

/** The lab behind a model ID, from a `vendor/` prefix, a Bedrock `vendor.` prefix or a known family; undefined when unknown. */
export function inferLab(modelId?: string | null): Lab | undefined {
  let id = modelId?.trim().toLowerCase();
  if (!id) return;
  const workersAi = /^@(cf|hf)\//.test(id); if (workersAi) id = id.slice(4);
  const parts = id.split("/"), name = parts[parts.length - 1];
  for (const vendor of parts.slice(0, -1)) if (vendors[vendor]) return familyLab(name, vendors[vendor]) ?? lab(vendors[vendor]);
  const bedrock = BEDROCK.exec(name);
  if (bedrock && vendors[bedrock[1]]) return familyLab(bedrock[2], vendors[bedrock[1]]) ?? lab(vendors[bedrock[1]]);
  return familyLab(name) ?? (workersAi ? lab("cloudflare") : undefined);
}
/** A family match; with `vendor`, only a family of that vendor (to pick a family icon such as FLUX or Grok). */
function familyLab(name: string, vendor?: LabKey): Lab | undefined {
  const hit = families.find(([pattern, key]) => (!vendor || key === vendor) && pattern.test(name));
  return hit ? lab(hit[1], hit[2]) : undefined;
}
/** The first lab found among candidate IDs (API name, upstream model IDs …). */
export const inferLabFrom = (ids: (string | null | undefined)[]) => ids.map(inferLab).find(Boolean);

// ---------------------------------------------------------------------------
// Components.
// ---------------------------------------------------------------------------
export type IconSize = "sm" | "md" | "lg" | "xl" | number;
const pixels = (size: IconSize = "md") => typeof size === "number" ? size : { sm: 14, md: 16, lg: 20, xl: 24 }[size];

function BrandSvg({ file, px, prefix, className }: { file: string; px: number; prefix: string; className?: string }) {
  const tree = iconTree(file);
  if (!tree) return null;
  return createElement("svg", { ...props(tree.attrs, prefix), width: px, height: px, focusable: "false", "aria-hidden": "true", className }, ...children(tree, prefix));
}

/** A brand logo, or the neutral fallback glyph. Always decorative. */
export function BrandIcon({ brand: key, size, className, fallback = "model" }: { brand?: BrandKey; size?: IconSize; className?: string; fallback?: "model" | "server" | "none" }) {
  const id = useId(), px = pixels(size), b = key ? brands[key] : undefined;
  if (!b) {
    if (fallback === "none") return null;
    const Glyph = fallback === "server" ? Server : Box;
    return <span aria-hidden="true" className={`${st.icon} ${st.fallback}${className ? ` ${className}` : ""}`} data-brand="none"><Glyph aria-hidden size={px} strokeWidth={1.75} /></span>;
  }
  // useId output contains colons, which are invalid in url(#…) references without escaping.
  const prefix = `omg-icon-${id.replace(/[^A-Za-z0-9_-]/g, "")}-`;
  return <span aria-hidden="true" className={`${st.icon}${className ? ` ${className}` : ""}`} data-brand={key}>
    {b.light === b.dark ? <BrandSvg file={fileOf(b, b.light)} px={px} prefix={prefix} />
      : <><BrandSvg file={fileOf(b, b.light)} px={px} prefix={`${prefix}l-`} className={st.lightOnly} /><BrandSvg file={fileOf(b, b.dark)} px={px} prefix={`${prefix}d-`} className={st.darkOnly} /></>}
  </span>;
}

/** The logo for a connection profile (`Provider.provider`); a server glyph for generic or unknown profiles. */
export function ProviderIcon({ profile, size, className }: { profile?: string | null; size?: IconSize; className?: string }) {
  return <BrandIcon brand={profile ? profileBrands[profile] : undefined} size={size} className={className} fallback="server" />;
}

/** The lab logo for a model, inferred from its IDs; the neutral glyph (or nothing) when unknown. */
export function LabIcon({ model, size, className, fallback = true }: { model?: string | null | (string | null | undefined)[]; size?: IconSize; className?: string; fallback?: boolean }) {
  const found = Array.isArray(model) ? inferLabFrom(model) : inferLab(model);
  return <BrandIcon brand={found?.icon} size={size} className={className} fallback={fallback ? "model" : "none"} />;
}

/** An icon beside text that stays on one line (labels, links). */
export function WithIcon({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return <span className={st.inline}>{icon}<span className={st.text}>{children}</span></span>;
}

/** A table cell: the icon beside a primary line and its secondary lines. */
export function IconCell({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return <span className={st.cell}>{icon}<span className={st.cellText}>{children}</span></span>;
}

/** Header meta: the connection profile's logo and name. */
export function ProviderBadge({ profile, label }: { profile: string; label: string }) {
  return <Badge variant="outline" className={st.badge}><ProviderIcon profile={profile} size="sm" />{label}</Badge>;
}

/** Header meta: the model's lab, or nothing when it cannot be inferred. */
export function LabBadge({ model }: { model: (string | null | undefined)[] }) {
  const found = inferLabFrom(model);
  return found ? <Badge variant="outline" className={st.badge}><BrandIcon brand={found.icon} size="sm" />{found.label}</Badge> : null;
}

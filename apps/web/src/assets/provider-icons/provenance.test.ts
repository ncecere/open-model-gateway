import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { allowedAttributes, allowedElements, brands, parseIconSvg, type IconNode } from "../../components/provider-icon";

/*
 * Copy-and-own integrity for the vendored Lobe Icons SVGs. provenance.json and
 * PROVENANCE.md record the pinned upstream commit and the SHA-256 of every
 * file, so an edit here, a partial refresh or an unlisted file fails until the
 * record is updated. See PROVENANCE.md.
 */
const dir = new URL("./", import.meta.url);
const read = (name: string) => readFileSync(new URL(name, dir));
const sha256 = (name: string) => createHash("sha256").update(read(name)).digest("hex");
const manifest = JSON.parse(read("provenance.json").toString("utf8")) as {
  version: number;
  source: { repository: string; commit: string; path: string; license: string; licenseFile: string; licenseSha256: string };
  files: Record<string, string>;
  omitted: Record<string, string>;
};
const provenance = read("PROVENANCE.md").toString("utf8");
const svgs = readdirSync(dir).filter(name => name.endsWith(".svg")).sort();
const files = Object.entries(manifest.files).map(([name, hash]) => ({ name, hash }));

describe("Lobe Icons provenance", () => {
  it("pins the upstream repository, commit and MIT license", () => {
    expect(manifest.version).toBe(1);
    expect(manifest.source.repository).toBe("https://github.com/lobehub/lobe-icons");
    expect(manifest.source.commit).toMatch(/^[0-9a-f]{40}$/);
    expect(manifest.source.path).toBe("packages/static-svg/icons");
    expect(manifest.source.license).toBe("MIT");
    const license = read(manifest.source.licenseFile).toString("utf8");
    expect(license.startsWith("MIT License\n")).toBe(true);
    expect(license).toContain("Copyright (c) 2023 LobeHub");
    expect(sha256(manifest.source.licenseFile)).toBe(manifest.source.licenseSha256);
    expect(provenance).toContain(manifest.source.commit);
    expect(provenance).toContain(`| \`LICENSE\` | \`${manifest.source.licenseSha256}\` |`);
  });

  it("lists exactly the SVG files present, in both records", () => {
    expect(Object.keys(manifest.files).sort()).toEqual(svgs);
    for (const { name, hash } of files) expect(provenance).toContain(`| \`${name}\` | \`${hash}\` |`);
    for (const omitted of Object.keys(manifest.omitted)) expect(svgs.some(name => name.startsWith(`${omitted}`))).toBe(false);
  });

  it.each(files)("$name matches its recorded upstream hash", ({ name, hash }) => {
    expect(name).toMatch(/^[a-z0-9]+(?:-color)?\.svg$/);
    expect(sha256(name)).toBe(hash);
  });

  it("has a mono file for every brand the dashboard uses, and each colored variant it asks for", () => {
    for (const [key, brand] of Object.entries(brands)) {
      expect(svgs, key).toContain(`${brand.slug}.svg`);
      if (brand.light === "color" || brand.dark === "color") expect(svgs, key).toContain(`${brand.slug}-color.svg`);
    }
  });
});

describe("Lobe Icons are inert", () => {
  it.each(svgs)("%s has no scripts, handlers, external references or foreignObject", name => {
    const source = read(name).toString("utf8");
    expect(source).not.toMatch(/<\s*script/i);
    expect(source).not.toMatch(/foreignObject/i);
    expect(source).not.toMatch(/\son[a-z]+\s*=/i);
    expect(source).not.toMatch(/(?:xlink:)?href\s*=/i);
    expect(source).not.toMatch(/javascript:|data:|<!|<\?|&/i);
    expect(source).not.toMatch(/@import/i);
    expect(source).not.toMatch(/url\((?!#)/i);
    // Only the SVG namespace may contain a URL.
    expect(source.replace('xmlns="http://www.w3.org/2000/svg"', "")).not.toMatch(/https?:|\/\//i);
  });

  it.each(svgs)("%s parses with only allowlisted elements and attributes", name => {
    const walk = (node: IconNode): void => {
      expect(allowedElements.has(node.tag)).toBe(true);
      for (const attribute of Object.keys(node.attrs)) expect(allowedAttributes.has(attribute)).toBe(true);
      node.children.forEach(walk);
    };
    const root = parseIconSvg(read(name).toString("utf8"));
    expect(root.tag).toBe("svg");
    walk(root);
  });

  it("rejects unsafe markup", () => {
    const wrap = (inner: string) => `<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">${inner}</svg>`;
    expect(() => parseIconSvg(wrap("<script>alert(1)</script>"))).toThrow(/Disallowed SVG element/);
    expect(() => parseIconSvg(wrap('<foreignObject></foreignObject>'))).toThrow(/Disallowed SVG element/);
    expect(() => parseIconSvg(wrap('<path d="M0 0" onclick="alert(1)"></path>'))).toThrow(/Disallowed SVG attribute/);
    expect(() => parseIconSvg(wrap('<path d="M0 0" fill="url(https://example.com/x)"></path>'))).toThrow(/Disallowed SVG value/);
    expect(() => parseIconSvg(wrap('<path d="M0 0" fill="javascript:alert(1)"></path>'))).toThrow(/Disallowed SVG value/);
    expect(() => parseIconSvg(wrap("<use></use>"))).toThrow(/Disallowed SVG element/);
    expect(() => parseIconSvg('<svg xmlns="http://example.com/"></svg>')).toThrow(/Disallowed SVG value/);
    expect(() => parseIconSvg(wrap("<path d=\"M0 0\">") )).toThrow(/Unbalanced/);
    expect(() => parseIconSvg(`${wrap("")}${wrap("")}`)).toThrow(/exactly one root/);
    expect(() => parseIconSvg(wrap("<!-- x -->"))).toThrow(/Unexpected SVG content/);
  });
});

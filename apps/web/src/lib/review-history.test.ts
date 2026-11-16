import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { describe, expect, it } from "vitest";

/*
 * Comments state invariants, not review history (AGENTS.md, #595). A comment
 * that records who asked for a change and when ("#89 review item 6", "found in
 * the review of #37") goes stale and hands the next agent a justification to
 * copy instead of a rule to follow; git history and the PR record why. This
 * check refuses such comments in the production sources of `apps/web/src`
 * (every `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` file except tests, which
 * are out of scope, and `.d.ts`).
 *
 * Comments are read from the TypeScript parser's token stream (the comment
 * ranges around every token), not by line patterns: a string, template, regular
 * expression, JSX attribute or JSX text that holds `//` or the words is not a
 * comment; `//`, `/* *\/`, `/** *\/` and a `{/* *\/}` in JSX are. Adjacent
 * own-line `//` comments are read as one comment.
 *
 * The patterns, what is refused and what is deliberately allowed live in
 * `apps/gateway/src/source_rules/review_history.patterns`, shared with the
 * Rust check (`source_rules/review_history.rs`). Each `refuse` and `allow` line
 * of that file runs here as a planted comment, in every comment form, before
 * the tree is scanned.
 *
 * A hit that cannot be told from product vocabulary mechanically goes on
 * ALLOWED with its reason; a stale entry fails. The list is empty.
 *
 * Cannot see: history written without one of the phrasings, and comments in
 * non-source files (`index.html`, `public/`, JSON, CSS).
 */

const web = fileURLToPath(new URL("../../", import.meta.url));
const patternsFile = fileURLToPath(new URL("../../../gateway/src/source_rules/review_history.patterns", import.meta.url));

/** (file, comment text as scanned, reason): comments that match a pattern and are not history. */
const ALLOWED: [file: string, text: string, reason: string][] = [];

// ---- patterns ----------------------------------------------------------

type Element =
  | { kind: "words"; words: string[]; optional: boolean }
  | { kind: "number" }
  | { kind: "skip"; most: number }
  | { kind: "notBefore"; words: string[] }
  | { kind: "notNext"; words: string[] };

type Pattern = { source: string; elements: Element[] } | { source: string; raw: string };

interface Rules {
  patterns: Pattern[];
  refuse: string[];
  allow: string[];
}

function parseRules(text: string): Rules {
  const rules: Rules = { patterns: [], refuse: [], allow: [] };
  for (const line of text.split("\n").map((l) => l.trimEnd())) {
    if (line === "" || line.startsWith("#")) continue;
    const space = line.indexOf(" ");
    if (space < 0) throw new Error(`review_history.patterns: unreadable line ${JSON.stringify(line)}`);
    const kind = line.slice(0, space);
    const rest = line.slice(space + 1);
    if (kind === "pattern") {
      const elements = rest.split(/\s+/).map((element): Element => {
        const optional = element.endsWith("?");
        const body = optional ? element.slice(0, -1) : element;
        if (body.startsWith("..")) return { kind: "skip", most: Number(body.slice(2)) };
        if (body === "<n>") return { kind: "number" };
        if (body.startsWith("^")) return { kind: "notBefore", words: body.slice(1).split("|") };
        if (body.startsWith("$")) return { kind: "notNext", words: body.slice(1).split("|") };
        return { kind: "words", words: body.split("|"), optional };
      });
      rules.patterns.push({ source: rest, elements });
    } else if (kind === "raw") rules.patterns.push({ source: rest, raw: rest });
    else if (kind === "refuse") rules.refuse.push(rest);
    else if (kind === "allow") rules.allow.push(rest);
    else throw new Error(`review_history.patterns: unknown line kind ${JSON.stringify(kind)}`);
  }
  return rules;
}

/** Lowercase words: runs of ASCII letters, runs of ASCII digits, `#` as `@`. */
function words(text: string): string[] {
  return [...text.toLowerCase().matchAll(/#|[a-z]+|[0-9]+/g)].map((m) => (m[0] === "#" ? "@" : m[0]));
}

function matchesFrom(elements: Element[], at: number, w: string[]): boolean {
  const [first, ...rest] = elements;
  if (first === undefined) return true;
  switch (first.kind) {
    case "skip":
      for (let n = 0; n <= first.most; n += 1) if (matchesFrom(rest, at + n, w)) return true;
      return false;
    case "notBefore":
      return (at === 0 || !first.words.includes(w[at - 1])) && matchesFrom(rest, at, w);
    case "notNext":
      return (at >= w.length || !first.words.includes(w[at])) && matchesFrom(rest, at, w);
    case "number":
      return at < w.length && /^[0-9]+$/.test(w[at]) && matchesFrom(rest, at + 1, w);
    case "words":
      return (
        (at < w.length && first.words.includes(w[at]) && matchesFrom(rest, at + 1, w)) || (first.optional && matchesFrom(rest, at, w))
      );
  }
}

const collapse = (text: string) => text.split(/\s+/).filter(Boolean).join(" ");

/** `text` occurs in `lower`; `<n>` in `text` stands for one or more digits. */
function rawMatches(lower: string, text: string): boolean {
  const escaped = text.split("<n>").map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  return new RegExp(escaped.join("[0-9]+")).test(lower);
}

/** The patterns that refuse `comment`. */
function refusedBy(rules: Rules, comment: string): string[] {
  const w = words(comment);
  const lower = collapse(comment).toLowerCase();
  return rules.patterns
    .filter((pattern) => {
      if ("raw" in pattern) return rawMatches(lower, pattern.raw);
      for (let start = 0; start < w.length; start += 1) if (matchesFrom(pattern.elements, start, w)) return true;
      return false;
    })
    .map((pattern) => pattern.source);
}

// ---- comments ----------------------------------------------------------

interface Block {
  line: number;
  text: string;
}

/** Every comment of a source text, from the parser's tokens; adjacent own-line `//` comments are one. */
function commentBlocks(fileName: string, text: string): Block[] {
  const kind = /\.[jt]sx$/.test(fileName) ? ts.ScriptKind.TSX : ts.ScriptKind.TS;
  const source = ts.createSourceFile(fileName, text, ts.ScriptTarget.Latest, true, kind);
  const ranges = new Map<number, ts.CommentRange>();
  const visit = (node: ts.Node) => {
    const children = node.getChildren(source);
    if (children.length === 0) {
      // JSX text is not trivia: `// x` in it is text.
      if (node.kind !== ts.SyntaxKind.JsxText) {
        for (const range of [...(ts.getLeadingCommentRanges(text, node.pos) ?? []), ...(ts.getTrailingCommentRanges(text, node.pos) ?? [])]) {
          ranges.set(range.pos, range);
        }
      }
      return;
    }
    children.forEach(visit);
  };
  visit(source);
  const ordered = [...ranges.values()].sort((a, b) => a.pos - b.pos);
  const blocks: Block[] = [];
  let lastEnd = -10;
  let lastOwnLineComment = false;
  for (const range of ordered) {
    const start = source.getLineAndCharacterOfPosition(range.pos);
    const end = source.getLineAndCharacterOfPosition(range.end);
    const lineStart = source.getPositionOfLineAndCharacter(start.line, 0);
    const ownLine = text.slice(lineStart, range.pos).trim() === "";
    const single = range.kind === ts.SyntaxKind.SingleLineCommentTrivia;
    // The `*` that leads each line of a block comment is not text.
    const body = single
      ? text.slice(range.pos + 2, range.end)
      : text
          .slice(range.pos + 2, range.end - 2)
          .split("\n")
          .map((line) => line.trimStart().replace(/^\*+/, ""))
          .join(" ");
    if (single && ownLine && lastOwnLineComment && start.line === lastEnd + 1) {
      blocks[blocks.length - 1].text += ` ${body}`;
    } else {
      blocks.push({ line: start.line + 1, text: body });
    }
    lastEnd = end.line;
    lastOwnLineComment = single && ownLine;
  }
  return blocks.map((block) => ({ ...block, text: collapse(block.text) }));
}

interface Hit {
  file: string;
  line: number;
  patterns: string[];
  text: string;
}

function scan(rules: Rules, file: string, text: string): { comments: number; hits: Hit[] } {
  const blocks = commentBlocks(file, text);
  const hits: Hit[] = [];
  for (const block of blocks) {
    const patterns = refusedBy(rules, block.text);
    if (patterns.length > 0) hits.push({ file, line: block.line, patterns, text: block.text });
  }
  return { comments: blocks.length, hits };
}

// ---- the tree ----------------------------------------------------------

const SOURCE_FILE = /\.[cm]?[jt]sx?$/;
const productionSource = (path: string) => SOURCE_FILE.test(path) && !/\.test\.[cm]?[jt]sx?$/.test(path) && !/\.d\.ts$/.test(path);

function walk(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, out);
    else out.push(path);
  }
  return out;
}

const rel = (path: string) => relative(web, path).split(sep).join("/");

function scanTree(rules: Rules) {
  const files = walk(join(web, "src")).filter(productionSource);
  let comments = 0;
  const hits: Hit[] = [];
  for (const path of files) {
    const found = scan(rules, rel(path), readFileSync(path, "utf8"));
    comments += found.comments;
    hits.push(...found.hits);
  }
  return { files: files.length, comments, hits };
}

// ---- tests -------------------------------------------------------------

const rules = parseRules(readFileSync(patternsFile, "utf8"));

/** `comment` written as every form of comment, trailing, in JSX, and wrapped over several lines. */
function forms(comment: string): { name: string; file: string; source: string }[] {
  const parts = comment.split(" ");
  const middle = Math.floor(parts.length / 2);
  const first = parts.slice(0, middle).join(" ");
  const second = parts.slice(middle).join(" ");
  return [
    { name: "line", file: "a.ts", source: `// ${comment}\nexport const x = 1;\n` },
    { name: "block", file: "a.ts", source: `/* ${comment} */\nexport const x = 1;\n` },
    { name: "jsdoc", file: "a.ts", source: `/** ${comment} */\nexport const x = 1;\n` },
    { name: "trailing", file: "a.ts", source: `export const x = 1; // ${comment}\n` },
    { name: "trailing block", file: "a.ts", source: `export const x = /* ${comment} */ 1;\n` },
    { name: "wrapped line", file: "a.ts", source: `// ${first}\n// ${second}\nexport const x = 1;\n` },
    { name: "wrapped jsdoc", file: "a.ts", source: `/**\n * ${first}\n * ${second}\n */\nexport const x = 1;\n` },
    { name: "jsx", file: "a.tsx", source: `export const X = () => <div>{/* ${comment} */}<span /></div>;\n` },
    { name: "jsx attribute", file: "a.tsx", source: `export const X = () => <div\n  // ${comment}\n  id="x" />;\n` },
    { name: "nested", file: "a.ts", source: `export function f() {\n  if (true) {\n    // ${comment}\n    return 1;\n  }\n}\n` },
  ];
}

describe("comments state invariants, not review history", () => {
  it("reads the shared pattern file", () => {
    expect(rules.patterns.length).toBeGreaterThanOrEqual(15);
    expect(rules.refuse.length).toBeGreaterThanOrEqual(25);
    expect(rules.allow.length).toBeGreaterThanOrEqual(20);
  });

  it("refuses every planted sample in every comment form", () => {
    for (const sample of rules.refuse) {
      expect(refusedBy(rules, sample), sample).not.toHaveLength(0);
      for (const form of forms(sample)) {
        const { comments, hits } = scan(rules, form.file, form.source);
        expect(comments, `${form.name}: ${sample}`).toBeGreaterThanOrEqual(1);
        expect(hits, `${form.name}: ${sample}`).toHaveLength(1);
      }
    }
  });

  it("passes every allowed sample in every comment form", () => {
    for (const sample of rules.allow) {
      for (const form of forms(sample)) {
        expect(scan(rules, form.file, form.source).hits, `${form.name}: ${sample}`).toEqual([]);
      }
    }
  });

  it("gives every pattern a refused sample of its own", () => {
    for (const pattern of rules.patterns) {
      expect(
        rules.refuse.some((sample) => refusedBy(rules, sample).includes(pattern.source)),
        `no refuse sample exercises: ${pattern.source}`,
      ).toBe(true);
    }
  });

  it("does not take a literal for a comment", () => {
    const planted = "#98 PR 2 review item 5";
    const source = [
      `export const a = "see // ${planted}";`,
      `export const b = 'a /* ${planted} */ b';`,
      `export const c = \`// ${planted} \${a} /* ${planted} */\`;`,
      `export const d = /\\/\\/ review item 3/.test("x");`,
      `export const e = "http://example.test/#98 PR 2 review item 5";`,
      `export const X = () => (`,
      `  <div title="// ${planted}">`,
      `    // ${planted} as text`,
      `    <b>/* ${planted} */</b>`,
      `  </div>`,
      `);`,
      `export const f = 1; // fine`,
      ``,
    ].join("\n");
    const { comments, hits } = scan(rules, "literals.tsx", source);
    expect(hits).toEqual([]);
    expect(comments).toBe(1);
  });

  it("reads a comment after a literal, a regular expression or a template", () => {
    const planted = "#98 PR 2 review item 5";
    for (const source of [
      `export const a = "x"; // ${planted}\n`,
      `export const a = 'x'; // ${planted}\n`,
      `export const a = /x\\//; // ${planted}\n`,
      `export const a = \`x\${1}y\`; // ${planted}\n`,
      `export const a = [1, 2]; /* ${planted} */\n`,
      `/* outer /* ${planted} */\nexport const a = 1;\n`,
    ]) {
      expect(scan(rules, "after.ts", source).hits, source).toHaveLength(1);
    }
  });

  it("joins only adjacent own-line line comments", () => {
    expect(scan(rules, "a.ts", "// review\n// item 5\nexport const x = 1;\n").hits).toHaveLength(1);
    for (const source of [
      "// review\n\n// item 5\nexport const x = 1;\n",
      "export const x = 1; // review\n// item 5\nexport const y = 2;\n",
      "// review\nexport const x = 1;\n// item 5\n",
      "/* review */\n// item 5\nexport const x = 1;\n",
      "// review\n/* item 5 */\nexport const x = 1;\n",
    ]) {
      expect(scan(rules, "a.ts", source).hits, source).toEqual([]);
    }
  });

  it("reads a block comment across its leading stars, and raw patterns take digits", () => {
    expect(scan(rules, "a.ts", "/**\n * Same as the page (rule\n * 8): words.\n */\nexport const x = 1;\n").hits).toHaveLength(1);
    expect(rawMatches("a (rule 12: b", "(rule <n>")).toBe(true);
    expect(rawMatches("a (rule x", "(rule <n>")).toBe(false);
    expect(rawMatches("finding 2 of the output", "finding <n>:")).toBe(false);
    expect(rawMatches("x finding 2 of, finding 4: y", "finding <n>:")).toBe(true);
  });

  it("matches an allowlist entry exactly", () => {
    const list: typeof ALLOWED = [["a.ts", "review #1 here", "a reason"]];
    const allowed = (hit: { file: string; text: string }) => list.some(([file, text]) => file === hit.file && text === hit.text);
    expect(allowed({ file: "a.ts", text: "review #1 here" })).toBe(true);
    expect(allowed({ file: "b.ts", text: "review #1 here" })).toBe(false);
    expect(allowed({ file: "a.ts", text: "review #1 here too" })).toBe(false);
    for (const [file, text, reason] of ALLOWED) {
      expect(file !== "" && text !== "" && reason.length > 10).toBe(true);
    }
  });

  it("finds no review history in the production sources", () => {
    const { files, comments, hits } = scanTree(rules);
    // A floor, so a walk that finds nothing cannot pass.
    // About 90% of what the tree held when the check landed (221 files, 2,272 comments).
    expect(
      files >= 199 && comments >= 2040,
      `read ${files} production files and ${comments} comments; the floor is 199 files and 2,040 comments`,
    ).toBe(true);
    for (const [file, text, reason] of ALLOWED) {
      expect(
        hits.some((hit) => hit.file === file && hit.text === text),
        `stale allowlist entry (${reason}): ${file}: ${text}`,
      ).toBe(true);
    }
    const refused = hits
      .filter((hit) => !ALLOWED.some(([file, text]) => file === hit.file && text === hit.text))
      .map((hit) => `${hit.file}:${hit.line}: ${hit.text.slice(0, 110)} [${hit.patterns.join(" / ")}]`);
    expect(
      refused,
      `${refused.length} comment(s) record review history; state the invariant instead (git history and the PR hold the rest)`,
    ).toEqual([]);
  });
});

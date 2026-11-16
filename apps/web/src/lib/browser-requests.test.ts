import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { describe, expect, it } from "vitest";

/*
 * AGENTS.md rules as checks (#591): "Browser API requests use same-origin
 * relative URLs, never absolute API URLs" and "Never put secrets in public
 * `VITE_*` variables".
 *
 * Source is read with the TypeScript compiler's parser (string literals, template
 * heads and call expressions, not text matches). Three checks:
 *
 * 1. Every network call in the production source (`fetch`, `new EventSource`,
 *    `new WebSocket`, `new XMLHttpRequest().open`, `navigator.sendBeacon`) takes
 *    a string whose first character is a single `/`, or is one of the reviewed
 *    expressions in PINNED_REQUESTS. A new call or a new URL expression fails
 *    until it is reviewed and added with its reason.
 * 2. Every absolute `http(s)://` or `ws(s)://` literal in the source is listed in
 *    ABSOLUTE_URLS with its reason (display text, placeholders, the SVG namespace,
 *    a base for parsing). A new one fails until it is reviewed.
 * 3. The `VITE_*` names read or defined anywhere (source, `vite.config.ts`, env
 *    files, workflows, Dockerfiles, compose files) are exactly PUBLIC_VITE_NAMES, none of them looks
 *    like a secret, `import.meta.env` is only read by name, and `envPrefix` is not
 *    changed.
 *
 * Cannot catch: a URL assembled from parts that is passed to a pinned call
 * (`api()` and `downloadExport` refuse anything but `/api/v1/` at run time, tested
 * in api.test.ts and by the call sites below), navigation by `<a href>`,
 * `window.open` or `location` (not API requests), a network call made through a
 * dependency, and secrets exposed under a name that is not on the list because
 * the reviewer allowed it.
 */

const web = fileURLToPath(new URL("../../", import.meta.url));
const repository = fileURLToPath(new URL("../../../../", import.meta.url));
const self = "apps/web/src/lib/browser-requests.test.ts";

/** Reviewed non-literal request URLs: "file | callee | first argument" and why it stays relative. */
const PINNED_REQUESTS: Record<string, string> = {
  "src/lib/api.ts | fetch | path": "api() refuses any path that does not start with /api/v1/ or contains :// (api.test.ts)",
  "src/lib/files.ts | fetch | `${wsPath(ws)}/files/upload`": "wsPath() is /api/v1/workspaces/<id>",
  "src/lib/settings.ts | fetch | logoPath": "logoPath is a /api/v1 template constant",
  "src/lib/subject-export.ts | fetch | path": "downloadExport() refuses any path that does not start with /api/v1/",
  "src/lib/usage-export.ts | fetch | `${wsPath(workspace)}/usage-export?${params}`": "wsPath() is /api/v1/workspaces/<id>",
};

/** Reviewed absolute URL literals: "file | literal" and what it is for. */
const ABSOLUTE_URLS: Record<string, string> = {
  "src/components/provider-icon.tsx | http://www.w3.org/2000/svg": "the SVG namespace allowed in sanitized provider icons, not a request",
  "src/lib/batch-scheduling.ts | http://gpu-host:8000/metrics": "input placeholder",
  "src/lib/bedrock.ts | https://vpce-….bedrock-runtime.<region>.vpce.amazonaws.com": "input placeholder",
  "src/lib/locations.ts | https://dashboard.invalid": "base for parsing a relative location, never requested",
  "src/pages/catalog.tsx | http://127.0.0.1:8000/v1": "default endpoint shown in the connection form, sent to the API as data",
  "src/pages/catalog.tsx | https://api.openai.com/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/catalog.tsx | https://api.anthropic.com/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/catalog.tsx | https://openrouter.ai/api/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/settings/general.tsx | https://help.example.com": "input placeholder",
};

/** Reviewed VITE_* variables. None today; each would be public in the bundle. */
const PUBLIC_VITE_NAMES: string[] = [];

const SECRET_NAME = /(KEY|TOKEN|SECRET|PASSW|CREDENTIAL|PRIVATE|AUTH|COOKIE|SESSION)/i;
const ABSOLUTE = /^(?:https?|wss?):\/\//i;
const NETWORK_FUNCTIONS = new Set(["fetch", "window.fetch", "globalThis.fetch", "self.fetch", "navigator.sendBeacon"]);
const NETWORK_CONSTRUCTORS = new Set(["EventSource", "WebSocket", "XMLHttpRequest", "window.WebSocket", "window.EventSource", "window.XMLHttpRequest"]);

function parse(file: string, text: string): ts.SourceFile {
  const kind = file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS;
  return ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, kind);
}

function visit(node: ts.Node, each: (node: ts.Node) => void): void {
  each(node);
  ts.forEachChild(node, (child) => visit(child, each));
}

/** `file | callee | first argument` of every network call. */
export function networkCalls(file: string, text: string): { key: string; first: ts.Expression | undefined }[] {
  const source = parse(file, text);
  const found: { key: string; first: ts.Expression | undefined }[] = [];
  visit(source, (node) => {
    if (!ts.isCallExpression(node) && !ts.isNewExpression(node)) return;
    const callee = node.expression.getText(source);
    const isNew = ts.isNewExpression(node);
    if (!(isNew ? NETWORK_CONSTRUCTORS.has(callee) : NETWORK_FUNCTIONS.has(callee))) return;
    const first = node.arguments?.[0];
    found.push({ key: `${file} | ${callee} | ${first ? first.getText(source) : ""}`, first });
  });
  return found;
}

function isRelativeLiteral(expression: ts.Expression | undefined): boolean | undefined {
  if (!expression) return false;
  if (ts.isStringLiteral(expression) || ts.isNoSubstitutionTemplateLiteral(expression)) {
    return /^\/(?!\/)/.test(expression.text);
  }
  if (ts.isTemplateExpression(expression)) {
    // A template that begins with text is judged by that text; one that begins with `${` is not provable.
    return expression.head.text === "" ? undefined : /^\/(?!\/)/.test(expression.head.text);
  }
  return undefined;
}

export function requestProblems(file: string, text: string, pinned: Record<string, string>): string[] {
  const problems: string[] = [];
  const seen = new Set<string>();
  for (const call of networkCalls(file, text)) {
    seen.add(call.key);
    const relative_ = isRelativeLiteral(call.first);
    if (relative_ === true) continue;
    if (relative_ === false) {
      problems.push(`${call.key}: not a same-origin relative URL (it must start with a single "/")`);
    } else if (!(call.key in pinned)) {
      problems.push(`${call.key}: a request URL that is not a literal; review it and add it to PINNED_REQUESTS`);
    }
  }
  return problems;
}

/** `file | literal` of every absolute URL string, template head, JSX attribute or JSX text. */
export function absoluteUrls(file: string, text: string): string[] {
  const source = parse(file, text);
  const found: string[] = [];
  visit(source, (node) => {
    let value: string | undefined;
    if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) value = node.text;
    else if (ts.isTemplateHead(node)) value = node.text;
    else if (ts.isJsxText(node)) value = node.text.trim();
    if (value !== undefined && ABSOLUTE.test(value.trim())) found.push(`${file} | ${value.trim()}`);
  });
  return found;
}

/** Everything about `VITE_*` names and `import.meta.env` in one source file. */
export function viteProblems(file: string, text: string): { names: string[]; problems: string[] } {
  const names = new Set<string>();
  const problems: string[] = [];
  const code = /\.(?:[cm]?[jt]sx?)$/.test(file);
  const source = parse(code ? file : "x.ts", code ? text : "");
  visit(source, (node) => {
    const shown = node.getText(source);
    if (ts.isPropertyAccessExpression(node) && /^(?:import\.meta\.env|process\.env)$/.test(node.expression.getText(source))) {
      if (node.name.text.startsWith("VITE_")) names.add(node.name.text);
    } else if (ts.isElementAccessExpression(node) && /^(?:import\.meta\.env|process\.env)$/.test(node.expression.getText(source))) {
      const arg = node.argumentExpression;
      if (ts.isStringLiteral(arg) || ts.isNoSubstitutionTemplateLiteral(arg)) {
        if (arg.text.startsWith("VITE_")) names.add(arg.text);
      } else problems.push(`${file}: ${shown} reads an environment variable by a computed name`);
    }
    // `import.meta.env` used as a whole (spread, passed, logged) would carry every public variable.
    if (ts.isMetaProperty(node) && shown === "import.meta") {
      const parent = node.parent;
      const reads = ts.isPropertyAccessExpression(parent) && parent.name.text === "env";
      if (reads) {
        const grand = parent.parent;
        const byName =
          (ts.isPropertyAccessExpression(grand) && grand.expression === parent) ||
          (ts.isElementAccessExpression(grand) && grand.expression === parent);
        if (!byName) problems.push(`${file}: ${grand.getText(source).slice(0, 60)} uses import.meta.env as a whole object`);
      }
    }
    if (ts.isIdentifier(node) && node.text === "envPrefix") problems.push(`${file}: envPrefix changes which variables reach the bundle`);
  });
  // Names written as text (workflows, env files, Dockerfiles, compose files).
  for (const match of text.matchAll(/\bVITE_[A-Z0-9_]+\b/g)) names.add(match[0]);
  return { names: [...names].sort(), problems };
}

function walk(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, out);
    else out.push(path);
  }
  return out;
}

const rel = (path: string) => relative(web, path).split(sep).join("/");
const productionSource = (path: string) => /\.(?:ts|tsx)$/.test(path) && !/\.test\.tsx?$/.test(path) && !/\.d\.ts$/.test(path);

function sources(): { file: string; text: string }[] {
  return walk(join(web, "src"))
    .filter(productionSource)
    .map((path) => ({ file: rel(path), text: readFileSync(path, "utf8") }));
}

/** Tracked files that could define or pass a VITE_* variable. */
function configurationFiles(): { file: string; text: string }[] {
  const listed = execFileSync("git", ["ls-files", "-z"], { cwd: repository, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 })
    .split("\0")
    .filter(Boolean);
  const wanted = (path: string) =>
    path !== self &&
    !path.startsWith("node_modules/") &&
    !path.endsWith("package-lock.json") &&
    !path.startsWith("docs/releases/") &&
    !/^CHANGELOG\.md$/.test(path) &&
    (/(^|\/)\.env[^/]*$/.test(path) ||
      /(^|\/)(?:vite\.config\.[cm]?[jt]s|Dockerfile[^/]*|compose[^/]*\.ya?ml|package\.json)$/.test(path) ||
      path.startsWith(".github/") ||
      path.startsWith("deploy/") ||
      path.startsWith("scripts/"));
  return listed.filter(wanted).map((path) => ({ file: path, text: readFileSync(join(repository, path), "utf8") }));
}

describe("browser requests are same-origin and relative (AGENTS.md, #591)", () => {
  it("every network call in the production source takes a relative URL or a reviewed expression", () => {
    const all = sources();
    expect(all.length).toBeGreaterThan(100);
    const problems = all.flatMap(({ file, text }) => requestProblems(file, text, PINNED_REQUESTS));
    expect(problems, "see PINNED_REQUESTS in apps/web/src/lib/browser-requests.test.ts").toEqual([]);
  });

  it("every pinned request still exists", () => {
    const keys = new Set(sources().flatMap(({ file, text }) => networkCalls(file, text).map((call) => call.key)));
    expect([...Object.keys(PINNED_REQUESTS)].filter((key) => !keys.has(key))).toEqual([]);
  });

  it("every absolute URL literal in the production source is reviewed", () => {
    const found = sources().flatMap(({ file, text }) => absoluteUrls(file, text));
    const unreviewed = found.filter((entry) => !(entry in ABSOLUTE_URLS));
    expect(unreviewed, "an absolute URL in the browser source: add it to ABSOLUTE_URLS with its reason").toEqual([]);
    const stale = Object.keys(ABSOLUTE_URLS).filter((entry) => !found.includes(entry));
    expect(stale).toEqual([]);
  });

  it("refuses planted absolute and unreviewed request URLs", () => {
    const planted = (code: string) => requestProblems("src/lib/x.ts", code, {});
    expect(planted('fetch("https://api.example.com/v1/x")')).toHaveLength(1);
    expect(planted('fetch("//evil.example/x")')).toHaveLength(1);
    expect(planted("fetch(`http://localhost:8080${path}`)")).toHaveLength(1);
    expect(planted("fetch(`${base}/api/v1/x`)")).toHaveLength(1);
    expect(planted("fetch(url)")).toHaveLength(1);
    expect(planted('new EventSource("https://example.com/stream")')).toHaveLength(1);
    expect(planted('new WebSocket("wss://example.com/ws")')).toHaveLength(1);
    expect(planted('navigator.sendBeacon("https://example.com/b", data)')).toHaveLength(1);
    expect(planted("window.fetch(url)")).toHaveLength(1);
    expect(planted('fetch("/api/v1/me")')).toEqual([]);
    expect(planted("fetch(`/api/v1/workspaces/${id}`)")).toEqual([]);
    expect(requestProblems("src/lib/x.ts", "fetch(url)", { "src/lib/x.ts | fetch | url": "reviewed" })).toEqual([]);
  });

  it("finds planted absolute URLs in strings, templates and JSX", () => {
    expect(absoluteUrls("src/x.tsx", 'const a = "https://x.example/a";')).toEqual(["src/x.tsx | https://x.example/a"]);
    expect(absoluteUrls("src/x.tsx", "const a = `http://x.example/${id}`;")).toEqual(["src/x.tsx | http://x.example/"]);
    expect(absoluteUrls("src/x.tsx", 'const a = <a href="wss://x.example">x</a>;')).toEqual(["src/x.tsx | wss://x.example"]);
    expect(absoluteUrls("src/x.tsx", "const a = <p>see http://x.example</p>;")).toEqual([]);
    expect(absoluteUrls("src/x.tsx", 'const a = "/api/v1/me";')).toEqual([]);
  });
});

describe("no secrets in public VITE_* variables (AGENTS.md, #591)", () => {
  it("the VITE_* names in use are exactly the reviewed list", () => {
    const names = new Set<string>();
    const problems: string[] = [];
    for (const { file, text } of [...sources(), ...configurationFiles()]) {
      const found = viteProblems(file, text);
      found.names.forEach((name) => names.add(name));
      problems.push(...found.problems);
    }
    expect(problems).toEqual([]);
    expect([...names].sort(), "a new VITE_* variable is public in the bundle: review it, then add it to PUBLIC_VITE_NAMES").toEqual(
      [...PUBLIC_VITE_NAMES].sort(),
    );
  });

  it("no reviewed name looks like a secret", () => {
    expect(PUBLIC_VITE_NAMES.filter((name) => SECRET_NAME.test(name))).toEqual([]);
  });

  it("scans the configuration that could define a variable", () => {
    const files = configurationFiles().map((entry) => entry.file);
    for (const expected of ["apps/web/.env.example", "apps/web/vite.config.ts", "Dockerfile", ".github/workflows/ci.yml"]) {
      expect(files).toContain(expected);
    }
  });

  it("finds planted VITE_ variables, whole-object reads and prefix changes", () => {
    expect(viteProblems("src/x.ts", "const k = import.meta.env.VITE_API_KEY;").names).toEqual(["VITE_API_KEY"]);
    expect(viteProblems("src/x.ts", 'const k = import.meta.env["VITE_TOKEN"];').names).toEqual(["VITE_TOKEN"]);
    expect(viteProblems("src/x.ts", "const k = process.env.VITE_SECRET;").names).toEqual(["VITE_SECRET"]);
    expect(viteProblems(".env", "VITE_PASSWORD=hunter2\n").names).toEqual(["VITE_PASSWORD"]);
    expect(viteProblems("src/x.ts", "const all = { ...import.meta.env };").problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "log(import.meta.env);").problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "const k = import.meta.env[name];").problems).toHaveLength(1);
    expect(viteProblems("vite.config.ts", 'export default { envPrefix: ["VITE_", "SECRET_"] };').problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "const m = import.meta.env.MODE;")).toEqual({ names: [], problems: [] });
    expect(SECRET_NAME.test("VITE_API_KEY") && SECRET_NAME.test("VITE_TOKEN") && SECRET_NAME.test("VITE_PASSWORD")).toBe(true);
    expect(SECRET_NAME.test("VITE_APP_TITLE")).toBe(false);
  });
});

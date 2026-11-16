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
 * heads and call expressions, not text matches), in every `.ts`, `.tsx`, `.js`,
 * `.jsx`, `.mjs` and `.cjs` file of `src/` (tests excluded), in `public/*.js`, and in
 * `index.html` (absolute URLs by pattern, inline scripts parsed). Four checks:
 *
 * 1. Every network call takes a string whose first character is a single `/`
 *    (not `//` or `/\`), or is one of the reviewed expressions in PINNED_REQUESTS.
 *    A network call is a call whose callee's final name is `fetch` or `sendBeacon`
 *    (`fetch(u)`, `window.fetch(u)`, `window["fetch"](u)`, `(window as any).fetch(u)`,
 *    `client.fetch(u)`), `fetch.call/apply/bind(..)`, `new EventSource`, `new WebSocket`,
 *    `new Worker`, `new SharedWorker`, `new Request` and `new XMLHttpRequest` (the
 *    constructor is refused outright: there is no relative form to check), a
 *    dynamic `import(u)` of anything but a literal, and `fetch` or `sendBeacon`
 *    used as a value (`const f = fetch`). A template that starts with `${` or
 *    is exactly `/` followed by `${` cannot be proved relative and must be pinned.
 *    A new call or a new URL expression fails until it is reviewed and pinned.
 * 2. Every absolute `http(s)://` or `ws(s)://` literal in the source is listed in
 *    ABSOLUTE_URLS with its reason (display text, placeholders, the SVG namespace,
 *    a base for parsing). A new one fails until it is reviewed.
 * 3. No HTTP-capable package (axios, ky, got, node-fetch, undici, superagent,
 *    socket.io, an Apollo or SWR data layer, ...) is imported, required or listed in
 *    `apps/web/package.json` unless it is in PINNED_HTTP_PACKAGES (none today).
 * 4. The `VITE_*` names read or defined anywhere (source, `vite.config.ts`, env
 *    files, workflows, Dockerfiles, compose files) are exactly PUBLIC_VITE_NAMES,
 *    none of them looks like a secret, `import.meta.env` is only read by name,
 *    `envPrefix` and `define` are not used in `vite.config.ts`, and `loadEnv` is
 *    called with a literal non-empty prefix.
 *
 * Cannot catch: a URL assembled from parts that is passed to a pinned call
 * (`api()` and `downloadExport` refuse anything but `/api/v1/` at run time, tested
 * in api.test.ts and by the call sites below); navigation and resource loads that
 * are not API requests: `<a href>`, `window.open`, `location.href =`, `.assign`,
 * `.replace`, `new Image().src =`, `<form action>`; a network call made through
 * a dependency or through a callee renamed first (`const f = x.get; f(u)` with `get`
 * not a network name); a request made by code generated at build time; and secrets
 * exposed under a name that is on the list because the reviewer allowed it.
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
  "src/pages/catalog.tsx | http://127.0.0.1:8000/v1": "input placeholder in the connection form",
  "src/pages/catalog.tsx | https://api.openai.com/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/catalog.tsx | https://api.anthropic.com/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/catalog.tsx | https://openrouter.ai/api/v1": "fixed provider endpoint shown in the connection form",
  "src/pages/settings/general.tsx | https://help.example.com": "input placeholder",
};

/** Reviewed HTTP-capable packages the browser code may use. None today. */
const PINNED_HTTP_PACKAGES: string[] = [];

/** Reviewed VITE_* variables. None today; each would be public in the bundle. */
const PUBLIC_VITE_NAMES: string[] = [];

const SECRET_NAME = /(KEY|TOKEN|SECRET|PASSW|CREDENTIAL|PRIVATE|AUTH|COOKIE|SESSION)/i;
/** A single leading `/`, then none of `/`, `\`, tab, LF, CR (browsers drop tabs and newlines in URLs). */
const RELATIVE = /^\/(?![/\\\t\n\r])/;
const ABSOLUTE = /^(?:https?|wss?):\/\//i;
const NETWORK_FUNCTIONS = new Set(["fetch", "sendBeacon", "importScripts"]);
const FUNCTION_METHODS = new Set(["call", "apply", "bind"]);
const NETWORK_CONSTRUCTORS = new Set(["EventSource", "WebSocket", "XMLHttpRequest", "Worker", "SharedWorker", "Request"]);
const HTTP_PACKAGE =
  /^(?:axios|ky|ky-universal|got|node-fetch|cross-fetch|isomorphic-fetch|whatwg-fetch|undici|superagent|ofetch|wretch|request|needle|socket\.io-client|ws|eventsource|reconnecting-websocket|@microsoft\/fetch-event-source|graphql-request|@apollo\/client|urql|@urql\/[^/]+|swr|@trpc\/client)(?:\/|$)/;
const GLOBAL_OBJECTS = new Set(["window", "globalThis", "self"]);
const SOURCE_FILE = /\.(?:[cm]?[jt]sx?)$/;

function parse(file: string, text: string): ts.SourceFile {
  const kind = /\.tsx$/.test(file)
    ? ts.ScriptKind.TSX
    : /\.jsx$/.test(file)
      ? ts.ScriptKind.JSX
      : /\.[cm]?js$/.test(file)
        ? ts.ScriptKind.JS
        : ts.ScriptKind.TS;
  return ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, kind);
}

function visit(node: ts.Node, each: (node: ts.Node) => void): void {
  each(node);
  ts.forEachChild(node, (child) => visit(child, each));
}

function unwrap(expression: ts.Expression): ts.Expression {
  let current = expression;
  while (
    ts.isParenthesizedExpression(current) ||
    ts.isAsExpression(current) ||
    ts.isNonNullExpression(current) ||
    ts.isTypeAssertionExpression(current) ||
    ts.isSatisfiesExpression(current)
  ) {
    current = current.expression;
  }
  return current;
}

/** The last name of an expression: `a.b.fetch`, `a["fetch"]` and `fetch` are all `fetch`. */
function finalName(expression: ts.Expression): string | undefined {
  const e = unwrap(expression);
  if (ts.isIdentifier(e)) return e.text;
  if (ts.isPropertyAccessExpression(e)) return e.name.text;
  if (ts.isElementAccessExpression(e) && ts.isStringLiteralLike(e.argumentExpression)) return e.argumentExpression.text;
  return undefined;
}

function receiver(expression: ts.Expression): ts.Expression | undefined {
  const e = unwrap(expression);
  return ts.isPropertyAccessExpression(e) || ts.isElementAccessExpression(e) ? e.expression : undefined;
}

type Site = { key: string; first: ts.Expression | undefined; provable: boolean };

/** `file | callee | first argument` of every network call, with the argument when it is checkable. */
export function networkCalls(file: string, text: string): Site[] {
  const source = parse(file, text);
  const found: Site[] = [];
  const argument = (node: ts.CallExpression | ts.NewExpression, at = 0) => node.arguments?.[at];
  const callee = (node: ts.CallExpression | ts.NewExpression) => node.expression.getText(source);
  visit(source, (node) => {
    if (ts.isNewExpression(node)) {
      const name = finalName(node.expression);
      if (name && NETWORK_CONSTRUCTORS.has(name)) {
        const first = argument(node);
        found.push({ key: `${file} | ${callee(node)} | ${first ? first.getText(source) : ""}`, first, provable: true });
      }
      return;
    }
    if (ts.isCallExpression(node)) {
      if (node.expression.kind === ts.SyntaxKind.ImportKeyword) {
        const first = argument(node);
        if (first && ts.isStringLiteralLike(first)) return;
        found.push({ key: `${file} | import() | ${first ? first.getText(source) : ""}`, first, provable: false });
        return;
      }
      const name = finalName(node.expression);
      const registers = name === "register" && receiver(node.expression) !== undefined && finalName(receiver(node.expression)!) === "serviceWorker";
      if (name && (NETWORK_FUNCTIONS.has(name) || registers)) {
        const first = argument(node);
        found.push({ key: `${file} | ${callee(node)} | ${first ? first.getText(source) : ""}`, first, provable: true });
      } else if (name && FUNCTION_METHODS.has(name)) {
        const target = receiver(node.expression);
        const targetName = target ? finalName(target) : undefined;
        if (targetName && NETWORK_FUNCTIONS.has(targetName)) {
          const args = node.arguments.map((arg) => arg.getText(source)).join(", ");
          found.push({ key: `${file} | ${callee(node)} | ${args}`, first: undefined, provable: false });
        }
      }
      return;
    }
    // `window[name]`, `globalThis[key]`, `self[k]`: any global could be named, `fetch` included.
    if (ts.isElementAccessExpression(node) && !ts.isStringLiteralLike(node.argumentExpression)) {
      const target = unwrap(node.expression);
      if (ts.isIdentifier(target) && GLOBAL_OBJECTS.has(target.text)) {
        found.push({ key: `${file} | ${node.getText(source)} (computed key) | `, first: undefined, provable: false });
        return;
      }
    }
    // `fetch` or `sendBeacon` named as a value (`const f = fetch`, `run(window.fetch)`).
    const isName =
      ts.isIdentifier(node) || ts.isPropertyAccessExpression(node) || ts.isElementAccessExpression(node);
    if (!isName) return;
    const name = finalName(node as ts.Expression);
    if (!name || !NETWORK_FUNCTIONS.has(name)) return;
    const parent = node.parent;
    if (ts.isIdentifier(node)) {
      if (ts.isPropertyAccessExpression(parent) && parent.name === node) return;
      if (ts.isImportSpecifier(parent) || ts.isExportSpecifier(parent) || ts.isImportClause(parent)) return;
      if (ts.isTypeQueryNode(parent) || ts.isTypeReferenceNode(parent)) return;
      if ((ts.isFunctionDeclaration(parent) || ts.isMethodDeclaration(parent)) && parent.name === node) return;
    }
    if (ts.isCallExpression(parent) && parent.expression === node) return;
    if (
      (ts.isPropertyAccessExpression(parent) || ts.isElementAccessExpression(parent)) &&
      parent.expression === node &&
      FUNCTION_METHODS.has(finalName(parent) ?? "")
    ) {
      return;
    }
    if ((ts.isPropertyAccessExpression(parent) || ts.isElementAccessExpression(parent)) && parent.expression === node) {
      // `fetch.x`: the value's property; the name itself is not called or passed.
      return;
    }
    found.push({ key: `${file} | ${node.getText(source)} (value) | `, first: undefined, provable: false });
  });
  return found;
}

function isRelativeLiteral(expression: ts.Expression | undefined): boolean | undefined {
  if (!expression) return false;
  if (ts.isStringLiteral(expression) || ts.isNoSubstitutionTemplateLiteral(expression)) {
    return RELATIVE.test(expression.text);
  }
  if (ts.isTemplateExpression(expression)) {
    // A template that begins with text is judged by that text; one that begins with `${`, or is
    // exactly `/` before its first `${` (the substitution could start with `/` or `\`), is not provable.
    const head = expression.head.text;
    if (head === "" || head === "/") return undefined;
    return RELATIVE.test(head);
  }
  return undefined;
}

export function requestProblems(file: string, text: string, pinned: Record<string, string>): string[] {
  const problems: string[] = [];
  for (const call of networkCalls(file, text)) {
    const relative_ = call.provable ? isRelativeLiteral(call.first) : undefined;
    if (relative_ === true) continue;
    if (relative_ === false) {
      problems.push(`${call.key}: not a same-origin relative URL (it must start with a single "/")`);
    } else if (!(call.key in pinned)) {
      problems.push(`${call.key}: a request URL that is not a checkable literal; review it and add it to PINNED_REQUESTS`);
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

/** The HTML page: absolute URLs by pattern, inline scripts as source. */
export function htmlProblems(file: string, text: string, pinned: Record<string, string>): { urls: string[]; problems: string[] } {
  const urls = [...text.matchAll(/(?:https?|wss?):\/\/[^\s"'<>)]+/gi)].map((m) => `${file} | ${m[0]}`);
  const problems: string[] = [];
  for (const match of text.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi)) {
    problems.push(...requestProblems(file, match[1], pinned));
  }
  return { urls, problems };
}

/** HTTP-capable packages imported, required or loaded dynamically by literal. */
export function httpPackages(file: string, text: string): string[] {
  const source = parse(file, text);
  const found: string[] = [];
  const note = (specifier: string) => {
    if (HTTP_PACKAGE.test(specifier)) found.push(`${file} | ${specifier}`);
  };
  visit(source, (node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      note(node.moduleSpecifier.text);
    } else if (ts.isCallExpression(node) && node.arguments.length > 0 && ts.isStringLiteralLike(node.arguments[0])) {
      if (node.expression.kind === ts.SyntaxKind.ImportKeyword || (ts.isIdentifier(node.expression) && node.expression.text === "require")) {
        note(node.arguments[0].text);
      }
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      const e = node.moduleReference.expression;
      if (ts.isStringLiteral(e)) note(e.text);
    }
  });
  return found;
}

/** Pinned packages that nothing imports and the manifest does not list. */
export function stalePackages(pinned: string[], used: string[], manifest: string): string[] {
  const parsed = JSON.parse(manifest) as Record<string, Record<string, string> | undefined>;
  const listed = ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"].flatMap((key) => Object.keys(parsed[key] ?? {}));
  return pinned.filter((name) => !used.some((entry) => entry.endsWith(`| ${name}`)) && !listed.includes(name));
}

/** The environment without the `GIT_*` variables git sets for hooks: they would redirect `git ls-files`. */
export function withoutGitVariables(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return Object.fromEntries(Object.entries(env).filter(([name]) => !name.startsWith("GIT_")));
}

export function dependencyProblems(manifest: string): string[] {
  const parsed = JSON.parse(manifest) as Record<string, Record<string, string> | undefined>;
  const names = ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"].flatMap((key) => Object.keys(parsed[key] ?? {}));
  return names.filter((name) => HTTP_PACKAGE.test(name) && !PINNED_HTTP_PACKAGES.includes(name)).map((name) => `package.json lists ${name}, an HTTP-capable package`);
}

/** Everything about `VITE_*` names, `import.meta.env` and the Vite configuration in one file. */
export function viteProblems(file: string, text: string): { names: string[]; problems: string[] } {
  const names = new Set<string>();
  const problems: string[] = [];
  const code = SOURCE_FILE.test(file);
  const config = /(^|\/)vite\.config\.[cm]?[jt]s$/.test(file);
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
    if (config) {
      if ((ts.isPropertyAssignment(node) || ts.isShorthandPropertyAssignment(node)) && node.name.getText(source) === "define") {
        problems.push(`${file}: define injects values into the bundle`);
      }
      if (ts.isCallExpression(node) && finalName(node.expression) === "loadEnv") {
        const prefix = node.arguments[2];
        if (!prefix || !ts.isStringLiteralLike(prefix) || prefix.text === "") {
          problems.push(`${file}: loadEnv needs a literal non-empty prefix (an empty one loads every variable)`);
        } else if (prefix.text.startsWith("VITE_")) {
          problems.push(`${file}: loadEnv with a VITE_ prefix puts values where the bundle can read them`);
        }
      }
    }
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
const productionSource = (path: string) => SOURCE_FILE.test(path) && !/\.test\.[cm]?[jt]sx?$/.test(path) && !/\.d\.ts$/.test(path);

function sources(): { file: string; text: string }[] {
  return [...walk(join(web, "src")).filter(productionSource), ...walk(join(web, "public")).filter((path) => SOURCE_FILE.test(path))].map(
    (path) => ({ file: rel(path), text: readFileSync(path, "utf8") }),
  );
}

function pages(): { file: string; text: string }[] {
  return [join(web, "index.html"), ...walk(join(web, "public")).filter((path) => /\.html?$/.test(path))].map((path) => ({
    file: rel(path),
    text: readFileSync(path, "utf8"),
  }));
}

/** Tracked files that could define or pass a VITE_* variable. */
function configurationFiles(): { file: string; text: string }[] {
  const listed = execFileSync("git", ["ls-files", "-z"], { cwd: repository, encoding: "utf8", env: withoutGitVariables(process.env), maxBuffer: 64 * 1024 * 1024 })
    .split("\0")
    .filter(Boolean);
  const wanted = (path: string) =>
    path !== self &&
    !path.startsWith("node_modules/") &&
    !path.endsWith("package-lock.json") &&
    !path.startsWith("docs/releases/") &&
    !/^CHANGELOG\.md$/.test(path) &&
    (/(^|\/)\.env[^/]*$/.test(path) ||
      /(^|\/)(?:vite\.config\.[cm]?[jt]s|Dockerfile[^/]*|(?:docker-)?compose[^/]*\.ya?ml|package\.json)$/.test(path) ||
      path.startsWith(".github/") ||
      path.startsWith("deploy/") ||
      path.startsWith("scripts/"));
  return listed.filter(wanted).map((path) => ({ file: path, text: readFileSync(join(repository, path), "utf8") }));
}

/** About 80% of the production files read today (measured: 222). */
const SOURCE_FLOOR = 180;

describe("browser requests are same-origin and relative (AGENTS.md, #591)", () => {
  it("every network call in the production source takes a relative URL or a reviewed expression", () => {
    const all = sources();
    expect(all.length).toBeGreaterThan(SOURCE_FLOOR);
    const problems = [
      ...all.flatMap(({ file, text }) => requestProblems(file, text, PINNED_REQUESTS)),
      ...pages().flatMap(({ file, text }) => htmlProblems(file, text, PINNED_REQUESTS).problems),
    ];
    expect(problems, "see PINNED_REQUESTS in apps/web/src/lib/browser-requests.test.ts").toEqual([]);
  });

  it("every pinned request still exists", () => {
    const keys = new Set(sources().flatMap(({ file, text }) => networkCalls(file, text).map((call) => call.key)));
    expect([...Object.keys(PINNED_REQUESTS)].filter((key) => !keys.has(key))).toEqual([]);
  });

  it("every absolute URL literal in the production source and the page is reviewed", () => {
    const found = [
      ...sources().flatMap(({ file, text }) => absoluteUrls(file, text)),
      ...pages().flatMap(({ file, text }) => htmlProblems(file, text, PINNED_REQUESTS).urls),
    ];
    const unreviewed = found.filter((entry) => !(entry in ABSOLUTE_URLS));
    expect(unreviewed, "an absolute URL in the browser source: add it to ABSOLUTE_URLS with its reason").toEqual([]);
    const stale = Object.keys(ABSOLUTE_URLS).filter((entry) => !found.includes(entry));
    expect(stale).toEqual([]);
  });

  it("no HTTP-capable package is used or listed unless pinned", () => {
    const used = sources().flatMap(({ file, text }) => httpPackages(file, text));
    expect(used.filter((entry) => !PINNED_HTTP_PACKAGES.some((name) => entry.endsWith(`| ${name}`)))).toEqual([]);
    const manifest = readFileSync(join(web, "package.json"), "utf8");
    expect(dependencyProblems(manifest)).toEqual([]);
    expect(stalePackages(PINNED_HTTP_PACKAGES, used, manifest)).toEqual([]);
  });

  it("a pinned package nothing uses is stale, and git runs without inherited GIT_* variables", () => {
    expect(stalePackages(["axios", "ky"], ["src/x.ts | axios"], '{"dependencies":{"react":"19"}}')).toEqual(["ky"]);
    expect(stalePackages(["ky"], [], '{"dependencies":{"ky":"1"}}')).toEqual([]);
    expect(withoutGitVariables({ GIT_DIR: "/x", GIT_INDEX_FILE: "/y", GIT_WORK_TREE: "/z", PATH: "/bin", HOME: "/h" })).toEqual({ PATH: "/bin", HOME: "/h" });
  });

  it("scans every kind of source and the page", () => {
    const files = sources().map((entry) => entry.file);
    expect(files).toContain("src/lib/api.ts");
    expect(files).toContain("src/main.tsx");
    expect(files).toContain("public/color-mode.js");
    expect(pages().map((entry) => entry.file)).toContain("index.html");
  });

  const planted = (code: string, file = "src/lib/x.ts") => requestProblems(file, code, {});

  it("refuses planted absolute, protocol-relative and unprovable request URLs", () => {
    expect(planted('fetch("https://api.example.com/v1/x")')).toHaveLength(1);
    expect(planted('fetch("//evil.example/x")')).toHaveLength(1);
    expect(planted('fetch("/\\\\evil.example/x")')).toHaveLength(1);
    expect(planted("fetch(`http://localhost:8080${path}`)")).toHaveLength(1);
    expect(planted("fetch(`${base}/api/v1/x`)")).toHaveLength(1);
    expect(planted("fetch(`/${path}`)")).toHaveLength(1);
    expect(planted("fetch(url)")).toHaveLength(1);
    for (const literal of ['"/\\t/evil.example"', '"/\\n/evil.example"', '"/\\r/evil.example"', '"/\\\\evil.example"', '"//evil.example"']) {
      expect(planted(`fetch(${literal})`), literal).toHaveLength(1);
    }
    expect(planted("fetch(`/\\t/evil.example${x}`)")).toHaveLength(1);
    expect(planted("importScripts('https://cdn.example/x.js')")).toHaveLength(1);
    expect(planted("importScripts('/sw-helper.js')")).toEqual([]);
    expect(planted("navigator.serviceWorker.register('https://evil.example/sw.js')")).toHaveLength(1);
    expect(planted("navigator.serviceWorker.register('/sw.js')")).toEqual([]);
    expect(planted("registry.register(plugin)")).toEqual([]);
    expect(planted('window[name]("/api/v1/me")')).toHaveLength(1);
    expect(planted("globalThis[key](u)")).toHaveLength(1);
    expect(planted("self[k]")).toHaveLength(1);
    expect(planted('window["fetch"]("/api/v1/me")')).toEqual([]);
    expect(planted("items[i]; row[key]")).toEqual([]);
    expect(requestProblems("src/x.ts", "window[name](u)", { "src/x.ts | window[name] (computed key) | ": "reviewed" })).toEqual([]);
    expect(planted('new EventSource("https://example.com/stream")')).toHaveLength(1);
    expect(planted('new WebSocket("wss://example.com/ws")')).toHaveLength(1);
    expect(planted('new Worker("https://example.com/w.js")')).toHaveLength(1);
    expect(planted('new SharedWorker("//example.com/w.js")')).toHaveLength(1);
    expect(planted('new Request("https://example.com/r")')).toHaveLength(1);
    expect(planted("const x = new XMLHttpRequest();")).toHaveLength(1);
    expect(planted('navigator.sendBeacon("https://example.com/b", data)')).toHaveLength(1);
    expect(planted("window.fetch(url)")).toHaveLength(1);
    expect(planted('fetch("/api/v1/me")')).toEqual([]);
    expect(planted("fetch(`/api/v1/workspaces/${id}`)")).toEqual([]);
    expect(planted('new WebSocket("/v1/realtime")')).toEqual([]);
    expect(requestProblems("src/lib/x.ts", "fetch(url)", { "src/lib/x.ts | fetch | url": "reviewed" })).toEqual([]);
  });

  it("refuses planted callee spellings and fetch used as a value", () => {
    for (const code of [
      'window["fetch"]("https://example.com/x")',
      '(window as any).fetch("https://example.com/x")',
      'globalThis.fetch("https://example.com/x")',
      'client.fetch("https://example.com/x")',
      'self?.fetch("https://example.com/x")',
      'navigator["sendBeacon"]("https://example.com/b")',
    ]) {
      expect(planted(code), code).toHaveLength(1);
    }
    expect(planted('(fetch as typeof fetch)("https://example.com/x")').length).toBeGreaterThanOrEqual(1);
    for (const code of [
      'fetch.call(window, "/api/v1/me")',
      'fetch.apply(window, ["/api/v1/me"])',
      "const f = fetch.bind(window);",
      "const f = fetch;",
      "run(window.fetch);",
      "const { fetch: f } = window;",
      'const f = window["fetch"];',
      "export const f = navigator.sendBeacon;",
    ]) {
      expect(planted(code).length, code).toBeGreaterThanOrEqual(1);
    }
    expect(planted("import(someUrl)")).toHaveLength(1);
    expect(planted('import("./page")')).toEqual([]);
    expect(planted("q.refetch(); queryClient.fetchQuery(x); const fetched = 1;")).toEqual([]);
    expect(planted("type F = typeof fetch; // fetch(url) in a comment")).toEqual([]);
  });

  it("reads JavaScript, JSX and the inline scripts of a page", () => {
    expect(planted('fetch("https://example.com/x")', "public/x.js")).toHaveLength(1);
    expect(planted('const a = <A onClick={() => fetch("https://example.com/x")} />;', "src/x.jsx")).toHaveLength(1);
    const page = '<html><script src="/color-mode.js"></script><script>fetch("https://example.com/x")</script></html>';
    expect(htmlProblems("index.html", page, {}).problems).toHaveLength(1);
    expect(htmlProblems("index.html", page.replace("fetch(", "noop("), {}).problems).toEqual([]);
    expect(htmlProblems("index.html", '<script src="https://cdn.example.com/x.js"></script><link href="http://a.example/s.css">', {}).urls).toEqual([
      "index.html | https://cdn.example.com/x.js",
      "index.html | http://a.example/s.css",
    ]);
  });

  it("finds planted absolute URLs in strings, templates and JSX", () => {
    expect(absoluteUrls("src/x.tsx", 'const a = "https://x.example/a";')).toEqual(["src/x.tsx | https://x.example/a"]);
    expect(absoluteUrls("src/x.tsx", "const a = `http://x.example/${id}`;")).toEqual(["src/x.tsx | http://x.example/"]);
    expect(absoluteUrls("src/x.tsx", 'const a = <a href="wss://x.example">x</a>;')).toEqual(["src/x.tsx | wss://x.example"]);
    expect(absoluteUrls("src/x.tsx", "const a = <p>see http://x.example</p>;")).toEqual([]);
    expect(absoluteUrls("src/x.tsx", 'const a = "/api/v1/me";')).toEqual([]);
  });

  it("finds planted HTTP packages in imports, requires, dynamic imports and the manifest", () => {
    expect(httpPackages("src/x.ts", 'import axios from "axios";')).toEqual(["src/x.ts | axios"]);
    expect(httpPackages("src/x.ts", 'import { get } from "ky/distribution";')).toEqual(["src/x.ts | ky/distribution"]);
    expect(httpPackages("src/x.ts", 'export * from "node-fetch";')).toEqual(["src/x.ts | node-fetch"]);
    expect(httpPackages("src/x.js", 'const got = require("got");')).toEqual(["src/x.js | got"]);
    expect(httpPackages("src/x.ts", 'const m = await import("undici");')).toEqual(["src/x.ts | undici"]);
    expect(httpPackages("src/x.ts", 'import useSWR from "swr";')).toEqual(["src/x.ts | swr"]);
    expect(httpPackages("src/x.ts", 'import { useQuery } from "@tanstack/react-query"; import r from "react";')).toEqual([]);
    expect(dependencyProblems('{"dependencies":{"axios":"1","react":"19"},"devDependencies":{"ky":"1"}}')).toEqual([
      "package.json lists axios, an HTTP-capable package",
      "package.json lists ky, an HTTP-capable package",
    ]);
    expect(dependencyProblems('{"dependencies":{"react":"19"}}')).toEqual([]);
  });
});

describe("no secrets in public VITE_* variables (AGENTS.md, #591)", () => {
  it("the VITE_* names in use are exactly the reviewed list", () => {
    const names = new Set<string>();
    const problems: string[] = [];
    for (const { file, text } of [...sources(), ...pages(), ...configurationFiles()]) {
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
    for (const expected of ["apps/web/.env.example", "apps/web/vite.config.ts", "Dockerfile", ".github/workflows/ci.yml", "compose.yaml"]) {
      expect(files).toContain(expected);
    }
  });

  it("finds planted VITE_ variables, whole-object reads and prefix changes", () => {
    expect(viteProblems("src/x.ts", "const k = import.meta.env.VITE_API_KEY;").names).toEqual(["VITE_API_KEY"]);
    expect(viteProblems("src/x.ts", 'const k = import.meta.env["VITE_TOKEN"];').names).toEqual(["VITE_TOKEN"]);
    expect(viteProblems("src/x.ts", "const k = process.env.VITE_SECRET;").names).toEqual(["VITE_SECRET"]);
    expect(viteProblems(".env", "VITE_PASSWORD=hunter2\n").names).toEqual(["VITE_PASSWORD"]);
    expect(viteProblems("docker-compose.yml", "environment:\n  - VITE_API_KEY=x\n").names).toEqual(["VITE_API_KEY"]);
    expect(viteProblems("src/x.ts", "const all = { ...import.meta.env };").problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "log(import.meta.env);").problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "const k = import.meta.env[name];").problems).toHaveLength(1);
    expect(viteProblems("vite.config.ts", 'export default { envPrefix: ["VITE_", "SECRET_"] };').problems).toHaveLength(1);
    expect(viteProblems("src/x.ts", "const m = import.meta.env.MODE;")).toEqual({ names: [], problems: [] });
    expect(SECRET_NAME.test("VITE_API_KEY") && SECRET_NAME.test("VITE_TOKEN") && SECRET_NAME.test("VITE_PASSWORD")).toBe(true);
    expect(SECRET_NAME.test("VITE_APP_TITLE")).toBe(false);
  });

  it("refuses planted define and loadEnv calls in the Vite configuration", () => {
    const config = (code: string) => viteProblems("apps/web/vite.config.ts", code).problems;
    expect(config('export default { define: { "import.meta.env.X": JSON.stringify(process.env.SECRET) } };')).toHaveLength(1);
    expect(config("const define = {}; export default { define };")).toHaveLength(1);
    expect(config('const env = loadEnv(mode, dir, "");')).toHaveLength(1);
    expect(config("const env = loadEnv(mode, dir);")).toHaveLength(1);
    expect(config("const env = loadEnv(mode, dir, prefix);")).toHaveLength(1);
    expect(config('const env = loadEnv(mode, dir, "VITE_");')).toHaveLength(1);
    expect(config('const env = loadEnv(mode, dir, "GATEWAY_");')).toEqual([]);
    // Only the configuration file is held to these rules.
    expect(viteProblems("src/x.ts", "const define = 1;").problems).toEqual([]);
  });
});

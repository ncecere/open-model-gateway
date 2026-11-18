// Static traversal of literal CI wrapper invocations. See docs/verification.md for its scope.
import assert from "node:assert/strict";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { posix } from "node:path";
import ts from "typescript";
import { shellCommands } from "./policy.mjs";

// Leave parent components intact until the filesystem reader has checked each prefix.
export function wrapperPath(dir, name) {
  if (posix.isAbsolute(name) || posix.isAbsolute(dir)) return posix.resolve(dir, name);
  return [...dir.split("/"), ...name.split("/")].filter(part => part && part !== ".").join("/") || ".";
}

export function indexedReader(root, files) {
  const base = realpathSync(root);
  const paths = new Set(files);
  const cache = new Map();
  return path => {
    if (posix.isAbsolute(path)) return;
    if (!cache.has(path)) {
      let full = base;
      for (const part of path.split("/")) {
        if (!part || part === ".") continue;
        if (full === base && part === "..") return;
        full = posix.join(full, part);
        const entry = lstatSync(full, { throwIfNoEntry: false });
        assert.ok(!entry?.isSymbolicLink(), `wrapper path contains a symlink: ${path}`);
        if (!entry) {
          assert.ok(!paths.has(posix.relative(base, full)), `indexed wrapper path is missing: ${path}`);
          return;
        }
      }
      if (!paths.has(posix.relative(base, full))) return;
      assert.ok(lstatSync(full).isFile(), `wrapper must be a regular file: ${path}`);
      cache.set(path, readFileSync(full, "utf8"));
    }
    return cache.get(path);
  };
}

// Only inspect executable arguments of process calls. Regexes, comments and
// messages mentioning cargo are common in tests that never start the tool.
function jsCommands(source) {
  const tree = ts.createSourceFile("wrapper.js", source, ts.ScriptTarget.Latest, false, ts.ScriptKind.JS);
  assert.equal(tree.parseDiagnostics.length, 0, "cannot parse JavaScript wrapper");
  const commands = [];
  function visit(node) {
    if (ts.isCallExpression(node)) {
      const target = node.expression;
      const name = ts.isPropertyAccessExpression(target) ? target.name.text : target.text;
      const [program, args] = node.arguments;
      if (program && ts.isStringLiteralLike(program)) {
        if (["exec", "execSync"].includes(name)) commands.push(...shellCommands(program.text));
        if (["spawn", "spawnSync", "execFile", "execFileSync"].includes(name)) {
          const words = [program];
          if (program.text === "npm" && args && ts.isArrayLiteralExpression(args)) {
            const [verb, script] = args.elements;
            if (verb && ts.isStringLiteralLike(verb) && verb.text === "run" && script && ts.isStringLiteralLike(script)) {
              words.push(verb, script);
            }
          }
          if (/^(node|bash|sh|python3?)$/.test(program.text) && args && ts.isArrayLiteralExpression(args)) {
            // Follow the literal script operand of a normal interpreter call.
            for (const arg of args.elements) {
              if (!ts.isStringLiteralLike(arg)) break;
              if (arg.text.startsWith("-")) continue;
              if (/\.(?:sh|bash|[cm]?js|py)$/.test(arg.text)) words.push(arg);
              break;
            }
          }
          commands.push(words);
        }
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(tree);
  return commands;
}

export function cargoScanner(read, shellSources) {
  const seen = new Set();

  // npm searches upward for the package. Only the root package's scripts are
  // in scope. An intervening manifest selects a different package.
  function selectsRootPackage(cwd) {
    const normalized = posix.relative(".", cwd) || ".";
    if (posix.isAbsolute(cwd) || normalized === ".." || normalized.startsWith("../") || cwd.includes("$")) return false;
    // The first lookup must see every literal component, including those before "..".
    const currentManifest = read(wrapperPath(cwd, "package.json"));
    if (normalized === ".") return true;
    if (currentManifest) return false;
    for (let dir = posix.dirname(normalized); dir !== "."; dir = posix.dirname(dir)) {
      if (read(wrapperPath(dir, "package.json"))) return false;
    }
    return true;
  }

  function scan(source, kind, cwd) {
    if (!source) return false;
    const commands = kind === "js" ? jsCommands(source) : kind === "shell" ? shellCommands(source) : [];
    const words = kind === "other" ? source.split(/[^a-zA-Z0-9_./-]+/)
      : commands.filter(([program]) => !["echo", "printf"].includes(program.text))
        .flatMap(command => command[0].text === "npm" ? [command[0]] : command)
        .filter(word => !word.variable).map(word => word.text);
    let cargo = words.some(word => word === "cargo" || word.endsWith("/cargo"));
    for (const word of words) {
      if (posix.isAbsolute(word) || (!word.startsWith("./") && !/\.(?:sh|bash|m?js|cjs|py)$/.test(word)
          && !word.startsWith("scripts/"))) continue;
      const path = wrapperPath(cwd, word);
      const key = JSON.stringify([cwd, path]);
      if (seen.has(key)) continue;
      seen.add(key);
      const script = read(path);
      if (!script) continue;
      const isShell = /\.(?:sh|bash)$/.test(path) || /^#![^\n]*\b(?:ba)?sh\b/.test(script);
      if (isShell) shellSources.set(path, script);
      const language = isShell ? "shell" : /\.[cm]?js$/.test(path) ? "js" : "other";
      if (scan(script, language, cwd)) cargo = true;
    }
    for (const [program, verb, name] of commands) {
      if (program.variable || program.text !== "npm" || verb?.variable || verb?.text !== "run" || !name || name.variable || !selectsRootPackage(cwd)) continue;
      const key = `npm:${name.text}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const manifest = read("package.json");
      const scripts = manifest && JSON.parse(manifest).scripts;
      if (typeof scripts?.[name.text] !== "string") continue;
      for (const event of [`pre${name.text}`, name.text, `post${name.text}`]) {
        if (typeof scripts[event] !== "string") continue;
        shellSources.set(`npm:${event}`, scripts[event]);
        if (scan(scripts[event], "shell", ".")) cargo = true;
      }
    }
    return cargo;
  }
  return scan;
}

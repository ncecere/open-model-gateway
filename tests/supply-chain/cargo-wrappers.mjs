// Static traversal of literal CI wrapper invocations. See docs/verification.md for its scope.
import assert from "node:assert/strict";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { posix } from "node:path";
import ts from "typescript";
import { shellCommands } from "./policy.mjs";

export function indexedReader(root, files) {
  const base = realpathSync(root);
  const paths = new Set(files);
  const cache = new Map();
  return path => {
    if (!paths.has(path)) return;
    if (!cache.has(path)) {
      const full = posix.join(base, path);
      assert.equal(realpathSync(full), full, `wrapper path contains a symlink: ${path}`);
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
    if (posix.isAbsolute(cwd) || cwd === ".." || cwd.startsWith("../") || cwd.includes("$")) return false;
    for (let dir = cwd; dir !== "."; dir = posix.dirname(dir)) {
      if (read(posix.join(dir, "package.json"))) return false;
    }
    return true;
  }

  function scan(source, kind, cwd) {
    if (!source) return false;
    const commands = kind === "js" ? jsCommands(source) : kind === "shell" ? shellCommands(source) : [];
    const words = kind === "other" ? source.split(/[^a-zA-Z0-9_./-]+/)
      : commands.filter(command => !["echo", "printf"].includes(command[0].text)).flat().map(word => word.text);
    let cargo = words.some(word => word === "cargo" || word.endsWith("/cargo"));
    for (const word of words) {
      if (posix.isAbsolute(word) || (!word.startsWith("./") && !/\.(?:sh|bash|m?js|cjs|py)$/.test(word)
          && !word.startsWith("scripts/"))) continue;
      const path = posix.join(cwd, word);
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
      if (program.text !== "npm" || verb?.text !== "run" || !name || !selectsRootPackage(cwd)) continue;
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

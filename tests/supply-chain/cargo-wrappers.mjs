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
          assert.ok(!paths.has(posix.normalize(path)), `indexed wrapper path is missing: ${path}`);
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

// Object properties take effect in order. A later literal cwd resolves an earlier spread.
function childDirectory(parent, options) {
  if (!options || !ts.isObjectLiteralExpression(options)) return parent;
  let cwd = parent;
  for (const prop of options.properties) {
    if (ts.isSpreadAssignment(prop) || (prop.name && ts.isComputedPropertyName(prop.name))) cwd = undefined;
    if (prop.name?.text === "cwd") {
      cwd = ts.isPropertyAssignment(prop) && ts.isStringLiteralLike(prop.initializer)
        ? wrapperPath(parent, prop.initializer.text) : undefined;
    }
  }
  return cwd;
}

// Keep the execution directory and the literal argv of each process call together.
function jsCommands(source, cwd) {
  const tree = ts.createSourceFile("wrapper.js", source, ts.ScriptTarget.Latest, false, ts.ScriptKind.JS);
  assert.equal(tree.parseDiagnostics.length, 0, "cannot parse JavaScript wrapper");
  const calls = [];
  function visit(node) {
    if (ts.isCallExpression(node)) {
      const target = node.expression;
      const name = ts.isPropertyAccessExpression(target) ? target.name.text : target.text;
      const shell = ["exec", "execSync"].includes(name);
      const direct = ["spawn", "spawnSync", "execFile", "execFileSync"].includes(name);
      const [program, args, third] = node.arguments;
      if ((shell || direct) && program && ts.isStringLiteralLike(program)) {
        const argv = args && ts.isArrayLiteralExpression(args) ? args.elements : [];
        const options = shell || (args && ts.isObjectLiteralExpression(args)) ? args : third;
        const words = argv.map(arg => ts.isStringLiteralLike(arg) ? { text: arg.text } : { variable: true, text: "" });
        const commands = shell ? shellCommands(program.text) : [[{ text: program.text }, ...words]];
        calls.push({ commands, cwd: childDirectory(cwd, options), direct });
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(tree);
  return calls;
}

// Interpreter eval strings and options with separate operands need manual review.
function interpreterLanguage(program) {
  if (program === "bash" || program === "sh") return "shell";
  if (program === "node") return "js";
  if (program === "python" || program === "python3") return "other";
}

function interpreterScript(args, language) {
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg.variable) return;
    if (arg.text === "-") return language === "shell" ? args[i + 1] : undefined;
    if (arg.text === "--") return args[i + 1];
    if (!arg.text.startsWith("-")) return arg;
    // Each of these modes consumes code, a module, or a separate option operand.
    if (language === "shell" && (/^-[^-]*[cos]/.test(arg.text) || ["--rcfile", "--init-file"].includes(arg.text))) return;
    if (language === "js" && ["-e", "-p", "--eval", "--print", "-r", "--require", "--import"].includes(arg.text)) return;
    if (language === "other" && ["-c", "-m", "-W", "-X"].includes(arg.text)) return;
  }
}

export function cargoScanner(read, shellSources) {
  const seen = new Set();
  const runningScripts = new Set();

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

  function follow(word, cwd, interpreter) {
    if (!cwd || word.variable || !word.text || posix.isAbsolute(word.text)) return false;
    if (!interpreter && !word.text.startsWith("./") && !word.text.startsWith("scripts/")
        && !/\.(?:sh|bash|[cm]?js|py)$/.test(word.text)) return false;
    const path = wrapperPath(cwd, word.text);
    const source = read(path);
    if (!source) return false;
    const isShell = /\.(?:sh|bash)$/.test(path) || /^#![^\n]*\b(?:ba)?sh\b/.test(source);
    const kind = interpreter ?? (isShell ? "shell" : /\.[cm]?js$/.test(path) ? "js" : "other");
    if (kind === "shell") shellSources.set(path, source);
    const key = JSON.stringify([posix.normalize(cwd), posix.normalize(path), kind]);
    if (seen.has(key)) return false;
    seen.add(key);
    return scan(source, kind, cwd);
  }

  function npmScript(command, cwd) {
    const [, verb, name, ...args] = command;
    if (!cwd || verb?.text !== "run" || verb.variable || !name || name.variable || !selectsRootPackage(cwd)) return false;
    const manifest = read("package.json");
    const scripts = manifest && JSON.parse(manifest).scripts;
    if (typeof scripts?.[name.text] !== "string" || runningScripts.has(name.text)) return false;
    const invocation = JSON.stringify(["npm", name.text, args]);
    if (seen.has(invocation)) return false;
    seen.add(invocation);
    runningScripts.add(name.text);
    let cargo = false;
    for (const event of [`pre${name.text}`, name.text, `post${name.text}`]) {
      if (typeof scripts[event] !== "string") continue;
      shellSources.set(`npm:${event}`, scripts[event]);
      const commands = shellCommands(scripts[event]);
      if (event === name.text && commands.length) {
        const separator = args.findIndex(word => !word.variable && word.text === "--");
        const forwarded = args.filter((arg, i) => (separator >= 0 && i > separator)
          || (i !== separator && !arg.text.startsWith("-")));
        commands.at(-1).push(...forwarded.map(arg => ({ ...arg, forwarded: true })));
      }
      if (scanCommands(commands, ".", false)) cargo = true;
    }
    runningScripts.delete(name.text);
    return cargo;
  }

  function scanCommands(commands, cwd, direct) {
    let cargo = false;
    for (const command of commands) {
      let first = 0;
      while (!command[first]?.variable && ["exec", "env", "command"].includes(command[first]?.text)) {
        const launcher = command[first++].text;
        if (command[first]?.text === "--") first++;
        if (launcher === "env") {
          while (/^[A-Za-z_][A-Za-z_0-9]*=/.test(command[first]?.text ?? "")) first++;
        }
      }
      const invocation = command.slice(first);
      const [program, ...args] = invocation;
      if (!program) continue;
      if (!program.variable && ["echo", "printf"].includes(program.text)) continue;
      if (!program.variable && program.text === "npm") {
        if (npmScript(invocation, cwd)) cargo = true;
        continue;
      }
      const interpreter = !program.variable && interpreterLanguage(program.text);
      // Direct process calls supply argv as data. Only a script operand is executable.
      const words = direct ? [program] : invocation;
      for (const word of words) {
        if (word.variable || (word.forwarded && word !== program)) continue;
        if (word.text === "cargo" || word.text.endsWith("/cargo")) cargo = true;
        if (follow(word, cwd)) cargo = true;
      }
      if (interpreter) {
        const script = interpreterScript(args, interpreter);
        if (script && follow(script, cwd, interpreter)) cargo = true;
      }
    }
    return cargo;
  }

  function scan(source, kind, cwd) {
    if (!source) return false;
    if (kind === "shell") return scanCommands(shellCommands(source), cwd, false);
    if (kind === "js") {
      let cargo = false;
      for (const call of jsCommands(source, cwd)) {
        if (scanCommands(call.commands, call.cwd, call.direct)) cargo = true;
      }
      return cargo;
    }
    const words = source.split(/[^a-zA-Z0-9_./-]+/).filter(Boolean).map(text => ({ text }));
    return scanCommands(words.map(word => [word]), cwd, false);
  }
  return scan;
}

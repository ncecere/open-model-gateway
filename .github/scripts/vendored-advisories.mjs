// Advisory-only upstream-base coverage for explicitly registered path packages.
// The real graph remains authoritative for the normal cargo-deny checks, builds,
// SBOMs and provenance. Never feed this projection to any other check.
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const registry = "registry+https://github.com/rust-lang/crates.io-index";
const object = v => v !== null && typeof v === "object" && !Array.isArray(v);
const string = v => typeof v === "string" && v.length > 0;
const strings = v => Array.isArray(v) && v.every(string);
const unique = v => new Set(v).size === v.length;
function requireThat(condition, message) {
  if (!condition) throw new Error(`vendored advisories: ${message}`);
}
function canonicalManifest(path, root) {
  requireThat(string(path) && isAbsolute(path), "manifest must be absolute");
  const rel = relative(root, path);
  requireThat(rel && !rel.startsWith(`..${sep}`) && rel !== ".." && !isAbsolute(rel), "manifest escapes workspace");
  requireThat(path === realpathSync(path) && path === resolve(path), "non-canonical manifest path (including symlinks)");
  return path;
}

export function projectMetadata(input, directory) {
  try {
    return project(input, realpathSync(directory));
  } catch (error) {
    if (error.message.startsWith("vendored advisories:")) throw error;
    throw new Error(`vendored advisories: ${error.message}`, { cause: error });
  }
}

// Deliberately accept a narrow, unambiguous TOML prefix, not a regex search
// through arbitrary TOML (which could mistake string contents for declarations).
// Cargo validates the rest of the document before this helper receives metadata.
function explicitMemberDirectories(root) {
  let text = readFileSync(join(root, "Cargo.toml"), "utf8").replace(/\r\n/g, "\n");
  const skip = () => { text = text.replace(/^(?:[ \t\n]|#[^\n]*(?:\n|$))*/, ""); };
  const take = pattern => {
    const match = text.match(pattern);
    requireThat(match, "cannot unambiguously parse explicit workspace members; root must start with [workspace] then a literal members array");
    text = text.slice(match[0].length);
    return match;
  };
  skip();
  take(/^\[workspace\][ \t]*(?:#[^\n]*)?\n/);
  skip();
  take(/^members[ \t]*=[ \t]*\[/);
  skip();
  const directories = [];
  while (!text.startsWith("]")) {
    const match = take(/^(?:"([A-Za-z0-9_./-]+)"|'([A-Za-z0-9_./-]+)')/);
    const path = match[1] ?? match[2];
    requireThat(path === "." || path.split("/").every(p => p && p !== "." && p !== ".."), "non-canonical explicit workspace member");
    directories.push(dirname(canonicalManifest(join(root, path, "Cargo.toml"), root)));
    skip();
    if (text.startsWith("]")) break;
    take(/^,/);
    skip();
  }
  take(/^\][ \t]*(?:#[^\n]*)?(?:\n|$)/);
  requireThat(directories.length > 0 && unique(directories), "empty or duplicate explicit workspace members");
  return new Set(directories);
}

function project(input, root) {
  const explicitMembers = explicitMemberDirectories(root);
  requireThat(object(input) && input.version === 1 && input.workspace_root === root, "unexpected metadata format or workspace root");
  requireThat(Array.isArray(input.packages) && input.packages.length > 0, "missing packages");
  requireThat(input.metadata === null || object(input.metadata), "unexpected workspace metadata");
  const registrations = input.metadata && Object.hasOwn(input.metadata, "vendored") ? input.metadata.vendored : {};
  requireThat(object(registrations), "vendored registrations must be a table");
  const byManifest = new Map();
  for (const [key, entry] of Object.entries(registrations)) {
    requireThat(object(entry) && Object.keys(entry).sort().join(",") === "manifest,name,version", `unexpected registration fields: ${key}`);
    requireThat(string(entry.name) && /^[A-Za-z0-9_-]+$/.test(entry.name) && string(entry.version)
      && /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(entry.version), `invalid name/version: ${key}`);
    requireThat(string(entry.manifest) && /^vendor\/.+\/Cargo\.toml$/.test(entry.manifest)
      && !entry.manifest.includes("\\") && entry.manifest.split("/").every(p => p && p !== "." && p !== ".."), `non-canonical vendor manifest: ${key}`);
    const manifest = canonicalManifest(join(root, entry.manifest), root);
    requireThat(!byManifest.has(manifest), `duplicate registration: ${key}`);
    byManifest.set(manifest, entry);
  }

  const packages = new Map();
  for (const p of input.packages) {
    requireThat(object(p) && string(p.id) && string(p.name) && string(p.version) && string(p.manifest_path)
      && isAbsolute(p.manifest_path) && (p.source === null || string(p.source)), "unexpected package shape");
    requireThat(!packages.has(p.id), `duplicate package ID: ${p.id}`);
    packages.set(p.id, p);
  }
  const members = input.workspace_members;
  const defaults = input.workspace_default_members;
  requireThat(strings(members) && members.length > 0 && unique(members) && members.every(id => packages.has(id)), "invalid workspace members");
  requireThat(strings(defaults) && unique(defaults) && defaults.every(id => members.includes(id)), "invalid default workspace members");
  requireThat(object(input.resolve) && Array.isArray(input.resolve.nodes)
    && (input.resolve.root === null || members.includes(input.resolve.root)), "missing resolve graph or invalid root");
  const nodes = new Map();
  for (const node of input.resolve.nodes) {
    requireThat(object(node) && packages.has(node.id) && !nodes.has(node.id) && strings(node.dependencies)
      && unique(node.dependencies) && Array.isArray(node.deps) && strings(node.features), "unexpected/duplicate graph node");
    for (const dep of node.deps) {
      requireThat(object(dep) && string(dep.name) && packages.has(dep.pkg) && Array.isArray(dep.dep_kinds)
        && dep.dep_kinds.length > 0 && dep.dep_kinds.every(k => object(k) && [null, "dev", "build"].includes(k.kind)
          && (k.target === null || string(k.target))), "invalid graph dependency");
    }
    const detailed = new Set(node.deps.map(d => d.pkg));
    requireThat(node.dependencies.length === detailed.size && node.dependencies.every(id => packages.has(id) && detailed.has(id)), "inconsistent graph references");
    nodes.set(node.id, node);
  }
  requireThat(nodes.size === packages.size, "incomplete resolve graph");
  const reachable = new Set();
  const visit = id => {
    if (reachable.has(id)) return;
    reachable.add(id);
    for (const dependency of nodes.get(id).dependencies) visit(dependency);
  };
  members.forEach(visit);
  requireThat(reachable.size === packages.size, "unreachable package in resolve graph");

  const remap = new Map();
  const used = new Set();
  for (const p of packages.values()) {
    const directory = dirname(p.manifest_path);
    const inVendor = directory === join(root, "vendor") || directory.startsWith(`${join(root, "vendor")}${sep}`);
    if (p.source !== null) {
      requireThat(!inVendor && !members.includes(p.id) && !byManifest.has(p.manifest_path), "vendor registration/member is not a path package");
      continue;
    }
    canonicalManifest(p.manifest_path, root);
    const prefix = `path+${pathToFileURL(dirname(p.manifest_path)).href}#`;
    requireThat(p.id === `${prefix}${p.name}@${p.version}` || p.id === `${prefix}${p.version}`, "inconsistent path package ID");
    const entry = byManifest.get(p.manifest_path);
    requireThat(!entry || !members.includes(p.id), "workspace member cannot be registered as vendored; exclude it explicitly");
    if (members.includes(p.id) && explicitMembers.has(directory) && !inVendor) continue;
    requireThat(entry, `unregistered path package: ${p.name} ${p.version}; only explicit non-vendor workspace members are first-party`);
    requireThat(entry.name === p.name && entry.version === p.version, `registration name/version mismatch: ${p.manifest_path}`);
    used.add(p.manifest_path);
    const id = `${registry}#${p.name}@${p.version}`;
    requireThat(!packages.has(id) && ![...remap.values()].includes(id), `projected package ID collision: ${id}`);
    remap.set(p.id, id);
  }
  requireThat(used.size === byManifest.size, "stale vendored registration");

  // Copy only after validating the complete input. References in both graph
  // representations must agree; dependency declarations/provenance stay real.
  const metadata = structuredClone(input);
  const mapped = id => remap.get(id) ?? id;
  for (const p of metadata.packages) if (remap.has(p.id)) {
    p.id = mapped(p.id);
    p.source = registry;
  }
  for (const node of metadata.resolve.nodes) {
    node.id = mapped(node.id);
    node.dependencies = node.dependencies.map(mapped);
    for (const dep of node.deps) dep.pkg = mapped(dep.pkg);
  }
  metadata.resolve.root = mapped(metadata.resolve.root);
  metadata.workspace_members = metadata.workspace_members.map(mapped);
  metadata.workspace_default_members = metadata.workspace_default_members.map(mapped);
  return { metadata, count: remap.size };
}

function command(binary, args, options = {}) {
  const result = spawnSync(binary, args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, ...options });
  if (result.error) throw result.error;
  requireThat(result.status !== null, `${binary} terminated by signal ${result.signal}`);
  return result;
}

export function validateScannerCoverage(stderr, metadata) {
  const prefix = String.raw`\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} `;
  const completion = new RegExp(`^${prefix}\\[INFO\\] gathered (0|[1-9]\\d*) crates in (0|[1-9]\\d*)ms$`);
  const pruning = new RegExp(`^${prefix}\\[DEBUG\\] filtered ([A-Za-z0-9_-]+) (\\S+)$`);
  let gathered;
  const filtered = new Set();
  for (const line of stderr.split("\n")) {
    if (line.includes("[INFO] gathered")) {
      const match = line.match(completion);
      requireThat(match && gathered === undefined && Number.isSafeInteger(Number(match[1])), "invalid/duplicate scanner graph completion diagnostic");
      gathered = Number(match[1]);
    } else if (/\] filtered\b/.test(line)) {
      const match = line.match(pruning);
      requireThat(match, "unparseable scanner filtered diagnostic");
      const candidates = metadata.packages.filter(p => p.name === match[1] && p.version === match[2]);
      requireThat(candidates.length === 1 && !filtered.has(candidates[0]), "unknown/ambiguous/duplicate scanner filtered package");
      filtered.add(candidates[0]);
    }
  }
  requireThat(gathered !== undefined, "scanner graph completion diagnostic missing");
  requireThat(gathered + filtered.size === metadata.packages.length,
    `scanner graph accounting mismatch: gathered ${gathered} + filtered ${filtered.size} != metadata ${metadata.packages.length}; upstream-base coverage not established`);
  for (const entry of Object.values(metadata.metadata.vendored)) {
    requireThat(![...filtered].some(p => p.name === entry.name && p.version === entry.version),
      `registered package filtered by cargo-deny: ${entry.name} ${entry.version}; upstream-base coverage not established`);
  }
}

function main() {
  requireThat(process.argv.slice(2).every(a => a === "--offline") && process.argv.length <= 3, "usage: node .github/scripts/vendored-advisories.mjs [--offline]");
  const offline = process.argv.includes("--offline") ? ["--offline"] : [];
  const binary = process.env.CARGO_DENY ?? "cargo-deny";
  const version = command(binary, ["--version"]);
  requireThat(version.status === 0 && version.stdout.trim() === "cargo-deny 0.20.2", "requires pinned cargo-deny 0.20.2");
  const raw = command("cargo", ["metadata", "--locked", "--all-features", "--format-version", "1", ...offline]);
  requireThat(raw.status === 0, `cargo metadata failed: ${raw.stderr}`);
  const { metadata, count } = projectMetadata(JSON.parse(raw.stdout), process.cwd());
  console.log(count === 0 ? "Upstream-base advisories: no registered vendored packages (real identities unchanged)."
    : `Upstream-base advisories: projecting ${count} explicitly registered vendored package(s); not a certification of patched code.`);
  const temporary = mkdtempSync(join(tmpdir(), "omg-upstream-advisories-"));
  try {
    const path = join(temporary, "metadata.json");
    writeFileSync(path, JSON.stringify(metadata), { mode: 0o600 });
    // Cargo-deny rebuilds/filters Cargo's graph (targets, features, exclusions).
    // In pinned 0.20.2 its build_with_metadata callback logs EVERY pruned package
    // at debug level (src/cargo-deny/common.rs). Refuse a green result if any
    // registered identity was pruned; do not invent extra roots/edges or change
    // the real policy to force inclusion. Strictly reconcile the gathered count
    // and each unique filtered identity against ALL projected packages, so lost
    // or changed filter diagnostics cannot silently disable this guard.
    const scan = command(binary, ["--log-level", "debug", "--color", "never", "--all-features", ...offline, "--metadata-path", path, "check", "advisories"]);
    process.stdout.write(scan.stdout);
    process.stderr.write(scan.stderr);
    if (scan.status === 0 && count > 0) validateScannerCoverage(scan.stderr, metadata);
    process.exitCode = scan.status;
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (error) { console.error(`vendored advisories: ${error.message}`); process.exitCode = 1; }
}

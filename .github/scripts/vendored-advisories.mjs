// Advisory-only upstream-base coverage for explicitly registered path packages.
// The real graph remains authoritative for the normal cargo-deny checks, builds,
// SBOMs and provenance. Never feed this projection to any other check.
import { spawnSync } from "node:child_process";
import { mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
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

function project(input, root) {
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
    if (p.source !== null) {
      requireThat(!members.includes(p.id) && !byManifest.has(p.manifest_path), "registration/member is not a path package");
      continue;
    }
    canonicalManifest(p.manifest_path, root);
    const prefix = `path+${pathToFileURL(dirname(p.manifest_path)).href}#`;
    requireThat(p.id === `${prefix}${p.name}@${p.version}` || p.id === `${prefix}${p.version}`, "inconsistent path package ID");
    const entry = byManifest.get(p.manifest_path);
    if (members.includes(p.id)) {
      requireThat(!entry, "workspace member cannot be registered as vendored");
      continue;
    }
    requireThat(entry, `unknown non-workspace path dependency: ${p.name} ${p.version}`);
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
    const scan = command(binary, ["--all-features", ...offline, "--metadata-path", path, "check", "advisories"], { stdio: "inherit" });
    process.exitCode = scan.status;
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (error) { console.error(`vendored advisories: ${error.message}`); process.exitCode = 1; }
}

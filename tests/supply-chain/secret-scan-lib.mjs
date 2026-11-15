// Shared helpers for the secret-scanning tests (#596): the workflow job, the config, a stand-in
// gitleaks on PATH that records its arguments, and temporary git repositories. Every secret
// here is built from pieces at run time; none is written into the repository.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadWorkflow, root, stepsOf } from "./lib.mjs";

export const CONFIG_FILE = ".gitleaks.toml";
export const HOOK = "scripts/secret-scan.py";
export const ZEROS = "0".repeat(40);
export const SCAN_STEP = "Scan the commits and the tree for secrets";
export const INSTALL_STEP = "Install gitleaks (pinned release, SHA-256 verified)";
export const FETCH = "bash .github/scripts/fetch-verified.sh";
export const ASSET = /^https:\/\/github\.com\/gitleaks\/gitleaks\/releases\/download\/v(\d+\.\d+\.\d+)\/gitleaks_\1_linux_x64\.tar\.gz$/;
export const RULES_IN_CANARY = ["aws-access-token", "omg-gateway-key", "omg-session-cookie", "omg-file-encryption-key", "omg-url-credentials", "generic-api-key", "private-key"];

export const copy = () => structuredClone(loadWorkflow("ci"));
export const jobOf = data => data.jobs["secret-scan"];
export const stepNamed = (data, name) => stepsOf(jobOf(data)).find(s => s.name === name);
export const scanStep = data => stepNamed(data, SCAN_STEP);
export const installStep = data => stepNamed(data, INSTALL_STEP);

export function pinOf(job) {
  const calls = stepsOf(job).filter(s => typeof s.run === "string" && s.run.includes("fetch-verified.sh"));
  if (calls.length !== 1) return null;
  const m = calls[0].run.match(/fetch-verified\.sh (\S+) ([0-9a-f]{64}) "\$\{RUNNER_TEMP\}\/gitleaks\.tar\.gz"/);
  const asset = m && m[1].match(ASSET);
  return asset ? { url: m[1], version: asset[1], sha256: m[2] } : null;
}

export function readConfig(text) {
  const dir = mkdtempSync(join(tmpdir(), "gitleaks-config-"));
  try {
    writeFileSync(join(dir, "c.toml"), text);
    const r = spawnSync("python3", ["-I", "-c", "import json, sys, tomllib; print(json.dumps(tomllib.load(open(sys.argv[1], 'rb'))))", join(dir, "c.toml")], { encoding: "utf8" });
    assert.equal(r.status, 0, r.stderr);
    return JSON.parse(r.stdout);
  } finally { rmSync(dir, { recursive: true, force: true }); }
}

// ---- git ----

export const gitEnv = () => ({
  ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_"))),
  GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
});
export function git(dir, ...args) {
  const r = spawnSync("git", args, { cwd: dir, encoding: "utf8", env: gitEnv() });
  assert.equal(r.status, 0, `git ${args.join(" ")}: ${r.stderr}`);
  return r.stdout.trim();
}
export const commitAll = (dir, message) => { git(dir, "add", "-A"); git(dir, "commit", "-q", "-m", message); return git(dir, "rev-parse", "HEAD"); };

// Random made-up credentials, generated per call.
const pick = (alphabet, n) => Array.from({ length: n }, () => alphabet[Math.floor(Math.random() * alphabet.length)]).join("");
const HEX = "0123456789abcdef";
const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
export const fakes = {
  aws: () => `AKIA${pick("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", 16)}`,
  gateway: () => `omg_${pick(HEX, 32)}.${pick(HEX, 64)}`,
  session: () => `omg_session=${pick(HEX, 64)}`,
  fileKey: () => `GATEWAY_FILE_KEYS=k${pick("0123456789", 4)}:${pick(B64, 43)}=`,
  url: () => `postgres://svc:${pick(B64, 20)}@db.internal:5432/app`,
  pem: () => ["-----BEGIN", " PRIVATE KEY-----\n", Array.from({ length: 3 }, () => pick(B64, 64)).join("\n"), "\n-----END", " PRIVATE KEY-----\n"].join(""),
};

// A repository with a config and three commits, a bin directory with a gitleaks on PATH
// (the stand-in, or the real scanner when `real` names one) and a log of its calls.
export function makeRepo({ real } = {}) {
  const base = mkdtempSync(join(tmpdir(), "secret-scan-fixture-"));
  const dir = join(base, "repo");
  const bin = join(base, "bin");
  mkdirSync(dir);
  mkdirSync(bin);
  git(dir, "init", "-q", "-b", "main");
  copyFileSync(join(root, CONFIG_FILE), join(dir, CONFIG_FILE));
  const c1 = commitAll(dir, "config");
  writeFileSync(join(dir, "a.txt"), "one\n");
  const c2 = commitAll(dir, "one");
  writeFileSync(join(dir, "b.txt"), "two\n");
  const c3 = commitAll(dir, "two");
  const log = join(base, "stub.log");
  if (real) symlinkSync(real, join(bin, "gitleaks"));
  else installStub(join(bin, "gitleaks"));
  return { base, dir, bin, log, c1, c2, c3, cleanup: () => rmSync(base, { recursive: true, force: true }) };
}

// The stand-in records its arguments, working directory and the report template it was
// given, and behaves as the environment says (STUB_*): canary scans (a path other than ".")
// find every rule or none; the real scans find one rule or nothing.
const STUB = `#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
rec = {"argv": a, "cwd": os.getcwd()}
if "--report-template" in a:
    rec["template"] = open(a[a.index("--report-template") + 1]).read()
if "--config" in a:
    rec["config"] = a[a.index("--config") + 1]
if os.environ.get("STUB_TREE") == "1" and a[:1] == ["dir"]:
    tree = {}
    for folder, _, names in os.walk("."):
        for name in names:
            path = os.path.join(folder, name)
            tree[os.path.relpath(path)] = open(path, "rb").read().decode("latin-1")
    rec["tree"] = tree
open(os.environ["STUB_LOG"], "a").write(json.dumps(rec) + "\\n")
if a == ["version"]:
    print(os.environ.get("STUB_VERSION", "8.30.1"))
    sys.exit(0)
mode, path = a[0], a[-1]
RULES = ${JSON.stringify(RULES_IN_CANARY)}
def finding(rule):
    print("finding rule=%s file=x.txt line=1 commit=" % rule)
if path != "." and os.environ.get("STUB_NO_CANARY") != "1":
    if os.environ.get("STUB_CANARY_" + mode.upper(), "find") == "miss":
        sys.exit(0)
    for rule in RULES:
        if rule != os.environ.get("STUB_CANARY_SKIP"):
            finding(rule)
    sys.exit(1)
if mode == "git" and os.environ.get("STUB_COUNT", "1") == "1":
    sys.stderr.write("INF 3 commits scanned.\\n")
if os.environ.get("STUB_SCAN_" + mode.upper(), "clean") == "find":
    finding("generic-api-key")
    sys.stderr.write("WRN leaks found: 1\\n")
    sys.exit(1)
sys.exit(0)
`;
function installStub(file) {
  writeFileSync(file, STUB);
  chmodSync(file, 0o755);
}
export const readCalls = log => (existsSync(log) ? readFileSync(log, "utf8").trim().split("\n").filter(Boolean).map(line => JSON.parse(line)) : []);

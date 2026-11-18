// Repository discovery must include source additions without reading ignored artifacts.
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { findDockerfiles } from "./lib.mjs";
import { buildPath, composeDotenvViolations, repoComposeFiles, tracked } from "./policy.mjs";
import { git } from "./secret-scan-lib.mjs";

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), "omg-inventory-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  git(dir, "init", "-q");
  const write = (path, text) => {
    mkdirSync(dirname(join(dir, path)), { recursive: true });
    writeFileSync(join(dir, path), text);
  };
  write(".gitignore", ".local/\nprivate/\nnode_modules/\ntarget*/\ndist/\n.turbo/\n");
  git(dir, "add", ".gitignore");
  return { dir, write };
}

function withEnv(values, run) {
  const before = Object.fromEntries(Object.keys(values).map(key => [key, process.env[key]]));
  try {
    Object.assign(process.env, values);
    return run();
  } finally {
    for (const [key, value] of Object.entries(before)) {
      if (value === undefined) delete process.env[key]; else process.env[key] = value;
    }
  }
}

test("Git inventories include tracked force-added and untracked source, excluding ignored artifacts", t => {
  const { dir, write } = fixture(t);
  const dockerfiles = ["Dockerfile", "new/CONTAINERFILE.release", "new/space name\nline/x.dockerfile", "x/.local/Dockerfile", "private/Containerfile"];
  const compose = ["compose.yaml", "new/docker-compose.dev.yml", "new/stack.yaml", "x/.local/compose.yaml", "private/stack.yml"];
  for (const file of [...dockerfiles, ".local/old/Dockerfile", "private/ignored/Dockerfile", "node_modules/x/Dockerfile", "Dockerfile.dockerignore"]) write(file, "FROM alpine:3\n");
  for (const file of [...compose, ".local/old/compose.yaml", "private/ignored/stack.yaml", ".github/compose.yaml"]) write(file, "services:\n  app: {}\n");
  git(dir, "add", "Dockerfile", "compose.yaml");
  git(dir, "add", "-f", "x/.local/Dockerfile", "x/.local/compose.yaml", "private/Containerfile", "private/stack.yml");
  assert.deepEqual(findDockerfiles(dir).map(path => path.slice(dir.length + 1)).sort(), dockerfiles.sort());
  assert.deepEqual(repoComposeFiles(dir), compose.sort());
  assert.deepEqual([...tracked(dir)].sort(), [".gitignore", "Dockerfile", "compose.yaml", "private/Containerfile", "private/stack.yml", "x/.local/Dockerfile", "x/.local/compose.yaml"].sort());
  for (const path of [".", "new", "x/.local", "private", ...dockerfiles, ...compose]) {
    assert.deepEqual(buildPath(path, ".", dir), { path }, path);
  }
  for (const path of [".local", ".local/old/Dockerfile", "private/ignored", "private/ignored/stack.yaml", "missing"]) {
    assert.match(buildPath(path, ".", dir).refused ?? "", /outside the Git source inventory/, path);
  }
  assert.deepEqual(buildPath("../.local/Dockerfile", "x/sub", dir), { path: "x/.local/Dockerfile" });
});

test("build inventories refuse untracked embedded repositories and tracked gitlinks", t => {
  const { dir, write } = fixture(t);
  write("nested/Dockerfile", "FROM scratch\n");
  write("nested/compose.yaml", "services: {}\n");
  const nested = join(dir, "nested");
  git(nested, "init", "-q");
  git(nested, "add", "Dockerfile", "compose.yaml");
  git(nested, "commit", "-qm", "fixture");
  function assertBoundary() {
    for (const inspect of [() => findDockerfiles(dir), () => repoComposeFiles(dir), () => buildPath(".", ".", dir)]) {
      assert.throws(inspect, /nested Git repository.*nested/);
    }
  }
  assertBoundary();
  git(dir, "add", "nested");
  assertBoundary();
  rmSync(nested, { recursive: true });
  assertBoundary();
});

test("Git inventories refuse undecodable filename bytes and preserve a literal BOM", t => {
  const { dir, write } = fixture(t);
  const bomName = "\ufeffexample.txt";
  write(bomName, "fixture");
  git(dir, "rm", "--cached", "-q", ".gitignore");
  git(dir, "add", bomName);
  assert.ok(tracked(dir).has(bomName));
  writeFileSync(Buffer.concat([Buffer.from(join(dir, "example-")), Buffer.from([0xff])]), "fixture");
  assert.throws(() => findDockerfiles(dir), /utf-8/i);
  git(dir, "add", "-A");
  assert.throws(() => tracked(dir), /utf-8/i);
});

test("Git discovery ignores inherited repository, index and config overrides", t => {
  const { dir, write } = fixture(t);
  write("Dockerfile", "FROM alpine:3\n");
  write("compose.yaml", "services: {}\n");
  git(dir, "add", "Dockerfile", "compose.yaml");
  const { dir: other } = fixture(t);
  const before = git(other, "status", "--porcelain");
  withEnv({
    GIT_DIR: join(other, ".git"), GIT_WORK_TREE: other, GIT_INDEX_FILE: join(other, ".git/index"),
    GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "core.bare", GIT_CONFIG_VALUE_0: "true",
  }, () => {
    assert.deepEqual(findDockerfiles(dir), [join(dir, "Dockerfile")]);
    assert.deepEqual(repoComposeFiles(dir), ["compose.yaml"]);
    assert.ok(tracked(dir).has("Dockerfile"));
    assert.deepEqual(buildPath("Dockerfile", ".", dir), { path: "Dockerfile" });
  });
  assert.equal(git(other, "status", "--porcelain"), before);
});

test("Git inventory errors cannot turn checks into empty or partial passes", t => {
  const { dir, write } = fixture(t);
  // Use a separate temporary directory so Git cannot find the fixture's parent repo.
  const outside = mkdtempSync(join(tmpdir(), "omg-no-git-"));
  t.after(() => rmSync(outside, { recursive: true, force: true }));
  const calls = [
    () => findDockerfiles(outside), () => repoComposeFiles(outside),
    () => tracked(outside), () => buildPath(".", ".", outside),
  ];
  for (const call of calls) assert.throws(call, /git|Git/);
  write("bin/git", `#!${process.execPath}\nprocess.stdout.write("Dockerfile\\0.env\\0");\nprocess.stderr.write("fixture git failure");\nprocess.exitCode = 128;\n`);
  chmodSync(join(dir, "bin/git"), 0o755);
  const checked = [
    () => findDockerfiles(dir), () => repoComposeFiles(dir), () => tracked(),
    () => buildPath(".", ".", dir), () => composeDotenvViolations(["compose.yaml"]),
  ];
  withEnv({ PATH: join(dir, "bin") }, () => {
    for (const call of checked) assert.throws(call, /fixture git failure/);
  });
  withEnv({ PATH: outside }, () => {
    for (const call of checked) assert.throws(call, /ENOENT/);
  });
});

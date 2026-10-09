// Runtime image contract (distroless, no shell): builds the image, or uses
// OMG_CONTAINER_IMAGE, and runs it hardened (read-only rootfs, no capabilities,
// no-new-privileges) against a disposable PostgreSQL container on a private
// Docker network. Secret import, umask and argument handling are unit- and
// process-tested in Rust (apps/gateway/src/startup.rs, apps/gateway/tests/startup.rs).
// Requires Docker. Never touches the demo or staging projects.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { after, before, test } from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));
const image = process.env.OMG_CONTAINER_IMAGE || "open-model-gateway:container-test";
const binary = "/usr/local/bin/open-model-gateway";
const version = readFileSync(join(root, "apps/gateway/Cargo.toml"), "utf8").match(/^version = "([^"]+)"$/m)[1];
const suffix = randomBytes(4).toString("hex");
const network = `omg-container-test-${suffix}`;
const database = `omg-container-test-pg-${suffix}`;
const password = randomBytes(16).toString("hex");
const work = mkdtempSync(join(tmpdir(), "omg-container-test-"));
const containers = new Set([database]);

function docker(args, options = {}) {
  const result = spawnSync("docker", args, { encoding: "utf8", timeout: 120_000, ...options });
  assert.equal(result.error, undefined, `docker ${args[0]}: ${result.error}`);
  return result;
}

function ok(args, options) {
  const result = docker(args, options);
  assert.equal(result.status, 0, `docker ${args.join(" ")}\n${result.stderr}`);
  return result.stdout.trim();
}

function secret(name, contents) {
  const path = join(work, name);
  writeFileSync(path, contents);
  chmodSync(path, 0o444); // the container user is 10001, not the file owner
  return path;
}

// The hardened run used by staging: read-only rootfs, no capabilities.
function hardened(name, secretPath, extra = []) {
  containers.add(name);
  return [
    "--name", name, "--network", network, "--read-only", "--cap-drop", "ALL",
    "--security-opt", "no-new-privileges:true", "--tmpfs", "/tmp:rw,noexec,nosuid,size=8m",
    "-e", "DATABASE_URL_FILE=/run/secrets/database_url", "-e", "GATEWAY_PUBLIC_URL=https://localhost:18443",
    "-e", "AWS_EC2_METADATA_DISABLED=true",
    "-v", `${secretPath}:/run/secrets/database_url:ro`, ...extra,
  ];
}

function psql(sql) {
  return ok(["exec", database, "psql", "-U", "postgres", "-d", "postgres", "-tAc", sql]);
}

async function waitFor(check, label, seconds = 90) {
  for (let i = 0; i < seconds * 2; i++) {
    if (check()) return;
    await new Promise(resolve => setTimeout(resolve, 500));
  }
  assert.fail(`timed out waiting for ${label}`);
}

let databaseUrlFile;

before(async () => {
  if (!process.env.OMG_CONTAINER_IMAGE) {
    const build = spawnSync("docker", ["build", "-t", image, "."], { cwd: root, stdio: "inherit", timeout: 3_600_000 });
    assert.equal(build.status, 0, "docker build failed");
  }
  ok(["network", "create", "--internal", network]);
  ok(["run", "-d", "--name", database, "--network", network, "-e", `POSTGRES_PASSWORD=${password}`, "postgres:17-alpine"]);
  await waitFor(() => docker(["exec", database, "pg_isready", "-U", "postgres", "-h", "127.0.0.1"]).status === 0, "PostgreSQL");
  databaseUrlFile = secret("database url", `postgres://postgres:${password}@${database}:5432/postgres\n`);
});

after(() => {
  for (const name of containers) docker(["rm", "-f", "-v", name]);
  docker(["network", "rm", network]);
  rmSync(work, { recursive: true, force: true });
});

test("image config: binary entrypoint, serve by default, UID 10001, exec-form healthcheck", () => {
  const config = JSON.parse(ok(["image", "inspect", image, "--format", "{{json .Config}}"]));
  assert.equal(config.User, "10001:10001");
  assert.deepEqual(config.Entrypoint, [binary]);
  assert.deepEqual(config.Cmd, ["serve"]);
  assert.deepEqual(config.Healthcheck.Test, ["CMD", binary, "healthcheck", "--timeout", "3s"]);
  assert.ok(config.Env.includes("GATEWAY_WEB_DIR=/app/web"));
  assert.equal(ok(["run", "--rm", image, "--version"]), `open-model-gateway ${version}`);
});

test("filesystem has no shell, package manager, curl, perl, Node or Cargo", () => {
  const name = `omg-container-test-fs-${suffix}`;
  containers.add(name);
  ok(["create", "--name", name, image]);
  const archive = join(work, "rootfs.tar");
  ok(["export", "--output", archive, name]);
  const listing = spawnSync("tar", ["-tf", archive], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
  assert.equal(listing.status, 0);
  const entries = new Set(listing.stdout.split("\n").map(entry => entry.replace(/\/$/, "")));
  const forbidden = [...entries].filter(entry =>
    /(^|\/)(sh|bash|dash|ash|busybox|curl|wget|perl|apt|apt-get|dpkg|node|npm|cargo|rustc|container-entrypoint)$/.test(entry)
    && /^(usr\/)?(local\/)?s?bin\//.test(entry));
  assert.deepEqual(forbidden, []);
  for (const required of ["usr/local/bin/open-model-gateway", "app/web/index.html", "etc/ssl/certs/ca-certificates.crt"]) {
    assert.ok(entries.has(required), `missing ${required}`);
  }
  assert.ok([...entries].some(entry => /(^|\/)libgcc_s\.so\.1$/.test(entry)), "libgcc_s is required by the binary");
  const shell = docker(["run", "--rm", "--entrypoint", "/bin/sh", image, "-c", "exit 0"]);
  assert.notEqual(shell.status, 0, "an image shell must not exist");
});

test("refuses ambiguous secrets before configuration without disclosure", () => {
  const result = docker(["run", "--rm", ...hardened(`omg-container-test-ambiguous-${suffix}`, databaseUrlFile,
    ["-e", "DATABASE_URL=postgres://direct-secret-marker"]), image]);
  containers.delete(`omg-container-test-ambiguous-${suffix}`);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.match(result.stderr, /secret configuration: DATABASE_URL: set either the variable or its _FILE companion, not both/);
  for (const value of ["direct-secret-marker", password, "/run/secrets/database_url"]) {
    assert.equal(result.stderr.includes(value), false, "must not disclose secrets or paths");
  }
});

test("serve never migrates; explicit migrate does; serve then becomes healthy read-only", async () => {
  // A fresh database: serve reads the _FILE secret, connects, and refuses to start.
  const refused = docker(["run", "--rm", ...hardened(`omg-container-test-serve-${suffix}`, databaseUrlFile), image]);
  assert.notEqual(refused.status, 0);
  assert.match(refused.stderr, /database schema is not ready; run `open-model-gateway migrate`/);
  assert.equal(refused.stderr.includes(password), false);
  assert.equal(psql("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'"), "0", "serve must not create tables");

  const migrated = docker(["run", "--rm", ...hardened(`omg-container-test-migrate-${suffix}`, databaseUrlFile), image, "migrate"]);
  assert.equal(migrated.status, 0, migrated.stderr);
  assert.match(migrated.stdout, /Migrations applied\./);
  assert.notEqual(psql("SELECT count(*) FROM public._sqlx_migrations"), "0");

  const name = `omg-container-test-gateway-${suffix}`;
  const id = ok(["run", "-d", ...hardened(name, databaseUrlFile, ["--health-interval", "1s", "--health-start-period", "60s"]), image]);
  const health = () => JSON.parse(ok(["inspect", "--format", "{{json .State}}", id]));
  await waitFor(() => health().Health?.Status === "healthy" || !health().Running, "Docker HEALTHCHECK");
  assert.equal(health().Health.Status, "healthy", docker(["logs", id]).stderr);

  const probe = docker(["exec", id, binary, "healthcheck"]);
  assert.equal(probe.status, 0, probe.stderr);
  assert.equal(probe.stdout, "healthy (HTTP 200)\n");
  // Loopback only by default; never a probe of another host.
  assert.equal(docker(["exec", id, binary, "healthcheck", "--url", `http://${database}:5432/`]).status, 1);
  assert.notEqual(docker(["exec", id, "/bin/sh", "-c", "exit 0"]).status, 0);
  // docker top needs a PID column; every process must run as 10001:10001.
  const top = ok(["top", id, "-eo", "pid,uid,gid"]).split("\n").slice(1).map(line => line.trim().split(/\s+/));
  assert.ok(top.length > 0);
  assert.deepEqual([...new Set(top.map(([, uid, gid]) => `${uid}:${gid}`))], ["10001:10001"]);
  assert.equal(JSON.parse(ok(["inspect", "--format", "{{json .HostConfig.ReadonlyRootfs}}", id])), true);

  // The binary is PID 1 and shuts down gracefully on SIGTERM without a wrapper.
  ok(["stop", "--time", "20", id]);
  assert.equal(health().ExitCode, 0);
});

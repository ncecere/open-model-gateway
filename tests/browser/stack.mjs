// Boots an isolated stack for the Playwright suite: a disposable PostgreSQL database,
// the gateway binary serving a test-only SPA build, the passwordless local OIDC issuer
// (tests only) and a deterministic mock upstream. No paid providers, never the demo.
import { spawn, spawnSync } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import pg from "pg";
import { createDemoIssuer } from "../../scripts/demo-oidc.mjs";
import { createMockUpstream } from "./mock-upstream.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const portBase = Number(process.env.OMG_BROWSER_PORT_BASE ?? 18290);
export const PORTS = { gateway: portBase + 1, issuer: portBase + 2, upstream: portBase + 3 };
export const BASE_URL = `http://127.0.0.1:${PORTS.gateway}`;
export const ISSUER_URL = `http://127.0.0.1:${PORTS.issuer}`;
export const UPSTREAM_URL = `http://127.0.0.1:${PORTS.upstream}/v1`;
const work = path.join(root, "target/browser-tests");
const webDir = process.env.OMG_BROWSER_WEB_DIR ? path.resolve(process.env.OMG_BROWSER_WEB_DIR) : path.join(work, "web-dist");
const binary = process.env.OMG_BROWSER_GATEWAY_BIN ? path.resolve(process.env.OMG_BROWSER_GATEWAY_BIN) : path.join(root, "target/debug/open-model-gateway");
// Local default: the disposable regression cluster (54339). Never the demo (54349).
const adminUrl = new URL(process.env.OMG_BROWSER_ADMIN_DATABASE_URL ?? "postgres://gateway:gateway@127.0.0.1:54339/gateway");
const run = (cmd, args, env) => {
  const r = spawnSync(cmd, args, { cwd: root, stdio: "inherit", env: env ?? process.env });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed (${r.status})`);
};

function gatewayEnv(databaseUrl) {
  const envFile = path.join(work, "gateway.env");
  writeFileSync(envFile, "# Browser-suite gateway: configuration comes from the process environment.\n");
  return {
    PATH: process.env.PATH, HOME: process.env.HOME, RUST_LOG: process.env.RUST_LOG ?? "open_model_gateway=info",
    GATEWAY_ENV_FILE: envFile, // never implicitly load a developer's root .env
    DATABASE_URL: databaseUrl,
    GATEWAY_ENV: "development",
    GATEWAY_LISTEN: `127.0.0.1:${PORTS.gateway}`,
    GATEWAY_WEB_DIR: webDir,
    GATEWAY_PUBLIC_URL: BASE_URL,
    GATEWAY_OIDC_ISSUER: ISSUER_URL,
    GATEWAY_OIDC_CLIENT_ID: "gateway-local-demo",
    GATEWAY_OIDC_GROUPS_CLAIM: "groups",
    GATEWAY_SECRET_ENV_ALLOWLIST: "",
    GATEWAY_LOCAL_UPSTREAMS: JSON.stringify([{ endpoint: UPSTREAM_URL, addresses: ["127.0.0.1"] }]),
    GATEWAY_ALERT_INTERVAL_SECONDS: "0",
  };
}

const listen = (server, port) => new Promise((resolve, reject) => { server.once("error", reject); server.listen(port, "127.0.0.1", resolve); });
const close = server => new Promise(resolve => server.close(() => resolve()));

export async function startStack() {
  mkdirSync(work, { recursive: true });
  if (adminUrl.port === "54349" || /enterprise_demo/.test(adminUrl.pathname)) throw new Error("Refusing to use the local demo database cluster");
  if (process.env.OMG_BROWSER_SKIP_BUILD !== "1") {
    run("npm", ["run", "build", "--workspace", "@omg/web", "--", "--outDir", webDir, "--emptyOutDir"]);
    if (!process.env.OMG_BROWSER_GATEWAY_BIN) run("cargo", ["build", "-p", "open-model-gateway"]);
  }
  if (!existsSync(path.join(webDir, "index.html"))) throw new Error(`Missing SPA build at ${webDir}`);
  if (!existsSync(binary)) throw new Error(`Missing gateway binary at ${binary}`);

  const database = `omg_browser_${Date.now().toString(36)}_${process.pid}`;
  const admin = new pg.Client({ connectionString: adminUrl.href });
  await admin.connect();
  await admin.query(`CREATE DATABASE ${database}`);
  await admin.end();
  const dbUrl = new URL(adminUrl.href); dbUrl.pathname = `/${database}`;
  const env = gatewayEnv(dbUrl.href);
  const state = { database, adminUrl: adminUrl.href };
  try {
    run(binary, ["migrate"], env);
    // Trusted first-administrator provisioning, exactly as an operator would.
    run(binary, ["provision-user", "--email", "operator@demo.invalid", "--platform-admin"], env);
    const upstream = createMockUpstream();
    const issuer = createDemoIssuer({ issuer: ISSUER_URL, callback: `${BASE_URL}/api/v1/auth/callback` });
    await listen(upstream.server, PORTS.upstream);
    await listen(issuer, PORTS.issuer);
    const log = createWriteStream(path.join(work, "gateway.log"));
    const gateway = spawn(binary, ["serve"], { cwd: root, env, stdio: ["ignore", "pipe", "pipe"] });
    gateway.stdout.pipe(log); gateway.stderr.pipe(log);
    Object.assign(state, { upstream, issuer, gateway });
    const deadline = Date.now() + 30_000;
    for (;;) {
      if (gateway.exitCode !== null) throw new Error(`gateway exited (${gateway.exitCode}); see target/browser-tests/gateway.log`);
      try { if ((await fetch(`${BASE_URL}/health/ready`)).ok) break; } catch { /* starting */ }
      if (Date.now() > deadline) throw new Error("gateway did not become ready");
      await new Promise(r => setTimeout(r, 200));
    }
    return state;
  } catch (error) { await stopStack(state); throw error; }
}

export async function stopStack(state) {
  if (!state) return;
  if (state.gateway && state.gateway.exitCode === null) {
    const exited = new Promise(resolve => state.gateway.once("exit", resolve));
    state.gateway.kill("SIGTERM");
    await Promise.race([exited, new Promise(r => setTimeout(r, 5000))]);
    if (state.gateway.exitCode === null) state.gateway.kill("SIGKILL");
  }
  if (state.issuer?.listening) await close(state.issuer);
  if (state.upstream?.server.listening) await close(state.upstream.server);
  if (state.database && process.env.OMG_BROWSER_KEEP_DB !== "1") {
    const admin = new pg.Client({ connectionString: state.adminUrl });
    await admin.connect();
    await admin.query(`DROP DATABASE IF EXISTS ${state.database} WITH (FORCE)`);
    await admin.end();
  }
}

// `node tests/browser/stack.mjs` keeps an isolated stack running for manual exploration.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const state = await startStack();
  console.log(`Browser-suite stack ready at ${BASE_URL} (database ${state.database}); Ctrl-C to stop and drop it.`);
  for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, () => stopStack(state).then(() => process.exit(0)));
}

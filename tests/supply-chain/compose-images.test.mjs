// Compose service images in the root and deploy stacks (#565).
import assert from "node:assert/strict";
import { test } from "node:test";
import { parseStrict, readLocalFromRepo, toData } from "./lib.mjs";
import { PINNED_IMAGE, checkedComposeFiles } from "./policy.mjs";

const staging = "deploy/staging/compose.yaml";
const loadtest = "deploy/loadtest/compose.yaml";
const candidate = "${GATEWAY_IMAGE:-open-model-gateway:staging}";
const loadImage = "${OMG_LOADTEST_IMAGE:-omg-loadtest:local}";

// staging.py builds gateway once for the gateway and its two ops commands.
// loadtest.py builds one tag before Compose starts the replicas and tools.
const stagingConsumers = new Set(["gateway", "migrate", "grant-runtime"]);
const loadConsumers = new Set(["gateway-1", "gateway-2", "gateway-3", "gateway-cli", "mock-upstream", "loadgen"]);
const stagingOverrides = new Map([["postgres", "STAGING_POSTGRES_IMAGE"], ["ingress", "STAGING_CADDY_IMAGE"]]);

function violations(file, data) {
  const found = [];
  for (const [name, service] of Object.entries(data.services ?? {})) {
    if (!service || !Object.hasOwn(service, "image")) continue;
    let image = service.image;
    if (service.pull_policy === "never") {
      if (file === staging && stagingConsumers.has(name) && image === candidate &&
          service.build?.context === "../.." && service.build.dockerfile === "Dockerfile") continue;
      if (file === loadtest && loadConsumers.has(name) && image === loadImage) continue;
    }
    // Only the two staging verifier slots support overriding a pinned default.
    if (file === staging && stagingOverrides.has(name) && typeof image === "string") {
      const prefix = "${" + stagingOverrides.get(name) + ":-";
      if (image.startsWith(prefix) && image.endsWith("}")) image = image.slice(prefix.length, -1);
    }
    if (typeof image !== "string" || !PINNED_IMAGE.test(image)) {
      found.push(`${file}: services.${name}.image must be pinned by digest or use its approved local build with pull_policy: never`);
    }
  }
  return found;
}

test("every upstream Compose service image in the root and deploy stacks has a digest", () => {
  const files = [...checkedComposeFiles()].filter(file => !file.includes("/") || file.startsWith("deploy/"));
  assert.deepEqual(files.flatMap(file => violations(file, toData(parseStrict(readLocalFromRepo(file), file)))), []);
});

test("fixtures distinguish pinned upstream defaults, local build outputs and overlays", () => {
  const pinned = "postgres:17-alpine@sha256:" + "a".repeat(64);
  const source = `x-db: &db {image: ${pinned}}\nservices:\n  db: {<<: *db}\n  app: {build: .}\n  overlay: {environment: {PROFILE: test}}\n`;
  assert.deepEqual(violations("compose.yml", toData(parseStrict(source, "compose.yml"))), []);
  const floating = source.replace(pinned, "postgres:17-alpine");
  assert.match(violations("compose.yml", toData(parseStrict(floating, "compose.yml"))).join(), /services\.db\.image/);
  const check = (file, name, service) => violations(file, { services: { [name]: service } });
  const local = { image: candidate, pull_policy: "never", build: { context: "../..", dockerfile: "Dockerfile" } };
  assert.deepEqual(check(staging, "gateway", local), []);
  assert.deepEqual(check(loadtest, "gateway-cli", { image: loadImage, pull_policy: "never" }), []);
  const wrapped = "${STAGING_POSTGRES_IMAGE:-" + pinned + "}";
  assert.deepEqual(check(staging, "postgres", { image: wrapped }), []);
  for (const [file, name, service] of [
    ...["postgres", "postgres:17-alpine", pinned.slice(0, -1), pinned + ":extra", "${IMAGE}", null, [pinned]].map(image => ["compose.yml", "db", { image }]),
    ["compose.yml", "db", { image: "postgres:17", build: "." }],
    [staging, "postgres", { image: "${STAGING_POSTGRES_IMAGE:-postgres:17}" }],
    [staging, "gateway", { ...local, build: undefined }],
    [loadtest, "loadgen", { image: loadImage }],
    [loadtest, "extra", { image: loadImage, pull_policy: "never" }],
    ["compose.yml", "loadgen", { image: loadImage, pull_policy: "never" }],
    ["compose.yml", "postgres", { image: wrapped }],
  ]) assert.equal(check(file, name, service).length, 1, `${file} ${name} ${JSON.stringify(service)}`);
});

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));
const text = (path) => readFileSync(join(root, path), "utf8");
const json = (path) => JSON.parse(text(path));
const cargoVersion = text("apps/gateway/Cargo.toml").match(/^version = "([^"]+)"$/m)[1];
// An explicit qualification target catches a consistently stale metadata set.
const target = process.env.GATEWAY_RELEASE_VERSION ?? cargoVersion;
assert.match(target, /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/);

test("gateway and browser manifests agree with the release target", () => {
  assert.equal(cargoVersion, target);
  assert.equal(json("package.json").version, target);
  assert.equal(json("apps/web/package.json").version, target);
  assert.equal(json("docs/openapi/management.openapi.json").info.version, target);
});

test("Cargo and npm lockfiles retain the release package versions", () => {
  const cargoLock = text("Cargo.lock").match(/\[\[package\]\]\nname = "open-model-gateway"\nversion = "([^"]+)"/)[1];
  assert.equal(cargoLock, target);
  const npm = json("package-lock.json");
  assert.equal(npm.version, target);
  assert.equal(npm.packages[""].version, target);
  assert.equal(npm.packages["apps/web"].version, target);
});

test("chart and default image identify the same release", () => {
  const chart = text("deploy/helm/open-model-gateway/Chart.yaml");
  assert.equal(chart.match(/^version: (\S+)$/m)[1], target);
  assert.equal(chart.match(/^appVersion: "([^"]+)"$/m)[1], target);
  assert.equal(text("deploy/helm/open-model-gateway/values.yaml").match(/^  tag: (\S+)$/m)[1], `v${target}`);
});

test("tagged notes exist and unreleased notes point to that baseline", () => {
  assert.ok(text(`docs/releases/v${target}.md`).startsWith(`# Open Model Gateway v${target}`));
  assert.ok(text("docs/releases/unreleased.md").includes(`[v${target}](v${target}.md)`));
});

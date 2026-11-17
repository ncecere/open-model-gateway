// Enumerate and reconcile every whole integration target with the CI inventory
// and libtest validators. The source ref supplies the same module its CI uses.
import assert from "node:assert/strict";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

try {
  const group = process.argv[2];
  assert.ok(["gateway", "workspace"].includes(group), "expected gateway or workspace");
  const { discoverTargets, parseListing, targetArgs, validateRun, runCargo } =
    await import(pathToFileURL(resolve(".github/scripts/rust-shards.mjs")));
  const metadata = JSON.parse(await runCargo(["metadata", "--no-deps", "--all-features", "--format-version", "1", "--locked"]));
  const artifacts = await runCargo(["test", "--workspace", "--all-features", "--no-run", "--message-format=json"]);
  const targets = discoverTargets(metadata, artifacts).filter(target => group === "gateway"
    ? target.packageName === "open-model-gateway" && target.kind !== "lib" && !["filestore_s3", "runtime_privileges"].includes(target.name)
    : target.packageName !== "open-model-gateway");
  assert.ok(targets.length, "no integration test targets selected");
  if (group === "gateway") assert.ok(targets.some(t => t.kind === "test"), "no gateway integration test targets selected");
  let failed = false;
  for (const target of targets) {
    try {
      const args = targetArgs(target);
      const entries = parseListing(await runCargo([...args, "--", "--list", "--format", "terse", "--color", "never"]));
      assert.deepEqual(entries, parseListing(await runCargo([...args, "--", "--list", "--format", "pretty", "--color", "never"]), true));
      const ignored = parseListing(await runCargo([...args, "--", "--list", "--ignored", "--format", "terse", "--color", "never"]));
      assert.ok(ignored.every(e => entries.some(t => t.name === e.name && t.kind === e.kind)), "ignored tests absent from inventory");
      console.log(`integration-target ${target.key}: listed=${entries.length} default-ignored=${ignored.length}`);
      // Whole targets with zero tests are legitimate and must still report zero.
      // A nonempty named target reporting zero cannot satisfy validateRun.
      const output = await runCargo([...args, "--", "--exact"], { echo: true });
      validateRun(output, { entries, ignored: ignored.map(e => e.name), total: entries.length });
    } catch (error) {
      console.error(`integration-target ${target.key}: ${error.stack}`);
      failed = true;
    }
  }
  if (group === "workspace") await runCargo(["test", "--workspace", "--exclude", "open-model-gateway", "--all-features", "--doc"], { echo: true });
  if (failed) process.exitCode = 1;
} catch (error) {
  console.error(error.stack);
  process.exitCode = 1;
}

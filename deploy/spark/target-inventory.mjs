import assert from "node:assert/strict";
import { isAbsolute, join } from "node:path";

export const TERMINAL = "providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence";
export const KEY_CONTENTION = "management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement";
export const DEDICATED_RUNTIME_PROBES = [
  "controlled_lock_stall_recovers_on_runtime_http_path", "enterprise_runtime_allowlist_and_rollback_probes",
  "populated_verifier_both", "populated_verifier_personal", "populated_verifier_scim",
];
export const SPECIAL = ["filestore_s3", "runtime_privileges"];

export function identity(packageId, target) {
  if (typeof packageId !== "string" || !packageId || typeof target?.name !== "string" || !target.name
      || !Array.isArray(target.kind) || !target.kind.length) throw new Error("Malformed Cargo target identity");
  const libs = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
  let kind;
  if (target.kind.every(k => libs.includes(k))) kind = "lib";
  else if (target.kind.length === 1 && ["bin", "test", "example", "bench"].includes(target.kind[0])) kind = target.kind[0];
  else throw new Error(`Unsupported Cargo test target kind: ${JSON.stringify(target.kind)}`);
  return { packageId, name: target.name, kind, key: JSON.stringify([packageId, [...target.kind].sort(), target.name]) };
}

export function checkSources(metadata, names) {
  assert.ok(Array.isArray(metadata.workspace_members) && metadata.workspace_members.every(id => typeof id === "string")
    && new Set(metadata.workspace_members).size === metadata.workspace_members.length, "malformed Cargo workspace membership");
  assert.ok(isAbsolute(metadata.workspace_root), "expected absolute Cargo workspace root");
  const packages = metadata.packages.filter(pkg =>
    pkg.name === "open-model-gateway" && metadata.workspace_members.includes(pkg.id));
  assert.equal(packages.length, 1, "expected one gateway workspace package");
  for (const name of names) {
    const targets = packages[0].targets.filter(target => target.name === name && identity(packages[0].id, target).kind === "test" && target.test === true);
    assert.equal(targets.length, 1, `expected one gateway test target named ${name}`);
    assert.equal(targets[0].src_path, join(metadata.workspace_root, "apps/gateway/tests", `${name}.rs`),
      `unexpected ${name} source path`);
  }
}

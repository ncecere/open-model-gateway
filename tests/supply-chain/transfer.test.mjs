import assert from "node:assert/strict";
import { spawnSync } from "../process.mjs";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadWorkflow, root, stepsOf } from "./lib.mjs";
import { bashPath, runBash } from "../bash-preflight.mjs";

bashPath();

const text = path => readFileSync(join(root, path), "utf8");

test("package pruning uses the organization API in both dry and delete modes", () => {
  const job = loadWorkflow("prune-dev-images").jobs.prune;
  assert.equal(job.env.OWNER, "bitop-dev");
  const script = stepsOf(job).find(step => step.name === "Prune").run;
  const work = mkdtempSync(join(tmpdir(), "omg-prune-test-"));
  try {
    // gh is replaced before running either mode: this test cannot delete a package.
    writeFileSync(join(work, "gh"), `#!${bashPath()}
printf '%s\\n' "$*" >> "$CALLS"
case "$*" in
  'api --paginate /orgs/bitop-dev/packages/container/open-model-gateway/versions?per_page=100')
    printf '%s' '[{"id":1,"created_at":"2000-01-01T00:00:00Z","metadata":{"container":{"tags":["dev-aaaaaaaaaaaa"]}}}]' ;;
  'api -X DELETE /orgs/bitop-dev/packages/container/open-model-gateway/versions/1') ;;
  *) exit 90 ;;
esac
`, { mode: 0o700 });
    for (const dry of ["true", "false"]) {
      const calls = join(work, `calls-${dry}`);
      const result = runBash(["-c", script], { encoding: "utf8", env: {
        ...process.env, PATH: `${work}:${process.env.PATH}`, CALLS: calls,
        OWNER: job.env.OWNER, PACKAGES: "open-model-gateway", OLDER_THAN_DAYS: "14",
        DRY_RUN: dry, GITHUB_STEP_SUMMARY: join(work, "summary"),
      } });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(textCalls(calls).includes("DELETE"), dry === "false");
      assert.doesNotMatch(textCalls(calls), /\/users\/|ncecere/);
    }
  } finally { rmSync(work, { recursive: true, force: true }); }
});
const textCalls = path => readFileSync(path, "utf8");

test("release dry runs keep historical image identities and use current repository links", () => {
  const work = mkdtempSync(join(tmpdir(), "omg-release-test-"));
  const digest = `sha256:${"a".repeat(64)}`;
  try {
    mkdirSync(join(work, "bin"));
    mkdirSync(join(work, "docs/releases"), { recursive: true });
    writeFileSync(join(work, "bin/docker"), `#!${bashPath()}
printf '%s\\n' "$*" >> "$CALLS"
printf '%s' '{"linux/amd64":{"SPDX":{"spdxVersion":"SPDX-2.3"}},"linux/arm64":{"SPDX":{"spdxVersion":"SPDX-2.3"}}}'
`, { mode: 0o700 });
    writeFileSync(join(work, "bin/gh"), `#!${bashPath()}\nexit 99\n`, { mode: 0o700 });
    for (const [tag, owner] of [["v0.18.0", "ncecere"], ["v0.18.0-rc.1", "ncecere"], ["v0.18.1", "bitop-dev"], ["v0.19.0-rc.1", "bitop-dev"], ["v1.0.0", "bitop-dev"]]) {
      const base = tag.split("-")[0];
      writeFileSync(join(work, `docs/releases/${base}.md`), "# Fixture\n[Security](../../SECURITY.md) [Operations](../operations.md) [Previous](v0.18.0.md)\n");
      const out = join(work, tag);
      const calls = join(work, `${tag}.calls`);
      const result = runBash([join(root, "scripts/github-release.sh")], { cwd: work, encoding: "utf8", env: {
        ...process.env, PATH: `${work}/bin:${process.env.PATH}`, CALLS: calls,
        TAG: tag, DIGEST: digest, RELEASE_DRY_RUN: "1", RELEASE_OUT: out,
      } });
      assert.equal(result.status, 0, result.stderr);
      const ref = `ghcr.io/${owner}/open-model-gateway@${digest}`;
      assert.equal(textCalls(join(out, `assets/open-model-gateway-${tag}.digest.txt`)), `${ref}\n`);
      const notes = textCalls(join(out, "notes.md"));
      assert.ok(notes.includes(`cosign verify ${ref}`));
      assert.ok(notes.includes(`--certificate-identity https://github.com/${owner}/open-model-gateway/.github/workflows/image.yml@refs/tags/${tag}`));
      assert.ok(notes.includes(`https://github.com/bitop-dev/open-model-gateway/blob/${tag}/SECURITY.md`));
      assert.doesNotMatch(notes, /\]\(\.\.\//);
      assert.ok(textCalls(calls).includes(ref));
      const checks = spawnSync("sha256sum", ["-c", "checksums.txt"], { cwd: join(out, "assets"), encoding: "utf8" });
      assert.equal(checks.status, 0, checks.stderr);
    }
  } finally { rmSync(work, { recursive: true, force: true }); }
});

test("current compare links and next-release upgrade guidance cover the transfer", () => {
  assert.doesNotMatch(text("CHANGELOG.md").split("\n").filter(line => /^\[[^\]]+\]:/.test(line)).join("\n"), /github\.com\/ncecere/);
  const notes = text("docs/releases/unreleased.md");
  for (const phrase of ["ghcr.io/bitop-dev/open-model-gateway", "ghcr.io/ncecere/open-model-gateway", "v0.18.0", "image.repository", "signature"]) {
    assert.ok(notes.includes(phrase), `upgrade guidance missing ${phrase}`);
  }
});

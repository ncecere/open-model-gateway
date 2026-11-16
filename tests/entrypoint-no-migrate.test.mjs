// AGENTS.md: "Never run migrations or bootstrap automatically in `serve`/the container
// entrypoint" (#591). The Helm chart is checked in tests/helm-chart.test.mjs and the Rust
// start-up path in apps/gateway/src/source_rules/serve_no_migrate.rs. This file reads the two
// Dockerfiles: the final stage's ENTRYPOINT and CMD (the HEALTHCHECK command is a separate
// instruction and is a read-only probe) must be exec form and together run exactly
// `open-model-gateway serve`: no shell, no wrapper script, and none of the commands that change
// the schema or seed data.
//
// Not covered: what the binary does for `serve` (Rust test), a compose file or an orchestrator
// that overrides the command, and a base image's own entrypoint (the final stage sets its own).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const STATE_CHANGING = ["migrate", "grants", "bootstrap-dev", "bootstrap-demo", "provision-user"];
const BINARY = "/usr/local/bin/open-model-gateway";

/** Instructions of the final build stage, with line continuations joined and comments dropped. */
export function finalStage(text) {
  const joined = text.replace(/\\\r?\n/g, " ").split(/\r?\n/).filter((line) => !/^\s*#/.test(line));
  const instructions = joined
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [keyword, ...rest] = line.split(/\s+/);
      return { keyword: keyword.toUpperCase(), argument: rest.join(" ") };
    });
  const lastFrom = instructions.map((i) => i.keyword).lastIndexOf("FROM");
  return instructions.slice(lastFrom + 1);
}

/** The effective command (ENTRYPOINT then CMD) or the reasons it is not acceptable. */
export function entrypointProblems(text) {
  const stage = finalStage(text);
  const problems = [];
  const last = (keyword) => stage.filter((i) => i.keyword === keyword).at(-1);
  const execForm = (instruction) => {
    if (!instruction) return [];
    try {
      const parsed = JSON.parse(instruction.argument);
      if (Array.isArray(parsed) && parsed.every((word) => typeof word === "string")) return parsed;
    } catch {
      // shell form
    }
    problems.push(`${instruction.keyword} ${instruction.argument} is not exec form (a JSON array)`);
    return [];
  };
  const command = [...execForm(last("ENTRYPOINT")), ...execForm(last("CMD"))];
  if (command[0] !== BINARY || command[1] !== "serve" || command.length !== 2) {
    problems.push(`the container must run exactly [${BINARY}, serve]; it runs ${JSON.stringify(command)}`);
  }
  for (const word of command) {
    if (STATE_CHANGING.includes(word)) problems.push(`the container command runs ${word}`);
  }
  if (!last("ENTRYPOINT")) problems.push("the final stage sets no ENTRYPOINT");
  return problems;
}

for (const dockerfile of ["Dockerfile", "deploy/loadtest/Dockerfile"]) {
  test(`${dockerfile}: the container runs only \`serve\` (#591)`, () => {
    assert.deepEqual(entrypointProblems(readFileSync(join(root, dockerfile), "utf8")), []);
  });
}

test("the Dockerfile check refuses planted migrate, bootstrap, shell and wrapper entrypoints (#591)", () => {
  const stage = (body) => `FROM scratch AS build\nRUN true\nFROM gcr.io/distroless/static\n${body}\n`;
  const ok = stage(`HEALTHCHECK CMD ["${BINARY}", "healthcheck"]\nENTRYPOINT ["${BINARY}"]\nCMD ["serve"]`);
  assert.deepEqual(entrypointProblems(ok), []);
  // The read-only probe is not the command.
  assert.deepEqual(entrypointProblems(stage(`ENTRYPOINT ["${BINARY}", "serve"]`)), []);
  for (const [body, expect] of [
    [`ENTRYPOINT ["${BINARY}"]\nCMD ["migrate"]`, "the container command runs migrate"],
    [`ENTRYPOINT ["${BINARY}", "migrate"]\nCMD ["serve"]`, "the container command runs migrate"],
    [`ENTRYPOINT ["${BINARY}"]\nCMD ["grants", "apply"]`, "the container command runs grants"],
    [`ENTRYPOINT ["${BINARY}"]\nCMD ["bootstrap-demo"]`, "the container command runs bootstrap-demo"],
    [`ENTRYPOINT ["${BINARY}"]\nCMD ["provision-user"]`, "the container command runs provision-user"],
    [`ENTRYPOINT ["${BINARY}", "serve", "--x"]`, "the container must run exactly"],
    [`ENTRYPOINT ${BINARY} migrate && ${BINARY} serve`, "is not exec form"],
    [`ENTRYPOINT ["wrapper", "-c", "${BINARY} migrate; ${BINARY} serve"]`, "the container must run exactly"],
    [`ENTRYPOINT ["/entrypoint.sh"]\nCMD ["serve"]`, "the container must run exactly"],
    [`CMD ["serve"]`, "the final stage sets no ENTRYPOINT"],
  ]) {
    const found = entrypointProblems(stage(body)).join("\n");
    assert.ok(found.includes(expect), `${body.replace(/\n/g, " ")} -> ${found}`);
  }
  // Only the last stage counts: a migrating builder stage is not the image.
  assert.deepEqual(entrypointProblems(`FROM a\nENTRYPOINT ["x", "migrate"]\n${ok}`), []);
  // Continuations and comments.
  assert.deepEqual(entrypointProblems(stage(`# CMD ["migrate"]\nENTRYPOINT \\\n  ["${BINARY}"]\nCMD ["serve"]`)), []);
});

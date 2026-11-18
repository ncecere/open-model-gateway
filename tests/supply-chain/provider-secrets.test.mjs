// Check credential inputs before any step can relay them through a wrapper, action or GITHUB_ENV.
import assert from "node:assert/strict";
import { test } from "node:test";
import { parseStrict, repoFiles, toData } from "./lib.mjs";

// GitHub secrets have arbitrary names. Check the source of a value, including
// whole-context and dynamic lookups, instead of guessing provider variable names.
function readsSecret(value, condition) {
  const identifier = /[\w-]+/y;
  let inExpression = condition;
  let quoted = false;
  let afterDot = false;
  let i = 0;
  while (i < value.length) {
    if (!inExpression) {
      const open = value.indexOf("${{", i);
      if (open === -1) break;
      i = open + 3;
      inExpression = true;
      afterDot = false;
      continue;
    }
    const ch = value[i];
    if (ch === "'") {
      if (quoted && value[i + 1] === "'") {
        i += 2;
        continue;
      }
      quoted = !quoted;
      afterDot = false;
    } else if (!quoted) {
      if (!condition && value.startsWith("}}", i)) {
        i += 2;
        inExpression = false;
        continue;
      }
      identifier.lastIndex = i;
      const word = identifier.exec(value);
      if (word) {
        if (!afterDot && word[0].toLowerCase() === "secrets") return true;
        afterDot = false;
        i = identifier.lastIndex;
        continue;
      }
      if (!/\s/.test(ch)) afterDot = ch === ".";
    }
    i++;
  }
  assert.ok(condition || !inExpression, "unterminated expression in workflow input");
  return false;
}

function checkInputs(data, file) {
  const jobs = Object.values(data.jobs ?? {});
  const steps = jobs.flatMap(job => job.steps ?? []).concat(data.runs?.steps ?? []);
  const conditions = new Set([...jobs, ...steps]);
  for (const [id, job] of Object.entries(data.jobs ?? {})) {
    assert.ok(!(job.uses && job.secrets === "inherit"), `${file}.jobs.${id}: inheriting secrets in a reusable workflow`);
  }
  function visit(parent, path) {
    for (const [key, value] of Object.entries(parent)) {
      const child = `${path}.${key}`;
      if (value && typeof value === "object") {
        visit(value, child);
      } else if (typeof value === "string" && readsSecret(value, (key === "if" && conditions.has(parent))
          || (parent === data.runs && ["pre-if", "post-if"].includes(key)))) {
        assert.ok(["GH_TOKEN", "GITHUB_TOKEN"].includes(key) && /^\$\{\{\s*secrets\.GITHUB_TOKEN\s*}}$/i.test(value),
          `${child}: only the automatic GitHub token may be imported from secrets`);
      }
    }
  }
  visit(data, file);
}

test("CI workflows and their local actions cannot import provider secrets", () => {
  for (const { file, text } of repoFiles()) checkInputs(toData(parseStrict(text, file)), file);
});

test("provider secret references and inheritance are refused regardless of the credential alias", () => {
  const ref = "${{ secrets.SYNTHETIC_PROVIDER }}";
  const job = { steps: [{ run: "cargo test" }] };
  for (const data of [
    { env: { OPEN_ROUTER: ref }, jobs: { tests: job } },
    { jobs: { tests: { ...job, env: { OPENAI_KEY: "${{ SeCrets['synthetic'] }}" } } } },
    { jobs: { tests: { steps: [{ env: { ANY: "${{ toJSON(secrets) }}" }, run: "cargo test" }] } } },
    { jobs: { tests: { steps: [{ run: `echo "ALIAS=${ref}" >> "$GITHUB_ENV"` }, ...job.steps] } } },
    { jobs: { tests: { uses: "./.github/workflows/tests.yml", secrets: "inherit" } } },
    { jobs: { tests: { uses: "./.github/workflows/tests.yml", secrets: { alias: ref } } } },
    { jobs: { tests: { steps: [{ uses: "./.github/actions/test", with: { token: ref } }] } } },
    { jobs: { tests: { steps: [{ run: "KEY=${{ format('}}', secrets.ALIAS) }} cargo test" }] } } },
    { jobs: { publish: { env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN_EXTRA }}" } } } },
    { jobs: { tests: { if: "secrets[vars.NAME]", ...job } } },
    { runs: { using: "node24", main: "main.js", post: "post.js", "post-if": "secrets.ALIAS" } },
    { runs: { using: "composite", steps: [{ run: `KEY=${ref} cargo test` }] } },
  ]) assert.throws(() => checkInputs(data, "fixture.yml"), /fixture\.yml.*secret/i);

  checkInputs({ jobs: { prose: { steps: [{ run: "echo 'scan for secrets at ${{ github.sha }}'" }] },
    tests: job, publish: { env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}" } } } }, "ok.yml");
});

test("unrelated properties, environment literals and token whitespace are accepted", () => {
  checkInputs({ jobs: { tests: { steps: [{
    env: { if: "secrets are documented here", secrets: "inherit", ARG: "${{ inputs.secrets }}",
      GH_TOKEN: "${{secrets.GITHUB_TOKEN}}" },
    if: "inputs.secrets && inputs['secrets']",
    run: "cargo test",
  }] } } }, "ok.yml");
});

test("expression boundaries respect Actions strings across interpolations", () => {
  const first = "${{ format('it''s }} and secrets', inputs.name) }}";
  assert.equal(readsSecret(first + " secrets outside ${{ inputs.secrets }}", false), false);
  assert.equal(readsSecret(first + " ${{ secrets.SYNTHETIC }}", false), true);
  assert.throws(() => readsSecret("label: ${{ inputs.name", false), /unterminated expression/);
});

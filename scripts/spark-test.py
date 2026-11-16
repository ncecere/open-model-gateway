#!/usr/bin/env python3
"""Run the test suites of any pushed ref of this repository on the Spark k3s test platform.

TEST ONLY. Plain kubectl on a developer machine may target production, so this script passes
the Spark kubeconfig explicitly to every kubectl call, refuses to run unless that file exists
and names the Spark server (the checks of scripts/spark-env.py, which it loads), runs every
tool with a scrubbed environment, and only ever touches the two namespaces
<base>-omg-test-s1 and <base>-omg-test-s2. See AGENTS.md "Running a branch on the cluster".

  run <name> --ref <branch|commit|tag> --suite lib|integration|web|checks|all
                                              start a run, wait for it, print the result
  status [name]                               the slots and their runs, or one run
  logs <name> [-f]                            the run's log
  result <name> [--summary PATH]              the result of a finished run
  down <name>                                 delete the run, its database and its pod
  purge                                       delete the idle slot namespaces and their caches

Two runs can be active at once (two slots): the platform quota (4 CPU and 16 GiB requested,
shared by all namespaces) leaves room for exactly that next to one demo environment. Each slot
is a namespace that holds one persistent cache volume (cargo registry, target directory, rustup,
npm cache, source checkout), so a run after a small change does not rebuild the world. A slot
is leased to one run at a time (a ConfigMap created atomically), so two runs never share a
target directory. `down` removes everything of a run except the cache; `purge` removes the cache.

The job fetches the public repository at the exact commit the ref resolves to (push first) and
verifies HEAD. Nothing but the job manifest and the script is uploaded.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import time
import importlib.util

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "spark"

# The Spark safety helpers are scripts/spark-env.py's: one implementation of the kubeconfig and
# server check, the scrubbed environment, the redaction and the ref resolution.
_spec = importlib.util.spec_from_file_location("spark_env", Path(__file__).resolve().with_name("spark-env.py"))
spark_env = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(spark_env)
SparkError = spark_env.SparkError

SLOTS = ("s1", "s2")  # at most two runs at once: see the module docstring and deploy/spark/test-job.yaml
SUITES = ("lib", "integration", "web", "checks", "all")
NAME = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,18}[a-z0-9])")
SHA = re.compile(r"[0-9a-f]{40}")
# What may be handed to libtest or vitest as extra arguments: no shell syntax, no quotes.
TEST_ARGS = re.compile(r"[A-Za-z0-9_:.,=/@-]+(?: [A-Za-z0-9_:.,=/@-]+)*")
LEASE = "omg-test-lease"
JOB = "omg-test"
CLUSTER = "omg-test-db"
SCRIPT_CONFIGMAP = "omg-test-script"
SUPERUSER_SECRET = "omg-test-pg-superuser"
CACHE_CLAIM = "omg-test-cache"
MARKER = "::omg-test::"
DEFAULT_TIMEOUT_MINUTES = 240
MAX_TIMEOUT_MINUTES = 480
POLL_SECONDS = 20
LEDGER_RETRIES = 24  # the platform needs about two minutes after a namespace delete
LEDGER_WAIT_SECONDS = 10
# What one run adds to the shared quota (deploy/spark/test-job.yaml and test-cluster.yaml: the job pod and
# the database; the node-copy init container is smaller than the main container and does not count).
# The cache volume is added when the slot has none yet. tests/spark-test.test.py checks these against the manifests.
RUN_NEEDS = {"requests.cpu": 1750, "requests.memory": 6656, "limits.cpu": 2800, "limits.memory": 11264, "pods": 3, "persistentvolumeclaims": 1, "requests.storage": 8192}
CACHE_NEEDS = {"persistentvolumeclaims": 1, "requests.storage": 30720}
ARCHITECTURE_NOTE = "aarch64 (the Spark); amd64 stays covered by GitHub CI"


def validate_name(name):
    if not NAME.fullmatch(name or ""):
        raise SparkError("name must be 2-20 characters: lowercase letters, digits and inner dashes")
    return name


def slot_namespace(base, slot):
    if slot not in SLOTS:
        raise SparkError(f"unknown slot {slot!r}")
    return f"{base}-omg-test-{slot}"


def check_namespace(base, namespace):
    """Only <base>-omg-test-<slot>: never an environment of spark-env.py, never anything else."""
    if namespace not in [f"{base}-omg-test-{slot}" for slot in SLOTS]:
        raise SparkError(f"refusing namespace {namespace!r}: only {base}-omg-test-<slot> namespaces are touched")
    return namespace


def kubectl_command(config, base, args, namespace=None):
    command = ["kubectl", "--kubeconfig", str(config)]
    if namespace is not None:
        command += ["-n", check_namespace(base, namespace)]
    return command + list(args)


def validate_test_args(text, suite):
    text = (text or "").strip()
    if not text:
        return ""
    if suite not in ("lib", "web"):
        raise SparkError("--test-args applies to the lib and web suites only")
    if not TEST_ARGS.fullmatch(text):
        raise SparkError("--test-args may contain letters, digits and _ : . , = / @ - separated by single spaces")
    return text


def render_job(text, suite, sha, test_args, timeout_minutes):
    """Substitute the job's few parameters, each after a strict check; no placeholder may remain."""
    if suite not in SUITES:
        raise SparkError(f"unknown suite {suite!r}")
    if not SHA.fullmatch(sha or ""):
        raise SparkError("the commit must be a full 40-character id")
    if not 1 <= timeout_minutes <= MAX_TIMEOUT_MINUTES:
        raise SparkError(f"--timeout must be 1-{MAX_TIMEOUT_MINUTES} minutes")
    values = {"__SUITE__": suite, "__SHA__": sha, "__TEST_ARGS__": validate_test_args(test_args, suite), "__DEADLINE__": str(timeout_minutes * 60)}
    for key, value in values.items():
        if key not in text:
            raise SparkError(f"manifest has no {key} placeholder")
        text = text.replace(key, value)
    if re.search(r"__[A-Z_]+__", text):
        raise SparkError("manifest still has a placeholder")
    return text


def superuser_secret():
    """The run's PostgreSQL superuser Secret, as JSON for stdin; the hex password is never an argument."""
    password = secrets.token_hex(24)
    spark_env._REGISTERED_SECRETS.append(password)
    return json.dumps({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": SUPERUSER_SECRET, "labels": {"app.kubernetes.io/name": "omg-test"}},
                       "type": "kubernetes.io/basic-auth", "stringData": {"username": "postgres", "password": password}})


# ---- reading the log ----

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
LIBTEST_RESULT = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")
LIBTEST_FAILED = re.compile(r"test (\S.*?) \.\.\. FAILED")
VITEST_TESTS = re.compile(r"^\s*Tests\s+(.*?)\s*\((\d+)\)\s*$")
VITEST_FAIL = re.compile(r"^\s*FAIL\s+(\S.*)$")


def parse_marker(line):
    """`::omg-test:: step-end suite=lib name=x exit=0` -> ("step-end", {"suite": "lib", ...})."""
    words = line[len(MARKER):].split()
    return (words[0] if words else ""), dict(w.split("=", 1) for w in words[1:] if "=" in w)


def parse_log(text):
    """Everything the summary needs from a run's log; unknown lines are ignored."""
    steps, results, failed_tests, vitest = [], [], [], []
    suites = {}
    done = commit = inventory = None
    current = ""
    for raw in ANSI.sub("", text).splitlines():
        line = raw.rstrip()
        if line.startswith(MARKER):
            kind, fields = parse_marker(line)
            if kind == "step-start":
                current = fields.get("suite", "")
            elif kind == "step-end":
                steps.append({"suite": fields.get("suite", ""), "name": fields.get("name", ""), "exit": int(fields.get("exit", "1")), "seconds": int(fields.get("seconds", "0"))})
            elif kind == "done":
                done = {"exit": int(fields.get("exit", "1")), "seconds": int(fields.get("seconds", "0"))}
            elif kind == "source":
                commit = fields.get("commit")
            elif kind == "lib-inventory":
                inventory = int(fields.get("tests", "0"))
            continue
        counts = suites.setdefault(current or "setup", {"passed": 0, "failed": 0, "ignored": 0, "targets": 0})
        if m := LIBTEST_RESULT.search(line):
            results.append(line)
            counts["passed"] += int(m.group(2))
            counts["failed"] += int(m.group(3))
            counts["ignored"] += int(m.group(4))
            counts["targets"] += 1
        elif m := LIBTEST_FAILED.fullmatch(line):
            failed_tests.append(m.group(1))
        elif (m := VITEST_TESTS.match(line)):
            pairs = {kind: int(n) for n, kind in re.findall(r"(\d+) (failed|passed|skipped|todo)", m.group(1))}
            vitest.append(line.strip())
            counts["passed"] += pairs.get("passed", 0)
            counts["failed"] += pairs.get("failed", 0)
            counts["ignored"] += pairs.get("skipped", 0) + pairs.get("todo", 0)
            counts["targets"] += 1
        elif m := VITEST_FAIL.match(line):
            failed_tests.append(m.group(1).strip())
    suites = {name: counts for name, counts in suites.items() if counts["targets"]}
    return {"steps": steps, "results": results, "vitest": vitest, "suites": suites, "failed_tests": list(dict.fromkeys(failed_tests)), "done": done, "commit": commit, "inventory": inventory}


def build_summary(name, suite, ref, sha, parsed, job_state, pod, wall_seconds):
    """The compact JSON of a finished run. No log text, no environment, no secret."""
    done = parsed["done"]
    if done is not None:
        exit_code = done["exit"]
    elif pod.get("exit_code") is not None:
        exit_code = pod["exit_code"]
    else:
        exit_code = 1
    failed_steps = [f"{s['suite']}/{s['name']}" for s in parsed["steps"] if s["exit"] != 0]
    totals = {key: sum(c[key] for c in parsed["suites"].values()) for key in ("passed", "failed", "ignored")}
    summary = {
        "name": name, "suite": suite, "ref": ref, "commit": sha, "architecture": ARCHITECTURE_NOTE,
        "result": "passed" if exit_code == 0 and job_state == "complete" else "failed",
        "exit_code": exit_code, "job": job_state,
        "duration_seconds": done["seconds"] if done else wall_seconds, "wall_seconds": wall_seconds,
        "counts": totals, "suites": parsed["suites"], "steps": parsed["steps"],
        "failed_steps": failed_steps, "failed_tests": parsed["failed_tests"][:200],
    }
    if pod.get("reason"):
        summary["pod_reason"] = pod["reason"]
    lib = parsed["suites"].get("lib")
    if parsed.get("inventory") is not None:
        summary["lib_tests_listed"] = parsed["inventory"]
        accounted = lib["passed"] + lib["failed"] + lib["ignored"] if lib else 0
        if accounted != parsed["inventory"]:
            summary["result"] = "failed"
            summary["exit_code"] = exit_code or 1
            summary["note"] = f"the gateway library lists {parsed['inventory']} tests but the shards account for {accounted}"
    if done is None:
        summary["note"] = "the run did not reach its end marker (the pod was stopped: out of memory, deadline or deletion)"
    return summary


def format_report(summary, parsed):
    lines = []
    lines.extend(parsed["results"] or ["(no `test result:` lines in the log)"])
    lines.extend(parsed["vitest"])
    if summary["failed_tests"]:
        lines.append("failing tests:")
        lines.extend(f"  {test}" for test in summary["failed_tests"])
    if summary["failed_steps"]:
        lines.append("failed steps: " + ", ".join(summary["failed_steps"]))
    lines.append(json.dumps(summary, separators=(",", ":")))
    return "\n".join(lines)


def write_summary(path, summary):
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags, 0o600)
    with os.fdopen(fd, "w") as handle:
        if not stat.S_ISREG(os.fstat(handle.fileno()).st_mode):
            raise SparkError(f"{path} is not a regular file")
        handle.write(json.dumps(summary, indent=2) + "\n")


def job_phase(job_json, pod_json=None, step=None):
    """A short phase from `get job -o json` (and the pod, and the log's current step)."""
    state = spark_env.job_state(job_json)
    if state == "complete":
        return "succeeded"
    if state == "failed":
        return "failed"
    pod = (pod_json or {}).get("status", {}).get("phase")
    if pod == "Running":
        return f"running {step}" if step else "running"
    return "starting"


def quantity(key, text):
    """A Kubernetes quantity in the unit RUN_NEEDS uses: milli-CPU, MiB, or a plain count."""
    text = str(text)
    if key.endswith(".cpu"):
        return int(text[:-1]) if text.endswith("m") else int(float(text) * 1000)
    if key.endswith(".memory") or key == "requests.storage":
        units = {"Ki": 1 / 1024, "Mi": 1, "Gi": 1024, "Ti": 1024 * 1024}
        for suffix, factor in units.items():
            if text.endswith(suffix):
                return int(float(text[:-2]) * factor)
        return int(text) // (1024 * 1024)
    return int(text)


def quota_shortfalls(quotas_json, needs):
    """What the shared quota cannot give, from `kubectl get resourcequota -o json` of the base namespace.

    The Spark quota is one global budget; the base namespace's ResourceQuota shows the hard limit minus
    what the other namespaces use, and its own `used`. Refusing here is clearer than a pod that stays Pending.
    """
    free = {}
    for item in json.loads(quotas_json).get("items", []):
        status = item.get("status", {})
        for key, hard in (status.get("hard") or {}).items():
            if key in needs:
                free[key] = quantity(key, hard) - quantity(key, (status.get("used") or {}).get(key, "0"))
    return [f"{key}: needs {need}, {free[key]} available" for key, need in needs.items() if key in free and free[key] < need]


def lease_text(lease):
    return f"{lease['name']} ({lease.get('suite')} {lease.get('ref')} {str(lease.get('sha', ''))[:12]})"


def elapsed_text(seconds):
    seconds = max(int(seconds), 0)
    return f"{seconds // 3600}h{seconds % 3600 // 60:02d}m{seconds % 60:02d}s" if seconds >= 3600 else f"{seconds // 60}m{seconds % 60:02d}s"


class SparkTest:
    def __init__(self, config, runner=None, launcher=None, clock=None, sleep=None):
        self.config = Path(config)
        self.runner = runner or spark_env.run
        self.launcher = launcher or spark_env.launch
        self.clock = clock or time.time
        self.sleep = sleep or time.sleep
        self._base = None

    @property
    def base(self):
        if self._base is None:
            self._base = spark_env.base_namespace(self.runner(kubectl_command(self.config, None, ["config", "view", "--minify", "-o", "json"]), capture=True))
        return self._base

    def kubectl(self, args, slot=None, stdin=None, capture=True):
        """Always captured: kubectl's "created"/"deleted" chatter is not ours to print."""
        namespace = slot_namespace(self.base, slot) if slot else None
        command = kubectl_command(self.config, self.base, args, namespace)
        for attempt in range(LEDGER_RETRIES + 1):
            try:
                return self.runner(command, stdin=stdin, capture=capture)
            except SparkError as error:
                # After namespaces were deleted, creating objects fails for about two minutes (AGENTS.md); it is not an error.
                if "QuantityLedger" not in str(error) or "not initialized" not in str(error) or attempt == LEDGER_RETRIES:
                    raise
                self.sleep(LEDGER_WAIT_SECONDS)

    def get_json(self, slot, args):
        """`get ... -o json` of one object, or None when it does not exist."""
        out = self.kubectl(["get", *args, "--ignore-not-found", "-o", "json"], slot=slot, capture=True).strip()
        return json.loads(out) if out else None

    # ---- slots and leases ----

    def existing_slots(self):
        prefix = f"namespace/{self.base}-omg-test-"
        found = {line.removeprefix(prefix) for line in self.kubectl(["get", "namespace", "-o", "name"], capture=True).split() if line.startswith(prefix)}
        return [slot for slot in SLOTS if slot in found]

    def lease(self, slot):
        document = self.get_json(slot, ["configmap", LEASE])
        return None if document is None else {**document.get("data", {}), "created": document["metadata"].get("creationTimestamp")}

    def leases(self):
        return {slot: self.lease(slot) for slot in self.existing_slots()}

    def create_slot(self, slot):
        namespace = slot_namespace(self.base, slot)
        try:
            self.kubectl(["create", "namespace", namespace])
        except SparkError as error:
            if "AlreadyExists" not in str(error):
                raise
        # Permissions arrive in seconds and the isolation policies a little later: wait for both.
        for _ in range(60):
            try:
                if len(self.kubectl(["get", "networkpolicy", "-o", "name"], slot=slot, capture=True).split()) >= 2:
                    return
            except SparkError as error:
                if "Forbidden" not in str(error):
                    raise
            self.sleep(2)
        raise SparkError(f"{namespace} never received its two network policies; refusing to deploy without isolation")

    def acquire(self, name, info):
        """Lease a free slot to this run, creating the slot namespace (one at a time) when needed."""
        leases = self.leases()
        for slot, lease in leases.items():
            if lease and lease.get("name") == name:
                raise SparkError(f"a run named {name!r} already exists in slot {slot}; use another name or `down {name}`")
        free = [s for s in SLOTS if s in leases and not leases[s]] + [s for s in SLOTS if s not in leases]
        data = [f"--from-literal={key}={value}" for key, value in {**info, "name": name, "started": str(int(self.clock()))}.items()]
        for slot in free:
            if slot not in leases:
                self.create_slot(slot)
            try:
                self.kubectl(["create", "configmap", LEASE, *data], slot=slot)
            except SparkError as error:
                if "AlreadyExists" in str(error):  # someone took it between our look and our create
                    continue
                raise
            return slot
        busy = "; ".join(f"{slot}: {lease_text(lease)}" for slot, lease in leases.items() if lease) or "no run is visible, but no slot could be leased"
        raise SparkError(f"all {len(SLOTS)} test slots are in use ({busy}). At most {len(SLOTS)} runs fit the shared quota next to a demo environment: "
                         "wait for one to finish and `down` it, or `down` one you no longer need")

    def find(self, name):
        validate_name(name)
        for slot, lease in self.leases().items():
            if lease and lease.get("name") == name:
                return slot, lease
        raise SparkError(f"no run named {name!r} (see `status`)")

    # ---- a run ----

    def start(self, name, suite, ref, test_args, timeout_minutes):
        validate_name(name)
        if suite not in SUITES:
            raise SparkError(f"unknown suite {suite!r}")
        resolved = spark_env.resolve_ref(ref)
        job = render_job((DEPLOY / "test-job.yaml").read_text(), suite, resolved.sha, test_args, timeout_minutes)
        cluster = (DEPLOY / "test-cluster.yaml").read_text()
        cache = (DEPLOY / "test-cache.yaml").read_text()
        script = DEPLOY / "test-run.sh"
        info = {"suite": suite, "ref": resolved.name, "sha": resolved.sha, "timeout_minutes": str(timeout_minutes)}
        slot = self.acquire(name, info)
        try:
            self.check_quota(slot)
        except BaseException:
            self.kubectl(["delete", "configmap", LEASE, "--ignore-not-found"], slot=slot)
            raise
        print(f"{name}: {suite} of {resolved.name} {resolved.sha[:12]} in {slot_namespace(self.base, slot)}")
        try:
            self.kubectl(["apply", "-f", "-"], slot=slot, stdin=cache)
            # `create`, not `apply`: apply would copy the password into the last-applied-configuration annotation.
            self.kubectl(["create", "-f", "-"], slot=slot, stdin=superuser_secret())
            self.kubectl(["create", "configmap", SCRIPT_CONFIGMAP, f"--from-file=test-run.sh={script}", f"--from-file=pg-forward.mjs={DEPLOY / 'pg-forward.mjs'}"], slot=slot)
            self.kubectl(["create", "-f", "-"], slot=slot, stdin=cluster)
            self.kubectl(["create", "-f", "-"], slot=slot, stdin=job)
        except BaseException:
            print(f"setup of {name} failed; removing it", file=sys.stderr)
            self.remove(slot)
            raise
        return slot, resolved

    def check_quota(self, slot):
        needs = dict(RUN_NEEDS)
        if self.get_json(slot, ["persistentvolumeclaim", CACHE_CLAIM]) is None:
            for key, value in CACHE_NEEDS.items():
                needs[key] += value
        short = quota_shortfalls(self.kubectl(["get", "resourcequota", "-o", "json"], capture=True), needs)
        if short:
            raise SparkError("the shared Spark quota cannot take another run now (" + "; ".join(short)
                             + "). Other runs or demo environments use it: `status`, `spark-env.py status`, and `down` what you no longer need")

    def pod_info(self, slot):
        pods = self.get_json(slot, ["pods", "-l", f"job-name={JOB}"])
        items = (pods or {}).get("items") or []
        if not items:
            return None, {}
        pod = items[0]
        terminated = {}
        for container in pod.get("status", {}).get("containerStatuses") or []:
            if container.get("name") == "tests":
                terminated = container.get("state", {}).get("terminated") or {}
        return pod, {"exit_code": terminated.get("exitCode"), "reason": terminated.get("reason")}

    def current_step(self, slot):
        """The step the pod is in, from the file the script keeps (the log tail may hold only compiler lines)."""
        try:
            return self.kubectl(["exec", f"job/{JOB}", "-c", "tests", "--", "cat", "/tmp/omg-step"], slot=slot, capture=True).strip() or None
        except SparkError:
            return None

    def phase(self, slot):
        job = self.kubectl(["get", f"job/{JOB}", "--ignore-not-found", "-o", "json"], slot=slot, capture=True).strip()
        if not job:
            return "preparing"
        pod, _ = self.pod_info(slot)
        return job_phase(job, pod, self.current_step(slot) if pod and pod.get("status", {}).get("phase") == "Running" else None)

    def wait(self, name, slot, quiet=False):
        """Poll until the job finishes; prints a line when the phase changes."""
        seen = None
        no_pod_since = None
        while True:
            phase = self.phase(slot)
            lease = self.lease(slot) or {}
            elapsed = self.clock() - int(lease.get("started", self.clock()))
            if phase != seen and not quiet:
                print(f"[{elapsed_text(elapsed)}] {name}: {phase}", flush=True)
                seen = phase
            if phase in ("succeeded", "failed"):
                return phase
            if phase == "starting" and self.pod_info(slot)[0] is None:
                no_pod_since = no_pod_since or self.clock()
                if self.clock() - no_pod_since > 120:
                    raise SparkError(f"the job has no pod after two minutes: {self.job_events(slot)}")
            else:
                no_pod_since = None
            self.sleep(POLL_SECONDS)

    def job_events(self, slot):
        try:
            text = self.kubectl(["get", "events", "--field-selector", "type=Warning", "-o", "custom-columns=REASON:.reason,MESSAGE:.message", "--no-headers"], slot=slot, capture=True)
        except SparkError:
            return "no events could be read"
        return "; ".join(spark_env.redact(line.strip()) for line in text.strip().splitlines()[-3:]) or "no warning events"

    def result(self, name, summary_path=None):
        """Print and return (exit code, summary) of a finished run."""
        slot, lease = self.find(name)
        job = self.kubectl(["get", f"job/{JOB}", "--ignore-not-found", "-o", "json"], slot=slot, capture=True).strip()
        if not job:
            raise SparkError(f"{name} has no job yet")
        state = spark_env.job_state(job)
        if state == "running":
            raise SparkError(f"{name} has not finished (`status {name}`, `logs {name} -f`)")
        _, pod = self.pod_info(slot)
        log = self.kubectl(["logs", f"job/{JOB}", "--tail=-1"], slot=slot, capture=True)
        parsed = parse_log(log)
        wall = self.wall_seconds(json.loads(job), lease)
        summary = build_summary(name, lease.get("suite"), lease.get("ref"), lease.get("sha"), parsed, state, pod, wall)
        if parsed["commit"] not in (None, lease.get("sha")):
            raise SparkError(f"the job ran commit {parsed['commit']}, not {lease.get('sha')}")
        print(format_report(summary, parsed))
        if summary_path:
            write_summary(summary_path, summary)
        return (0 if summary["result"] == "passed" else 1), summary

    def wall_seconds(self, job, lease):
        status = job.get("status", {})
        start = status.get("startTime")
        end = status.get("completionTime") or next((c.get("lastTransitionTime") for c in status.get("conditions") or [] if c.get("type") == "Failed"), None)
        if start and end:
            parse = lambda value: datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
            return int(parse(end) - parse(start))
        return int(self.clock() - int(lease.get("started", self.clock())))

    def run(self, name, suite, ref, test_args, timeout_minutes, keep, detach, summary_path):
        slot, resolved = self.start(name, suite, ref, test_args, timeout_minutes)
        print(f"started. `spark-test.py status {name}`, `logs {name} -f`, `result {name}`, `down {name}`")
        if detach:
            return 0
        try:
            self.wait(name, slot)
        except KeyboardInterrupt:
            print(f"\nstopped waiting; the run continues on the cluster. `status {name}`, `result {name}`, `down {name}`", file=sys.stderr)
            raise
        code, _ = self.result(name, summary_path)
        if keep:
            print(f"kept: `logs {name}`, `down {name}` when done")
        else:
            self.down(name)
        return code

    # ---- removal ----

    def remove(self, slot):
        """Delete everything of the run in the slot except the cache volume, the lease last."""
        namespace = slot_namespace(self.base, slot)
        for kind, name in (("job", JOB), ("cluster.postgresql.cnpg.io", CLUSTER)):
            self.kubectl(["delete", kind, name, "--ignore-not-found", "--wait=true", "--timeout=180s"], slot=slot)
        self.kubectl(["delete", "secret", SUPERUSER_SECRET, "--ignore-not-found"], slot=slot)
        self.kubectl(["delete", "secret", "-l", f"cnpg.io/cluster={CLUSTER}", "--ignore-not-found"], slot=slot)
        self.kubectl(["delete", "configmap", SCRIPT_CONFIGMAP, "--ignore-not-found"], slot=slot)
        for _ in range(60):
            left = [line for line in self.kubectl(["get", "pods,persistentvolumeclaims", "-o", "name"], slot=slot, capture=True).split() if line != f"persistentvolumeclaim/{CACHE_CLAIM}"]
            if not left:
                break
            self.sleep(3)
        else:
            raise SparkError(f"{namespace} still has {', '.join(left)} after 3 minutes; the lease is kept so the slot is not reused: retry `down`")
        self.kubectl(["delete", "configmap", LEASE, "--ignore-not-found"], slot=slot)

    def down(self, name):
        slot, _ = self.find(name)
        self.remove(slot)
        print(f"removed {name} (slot {slot} is free; its cache volume stays). Creating pods can fail briefly after a delete; wait and retry.")

    def purge(self):
        leases = self.leases()
        if not leases:
            print("no slot namespaces")
            return
        for slot, lease in leases.items():
            if lease:
                print(f"slot {slot} is in use by {lease_text(lease)}; not purged")
                continue
            self.kubectl(["delete", "namespace", slot_namespace(self.base, slot), "--wait=false"])
            print(f"deleted {slot_namespace(self.base, slot)} and its cache volume")

    # ---- reading ----

    def status(self, name=None):
        leases = self.leases()
        if name:
            slot, lease = self.find(name)
            print(f"{name}: {lease_text(lease)} slot {slot} {self.phase(slot)} elapsed {elapsed_text(self.clock() - int(lease.get('started', self.clock())))}")
            print(self.kubectl(["get", "pods,jobs,clusters.postgresql.cnpg.io"], slot=slot, capture=True), end="")
            return
        if not leases:
            print("no slot namespaces (the first run creates them)")
        for slot, lease in leases.items():
            if lease:
                print(f"{slot}: {lease_text(lease)} {self.phase(slot)} elapsed {elapsed_text(self.clock() - int(lease.get('started', self.clock())))}")
            else:
                print(f"{slot}: free")

    def logs(self, name, follow):
        slot, _ = self.find(name)
        command = kubectl_command(self.config, self.base, ["logs", f"job/{JOB}", *(["-f"] if follow else ["--tail=-1"])], slot_namespace(self.base, slot))
        try:
            code = self.launcher(command).returncode
        except KeyboardInterrupt:
            return
        if code:
            raise SparkError(f"kubectl logs exited with status {code}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="start a run and wait for the result")
    run.add_argument("name", help="2-20 characters: lowercase letters, digits, dashes")
    run.add_argument("--ref", required=True, help="branch, commit or tag of this clone; it must already be pushed")
    run.add_argument("--suite", required=True, choices=SUITES)
    run.add_argument("--test-args", default="", help="extra libtest (lib) or vitest (web) arguments, for example a test filter")
    run.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_MINUTES, metavar="MINUTES", help=f"deadline of the job (default {DEFAULT_TIMEOUT_MINUTES}, at most {MAX_TIMEOUT_MINUTES})")
    run.add_argument("--summary", metavar="PATH", help="write the compact JSON summary to PATH")
    run.add_argument("--keep", action="store_true", help="do not `down` the run after printing its result")
    run.add_argument("--detach", action="store_true", help="start the run and return; read it with status, logs and result")
    status = commands.add_parser("status", help="the slots and their runs, or one run")
    status.add_argument("name", nargs="?")
    logs = commands.add_parser("logs", help="the log of a run")
    logs.add_argument("name")
    logs.add_argument("-f", "--follow", action="store_true")
    result = commands.add_parser("result", help="the result of a finished run")
    result.add_argument("name")
    result.add_argument("--summary", metavar="PATH")
    commands.add_parser("down", help="delete a run and its database; the cache stays").add_argument("name")
    commands.add_parser("purge", help="delete the idle slot namespaces and their cache volumes")
    args = parser.parse_args(argv)
    try:
        spark = SparkTest(spark_env.kubeconfig_path(os.environ))
        spark_env.require_tools(("kubectl", "git") if args.command == "run" else ("kubectl",))
        code = 0
        if args.command == "run":
            code = spark.run(args.name, args.suite, args.ref, args.test_args, args.timeout, args.keep, args.detach, args.summary)
        elif args.command == "result":
            code, _ = spark.result(args.name, args.summary)
        elif args.command == "status":
            spark.status(args.name)
        elif args.command == "logs":
            spark.logs(args.name, args.follow)
        elif args.command == "down":
            spark.down(args.name)
        else:
            spark.purge()
        raise SystemExit(code)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        raise SystemExit(130) from None
    except SparkError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2) from None


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Run the test suites of any pushed ref of this repository on the Spark k3s test platform.

TEST ONLY. Plain kubectl on a developer machine may target production, so this script passes
the Spark kubeconfig explicitly to every kubectl call, refuses to run unless that file exists
and names the Spark server (the checks of scripts/spark-env.py, which it loads), runs every
tool with a scrubbed environment, and only ever touches the two namespaces
<base>-omg-test-s1 and <base>-omg-test-s2, and only when they carry the label
app.kubernetes.io/managed-by=omg-spark-test that this tool puts on them. See AGENTS.md
"Running a branch on the cluster".

  run <name> --ref <branch|commit|tag> --suite lib|integration|web|checks|all
                                              start a run, wait for it, print the result
  status [name]                               the slots and their runs, or one run
  logs <name> [-f]                            the run's log
  result <name> [--summary PATH]              the result of a finished run
  down <name>                                 delete the run, its database and its pod
  purge                                       delete the idle slot namespaces and their caches

Exit status of run and result: 0 every suite passed in full; 1 a suite failed (a test, a check, a
run that executed no tests, a step that did not run); 2 an error of the tool or the platform, or a
run the platform killed (out of memory, deadline, eviction) or one whose setup failed (for example
an unpushed commit); 3 passed but partial (--test-args narrowed it); 4 `run --detach` started a
run and says nothing about its outcome.

Two runs can be active at once (two slots): the platform quota (4 CPU and 16 GiB requested,
shared by all namespaces) leaves room for exactly that next to one demo environment. Each slot
is a namespace that holds one persistent cache volume, so a run after a small change does not
rebuild the world. A slot is leased to one run at a time (a suspended Job created atomically), so two
runs never share a target directory. `down` removes everything of a run except the cache; `purge`
removes the cache. The cache is trusted only as far as the refs that ran in the slot: purge after
a ref you would not run on your own machine.

The job fetches the public repository at the exact commit the ref resolves to (push first) and
verifies HEAD. Nothing but the job manifest and the script is uploaded. The job runs the ref's build
scripts, proc-macros, npm install scripts and tests with internet access: test only refs you would run
locally.
"""
import argparse
import datetime
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import time

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
NONCE = re.compile(r"[0-9a-f]{16}")
# What may be handed to libtest or vitest as extra arguments: no shell syntax, no quotes ...
TEST_ARGS = re.compile(r"[A-Za-z0-9_:.,=/@-]+(?: [A-Za-z0-9_:.,=/@-]+)*")
# ... and nothing that makes the test binary or Vitest print or do something other than run tests.
REFUSED_ARGS = ("--list", "--help", "--version", "--format", "--show-output", "--report-time", "--ensure-time", "--shuffle", "--watch", "--ui", "--update", "--reporter", "--coverage", "--run=false")
REFUSED_SHORT = ("-h", "-V", "-u")
LEASE = "omg-test-lease"  # legacy: active pre-fencing slots must be drained with the old tool
RUN_INFO = "omg-spark-run"
RUN_PHASE = "omg-spark-phase"
CLEANUP_FINALIZER = "spark-test.bitop.dev/cleanup"
JOB = "omg-test"
CLUSTER = "omg-test-db"
SCRIPT_CONFIGMAP = "omg-test-script"
SUPERUSER_SECRET = "omg-test-pg-superuser"
CACHE_CLAIM = "omg-test-cache"
MANAGED_LABEL = "app.kubernetes.io/managed-by"
MANAGED_VALUE = "omg-spark-test"
MARKER = "::omg-test::"
DEFAULT_TIMEOUT_MINUTES = {"web": 30, "checks": 45, "integration": 60, "lib": 120, "all": 180}
MAX_TIMEOUT_MINUTES = 480
POLL_SECONDS = 20
STUCK_SECONDS = 600  # a pod that cannot start (image pull, scheduling, volume) for this long fails the wait
TRANSIENT_ERRORS = 3  # consecutive failed polls tolerated
LEDGER_RETRIES = 24  # the platform needs about two minutes after a namespace delete
LEDGER_WAIT_SECONDS = 10
EXIT_PASSED, EXIT_FAILED, EXIT_ERROR, EXIT_PARTIAL, EXIT_STARTED = 0, 1, 2, 3, 4
DEFAULT_CPU_LIMIT_M = 1800  # deploy/spark/test-job.yaml; --fast raises the limit, never the request
FAST_MAX_CPU_M = 6000
FAST_MIN_EXTRA_M = 500
DB_CPU_LIMIT_M = 1000
LOG_DIR = ROOT / ".local" / "spark-test"  # .local/ is ignored by git and kept out of image build contexts
# What one run adds to the shared quota (deploy/spark/test-job.yaml and test-cluster.yaml: the job pod and
# the database; the node-copy init container is smaller than the main container and does not count).
# The cache volume is added when the slot has none yet. tests/spark-test.test.py checks these against the manifests.
RUN_NEEDS = {"requests.cpu": 1750, "requests.memory": 6656, "limits.cpu": 2800, "limits.memory": 11264, "pods": 3, "persistentvolumeclaims": 1, "requests.storage": 8192}
CACHE_NEEDS = {"persistentvolumeclaims": 1, "requests.storage": 20480}
ARCHITECTURE_NOTE = "aarch64 (the Spark); amd64 stays covered by GitHub CI"
# Steps each suite must have completed; a step that did not run is a failure, never a pass.
REQUIRED_STEPS = {
    "checks": ("fmt", "clippy-workspace", "clippy-shipped"),
    "lib": ("npm-ci", "build"),
    "integration": ("npm-ci", "targets", "runtime-privileges", "runtime-privileges-ignored", "key-issuance-contention", "anthropic-terminal", "tools", "doctests"),
    "web": ("npm-ci", "typecheck", "vitest"),
}
LIB_SHARDS = 4
TEST_SUITES = ("lib", "integration", "web")  # suites that must execute at least one test
SUITE_PARTS = {"lib": ("lib",), "integration": ("integration",), "web": ("web",), "checks": ("checks",), "all": ("web", "checks", "lib", "integration")}


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
    for word in text.split(" "):
        if word in REFUSED_SHORT or word.startswith(REFUSED_ARGS):
            raise SparkError(f"--test-args {word!r} would not run tests (it lists, prints help or changes the output)")
    return text


def cpu_plan(cpu_limit_m):
    """Cargo jobs, libtest threads and Vitest workers that follow a CPU limit (1800m gives 2, 4 and 1).

    Node's availableParallelism() rounds a cgroup limit down (1 under 1800m, measured), which is what Vitest sizes its
    workers by; the explicit --maxWorkers makes that the recorded choice and scales it with --fast.
    """
    jobs = max(1, min(4, math.ceil(cpu_limit_m / 1000)))
    return {"jobs": jobs, "threads": max(4, 2 * jobs), "workers": max(1, min(4, cpu_limit_m // 1000))}


def render_job(text, suite, sha, test_args, timeout_minutes, nonce=None, fresh=False, cpu_limit_m=DEFAULT_CPU_LIMIT_M, npm_flags=""):
    """Substitute the job's few parameters, each after a strict check; no placeholder may remain."""
    if suite not in SUITES:
        raise SparkError(f"unknown suite {suite!r}")
    if not SHA.fullmatch(sha or ""):
        raise SparkError("the commit must be a full 40-character id")
    if not 1 <= timeout_minutes <= MAX_TIMEOUT_MINUTES:
        raise SparkError(f"--timeout must be 1-{MAX_TIMEOUT_MINUTES} minutes")
    nonce = nonce or secrets.token_hex(8)
    if not NONCE.fullmatch(nonce):
        raise SparkError("the nonce must be 16 hexadecimal characters")
    if not isinstance(cpu_limit_m, int) or not DEFAULT_CPU_LIMIT_M <= cpu_limit_m <= FAST_MAX_CPU_M:
        raise SparkError(f"the CPU limit must be {DEFAULT_CPU_LIMIT_M}-{FAST_MAX_CPU_M}m")
    if npm_flags not in ("", "--ignore-scripts"):
        raise SparkError("unsupported npm flags")
    plan = cpu_plan(cpu_limit_m)
    values = {"__SUITE__": suite, "__SHA__": sha, "__TEST_ARGS__": validate_test_args(test_args, suite), "__DEADLINE__": str(timeout_minutes * 60), "__NONCE__": nonce,
              "__FRESH__": "1" if fresh else "0", "__JOBS__": str(plan["jobs"]), "__TEST_THREADS__": str(plan["threads"]), "__WEB_WORKERS__": str(plan["workers"]), "__NPM_FLAGS__": npm_flags}
    for key, value in values.items():
        if key not in text:
            raise SparkError(f"manifest has no {key} placeholder")
        text = text.replace(key, value)
    if cpu_limit_m != DEFAULT_CPU_LIMIT_M:
        default = f'limits: {{cpu: "{DEFAULT_CPU_LIMIT_M}m", memory: 10Gi}}'
        if text.count(default) != 1:
            raise SparkError("manifest has no CPU limit to raise")
        text = text.replace(default, f'limits: {{cpu: "{cpu_limit_m}m", memory: 10Gi}}')
    if re.search(r"__[A-Z_]+__", text):
        raise SparkError("manifest still has a placeholder")
    return text


def superuser_secret():
    """The run's PostgreSQL superuser Secret, as JSON for stdin; the hex password is never an argument."""
    password = secrets.token_hex(24)
    spark_env._REGISTERED_SECRETS.append(password)
    return json.dumps({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": SUPERUSER_SECRET, "labels": {"app.kubernetes.io/name": "omg-test"}},
                       "type": "kubernetes.io/basic-auth", "stringData": {"username": "postgres", "password": password}})


def namespace_manifest(name):
    return json.dumps({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name, "labels": {MANAGED_LABEL: MANAGED_VALUE}}})


def load_coverage():
    return json.loads((DEPLOY / "test-coverage.json").read_text())


def not_covered(suite, coverage=None):
    """What a run of the suite does not cover, from deploy/spark/test-coverage.json (compared with ci.yml by a test)."""
    coverage = coverage or load_coverage()
    items = [f"{entry['ci']} ({entry['why']})" for entry in coverage["uncovered"] if suite in entry["suites"]]
    return items + list(coverage["always"])


# ---- reading the log ----

# Every terminal escape sequence (CSI, OSC with either terminator, DCS/PM/APC strings, two-character sequences) is removed,
# then anything that is not printable ASCII.
ESCAPES = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\|$)|[PX^_][^\x1b]*(?:\x1b\\|$)|[@-Z\\-_])")
LIBTEST_RESULT = re.compile(r"test result: (ok|FAILED)\. (\d{1,9}) passed; (\d{1,9}) failed; (\d{1,9}) ignored; (\d{1,9}) measured; (\d{1,9}) filtered out(?:; finished in \d+(?:\.\d+)?s)?")
LIBTEST_FAILED = re.compile(r"test (\S.*?) \.\.\. FAILED")
VITEST_TESTS = re.compile(r"\s*Tests\s+((?:\d{1,9} (?:failed|passed|skipped|todo)(?: \| )?)+)\s*\((\d{1,9})\)\s*")
VITEST_FAIL = re.compile(r"\s*FAIL\s+(\S.*)")
STEP_NAME = re.compile(r"[a-z0-9/-]{1,60}")
MARKER_KINDS = ("run", "environment", "step-start", "step-end", "source", "cache", "database", "lib-inventory", "lib-shard", "suite-start", "suite-end", "setup-failed", "done", "terminated")
MARKER_FIELD = re.compile(r"([a-z_]{1,24})=([A-Za-z0-9._/:@,+-]{1,80})")
NUMERIC_FIELDS = ("exit", "seconds", "tests", "index")
NAME_LIMIT = 200
FAILURE_BLOCKS = 10
FAILURE_BLOCK_LINES = 40


def sanitize(text, limit=NAME_LIMIT):
    """Printable ASCII only, no escape sequence of any kind, bounded."""
    text = ESCAPES.sub("", str(text))
    return "".join(c for c in text if " " <= c <= "~")[:limit]


def parse_marker(line, nonce):
    """`::omg-test::<nonce>:: step-end suite=lib name=x exit=0` -> ("step-end", {...}), or None for a line that is not a valid marker of this run."""
    prefix = f"{MARKER}{nonce}:: "
    if not nonce or not NONCE.fullmatch(nonce) or not line.startswith(prefix):
        return None
    words = line[len(prefix):].rstrip().split(" ")
    kind, fields = words[0], {}
    if kind not in MARKER_KINDS:
        return None
    for word in words[1:]:
        if not word:
            continue
        m = MARKER_FIELD.fullmatch(word)
        if not m:
            return None  # a malformed field makes the whole line forged or garbled: ignore it
        key, value = m.groups()
        if key in NUMERIC_FIELDS and not re.fullmatch(r"\d{1,9}", value):
            return None
        fields[key] = value
    if kind in ("step-start", "step-end") and (not STEP_NAME.fullmatch(fields.get("name", "")) or not STEP_NAME.fullmatch(fields.get("suite", ""))):
        return None
    if kind == "step-end" and not {"exit", "seconds"} <= set(fields):
        return None
    if kind == "done" and not {"exit", "seconds"} <= set(fields):
        return None
    return kind, fields


def parse_log(text, nonce):
    """Everything the summary needs from a run's log. Only marker lines carrying the run's nonce are believed; every number is a regex capture."""
    steps, results, failed_tests, vitest, tail = [], [], [], [], []
    suites = {}
    done = commit = inventory = database = cache = None
    setup_failed = terminated = False
    current = ""
    for raw in text.splitlines():
        line = sanitize(raw, 400).rstrip()
        marker = parse_marker(line, nonce)
        if marker:
            kind, fields = marker
            if kind == "step-start":
                current = fields["suite"]
            elif kind == "step-end":
                steps.append({"suite": fields["suite"], "name": fields["name"], "exit": int(fields["exit"]), "seconds": int(fields["seconds"])})
            elif kind == "done":
                done = {"exit": int(fields["exit"]), "seconds": int(fields["seconds"])}
            elif kind == "source":
                commit = fields.get("commit") if SHA.fullmatch(fields.get("commit", "")) else None
            elif kind == "lib-inventory":
                inventory = int(fields.get("tests", "0"))
            elif kind == "database":
                found = {key: fields[key] for key in ("version", "lc_collate", "max_connections") if key in fields}
                if found or fields.get("unavailable") == "1":
                    database = found or {"unavailable": True}
            elif kind == "cache":
                cache = {key: int(value) for key, value in fields.items() if re.fullmatch(r"\d{1,9}", value)}
            elif kind == "setup-failed":
                setup_failed = True
            elif kind == "terminated":
                terminated = True
            continue
        if line.startswith(MARKER):
            continue  # a forged or foreign marker is neither believed nor shown
        if line.strip():
            tail.append(line)
            del tail[:-20]
        counts = suites.setdefault(current or "setup", {"passed": 0, "failed": 0, "ignored": 0, "targets": 0})
        if m := LIBTEST_RESULT.fullmatch(line):
            status, passed, failed, ignored, measured, filtered = m.groups()
            results.append(f"test result: {status}. {int(passed)} passed; {int(failed)} failed; {int(ignored)} ignored; {int(measured)} measured; {int(filtered)} filtered out")
            counts["passed"] += int(passed)
            counts["failed"] += int(failed)
            counts["ignored"] += int(ignored)
            counts["targets"] += 1
        elif m := LIBTEST_FAILED.fullmatch(line):
            failed_tests.append(sanitize(m.group(1)))
        elif m := VITEST_TESTS.fullmatch(line):
            pairs = {kind: int(n) for n, kind in re.findall(r"(\d+) (failed|passed|skipped|todo)", m.group(1))}
            vitest.append("Tests  " + " | ".join(f"{n} {kind}" for kind, n in pairs.items()) + f" ({int(m.group(2))})")
            counts["passed"] += pairs.get("passed", 0)
            counts["failed"] += pairs.get("failed", 0)
            counts["ignored"] += pairs.get("skipped", 0) + pairs.get("todo", 0)
            counts["targets"] += 1
        elif m := VITEST_FAIL.fullmatch(line):
            failed_tests.append(sanitize(m.group(1)))
    suites = {name: counts for name, counts in suites.items() if counts["targets"]}
    return {"steps": steps, "results": results, "vitest": vitest, "suites": suites, "failed_tests": list(dict.fromkeys(failed_tests))[:200], "done": done, "commit": commit,
            "inventory": inventory, "database": database, "cache": cache, "setup_failed": setup_failed, "terminated": terminated, "tail": tail}


def failure_blocks(text, names):
    """The libtest output blocks (`---- name stdout ----`) of the failing tests: sanitised and bounded."""
    wanted = set(names)
    blocks, current, lines = [], None, []
    for raw in text.splitlines():
        line = sanitize(raw, 400).rstrip()
        m = re.fullmatch(r"---- (\S.*?) stdout ----", line)
        if m or line == "failures:":
            if current is not None and lines:
                blocks.append((current, lines))
            current, lines = (m.group(1), []) if m and sanitize(m.group(1)) in wanted else (None, [])
            continue
        if current is not None:
            lines.append(line)
            del lines[FAILURE_BLOCK_LINES:]
    if current is not None and lines:
        blocks.append((current, lines))
    return [(name, [line for line in body if line.strip()][:FAILURE_BLOCK_LINES]) for name, body in blocks[:FAILURE_BLOCKS]]


def expected_problems(suite, parsed):
    """Reasons a run that exited 0 still did not run what the suite means: steps missing, no test executed."""
    problems = []
    finished = {(s["suite"], s["name"]) for s in parsed["steps"]}
    # npm dependencies are installed once and reused by later suites in the same pod.
    npm_installed = any(s["suite"] in SUITE_PARTS[suite] and s["name"] == "npm-ci" and s["exit"] == 0 for s in parsed["steps"])
    for part in SUITE_PARTS[suite]:
        required = list(REQUIRED_STEPS.get(part, ()))
        if part == "lib":
            ran_filtered = ("lib", "filtered") in finished
            required += ["filtered"] if ran_filtered else [f"shard-{i}" for i in range(LIB_SHARDS)]
        for name in required:
            if not (npm_installed if name == "npm-ci" else (part, name) in finished):
                problems.append(f"step {part}/{name} did not run")
        if part in TEST_SUITES and parsed["suites"].get(part, {}).get("passed", 0) == 0:
            problems.append(f"suite {part} executed no tests")
    return problems


def build_summary(name, suite, ref, sha, parsed, job_state, pod, wall_seconds, test_args="", fresh=False, fast=None):
    """The compact JSON of a finished run. No log text, no environment, no secret."""
    done = parsed["done"]
    notes = []
    exit_code = done["exit"] if done is not None else (pod.get("exit_code") if pod.get("exit_code") is not None else 1)
    failed_steps = [f"{s['suite']}/{s['name']}" for s in parsed["steps"] if s["exit"] != 0]
    totals = {key: sum(c[key] for c in parsed["suites"].values()) for key in ("passed", "failed", "ignored")}
    if parsed["setup_failed"]:
        result = "setup failed"
    elif done is None:
        result = "killed"
        notes.append("the run did not reach its end marker: the pod was stopped (out of memory, deadline, eviction or deletion)")
    elif exit_code != 0 or job_state != "complete" or failed_steps or totals["failed"]:
        result = "failed"
    else:
        result = "passed"
    if result == "passed":
        problems = expected_problems(suite, parsed)
        if problems:
            result = "failed"
            exit_code = exit_code or 1
            notes.extend(problems)
    lib = parsed["suites"].get("lib")
    if parsed.get("inventory") is not None and result in ("passed", "failed"):
        accounted = lib["passed"] + lib["failed"] + lib["ignored"] if lib else 0
        if accounted != parsed["inventory"]:
            result = "failed"
            exit_code = exit_code or 1
            notes.append(f"the gateway library lists {parsed['inventory']} tests but the shards account for {accounted}")
    partial = bool(test_args)
    summary = {
        "name": name, "suite": suite, "ref": ref, "commit": sha, "architecture": ARCHITECTURE_NOTE,
        "result": result, "partial": partial, "test_args": test_args, "fresh_target": bool(fresh),
        "exit_code": exit_code, "job": job_state,
        "duration_seconds": done["seconds"] if done else wall_seconds, "wall_seconds": wall_seconds,
        "counts": totals, "suites": parsed["suites"], "steps": parsed["steps"],
        "failed_steps": failed_steps, "failed_tests": parsed["failed_tests"],
        "not_covered": not_covered(suite),
    }
    if fast:
        summary["fast_cpu_limit_m"] = fast
    if parsed.get("inventory") is not None:
        summary["lib_tests_listed"] = parsed["inventory"]
    for key in ("database", "cache"):
        if parsed[key]:
            summary[key] = parsed[key]
    if pod.get("reason"):
        summary["pod_reason"] = sanitize(pod["reason"], 60)
    if notes:
        summary["notes"] = notes
    if partial and result == "passed":
        summary["notes"] = summary.get("notes", []) + [f"partial: only the tests selected by --test-args {test_args!r} ran; this is not a pass of the suite"]
    return summary


def result_label(summary):
    return "passed (partial)" if summary["result"] == "passed" and summary["partial"] else summary["result"]


def result_exit(summary):
    label = result_label(summary)
    return {"passed": EXIT_PASSED, "passed (partial)": EXIT_PARTIAL, "failed": EXIT_FAILED}.get(label, EXIT_ERROR)


def format_report(summary, parsed, log_text=""):
    lines = [f"RESULT: {result_label(summary)}: {summary['suite']} of {sanitize(summary['ref'], 60)} {summary['commit'][:12]}" + (f" (partial: --test-args {summary['test_args']})" if summary["partial"] else "")]
    lines.extend(parsed["results"] or ["(no `test result:` lines in the log)"])
    lines.extend(parsed["vitest"])
    if summary["failed_tests"]:
        lines.append("failing tests:")
        lines.extend(f"  {test}" for test in summary["failed_tests"])
    if summary["failed_steps"]:
        lines.append("failed steps: " + ", ".join(summary["failed_steps"]))
    for note in summary.get("notes", []):
        lines.append(f"note: {note}")
    if summary["result"] == "setup failed":
        lines.append("setup failed; the last lines of the log (sanitised):")
        lines.extend(f"  {line}" for line in parsed["tail"])
    elif summary["failed_tests"] and log_text:
        blocks = failure_blocks(log_text, summary["failed_tests"])
        if blocks:
            lines.append("failure output (sanitised, bounded):")
            for test, body in blocks:
                lines.append(f"  ---- {sanitize(test)} ----")
                lines.extend(f"    {line}" for line in body)
    lines.append("not covered by this run: " + "; ".join(summary["not_covered"]))
    lines.append(json.dumps(summary, separators=(",", ":")))
    return "\n".join(lines)


def private_file(path, truncate=True):
    flags = os.O_WRONLY | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0) | (os.O_TRUNC if truncate else 0)
    return os.open(path, flags, 0o600)


def check_writable(path):
    """Open (and leave empty) the --summary or --log path before the run starts, so a bad path does not cost a run."""
    if not path:
        return
    try:
        fd = private_file(path, truncate=False)
    except OSError as error:
        raise SparkError(f"cannot write {path}: {error.strerror}") from None
    with os.fdopen(fd, "w") as handle:
        if not stat.S_ISREG(os.fstat(handle.fileno()).st_mode):
            raise SparkError(f"{path} is not a regular file")
        os.fchmod(handle.fileno(), 0o600)


def write_private(path, text):
    try:
        fd = private_file(path)
    except OSError as error:
        raise SparkError(f"cannot write {path}: {error.strerror}") from None
    with os.fdopen(fd, "w") as handle:
        if not stat.S_ISREG(os.fstat(handle.fileno()).st_mode):
            raise SparkError(f"{path} is not a regular file")
        os.fchmod(handle.fileno(), 0o600)
        handle.write(text)


def write_summary(path, summary):
    write_private(path, json.dumps(summary, indent=2) + "\n")


def sanitized_log(text):
    """The log as it may be saved: no escape sequences, printable ASCII, generated passwords redacted."""
    return "\n".join(spark_env.redact(sanitize(line, 2000)) for line in text.splitlines()) + "\n"


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


STUCK_WAITING = ("ImagePullBackOff", "ErrImagePull", "CreateContainerConfigError", "CreateContainerError", "InvalidImageName", "RunContainerError")


def stuck_reason(pod):
    """Why a pod cannot start (image pull, configuration, scheduling, volume), or None while it is merely starting."""
    status = (pod or {}).get("status", {})
    if status.get("phase") not in (None, "Pending"):
        return None
    for container in (status.get("initContainerStatuses") or []) + (status.get("containerStatuses") or []):
        waiting = (container.get("state") or {}).get("waiting") or {}
        if waiting.get("reason") in STUCK_WAITING:
            return f"{waiting['reason']}: {sanitize(waiting.get('message', ''), 160)}".rstrip(": ")
    for condition in status.get("conditions") or []:
        if condition.get("type") == "PodScheduled" and condition.get("status") == "False":
            return f"{condition.get('reason', 'Unschedulable')}: {sanitize(condition.get('message', ''), 160)}".rstrip(": ")
    return None


def quantity(key, text):
    """A Kubernetes quantity in the unit RUN_NEEDS uses: milli-CPU, MiB, or a plain count."""
    m = re.fullmatch(r"([0-9]*\.?[0-9]+(?:[eE][+-]?[0-9]+)?)([A-Za-z]*)", str(text).strip())
    if not m:
        raise SparkError(f"unreadable quantity {text!r}")
    number, suffix = float(m.group(1)), m.group(2)
    if key.endswith(".cpu"):
        return int(number if suffix == "m" else number * 1000)
    if key.endswith(".memory") or key == "requests.storage":
        factors = {"": 1, "k": 1000, "M": 1000**2, "G": 1000**3, "T": 1000**4, "P": 1000**5, "Ki": 1024, "Mi": 1024**2, "Gi": 1024**3, "Ti": 1024**4, "Pi": 1024**5}
        if suffix not in factors:
            raise SparkError(f"unreadable quantity {text!r}")
        return int(number * factors[suffix] / (1024 * 1024))
    return int(number)


def quota_free(quotas_json, keys):
    """The free amount of each key, from `kubectl get resourcequota -o json` of the base namespace.

    The Spark quota is one global budget; the base namespace's ResourceQuota shows the hard limit minus what
    the other namespaces use, and its own `used`. When several objects define a key the smallest wins.
    """
    free = {}
    for item in json.loads(quotas_json).get("items", []):
        status = item.get("status", {})
        for key, hard in (status.get("hard") or {}).items():
            if key in keys:
                room = quantity(key, hard) - quantity(key, (status.get("used") or {}).get(key, "0"))
                free[key] = min(room, free[key]) if key in free else room
    return free


def quota_shortfalls(quotas_json, needs):
    free = quota_free(quotas_json, needs)
    return [f"{key}: needs {need}, {free[key]} available" for key, need in needs.items() if key in free and free[key] < need]


def fast_cpu_limit(free_limit_cpu_m):
    """The CPU limit --fast asks for: all the free limit quota beyond one normal run, up to FAST_MAX_CPU_M; None when that adds too little."""
    extra = free_limit_cpu_m - RUN_NEEDS["limits.cpu"]
    if extra < FAST_MIN_EXTRA_M:
        return None
    return min(DEFAULT_CPU_LIMIT_M + extra, FAST_MAX_CPU_M)


def lease_text(lease):
    args = f" --test-args {lease['test_args']!r}" if lease.get("test_args") else ""
    return f"{lease['name']} ({lease.get('suite')} {lease.get('ref')} {str(lease.get('sha', ''))[:12]}{args})"


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

    def slot_state(self, slot):
        """None when the slot namespace does not exist; otherwise whether it is ours and whether it is being deleted."""
        namespace = slot_namespace(self.base, slot)
        out = self.kubectl(["get", "namespace", namespace, "--ignore-not-found", "-o", "json"], capture=True).strip()
        if not out:
            return None
        document = json.loads(out)
        return {"uid": document["metadata"]["uid"], "ours": (document.get("metadata", {}).get("labels") or {}).get(MANAGED_LABEL) == MANAGED_VALUE,
                "terminating": bool(document["metadata"].get("deletionTimestamp")) or (document.get("status") or {}).get("phase") == "Terminating"}

    def existing_slots(self, allow_terminating=False):
        """The slot namespaces that exist and are ours. An existing namespace of that name without our label is refused; one being deleted gives a retry message."""
        found = []
        for slot in SLOTS:
            state = self.slot_state(slot)
            if state is None:
                continue
            namespace = slot_namespace(self.base, slot)
            if not state["ours"]:
                raise SparkError(f"namespace {namespace} exists but does not carry {MANAGED_LABEL}={MANAGED_VALUE}: it was not created by this tool, so it is not used, stopped or deleted. Remove it yourself or leave the slot alone")
            if state["terminating"] and not allow_terminating:
                raise SparkError(f"{namespace} is being deleted (a `purge`, or a finished delete): retry in about a minute")
            found.append(slot)
        return found

    def lease(self, slot):
        document = self.get_json(slot, ["job", JOB])
        if document:
            meta = document["metadata"]
            annotations = meta.get("annotations", {})
            if RUN_INFO not in annotations:
                raise SparkError(f"slot {slot} has a Job without fenced run metadata; drain it with the prior tool before upgrading")
            return {**json.loads(annotations[RUN_INFO]), "uid": meta["uid"], "version": meta["resourceVersion"],
                    "phase": annotations[RUN_PHASE], "created": meta.get("creationTimestamp")}
        if self.get_json(slot, ["configmap", LEASE]):
            raise SparkError(f"slot {slot} has a legacy active lease: finish/down it with the prior tool before upgrading; do not mix runner versions")
        return None

    def require_run(self, slot, expected):
        current = self.lease(slot)
        if not current or any(current.get(key) != expected.get(key) for key in ("uid", "namespace_uid", "nonce", "name", "sha")):
            raise SparkError(f"the lease of slot {slot} no longer belongs to {expected.get('name')!r}; the slot was removed or reused")
        return current

    def patch_run(self, slot, expected, phase, changes):
        # JSON Patch tests and changes are one API-server transaction. A stale writer cannot
        # activate a deleting Job or patch another incarnation, even after its last GET.
        patch = [{"op": "test", "path": "/metadata/uid", "value": expected["uid"]},
                 {"op": "test", "path": f"/metadata/annotations/{RUN_PHASE}", "value": phase}, *changes]
        self.kubectl(["patch", "job", JOB, "--type=json", "-p", json.dumps(patch)], slot=slot)

    def delete_object(self, slot, resource, document, wait=True):
        """DELETE with an API-server UID precondition, never a name-only or collection DELETE."""
        meta = document["metadata"]
        prefix = "/apis/batch/v1" if resource == "jobs" else "/apis/postgresql.cnpg.io/v1" if resource == "clusters" else "/api/v1"
        if slot is None:
            check_namespace(self.base, meta["name"])
        path = f"{prefix}/namespaces/{slot_namespace(self.base, slot)}/{resource}/{meta['name']}" if slot else f"/api/v1/namespaces/{meta['name']}"
        options = {"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": meta["uid"]}, "propagationPolicy": "Foreground"}
        try:
            self.kubectl(["delete", "--raw", path, "-f", "-"], stdin=json.dumps(options))
        except SparkError as error:
            if "NotFound" not in str(error):
                raise
        if wait:
            for _ in range(60):
                current = self.get_json(slot, ["clusters.postgresql.cnpg.io" if resource == "clusters" else resource, meta["name"]])
                if current is None or current["metadata"]["uid"] != meta["uid"]:
                    return
                self.sleep(3)
            raise SparkError(f"{resource}/{meta['name']} still deleting after 3 minutes; retry `down`")

    def run_manifest(self, info):
        text = render_job((DEPLOY / "test-job.yaml").read_text(), info["suite"], info["sha"], info.get("test_args", ""),
                          int(info.get("timeout_minutes", "60")), nonce=info["nonce"], fresh=info.get("fresh") == "1",
                          cpu_limit_m=int(info.get("fast_cpu_limit_m") or DEFAULT_CPU_LIMIT_M), npm_flags=NPM_FLAGS)
        for name in (SUPERUSER_SECRET, SCRIPT_CONFIGMAP, CLUSTER):
            text = text.replace(name, f"{name}-{info['nonce']}")
        metadata = {RUN_INFO: json.dumps(info), RUN_PHASE: info["phase"]}
        text = text.replace("  name: omg-test\n", f"  name: omg-test\n  annotations: {json.dumps(metadata)}\n", 1)
        if info["phase"] != "purging":
            text = text.replace("spec:\n", f"  finalizers: [{CLEANUP_FINALIZER}]\nspec:\n", 1)
        return text.replace("spec:\n", "spec:\n  suspend: true\n", 1)

    def owned_manifest(self, text, handle):
        owner = [{"apiVersion": "batch/v1", "kind": "Job", "name": JOB, "uid": handle["uid"]}]
        if text.startswith("{"):
            document = json.loads(text)
            document["metadata"]["name"] += f"-{handle['nonce']}"
            document["metadata"]["ownerReferences"] = owner
            return json.dumps(document)
        for name in (SUPERUSER_SECRET, CLUSTER):
            text = text.replace(name, f"{name}-{handle['nonce']}")
        return text.replace("metadata:\n", f"metadata:\n  ownerReferences: {json.dumps(owner)}\n", 1)

    def leases(self, allow_terminating=False):
        return {slot: self.lease(slot) for slot in self.existing_slots(allow_terminating)}

    def create_slot(self, slot):
        namespace = slot_namespace(self.base, slot)
        state = self.slot_state(slot)
        if state is not None:
            if not state["ours"]:
                raise SparkError(f"namespace {namespace} exists but does not carry {MANAGED_LABEL}={MANAGED_VALUE}: refusing to use it")
            if state["terminating"]:
                raise SparkError(f"{namespace} is being deleted (a `purge` is finishing): retry in about a minute")
        else:
            try:
                self.kubectl(["create", "-f", "-"], stdin=namespace_manifest(namespace))
            except SparkError as error:
                if "AlreadyExists" not in str(error):
                    raise
        # Permissions arrive in seconds and the isolation policies a little later: wait for both.
        for _ in range(60):
            try:
                if len(self.kubectl(["get", "networkpolicy", "-o", "name"], slot=slot, capture=True).split()) >= 2:
                    break
            except SparkError as error:
                if "Forbidden" not in str(error):
                    raise
            self.sleep(2)
        else:
            raise SparkError(f"{namespace} never received its two network policies; refusing to deploy without isolation")
        state = self.slot_state(slot)
        if not state or not state["ours"]:
            raise SparkError(f"{namespace} does not carry {MANAGED_LABEL}={MANAGED_VALUE} after creation; refusing to use it")

    def acquire(self, name, info):
        """The suspended Job is both the atomic slot reservation and the launch fence."""
        leases = self.leases()
        for slot, lease in leases.items():
            if lease and lease.get("name") == name:
                raise SparkError(f"a run named {name!r} already exists in slot {slot}; use another name or `down {name}`")
        free = [s for s in SLOTS if s in leases and not leases[s]] + [s for s in SLOTS if s not in leases]
        mine = {**info, "nonce": info.get("nonce") or secrets.token_hex(8), "name": name, "phase": "setup",
                "started": str(int(self.clock())), "started_ms": str(int(self.clock() * 1000))}
        for slot in free:
            if slot not in leases:
                self.create_slot(slot)
            state = self.slot_state(slot)
            if not state or not state["ours"] or state["terminating"]:
                continue
            mine["namespace_uid"] = state["uid"]
            try:
                # Capture UID from CREATE's response, never rebind to a name read after creation.
                out = self.kubectl(["create", "-f", "-", "-o", "json"], slot=slot, stdin=self.run_manifest(mine))
            except SparkError as error:
                if "AlreadyExists" in str(error):
                    continue
                raise
            handle = {**mine, "uid": json.loads(out)["metadata"]["uid"]}
            after = self.slot_state(slot)
            if not after or after["uid"] != handle["namespace_uid"]:
                self.remove(slot, handle)
                raise SparkError("namespace incarnation changed during admission; retry")
            self.refuse_duplicate(name, slot, handle)
            return slot, handle
        busy = "; ".join(f"{slot}: {lease_text(lease)}" for slot, lease in leases.items() if lease) or "no run is visible, but no slot could be leased"
        raise SparkError(f"all {len(SLOTS)} test slots are in use ({busy}). At most {len(SLOTS)} runs fit the shared quota next to a demo environment: "
                         "wait for one to finish and `down` it, or `down` one you no longer need")

    def refuse_duplicate(self, name, slot, mine):
        for other_slot in SLOTS:
            if other_slot == slot or self.slot_state(other_slot) is None:
                continue
            other = self.lease(other_slot)
            if other and other.get("name") == name and other.get("nonce") != mine.get("nonce"):
                theirs = (int(other.get("started_ms", "0") or 0), other.get("nonce", ""))
                ours = (int(mine["started_ms"]), mine.get("nonce", ""))
                if theirs <= ours:
                    self.remove(slot, mine)
                    raise SparkError(f"a run named {name!r} was started at the same time in slot {other_slot}; this one gave way. Use another name")

    def find(self, name, missing_ok=False, allow_terminating=False):
        validate_name(name)
        for slot, lease in self.leases(allow_terminating).items():
            if lease and lease.get("name") == name:
                return slot, lease
        if missing_ok:
            return None, None
        raise SparkError(f"no run named {name!r} (see `status`)")

    # ---- a run ----

    def fast_plan(self):
        free = quota_free(self.kubectl(["get", "resourcequota", "-o", "json"], capture=True), ["limits.cpu"]).get("limits.cpu")
        if free is None:
            raise SparkError("--fast: the quota does not report a CPU limit budget")
        limit = fast_cpu_limit(free)
        if limit is None:
            raise SparkError(f"--fast does not fit: {free}m of CPU limits are free and a normal run needs {RUN_NEEDS['limits.cpu']}m plus at least {FAST_MIN_EXTRA_M}m. "
                             "Wait for other runs or demo environments to finish, or run without --fast")
        return limit

    def start(self, name, suite, ref, test_args, timeout_minutes, fresh=False, fast=False):
        validate_name(name)
        if suite not in SUITES:
            raise SparkError(f"unknown suite {suite!r}")
        validate_test_args(test_args, suite)
        if not 1 <= timeout_minutes <= MAX_TIMEOUT_MINUTES:
            raise SparkError(f"--timeout must be 1-{MAX_TIMEOUT_MINUTES} minutes")
        resolved = spark_env.resolve_ref(ref)
        nonce = secrets.token_hex(8)
        cpu_limit = self.fast_plan() if fast else DEFAULT_CPU_LIMIT_M
        info = {"suite": suite, "ref": resolved.name, "sha": resolved.sha, "timeout_minutes": str(timeout_minutes), "nonce": nonce, "test_args": test_args or "",
                "fresh": "1" if fresh else "0", "fast_cpu_limit_m": str(cpu_limit) if fast else ""}
        slot, handle = self.acquire(name, info)
        print(f"{name}: {suite} of {resolved.name} {resolved.sha[:12]} in {slot_namespace(self.base, slot)}" + (f", CPU limit {cpu_limit}m" if fast else "")
              + (f", PARTIAL (--test-args {test_args})" if test_args else "") + (", empty target directory" if fresh else ""))
        try:
            self.check_quota(slot, cpu_limit)
            self.require_run(slot, handle)
            try:
                # Cache is shared between generations: CREATE only, never apply/patch a replacement.
                self.kubectl(["create", "-f", "-"], slot=slot, stdin=(DEPLOY / "test-cache.yaml").read_text())
            except SparkError as error:
                if "AlreadyExists" not in str(error):
                    raise
            script = json.dumps({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": SCRIPT_CONFIGMAP},
                                 "data": {name: (DEPLOY / name).read_text() for name in ("test-run.sh", "pg-forward.mjs", "test-integration.mjs", "target-inventory.mjs")}})
            for manifest in (superuser_secret(), script, (DEPLOY / "test-cluster.yaml").read_text()):
                self.require_run(slot, handle)
                self.kubectl(["create", "-f", "-"], slot=slot, stdin=self.owned_manifest(manifest, handle))
            self.patch_run(slot, handle, "setup", [
                {"op": "replace", "path": f"/metadata/annotations/{RUN_PHASE}", "value": "running"},
                {"op": "replace", "path": "/spec/suspend", "value": False}])
        except BaseException:
            print(f"setup of {name} failed; removing its generation", file=sys.stderr)
            try:
                self.remove(slot, handle)
            except SparkError as cleanup_error:
                print(f"generation cleanup stopped: {cleanup_error}; inspect `status`, retry `down {name}` for a retained run", file=sys.stderr)
            raise
        return slot, handle

    def check_quota(self, slot, cpu_limit=DEFAULT_CPU_LIMIT_M):
        needs = dict(RUN_NEEDS)
        needs["limits.cpu"] = cpu_limit + DB_CPU_LIMIT_M
        if self.get_json(slot, ["persistentvolumeclaim", CACHE_CLAIM]) is None:
            for key, value in CACHE_NEEDS.items():
                needs[key] += value
        short = quota_shortfalls(self.kubectl(["get", "resourcequota", "-o", "json"], capture=True), needs)
        if short:
            raise SparkError("the shared Spark quota cannot take another run now (" + "; ".join(short)
                             + "). Other runs or demo environments use it: `status`, `spark-env.py status`, and `down` what you no longer need")

    def pod_info(self, slot, handle=None):
        selector = f"controller-uid={handle['uid']}" if handle else f"job-name={JOB}"
        pods = self.get_json(slot, ["pods", "-l", selector])
        items = (pods or {}).get("items") or []
        if not items:
            return None, {}
        pod = items[0]
        terminated = {}
        for container in pod.get("status", {}).get("containerStatuses") or []:
            if container.get("name") == "tests":
                terminated = container.get("state", {}).get("terminated") or {}
        reason = pod.get("status", {}).get("reason") or terminated.get("reason")  # status.reason is "Evicted" for an evicted pod
        return pod, {"exit_code": terminated.get("exitCode"), "reason": reason}

    def current_step(self, slot, pod=None):
        """The step the pod is in, from the file the script keeps (the log tail may hold only compiler lines)."""
        try:
            step = self.kubectl(["exec", pod["metadata"]["name"] if pod else f"job/{JOB}", "-c", "tests", "--", "cat", "/tmp/omg-step"], slot=slot, capture=True).strip()
        except SparkError:
            return None
        return step if STEP_NAME.fullmatch(step) else None

    def job_json(self, slot):
        return self.kubectl(["get", f"job/{JOB}", "--ignore-not-found", "-o", "json"], slot=slot, capture=True).strip()

    def phase(self, slot):
        handle = self.lease(slot)
        if handle and handle.get("phase") in ("setup", "cleaning", "purging"):
            return handle["phase"]
        job = self.job_json(slot)
        if not job:
            return "preparing"
        pod, _ = self.pod_info(slot)
        return job_phase(job, pod, self.current_step(slot) if pod and pod.get("status", {}).get("phase") == "Running" else None)

    def wait(self, name, slot, lease=None, quiet=False):
        """Poll until the job finishes; prints a line when the phase changes. Ends with an error, never loops forever, never watches another run."""
        lease = lease or self.lease(slot) or {}
        seen = stuck_since = no_pod_since = None
        failures = 0
        while True:
            try:
                current = self.require_run(slot, lease)
                if current["phase"] != "running":
                    raise SparkError(f"the Job no longer belongs to a running generation of {name!r} ({current['phase']}); inspect `status` and retry `down {name}`")
                job = self.job_json(slot)
                if not job:
                    # start() creates the Job synchronously before wait() is called.
                    raise SparkError(f"the job of {name!r} vanished (deleted by `down` or by hand); run kept: `down {name}`")
                else:
                    if json.loads(job)["metadata"]["uid"] != lease["uid"]:
                        raise SparkError(f"the Job no longer belongs to {name!r}")
                    pod, _ = self.pod_info(slot, lease)
                    phase = job_phase(job, pod, self.current_step(slot, pod) if pod and pod.get("status", {}).get("phase") == "Running" else None)
                    self.require_run(slot, lease)
                failures = 0
            except SparkError as error:
                if "no longer belongs" in str(error) or "vanished" in str(error):
                    raise
                failures += 1
                if failures > TRANSIENT_ERRORS:
                    raise SparkError(f"{failures} consecutive kubectl errors while waiting: {error}") from None
                self.sleep(POLL_SECONDS)
                continue
            elapsed = self.clock() - int(lease.get("started", self.clock()))
            if phase != seen and not quiet:
                print(f"[{elapsed_text(elapsed)}] {name}: {phase}", flush=True)
                seen = phase
            if phase in ("succeeded", "failed"):
                return phase
            reason = stuck_reason(pod) if pod else None
            if reason:
                stuck_since = stuck_since or self.clock()
                if self.clock() - stuck_since > STUCK_SECONDS:
                    raise SparkError(f"the pod cannot start ({reason}); warning events: {self.job_events(slot)}; run kept: `down {name}`")
            else:
                stuck_since = None
            if phase == "starting" and pod is None and job:
                no_pod_since = no_pod_since or self.clock()
                if self.clock() - no_pod_since > 120:
                    raise SparkError(f"the job has no pod after two minutes: {self.job_events(slot)}; run kept: `down {name}`")
            else:
                no_pod_since = None
            self.sleep(POLL_SECONDS)

    def job_events(self, slot):
        try:
            text = self.kubectl(["get", "events", "--field-selector", "type=Warning", "-o", "custom-columns=REASON:.reason,MESSAGE:.message", "--no-headers"], slot=slot, capture=True)
        except SparkError:
            return "no events could be read"
        return "; ".join(sanitize(spark_env.redact(line.strip()), 200) for line in text.strip().splitlines()[-3:]) or "no warning events"

    def result(self, name, summary_path=None, log_path=None, expected=None):
        """Print and return (exit code, summary) of a finished run. A run that did not fully pass saves its sanitised log."""
        slot, lease = expected if expected else self.find(name)
        self.require_run(slot, lease)
        job = self.job_json(slot)
        if not job:
            raise SparkError(f"{name} has no job yet")
        if json.loads(job)["metadata"]["uid"] != lease["uid"]:
            raise SparkError("the result Job generation changed")
        state = spark_env.job_state(job)
        if state == "running":
            raise SparkError(f"{name} has not finished (`status {name}`, `logs {name} -f`)")
        pod_object, pod = self.pod_info(slot, lease)
        if state == "failed" and not pod.get("reason"):
            reason = next((c.get("reason") for c in json.loads(job).get("status", {}).get("conditions") or [] if c.get("type") == "Failed"), None)
            if reason:
                pod = {**pod, "reason": reason}  # DeadlineExceeded
        if not pod_object:
            raise SparkError("the run has no pod to read")
        log = self.kubectl(["logs", pod_object["metadata"]["name"], "--tail=-1"], slot=slot, capture=True)
        self.require_run(slot, lease)
        parsed = parse_log(log, lease.get("nonce", ""))
        wall = self.wall_seconds(json.loads(job), lease)
        fast = int(lease["fast_cpu_limit_m"]) if lease.get("fast_cpu_limit_m") else None
        summary = build_summary(name, lease.get("suite"), lease.get("ref"), lease.get("sha"), parsed, state, pod, wall, test_args=lease.get("test_args", ""), fresh=lease.get("fresh") == "1", fast=fast)
        if parsed["commit"] not in (None, lease.get("sha")):
            raise SparkError(f"the job ran commit {parsed['commit']}, not {lease.get('sha')}")
        saved = None
        if log_path or result_label(summary) != "passed":
            saved = Path(log_path) if log_path else LOG_DIR / f"{name}-{str(lease.get('sha', ''))[:12]}.log"
            if not log_path:
                saved.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            write_private(saved, sanitized_log(log))
        print(format_report(summary, parsed, log))
        if saved:
            print(f"log (sanitised, redacted): {saved}")
        if summary_path:
            write_summary(summary_path, summary)
        return result_exit(summary), summary

    def wall_seconds(self, job, lease):
        status = job.get("status", {})
        start = status.get("startTime")
        end = status.get("completionTime") or next((c.get("lastTransitionTime") for c in status.get("conditions") or [] if c.get("type") == "Failed"), None)
        if start and end:
            parse = lambda value: datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
            return int(parse(end) - parse(start))
        return int(self.clock() - int(lease.get("started", self.clock())))

    def run(self, name, suite, ref, test_args, timeout_minutes, keep, detach, summary_path, log_path=None, fresh=False, fast=False):
        timeout_minutes = timeout_minutes or DEFAULT_TIMEOUT_MINUTES[suite]
        check_writable(summary_path)
        check_writable(log_path)
        slot, lease = self.start(name, suite, ref, test_args, timeout_minutes, fresh=fresh, fast=fast)
        print(f"started. `spark-test.py status {name}`, `logs {name} -f`, `result {name}`, `down {name}`")
        if detach:
            print(f"detached: nothing is known about the outcome yet (exit status {EXIT_STARTED} means started); read it with `result {name}`")
            return EXIT_STARTED
        try:
            self.wait(name, slot, lease)
            code, summary = self.result(name, summary_path, log_path, expected=(slot, lease))
        except KeyboardInterrupt:
            print(f"\nstopped waiting; the run continues on the cluster. run kept: `down {name}` (`status {name}`, `result {name}`)", file=sys.stderr)
            raise
        except BaseException as error:
            if not isinstance(error, SparkError):
                print(f"unexpected {type(error).__name__} while waiting", file=sys.stderr)
            print(f"run kept: `down {name}` (`status {name}`, `logs {name}`)", file=sys.stderr)
            raise
        if keep:
            print(f"run kept: `logs {name}`, `down {name}` when done")
        else:
            try:
                self.down(name, expected=(slot, lease))
            except SparkError as error:
                print(f"run kept: `down {name}` ({error})", file=sys.stderr)
        return code

    # ---- removal ----

    def remove(self, slot, handle):
        """Fence launch first. Keep the deleting Job as a recoverable reservation until GC finishes."""
        current = self.require_run(slot, handle)
        if current["phase"] == "purging":
            raise SparkError("a purge reservation is recovered with `purge`")
        if current["phase"] != "cleaning":
            self.patch_run(slot, handle, current["phase"], [
                {"op": "replace", "path": f"/metadata/annotations/{RUN_PHASE}", "value": "cleaning"},
                {"op": "replace", "path": "/spec/suspend", "value": True}])
        job = self.get_json(slot, ["job", JOB])
        if job is None or job["metadata"]["uid"] != handle["uid"]:
            raise SparkError("the cleanup Job generation changed")
        self.delete_object(slot, "jobs", job, wait=False)
        cluster_name = f"{CLUSTER}-{handle['nonce']}"
        cluster = self.get_json(slot, ["clusters.postgresql.cnpg.io", cluster_name])
        if cluster:
            self.delete_object(slot, "clusters", cluster)
        # Only this generation's pods/volumes/secrets. All DELETEs carry the observed UID;
        # a timeout or a paused second cleaner cannot delete a replacement object.
        for _ in range(60):
            self.require_run(slot, handle)
            objects = self.get_json(slot, ["pods,persistentvolumeclaims,secrets,configmaps"])
            left = []
            for obj in (objects or {}).get("items", []):
                meta = obj["metadata"]
                owners = meta.get("ownerReferences", [])
                owned = any(owner["uid"] == handle["uid"] for owner in owners)
                database = meta.get("labels", {}).get("cnpg.io/cluster") == cluster_name
                if not (owned or database):
                    continue
                resource = {"Pod": "pods", "PersistentVolumeClaim": "persistentvolumeclaims", "Secret": "secrets", "ConfigMap": "configmaps"}[obj["kind"]]
                self.delete_object(slot, resource, obj, wait=False)
                left.append(meta["name"])
            if not left:
                break
            self.sleep(3)
        else:
            raise SparkError(f"slot {slot} still has {', '.join(left)} after 3 minutes; the lease is kept: retry `down`")
        # Remove only our finalizer, preserving any controller's foreground-deletion barrier.
        current_job = self.get_json(slot, ["job", JOB])
        if current_job is None or current_job["metadata"]["uid"] != handle["uid"]:
            raise SparkError("the cleanup Job generation changed")
        finalizers = current_job["metadata"].get("finalizers", [])
        if CLEANUP_FINALIZER in finalizers:
            patch = [{"op": "test", "path": "/metadata/resourceVersion", "value": current_job["metadata"]["resourceVersion"]},
                     {"op": "replace", "path": "/metadata/finalizers", "value": [f for f in finalizers if f != CLEANUP_FINALIZER]}]
            self.patch_run(slot, handle, "cleaning", patch)

    def down(self, name, expected=None):
        slot, handle = expected if expected else self.find(name, missing_ok=True, allow_terminating=True)
        if slot is None:
            print(f"no run named {name!r}: nothing to remove")
            return
        self.remove(slot, handle)
        print(f"removed {name} from slot {slot}; foreground deletion may still be finishing.")

    def purge(self):
        states = {slot: self.slot_state(slot) for slot in SLOTS}
        existing = {slot: state for slot, state in states.items() if state}
        for slot, state in existing.items():
            if not state["ours"]:
                raise SparkError(f"namespace {slot_namespace(self.base, slot)} exists but does not carry {MANAGED_LABEL}={MANAGED_VALUE}: it is not ours, nothing was deleted")
        if not existing:
            print("no slot namespaces")
            return
        for slot, state in existing.items():
            if state["terminating"]:
                print(f"{slot_namespace(self.base, slot)} is already being deleted")
                continue
            lease = self.lease(slot)
            if not lease:
                info = {"name": "purge", "phase": "purging", "suite": "checks", "sha": "0" * 40, "nonce": secrets.token_hex(8), "namespace_uid": state["uid"]}
                try:
                    out = self.kubectl(["create", "-f", "-", "-o", "json"], slot=slot, stdin=self.run_manifest(info))
                except SparkError as error:
                    if "AlreadyExists" not in str(error):
                        raise
                    print(f"slot {slot} acquired concurrently; not purged")
                    continue
                lease = {**info, "uid": json.loads(out)["metadata"]["uid"]}
            if lease.get("phase") != "purging":
                print(f"slot {slot} is in use by {lease_text(lease)}; not purged")
                continue
            current_state = self.slot_state(slot)
            if not current_state or current_state["uid"] != lease["namespace_uid"]:
                # A delayed CREATE may have reached a recreated namespace. Remove only that
                # obsolete sentinel, so retrying purge/admission is possible in the new namespace.
                sentinel = self.get_json(slot, ["job", JOB])
                if sentinel and sentinel["metadata"]["uid"] == lease["uid"]:
                    self.delete_object(slot, "jobs", sentinel)
                print(f"slot {slot} namespace incarnation changed; obsolete purge reservation removed; retry")
                continue
            if not current_state["ours"] or current_state["terminating"]:
                raise SparkError(f"slot {slot} is no longer an active managed namespace; retry purge")
            # The reservation stays until namespace deletion, including interrupted purge recovery.
            self.delete_object(None, "namespaces", {"metadata": {"name": slot_namespace(self.base, slot), "uid": lease["namespace_uid"]}}, wait=False)
            print(f"deleted {slot_namespace(self.base, slot)} and its cache volume")

    # ---- reading ----

    def status(self, name=None):
        if name:
            slot, lease = self.find(name)
            print(f"{name}: {lease_text(lease)} slot {slot} {self.phase(slot)} elapsed {elapsed_text(self.clock() - int(lease.get('started', self.clock())))}")
            print(self.kubectl(["get", "pods,jobs,clusters.postgresql.cnpg.io"], slot=slot, capture=True), end="")
            return
        shown = False
        for slot in SLOTS:
            state = self.slot_state(slot)
            if state is None:
                continue
            shown = True
            if not state["ours"]:
                print(f"{slot}: namespace exists but is NOT labelled {MANAGED_LABEL}={MANAGED_VALUE}; not used")
            elif state["terminating"]:
                print(f"{slot}: being deleted")
            else:
                lease = self.lease(slot)
                print(f"{slot}: {lease_text(lease)} {self.phase(slot)} elapsed {elapsed_text(self.clock() - int(lease.get('started', self.clock())))}" if lease else f"{slot}: free")
        if not shown:
            print("no slot namespaces (the first run creates them)")

    def logs(self, name, follow):
        slot, handle = self.find(name)
        pod, _ = self.pod_info(slot, handle)
        if follow and not pod:
            for _ in range(20):
                self.sleep(1)
                self.require_run(slot, handle)
                pod, _ = self.pod_info(slot, handle)
                if pod:
                    break
        if not pod:
            raise SparkError(f"{name} has no pod yet")
        self.require_run(slot, handle)
        command = kubectl_command(self.config, self.base, ["logs", pod["metadata"]["name"], *(["-f"] if follow else ["--tail=-1"])], slot_namespace(self.base, slot))
        try:
            code = self.launcher(command).returncode
        except KeyboardInterrupt:
            return
        if code:
            raise SparkError(f"kubectl logs exited with status {code}")


NPM_FLAGS = "--ignore-scripts"  # set from the measurement in docs/verification.md: "--ignore-scripts" when the web and Rust suites pass with it


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="start a run and wait for the result")
    run.add_argument("name", help="2-20 characters: lowercase letters, digits, dashes")
    run.add_argument("--ref", required=True, help="branch, commit or tag of this clone; it must already be pushed")
    run.add_argument("--suite", required=True, choices=SUITES)
    run.add_argument("--test-args", default="", help="extra libtest (lib) or vitest (web) arguments, for example a test filter; the run is then reported as partial")
    run.add_argument("--timeout", type=int, default=None, metavar="MINUTES", help=f"deadline of the job (default by suite {DEFAULT_TIMEOUT_MINUTES}, at most {MAX_TIMEOUT_MINUTES})")
    run.add_argument("--summary", metavar="PATH", help="write the compact JSON summary to PATH (checked before the run starts)")
    run.add_argument("--log", metavar="PATH", help="write the sanitised, redacted log to PATH (0600); a run that did not fully pass saves it to .local/spark-test/ without this")
    run.add_argument("--fresh", action="store_true", help="start with an empty target directory (the cache's target/ cannot be verified)")
    run.add_argument("--fast", action="store_true", help="raise the CPU limit (not the request) as far as the free limits quota allows, for one urgent run")
    run.add_argument("--keep", action="store_true", help="do not `down` the run after printing its result")
    run.add_argument("--detach", action="store_true", help=f"start the run and return with exit status {EXIT_STARTED}; read it with status, logs and result")
    status = commands.add_parser("status", help="the slots and their runs, or one run")
    status.add_argument("name", nargs="?")
    logs = commands.add_parser("logs", help="the log of a run")
    logs.add_argument("name")
    logs.add_argument("-f", "--follow", action="store_true")
    result = commands.add_parser("result", help="the result of a finished run")
    result.add_argument("name")
    result.add_argument("--summary", metavar="PATH")
    result.add_argument("--log", metavar="PATH")
    commands.add_parser("down", help="delete a run and its database; the cache stays (an unknown run is not an error)").add_argument("name")
    commands.add_parser("purge", help="delete the idle slot namespaces and their cache volumes")
    args = parser.parse_args(argv)
    try:
        spark = SparkTest(spark_env.kubeconfig_path(os.environ))
        spark_env.require_tools(("kubectl", "git") if args.command == "run" else ("kubectl",))
        code = 0
        if args.command == "run":
            code = spark.run(args.name, args.suite, args.ref, args.test_args, args.timeout, args.keep, args.detach, args.summary, log_path=args.log, fresh=args.fresh, fast=args.fast)
        elif args.command == "result":
            check_writable(args.summary)
            check_writable(args.log)
            code, _ = spark.result(args.name, args.summary, args.log)
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
        raise SystemExit(EXIT_ERROR) from None
    except Exception as error:  # a bug of the tool must not look like a test failure, and prints no traceback with environment details
        print(f"internal error ({type(error).__name__}): {sanitize(str(error), 200)}. A started run is kept: `status`, `down <name>`", file=sys.stderr)
        raise SystemExit(EXIT_ERROR) from None


if __name__ == "__main__":
    main()

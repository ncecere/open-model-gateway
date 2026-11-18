"""deploy/spark/test-run.sh run for real with stub cargo, npm and rustup, a real temporary git repository
and a real cache directory. No cluster, registry or network access."""
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "deploy" / "spark" / "test-run.sh"
JOB = ROOT / "deploy" / "spark" / "test-job.yaml"
spec = importlib.util.spec_from_file_location("spark_test", ROOT / "scripts" / "spark-test.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

# git sets GIT_DIR and GIT_INDEX_FILE when it runs a hook, and `git -C folder` does not
# override them: the temporary repository below would be bypassed.
for _name in [name for name in os.environ if name.startswith("GIT_")]:
    del os.environ[_name]

NONCE = "0123456789abcdef"
PASSWORD = "s3cr3t-p4ss/word&x"  # characters that are special to sed

CARGO = r"""#!/usr/bin/env bash
if [ "${1:-}" = --color ]; then shift 2; fi
echo "cargo $*" >> "$STUB_LOG"
pw=$(cat "$STUB_PASSWORD_FILE")
echo "env cargo db=${DATABASE_URL:+set} vars=$(env | grep -F -- "$pw" | cut -d= -f1 | tr '\n' ',') nonce=$(env | grep -c '^OMG_NONCE=')" >> "$STUB_LOG"
args="$*"
if [[ "$1" == test && " $args " != *" --no-run "* && " $args " != *" --list "* ]]; then
  if [[ "${DATABASE_URL:-}" != "postgres://postgres:${pw}@127.0.0.1:5432/gateway" ]]; then
    echo "gateway fixtures require the run's database through the literal loopback relay"
    exit 101
  fi
fi
case "$1" in
  metadata) cat "$STUB_METADATA"; exit 0 ;;
esac
case "$args" in
  *"--test runtime_privileges"*)
    probes=(enterprise_runtime_allowlist_and_rollback_probes controlled_lock_stall_recovers_on_runtime_http_path)
    if [ "${STUB_MISSING_PROBE:-}" = 1 ]; then probes=("${probes[0]}"); fi
    n=${#probes[@]}
    if [[ "$args" == *"--list"* ]]; then
      printf '%s: test\n' "${probes[@]}"
    else
      if [[ "$args" == *"--ignored"* && "${STUB_EMPTY_RUNTIME_RUN:-}" = 1 ]]; then n=0; fi
      passed=0 ignored=$n
      if [[ "$args" == *"--ignored"* ]]; then passed=$n ignored=0; fi
      echo "running $n tests"
      echo "test result: ok. $passed passed; 0 failed; $ignored ignored; 0 measured; 0 filtered out; finished in 0.01s"
    fi
    exit 0 ;;
  *"--message-format=json"*)
    "$REAL_NODE" -e 'const m = require(process.env.STUB_METADATA); for (const p of m.packages) for (const t of p.targets) if (t.test && t.name !== process.env.STUB_MISSING_ARTIFACT) console.log(JSON.stringify({reason:"compiler-artifact", package_id:p.id, target:t, profile:{test:true}, executable:"/fake/"+t.name})); console.log(JSON.stringify({reason:"build-finished", success:true}));'
    exit 0 ;;
  *"--list"*)
    if [[ "$args" == *"--lib"* ]] && [[ "$args" == *"-p open-model-gateway"* ]]; then cat "$STUB_LISTING"; exit 0; fi
    [[ "$args" == *"--bin"* ]] && n=0 || n=1
    [[ "$args" == *"--ignored"* ]] && n=0
    [ "$n" = 1 ] && echo "fixture::test: test"
    [[ "$args" == *"--format pretty"* ]] && printf "\n%s tests, 0 benchmarks\n" "$n"
    exit 0 ;;
  *"--no-run"*) exit 0 ;;
  *"-- --exact "*)
    names="${args#*-- --exact }"
    n=$(wc -w <<<"$names")
    if [ -n "${STUB_FAIL_NAME:-}" ] && [[ " $names " == *" $STUB_FAIL_NAME "* ]]; then
      echo "test $STUB_FAIL_NAME ... FAILED"
      echo "test result: FAILED. $((n - 1)) passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
      exit 101
    fi
    echo "running $n tests"
    echo "test result: ok. $n passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
    exit 0 ;;
  *"--bin"*) echo "running 0 tests"; echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"; exit 0 ;;
  *)
    echo "running 1 tests"
    echo "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
    [ -n "${STUB_SLEEP:-}" ] && sleep "$STUB_SLEEP"
    exit 0 ;;
esac
"""

NPM = r"""#!/usr/bin/env bash
echo "npm $*" >> "$STUB_LOG"
pw=$(cat "$STUB_PASSWORD_FILE")
echo "env npm db=${DATABASE_URL:+set} vars=$(env | grep -F -- "$pw" | cut -d= -f1 | tr '\n' ',') nonce=$(env | grep -c '^OMG_NONCE=')" >> "$STUB_LOG"
case "$*" in
  *test:web* | *"run test --workspace @omg/web"*) echo "      Tests  5 passed (5)" ;;
esac
exit 0
"""

RUSTUP = r"""#!/usr/bin/env bash
echo "rustup $*" >> "$STUB_LOG"
exit 0
"""

RUSTC = r"""#!/usr/bin/env bash
echo "rustc 1.99.1 (stub)"
"""

NODE = r"""#!/usr/bin/env bash
case "$1" in
  */pg-forward.mjs) exec sleep 30 ;;
  *) exec "$REAL_NODE" "$@" ;;
esac
"""


def metadata(tests):
    def target(kind, name):
        return {"kind": [kind], "name": name, "test": True}
    packages = [
        {"id": "open-model-gateway", "name": "open-model-gateway", "targets": [target("test", name) for name in tests] + [target("lib", "open_model_gateway"), target("bin", "empty_gateway")]},
        {"id": "fixture-tools", "name": "fixture-tools", "targets": [target("lib", "fixture_tools"), target("bin", "empty_tool")]},
    ]
    return json.dumps({"workspace_members": [p["id"] for p in packages], "packages": packages})


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@x"})


class Listener:
    """Stands in for the PostgreSQL service: wait_db only needs the port to accept a connection."""

    def __init__(self):
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(20)
        self.port = self.sock.getsockname()[1]
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                conn, _ = self.sock.accept()
                conn.close()
            except OSError:
                return

    def close(self):
        self.sock.close()


class Pod(unittest.TestCase):
    """A fake pod filesystem: cache, secrets, scripts, image toolchain, stub binaries and a git repository to fetch from."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.root = root
        self.cache, self.secrets, self.scripts, self.tmpdir = root / "cache", root / "secrets", root / "scripts", root / "tmp"
        self.noderoot, self.image_rustup, self.repo = root / "noderoot", root / "image-rustup", root / "repo"
        for folder in (self.cache, self.secrets, self.scripts, self.tmpdir, self.noderoot / "bin", self.image_rustup / "toolchains" / "1.99.1" / "bin"):
            folder.mkdir(parents=True)
        (self.secrets / "password").write_text(PASSWORD)
        (self.image_rustup / "toolchains" / "1.99.1" / "bin" / "rustc").write_text("image rustc")
        for name in ("pg-forward.mjs", "test-integration.mjs"):
            shutil.copy(ROOT / "deploy" / "spark" / name, self.scripts / name)
        for name, body in (("cargo", CARGO), ("npm", NPM), ("rustup", RUSTUP), ("rustc", RUSTC), ("node", NODE)):
            path = self.noderoot / "bin" / name
            path.write_text(body)
            path.chmod(0o755)
        self.log = root / "stub.log"
        self.listing = root / "listing.txt"
        self.metadata = root / "metadata.json"
        self.metadata.write_text(metadata(["alpha", "beta", "filestore_s3", "runtime_privileges"]))
        self.names = [f"mod_{c}::test_{i}" for i, c in enumerate("bAcB_aC") for i in range(3)] + ["zeta::t", "alpha::t", "Alpha::t", "a_b::t", "aB::t"]
        self.names = sorted(set(self.names), key=lambda s: s[::-1])  # an order that is not the sorted one
        self.write_listing(self.names)
        self.db = Listener()
        self.addCleanup(self.db.close)
        self.addCleanup(self.tmp.cleanup)
        self.commit = self.make_repo()

    def write_listing(self, names):
        self.listing.write_text("".join(f"{n}: test\n" for n in names) + "bench_one: benchmark\n")

    def make_repo(self, files=None, message="one"):
        if not (self.repo / ".git").exists():
            self.repo.mkdir(exist_ok=True)
            git("init", "-q", "-b", "main", cwd=self.repo)
            git("config", "uploadpack.allowAnySHA1InWant", "true", cwd=self.repo)
        files = files or {"rust-toolchain.toml": '[toolchain]\nchannel = "1.99.1"\ncomponents = ["rustfmt", "clippy"]\n', "Cargo.toml": "[workspace]\n", "package.json": "{}\n", "apps/gateway/src/lib.rs": "// lib\n", "README.md": "readme\n", ".github/scripts/rust-shards.mjs": (ROOT / ".github/scripts/rust-shards.mjs").read_text()}
        for name, text in files.items():
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        git("add", "-A", cwd=self.repo)
        git("commit", "-q", "-m", message, cwd=self.repo)
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.repo, check=True, capture_output=True, text=True).stdout.strip()

    def env(self, **extra):
        env = {
            "PATH": f"{self.noderoot / 'bin'}:/usr/bin:/bin", "HOME": str(self.root / "realhome"), "REAL_NODE": shutil.which("node") or "node",
            "OMG_CACHE": str(self.cache), "OMG_SECRETS": str(self.secrets), "OMG_SCRIPTS": str(self.scripts), "OMG_TMP": str(self.tmpdir),
            "OMG_NODE_ROOT": str(self.noderoot), "OMG_IMAGE_RUSTUP": str(self.image_rustup), "OMG_DB_PORT": str(self.db.port), "OMG_DB_HOST": "127.0.0.1",
            "OMG_DB_WAIT_SLEEP": "0.1", "OMG_SUITE": "lib", "OMG_SHA": self.commit, "OMG_REPO_URL": f"file://{self.repo}", "OMG_NONCE": NONCE,
            "OMG_JOBS": "2", "OMG_TEST_THREADS": "4", "OMG_WEB_WORKERS": "1", "OMG_TEST_ARGS": "", "OMG_TARGET_LIMIT_GIB": "12", "OMG_CACHE_LIMIT_GIB": "17",
            "STUB_LOG": str(self.log), "STUB_LISTING": str(self.listing), "STUB_METADATA": str(self.metadata), "STUB_PASSWORD_FILE": str(self.secrets / "password"),
        }
        env.update({k: str(v) for k, v in extra.items()})
        return env

    def run_script(self, timeout=60, **extra):
        return subprocess.run(["bash", str(SCRIPT)], env=self.env(**extra), capture_output=True, text=True, timeout=timeout, start_new_session=True)

    def call_functions(self, body, **extra):
        """Source the script (without running main) and run shell code that calls its functions."""
        program = f'source "{SCRIPT}"\n' + body
        return subprocess.run(["bash", "-c", program], env=self.env(OMG_SOURCE_ONLY=1, **extra), capture_output=True, text=True, timeout=60, start_new_session=True)

    def stub_calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []


class Shards(Pod):
    def test_the_split_covers_every_listed_test_exactly_once_in_cis_order(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        ordered = sorted(self.names)  # LC_ALL=C order is the code point order, as rust-shards.mjs compares strings
        exact = [line for line in self.stub_calls() if line.startswith("cargo") and "-- --exact" in line]
        self.assertEqual(len(exact), 4)
        seen = []
        for index, line in enumerate(exact):
            names = line.split("-- --exact ", 1)[1].split()
            self.assertEqual(names, ordered[index::4], f"shard {index}: element i goes to shard i % count, as in CI")
            seen.extend(names)
        self.assertEqual(sorted(seen), ordered)
        self.assertEqual(len(seen), len(set(seen)))
        self.assertNotIn("bench_one", seen)
        summary = self.summary(result.stdout, "lib")
        self.assertEqual(summary["result"], "failed" if summary["suites"]["lib"]["passed"] == 0 else summary["result"])
        self.assertEqual(summary["lib_tests_listed"], len(ordered))

    def summary(self, stdout, suite):
        parsed = tool.parse_log(stdout, NONCE)
        return tool.build_summary("t", suite, "main", self.commit, parsed, "complete", {}, 1)

    def test_a_failing_shard_does_not_stop_the_later_shards_and_the_exit_status_is_one(self):
        victim = sorted(self.names)[1]  # shard 1
        result = self.run_script(OMG_SUITE="all", STUB_FAIL_NAME=victim)
        self.assertEqual(result.returncode, 1, result.stdout[-2000:])
        parsed = tool.parse_log(result.stdout, NONCE)
        by_name = {(s["suite"], s["name"]): s["exit"] for s in parsed["steps"]}
        self.assertEqual(by_name[("lib", "shard-0")], 0)
        self.assertNotEqual(by_name[("lib", "shard-1")], 0)
        self.assertEqual((by_name[("lib", "shard-2")], by_name[("lib", "shard-3")]), (0, 0), "later shards still ran")
        self.assertIn(("integration", "targets"), by_name, "later suites still ran")
        self.assertEqual(parsed["done"]["exit"], 1)
        self.assertIn(victim, parsed["failed_tests"])
        summary = tool.build_summary("t", "all", "main", self.commit, parsed, "failed", {}, 1)
        self.assertEqual((summary["result"], tool.result_exit(summary)), ("failed", 1))

    def test_a_shard_whose_counts_do_not_add_up_fails(self):
        # a cargo that prints fewer results than the tests selected: the script's own accounting fails the shard
        stub = self.noderoot / "bin" / "cargo"
        stub.write_text(stub.read_text().replace('echo "test result: ok. $n passed;', 'echo "test result: ok. $((n - 1)) passed;'))
        result = self.run_script()
        self.assertEqual(result.returncode, 1)
        self.assertIn("are not the", result.stdout)

    def test_an_empty_listing_is_an_error(self):
        self.listing.write_text("")
        result = self.run_script()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no lib tests listed", result.stdout)

    def test_a_failed_listing_with_partial_output_never_runs_a_partial_inventory(self):
        stub = self.noderoot / "bin" / "cargo"
        stub.write_text(stub.read_text().replace('cat "$STUB_LISTING"; exit 0', 'head -n 8 "$STUB_LISTING"; exit 1'))
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("cannot list the gateway library tests", result.stdout)
        self.assertFalse([c for c in self.stub_calls() if "-- --exact " in c])

    def test_all_suites_share_one_successful_npm_installation(self):
        result = self.run_script(OMG_SUITE="all", timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout[-3000:])
        summary = self.summary(result.stdout, "all")
        self.assertEqual((summary["result"], tool.result_exit(summary)), ("passed", 0), summary)
        self.assertEqual(self.stub_calls().count("npm ci"), 1)

    def test_a_filter_runs_unsharded(self):
        result = self.run_script(OMG_TEST_ARGS="--ignored some_test")
        self.assertEqual(result.returncode, 0, result.stdout)
        calls = [c for c in self.stub_calls() if c.startswith("cargo test") and "--lib" in c and "some_test" in c]
        self.assertEqual(len(calls), 1)
        self.assertNotIn("--exact", calls[0])

    def test_arguments_that_do_not_run_tests_are_refused_by_the_script_too(self):
        for argument in ("--list", "--help", "--format=json", "--shuffle"):
            result = self.run_script(OMG_TEST_ARGS=argument)
            self.assertEqual(result.returncode, 1, argument)
            self.assertIn("refused argument", result.stdout)
        self.assertFalse([c for c in self.stub_calls() if "--help" in c or "--format=json" in c])


class Integration(Pod):
    def test_bulk_test_compilation_is_serial_even_with_more_slot_jobs(self):
        for jobs in ("2", "6"):
            with self.subTest(jobs=jobs):
                self.log.unlink(missing_ok=True)
                result = self.run_script(OMG_SUITE="integration", OMG_JOBS=jobs, timeout=120)
                self.assertEqual(result.returncode, 0, result.stdout[-3000:])
                builds = [c for c in self.stub_calls() if "--message-format=json" in c]
                self.assertEqual(len(builds), 2, "both target groups retain complete workspace discovery")
                for build in builds:
                    self.assertIn("--jobs 1", build)
                    self.assertIn("--workspace --all-features --no-run", build)

    def test_runtime_probe_must_be_present_and_ignored(self):
        result = self.run_script(OMG_SUITE="integration", STUB_MISSING_PROBE="1", timeout=120)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing ignored runtime probe: controlled_lock_stall_recovers_on_runtime_http_path", result.stdout)


    def test_runtime_ignored_run_must_execute_the_listed_probes(self):
        result = self.run_script(OMG_SUITE="integration", STUB_EMPTY_RUNTIME_RUN="1", timeout=120)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Libtest execution counts do not match", result.stdout)

    def test_a_missing_named_cargo_artifact_fails_before_any_tests_run(self):
        result = self.run_script(OMG_SUITE="integration", STUB_MISSING_ARTIFACT="alpha", timeout=120)
        self.assertNotEqual(result.returncode, 0, result.stdout[-3000:])
        self.assertIn("Missing Cargo test executables", result.stdout)
        self.assertFalse([c for c in self.stub_calls() if c.startswith("cargo test") and "--no-run" not in c])

    def test_a_missing_named_integration_target_cannot_report_a_full_pass(self):
        stub = self.noderoot / "bin" / "cargo"
        missing = r'''
if [[ "$args" == *"--test alpha"* ]] && [[ "$args" != *"--list"* ]] && [[ "$args" != *"--no-run"* ]]; then
  echo "running 0 tests"
  echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"
  exit 0
fi
'''
        stub.write_text(stub.read_text().replace('args="$*"\n', 'args="$*"\n' + missing))
        result = self.run_script(OMG_SUITE="integration", timeout=120)
        self.assertNotEqual(result.returncode, 0, result.stdout[-3000:])
        parsed = tool.parse_log(result.stdout, NONCE)
        summary = tool.build_summary("t", "integration", "main", self.commit, parsed, "failed", {}, 1)
        self.assertEqual((summary["result"], tool.result_exit(summary)), ("failed", 1))

    def test_workspace_datetime_tests_receive_the_disposable_loopback_database(self):
        stub = self.noderoot / "bin" / "cargo"
        guard = r'''
if [[ "$args" == *"-p fixture-tools"* ]] || [[ "$args" == *"--workspace --exclude open-model-gateway"* ]]; then
  if [ "${DATETIME_PG_URL:-}" != "${DATABASE_URL:-}" ] || [[ "$DATETIME_PG_URL" != *@127.0.0.1:5432/gateway ]]; then
    echo "datetime workspace tests require the run's explicit loopback database"
    exit 101
  fi
elif [[ "$args" != *"metadata"* && "$args" != *"--no-run"* ]] && [ -n "${DATETIME_PG_URL:-}" ]; then
  echo "datetime database credential leaked outside the workspace step"
  exit 102
fi
'''
        stub.write_text(stub.read_text().replace('args="$*"\n', 'args="$*"\n' + guard))
        result = self.run_script(OMG_SUITE="all", timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout[-3000:])
        self.assertNotIn(PASSWORD, result.stdout + result.stderr)

    def test_an_empty_integration_selection_is_an_error(self):
        self.metadata.write_text(metadata(["filestore_s3", "runtime_privileges"]))
        result = self.run_script(OMG_SUITE="integration")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no gateway integration test targets selected", result.stdout)
        self.assertFalse([c for c in self.stub_calls() if c.startswith("cargo test") and "--no-run" not in c], "no test ran")

    def test_a_missing_exact_integration_probe_fails_even_when_other_tests_pass(self):
        stub = self.noderoot / "bin" / "cargo"
        original = stub.read_text()
        for probe in ("key_issuance_contention_measurement", "terminal_fidelity_official_sdks_and_priced_http_evidence"):
            with self.subTest(probe=probe):
                zero = f'if [[ "$args" == *"{probe}"* ]]; then echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 2491 filtered out; finished in 0.00s"; exit 0; fi\n'
                stub.write_text(original.replace('args="$*"\n', 'args="$*"\n' + zero))
                result = self.run_script(OMG_SUITE="integration", timeout=120)
                self.assertNotEqual(result.returncode, 0, result.stdout[-3000:])
                self.assertIn("expected exactly one passing probe", result.stdout)
                parsed = tool.parse_log(result.stdout, NONCE)
                summary = tool.build_summary("t", "integration", "main", self.commit, parsed, "failed", {}, 1)
                self.assertEqual((summary["result"], tool.result_exit(summary)), ("failed", 1))

    def test_the_targets_exclude_the_two_that_have_their_own_steps_and_the_commands_are_those_of_ci(self):
        result = self.run_script(OMG_SUITE="integration", timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout[-3000:])
        calls = [c for c in self.stub_calls() if c.startswith("cargo test")]
        self.assertIn("cargo test -p open-model-gateway --all-features --test alpha -- --exact", calls)
        self.assertIn("cargo test -p open-model-gateway --all-features --test beta -- --exact", calls)
        self.assertIn("cargo test -p open-model-gateway --all-features --bin empty_gateway -- --exact", calls)
        self.assertNotIn("filestore_s3 -- --exact", " ".join(calls))
        self.assertEqual(sum(1 for c in calls if c.endswith("--doc")), 2, "one gateway and one non-gateway doctest run")
        self.assertIn("cargo test -p open-model-gateway --all-features --doc", calls)
        self.assertIn("cargo test --workspace --exclude open-model-gateway --all-features --doc", calls)
        self.assertIn("running 0 tests", result.stdout, "legitimate empty binary targets are exercised")
        parsed = tool.parse_log(result.stdout, NONCE)
        self.assertEqual({s["name"] for s in parsed["steps"] if s["suite"] == "integration"}, set(tool.REQUIRED_STEPS["integration"]))


class CacheDiagnostics(Pod):
    def test_existing_cache_measurement_failure_stops_pruning(self):
        result = self.call_functions('mkdir -p "$OMG_CACHE/npm"; du() { return 1; }; prune_cache')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("cache total_mib=", result.stdout)


    def test_populated_cache_and_target_pruning(self):
        result = self.call_functions(
            'mkdir -p "$OMG_CACHE/target/debug/incremental" "$SRC"; '
            'dd if=/dev/zero of="$OMG_CACHE/target/debug/incremental/test" bs=1M count=2 status=none; '
            'prune_cache && test ! -e "$OMG_CACHE/target/debug/incremental"', OMG_TARGET_LIMIT_GIB="0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("removing incremental data", result.stdout)
        self.assertIn("target_mib=0", result.stdout)

    def test_missing_cache_subdirectories_report_zero_without_integer_errors(self):
        result = self.call_functions('mkdir -p "$OMG_CACHE/target" "$OMG_CACHE/npm" "$SRC"; prune_cache')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertRegex(result.stdout, r"cache total_mib=\d+ target_mib=0 crates_mib=0 npm_mib=0 src_mib=0")


class WebArguments(Pod):
    def test_real_npm_delivers_the_worker_limit_and_filter_to_vitest(self):
        # Exercise npm's script boundary, not the stub's argv echo: nested npm
        # wrappers can consume option flags before the workspace test sees them.
        real_npm = shutil.which("npm")
        self.assertIsNotNone(real_npm)
        root_script = json.loads((ROOT / "package.json").read_text())["scripts"]["test:web"]
        self.commit = self.make_repo({
            "package.json": json.dumps({"private": True, "workspaces": ["web"], "scripts": {"test:web": root_script, "typecheck:web": "node -e ''"}}),
            "web/package.json": json.dumps({"name": "@omg/web", "scripts": {"test": "node args.cjs"}}),
            "web/args.cjs": 'require("fs").writeFileSync(process.env.STUB_LOG + "-web-args", JSON.stringify(process.argv.slice(2))); console.log("Tests  5 passed (5)");',
        }, message="web scripts")
        npm = self.noderoot / "bin" / "npm"
        npm.unlink()
        npm.symlink_to(real_npm)
        result = self.call_functions('setup_environment >/dev/null; reset_cache && prepare_source && { NPM_READY=ok; suite_web; }', OMG_TEST_ARGS="deployment-preview-focus")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        args = json.loads(Path(str(self.log) + "-web-args").read_text())
        self.assertEqual(args, ["--maxWorkers=1", "deployment-preview-focus"])


class Secrets(Pod):
    def test_the_password_is_in_no_environment_but_the_database_steps(self):
        result = self.run_script(OMG_SUITE="all", timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout[-3000:])
        records = [line for line in self.stub_calls() if line.startswith("env ")]
        self.assertTrue(records)
        for record in records:
            self.assertEqual(record.count("nonce=0"), 1, f"the nonce is not exported to {record}")
        calls = self.stub_calls()
        for index, line in enumerate(calls):
            if not line.startswith("env "):
                continue
            command = calls[index - 1]
            if "db=set" in line:
                variables = "DATABASE_URL,DATETIME_PG_URL," if ("test --workspace --exclude open-model-gateway" in command or "-p fixture-tools" in command or "metadata" in command or "--no-run --message-format" in command) and "DATETIME_PG_URL," in line else "DATABASE_URL,"
                self.assertIn(f"vars={variables}", line, command)
                self.assertTrue(command.startswith(("cargo test", "cargo metadata")), command)
            else:
                self.assertIn("vars= ", line + " ", command)
                self.assertTrue(re.search(r"vars=\s*nonce", line), f"no variable holds the password for: {command}")
        for name in ("npm ci", "cargo fmt", "cargo clippy", "cargo build", "cargo test -p open-model-gateway --all-features --lib --no-run"):
            matching = [i for i, c in enumerate(calls) if c.startswith(name)]
            for i in matching:
                self.assertIn("db= ", calls[i + 1] + " ", f"{calls[i]} has no database URL")
        self.assertNotIn(PASSWORD, result.stdout + result.stderr)

    def test_redact_removes_the_password_and_urls_with_passwords(self):
        lines = [f"connect postgres://postgres:{PASSWORD}@db:5432/gateway failed", f"raw {PASSWORD} here", "postgresql://someone:anything@host/x", "nothing to see"]
        result = subprocess.run(["bash", "-c", f'source "{SCRIPT}"\nredact'], input="\n".join(lines) + "\n", env=self.env(OMG_SOURCE_ONLY=1), capture_output=True, text=True, timeout=30)
        self.assertNotIn(PASSWORD, result.stdout)
        self.assertNotIn("anything", result.stdout)
        self.assertIn("postgres://postgres:[redacted]@db:5432/gateway failed", result.stdout)
        self.assertIn("raw [redacted] here", result.stdout)
        self.assertIn("nothing to see", result.stdout)

    def test_the_nonce_is_not_exported_and_markers_carry_it(self):
        result = self.call_functions('mark hello a=b; env | grep -c "^OMG_NONCE=" || true')
        self.assertEqual(result.stdout.splitlines(), [f"::omg-test::{NONCE}:: hello a=b", "0"])


class Source(Pod):
    def plant(self):
        """Everything an earlier run (of any ref) could have left in the cache."""
        cache = self.cache
        (cache / "cargo" / "registry" / "cache" / "index.crates.io-x").mkdir(parents=True)
        (cache / "cargo" / "registry" / "cache" / "index.crates.io-x" / "serde-1.0.crate").write_text("crate")
        (cache / "cargo" / "git" / "db" / "dep").mkdir(parents=True)
        (cache / "cargo" / "git" / "db" / "dep" / "HEAD").write_text("db")
        (cache / "npm" / "_cacache").mkdir(parents=True)
        (cache / "npm" / "_cacache" / "entry").write_text("npm")
        (cache / "target" / "debug").mkdir(parents=True)
        (cache / "target" / "debug" / "artifact").write_text("built")
        (cache / "cargo" / "config.toml").write_text('[build]\nrustc-wrapper = "/cache/evil"\n')
        (cache / "cargo" / "config").write_text("legacy")
        (cache / "cargo" / "credentials.toml").write_text("token")
        (cache / "cargo" / "bin").mkdir(parents=True)
        (cache / "cargo" / "bin" / "cargo-evil").write_text("evil")
        (cache / "cargo" / "registry" / "src" / "x").mkdir(parents=True)
        (cache / "cargo" / "registry" / "src" / "x" / "build.rs").write_text("planted build script")
        (cache / "cargo" / "registry" / "index" / "i").mkdir(parents=True)
        (cache / "cargo" / "registry" / "index" / "i" / ".cache").write_text("index")
        (cache / "home").mkdir()
        (cache / "home" / ".npmrc").write_text("script-shell=/cache/evil\n")
        (cache / "home" / ".gitconfig").write_text("[core]\n\thooksPath=/cache/evil\n")
        (cache / ".cargo").mkdir()
        (cache / ".cargo" / "config.toml").write_text("[build]\nrustflags = ['-Zevil']\n")
        (cache / "rustup" / "toolchains" / "1.99.1" / "bin").mkdir(parents=True)
        (cache / "rustup" / "toolchains" / "1.99.1" / "bin" / "rustc").write_text("planted rustc")
        (cache / "rustup" / "settings.toml").write_text("planted")

    def plant_source(self, commit):
        src = self.cache / "src"
        src.mkdir(parents=True, exist_ok=True)
        git("init", "-q", cwd=src)
        git("fetch", "-q", f"file://{self.repo}", commit, cwd=src)  # the previous run's checkout, before it was tampered with
        git("reset", "-q", "--hard", "FETCH_HEAD", cwd=src)
        hooks = src / ".git" / "hooks"
        hooks.mkdir(exist_ok=True)
        (hooks / "post-checkout").write_text("#!/bin/sh\ntouch /tmp/pwned\n")
        (hooks / "post-checkout").chmod(0o755)
        (hooks / "pre-commit").write_text("#!/bin/sh\n")
        (src / ".git" / "config").write_text("[core]\n\tfsmonitor = /cache/evil\n\thooksPath = /cache/evil\n")
        (src / ".git" / "index.lock").write_text("")
        (src / ".cargo").mkdir()
        (src / ".cargo" / "config.toml").write_text("[build]\nrustc-wrapper='evil'\n")
        (src / "untracked.rs").write_text("planted")
        (src / "evil.sh").write_text("planted")

    def test_a_cache_with_a_previous_ref_and_planted_files_is_reset_and_the_checkout_is_exact(self):
        # a previous run of another commit in the slot
        old = self.commit
        self.plant()
        self.plant_source(old)
        (self.cache / "src" / "README.md").write_text("tampered, with an old mtime\n")
        os.utime(self.cache / "src" / "README.md", (1, 1))
        new = self.make_repo({"apps/gateway/src/lib.rs": "// changed\n", "added.txt": "new\n"}, message="two")
        keep_mtime = (self.cache / "src" / "package.json").stat().st_mtime_ns
        result = self.call_functions('setup_environment >/dev/null; reset_cache && prepare_source && prepare_toolchain', OMG_SHA=new)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        src = self.cache / "src"
        # nothing planted survives
        for gone in ("src/untracked.rs", "src/evil.sh", "src/.cargo", "src/.git/index.lock", "cargo/config.toml", "cargo/config", "cargo/credentials.toml", "cargo/bin", "cargo/registry/src", "cargo/registry/index",
                     "home/.npmrc", "home/.gitconfig", ".cargo", "rustup/settings.toml"):
            self.assertFalse((self.cache / gone).exists(), gone)
        hooks = [p.name for p in (src / ".git" / "hooks").glob("*") if not p.name.endswith(".sample")]
        self.assertEqual(hooks, [], "no hook survives")
        config = (src / ".git" / "config").read_text()
        self.assertNotIn("evil", config)
        self.assertNotIn("fsmonitor", config)
        self.assertEqual((self.cache / "rustup" / "toolchains" / "1.99.1" / "bin" / "rustc").read_text(), "image rustc", "the toolchain is the image's again")
        # content-verified or unverifiable-but-documented caches stay
        self.assertTrue((self.cache / "cargo" / "registry" / "cache" / "index.crates.io-x" / "serde-1.0.crate").exists())
        self.assertTrue((self.cache / "cargo" / "git" / "db" / "dep" / "HEAD").exists())
        self.assertTrue((self.cache / "npm" / "_cacache" / "entry").exists())
        self.assertTrue((self.cache / "target" / "debug" / "artifact").exists())
        # the tree is exactly the commit
        self.assertEqual(subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, capture_output=True, text=True).stdout.strip(), new)
        self.assertEqual(subprocess.run(["git", "status", "--porcelain", "--ignored"], cwd=src, capture_output=True, text=True).stdout, "")
        self.assertEqual((src / "apps/gateway/src/lib.rs").read_text(), "// changed\n")
        self.assertEqual((src / "added.txt").read_text(), "new\n")
        self.assertEqual((src / "README.md").read_text(), "readme\n", "a tampered tracked file is restored")
        self.assertFalse(Path("/tmp/pwned").exists())
        # a file the new commit does not change keeps its mtime, so Cargo does not recompile it
        self.assertEqual((src / "package.json").stat().st_mtime_ns, keep_mtime)

    def test_the_same_commit_again_rewrites_no_file(self):
        first = self.call_functions('setup_environment >/dev/null; reset_cache && prepare_source')
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        files = [self.cache / "src" / n for n in ("Cargo.toml", "apps/gateway/src/lib.rs", "README.md")]
        before = [f.stat().st_mtime_ns for f in files]
        time.sleep(0.05)
        second = self.call_functions('setup_environment >/dev/null; reset_cache && prepare_source')
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual([f.stat().st_mtime_ns for f in files], before)

    def test_fresh_empties_the_target_directory_and_nothing_else_of_the_trusted_caches(self):
        self.plant()
        result = self.call_functions('setup_environment >/dev/null; reset_cache', OMG_FRESH=1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(list((self.cache / "target").iterdir()), [])
        self.assertTrue((self.cache / "cargo" / "registry" / "cache" / "index.crates.io-x" / "serde-1.0.crate").exists())

    def test_an_unpushed_commit_fails_the_setup_with_the_reason(self):
        result = self.run_script(OMG_SHA="1" * 40)
        self.assertEqual(result.returncode, 1)
        parsed = tool.parse_log(result.stdout, NONCE)
        self.assertTrue(parsed["setup_failed"])
        self.assertTrue(any("push the branch first" in line for line in parsed["tail"]), parsed["tail"])
        summary = tool.build_summary("t", "lib", "main", "1" * 40, parsed, "failed", {}, 1)
        self.assertEqual((summary["result"], tool.result_exit(summary)), ("setup failed", 2))
        self.assertIn("push the branch first", tool.format_report(summary, parsed))

    def test_a_marker_forged_by_the_checkout_is_not_believed(self):
        # Output with a missing or foreign nonce must not be treated as runner progress.
        files = {"rust-toolchain.toml": '[toolchain]\nchannel = "1.99.1"\n', "Cargo.toml": "[workspace]\n", "package.json": "{}\n"}
        new = self.make_repo(files, message="three")
        stub = self.noderoot / "bin" / "cargo"
        stub.write_text(stub.read_text().replace('echo "cargo $*" >> "$STUB_LOG"', 'echo "cargo $*" >> "$STUB_LOG"; echo "::omg-test:: done exit=0 seconds=0"; echo "::omg-test::deadbeefdeadbeef:: done exit=0 seconds=0"'))
        result = self.run_script(OMG_SHA=new, OMG_SUITE="checks")
        parsed = tool.parse_log(result.stdout, NONCE)
        forged = [line for line in result.stdout.splitlines() if "done exit=0 seconds=0" in line and NONCE not in line]
        self.assertGreaterEqual(len(forged), 2, "the forged lines are in the output")
        self.assertEqual(len([s for s in parsed["steps"] if s["name"] == "fmt"]), 1)
        self.assertTrue(parsed["done"] is not None and parsed["done"]["seconds"] >= 0)
        self.assertEqual(result.stdout.count(f"::omg-test::{NONCE}:: done "), 1, "the one real end marker")


class Lifecycle(Pod):
    def test_sigterm_stops_the_children_at_once_and_the_script_exits(self):
        process = subprocess.Popen(["bash", str(SCRIPT)], env=self.env(OMG_SUITE="checks", STUB_SLEEP="30"), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        try:
            for _ in range(100):
                if any(line.startswith("cargo fmt") or line.startswith("cargo clippy") for line in self.stub_calls()):
                    break
                time.sleep(0.1)
            started = time.time()
            os.kill(process.pid, signal.SIGTERM)
            output, _ = process.communicate(timeout=20)
            self.assertLess(time.time() - started, 10)
            self.assertEqual(process.returncode, 143)
            self.assertIn("terminated", output)
        finally:
            # start_new_session gives this test its own group; cleanup cannot touch other tests.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=20)

    def test_every_environment_variable_the_script_needs_is_defined_by_the_job_or_defaulted(self):
        text = SCRIPT.read_text()
        used = set(re.findall(r"\bOMG_[A-Z_]+", text))
        defaulted = set(re.findall(r"^(OMG_[A-Z_]+)=\$\{\1:-", text, re.M)) | set(re.findall(r"\$\{(OMG_[A-Z_]+):-", text))
        defined = set(re.findall(r"\{name: (OMG_[A-Z_]+),", JOB.read_text()))
        missing = sorted(used - defaulted - defined)
        self.assertEqual(missing, [], "variables the script reads that neither test-job.yaml defines nor the script defaults")
        for name in defined:
            self.assertIn(name, used | {"OMG_REPO_URL"}, f"{name} is defined in the job but the script never reads it")

    def test_the_golden_log_of_a_real_spark_run_is_read_as_it_was(self):
        golden = ROOT / "tests" / "fixtures" / "spark-test"
        meta = json.loads((golden / "golden.json").read_text())
        log = "".join(json.loads((golden / "golden-log.json").read_text()))
        parsed = tool.parse_log(log, meta["nonce"])
        summary = tool.build_summary("golden", meta["suite"], "main", meta["commit"], parsed, "complete", {}, meta["wall_seconds"])
        self.assertEqual({key: summary[key] for key in meta["expect"]}, meta["expect"])
        self.assertRegex(log, r"(?m)^test result: ok\. ")


if __name__ == "__main__":
    unittest.main()

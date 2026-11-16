"""Spark test-suite runner safety tests. No cluster, registry or network access."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("spark_test", ROOT / "scripts" / "spark-test.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)
spark_env = tool.spark_env

# git sets GIT_DIR and GIT_INDEX_FILE when it runs a hook, and `git -C folder` does not
# override them: the ref resolution below would then read that repository instead of the
# temporary one.
for _name in [name for name in os.environ if name.startswith("GIT_")]:
    del os.environ[_name]

SHA = "0123456789abcdef" * 2 + "01234567"
# Refs resolve to a fixed commit: the tests never read this repository's history.
spark_env.resolve_ref = lambda ref, runner=None, repo=None: spark_env.Ref(ref, SHA, False)

SERVER = "https://bitop-spark.taila06ed.ts.net:6443"
CONFIG = Path("/k/spark.yaml")
BASE = "coder-03"


def config_view(server=SERVER, namespace=BASE):
    context = {"cluster": "c", "user": "u"}
    if namespace is not None:
        context["namespace"] = namespace
    return json.dumps({"clusters": [{"name": "c", "cluster": {"server": server}}], "contexts": [{"name": "c", "context": context}], "current-context": "c"})


def quota_json(**free):
    """The base namespace's ResourceQuota objects: hard limits minus what the other namespaces use."""
    hard = {"requests.cpu": "4", "requests.memory": "16Gi", "limits.cpu": "8", "limits.memory": "24Gi", "pods": "40", "persistentvolumeclaims": "12", "requests.storage": "100Gi"}
    hard.update(free)
    return json.dumps({"items": [{"status": {"hard": hard, "used": {key: "0" for key in hard}}}]})


class Cluster:
    """A fake `run`: models the slot namespaces, their lease ConfigMaps and one job, and records every call."""

    def __init__(self, namespaces=(), leases=None, job="running", leftovers=0, fail_on=None, log="", pod_phase="Running", quota=None):
        self.calls = []
        self.quota = quota if quota is not None else quota_json()
        self.namespaces = set(namespaces)
        self.leases = dict(leases or {})  # slot -> data
        self.job = job
        self.leftovers = leftovers
        self.fail_on = fail_on
        self.log = log
        self.pod_phase = pod_phase

    def __call__(self, command, stdin=None, capture=False):
        self.calls.append((list(command), stdin))
        joined = " ".join(command)
        if self.fail_on and self.fail_on in joined:
            raise tool.SparkError(f"{spark_env.describe(command)} failed (exit 1): boom")
        if "config view" in joined:
            return config_view()
        ns = re.search(r"-n (\S+)", joined)
        slot = ns.group(1).rsplit("-", 1)[1] if ns else None
        if "create namespace" in joined:
            self.namespaces.add(command[-1].rsplit("-", 1)[1])
            return ""
        if joined.endswith("get namespace -o name"):
            return "".join(f"namespace/{BASE}-omg-test-{s}\nnamespace/{BASE}-omg-other\n" for s in sorted(self.namespaces))
        if "get networkpolicy" in joined:
            return "networkpolicy/a\nnetworkpolicy/b\n"
        if "create configmap omg-test-lease" in joined:
            if slot in self.leases:
                raise tool.SparkError("kubectl create failed (exit 1): configmaps \"omg-test-lease\" already exists (AlreadyExists)")
            self.leases[slot] = dict(arg.removeprefix("--from-literal=").split("=", 1) for arg in command if arg.startswith("--from-literal="))
            return ""
        if "get configmap omg-test-lease" in joined:
            return json.dumps({"metadata": {"creationTimestamp": "t"}, "data": self.leases[slot]}) if slot in self.leases else ""
        if "delete configmap omg-test-lease" in joined:
            self.leases.pop(slot, None)
            return ""
        if "get job/omg-test" in joined:
            conditions = {"complete": [{"type": "Complete", "status": "True"}], "failed": [{"type": "Failed", "status": "True"}], "running": []}[self.job]
            return json.dumps({"status": {"conditions": conditions, "startTime": "2026-11-16T10:00:00Z", "completionTime": "2026-11-16T10:10:05Z"}})
        if "get pods -l job-name=omg-test" in joined:
            return json.dumps({"items": [{"status": {"phase": self.pod_phase, "containerStatuses": [{"name": "tests", "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]}}]})
        if "exec job/omg-test" in joined:
            return "lib/lib\n"
        if "logs job/omg-test" in joined:
            return self.log
        if "get resourcequota" in joined:
            return self.quota
        if "get persistentvolumeclaim omg-test-cache" in joined:
            return ""
        if "get pods,persistentvolumeclaims" in joined:
            return "persistentvolumeclaim/omg-test-cache\n" + "".join(f"persistentvolumeclaim/omg-test-db-{i}\n" for i in range(self.leftovers))
        return ""

    def commands(self):
        return [" ".join(command) for command, _ in self.calls]


def make(cluster, clock=None, sleep=None):
    ticks = iter(range(10_000, 10_000_000, 1))
    return tool.SparkTest(CONFIG, runner=cluster, clock=clock or (lambda: next(ticks)), sleep=sleep or (lambda seconds: None))


def quiet():
    return contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO())


LOG_PASS = f"""::omg-test:: run suite=all commit={SHA}
::omg-test:: step-start suite=setup name=source
::omg-test:: source commit={SHA}
::omg-test:: step-end suite=setup name=source exit=0 seconds=2
::omg-test:: step-start suite=lib name=lib
running 3 tests
test a::b ... ok
test result: ok. 2441 passed; 0 failed; 9 ignored; 0 measured; 0 filtered out; finished in 3000.12s
::omg-test:: step-end suite=lib name=lib exit=0 seconds=3100
::omg-test:: step-start suite=integration name=targets
test result: ok. 10 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
::omg-test:: step-end suite=integration name=targets exit=0 seconds=50
::omg-test:: step-start suite=web name=vitest
 Test Files  200 passed (200)
      Tests  1903 passed (1903)
::omg-test:: step-end suite=web name=vitest exit=0 seconds=90
::omg-test:: done exit=0 seconds=3300
"""

LOG_FAIL = f"""::omg-test:: step-start suite=lib name=lib
test good::one ... ok
test bad::one ... FAILED
test bad::two ... FAILED
test result: FAILED. 8 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.00s
::omg-test:: step-end suite=lib name=lib exit=101 seconds=40
::omg-test:: step-start suite=web name=vitest
 FAIL  src/a.test.ts > suite > fails
 FAIL  src/a.test.ts > suite > fails
\x1b[31m Tests  1 failed | 1902 skipped | 4 passed (1907)\x1b[0m
::omg-test:: step-end suite=web name=vitest exit=1 seconds=9
::omg-test:: done exit=1 seconds=60
"""


class Names(unittest.TestCase):
    def test_run_names(self):
        for name in ("ab", "wt-653", "a" * 20):
            self.assertEqual(tool.validate_name(name), name)
        for name in ("", "a", "a" * 21, "UP", "-x", "x-", "a_b", "a/b", "x y", "../x", "a\n", None):
            with self.assertRaises(tool.SparkError, msg=repr(name)):
                tool.validate_name(name)

    def test_only_the_two_slot_namespaces_are_touched(self):
        for slot in tool.SLOTS:
            tool.check_namespace(BASE, f"{BASE}-omg-test-{slot}")
        for namespace in ("coder-03", "coder-03-omg-test", "coder-03-omg-test-s3", "coder-03-omg-test-s1x", "coder-03-omg-test-", "coder-03-omg-demo", "coder-03-omg-test-s1 ",
                          "coder-02-omg-test-s1", "default", "kube-system", "coder-03-test-s1", "coder-03-omg-test-S1"):
            with self.assertRaises(tool.SparkError, msg=namespace):
                tool.check_namespace(BASE, namespace)
        with self.assertRaises(tool.SparkError):
            tool.slot_namespace(BASE, "s3")

    def test_two_slots_is_the_cap(self):
        self.assertEqual(len(tool.SLOTS), 2)

    def test_spark_env_and_spark_test_namespaces_never_overlap(self):
        # spark-env.py environments are <base>-omg-<name>; this tool owns <base>-omg-test-*.
        for slot in tool.SLOTS:
            with self.assertRaises(spark_env.SparkError):
                spark_env.check_namespace(BASE, f"{BASE}-omg-test-{slot}")
        for name in ("test-s1", "test-anything", "test-"):
            with self.assertRaises(spark_env.SparkError, msg=name):
                spark_env.validate_name(name)
        spark_env.validate_name("tester")
        spark_env.check_namespace(BASE, f"{BASE}-omg-tester")
        with self.assertRaises(tool.SparkError):
            tool.check_namespace(BASE, f"{BASE}-omg-tester")


class Safety(unittest.TestCase):
    def test_the_default_runner_is_the_scrubbed_spark_env_one(self):
        self.assertIs(tool.SparkTest(CONFIG).runner, tool.spark_env.run)
        self.assertIs(tool.SparkTest(CONFIG).launcher, tool.spark_env.launch)

    def test_a_kubeconfig_for_another_cluster_is_refused_before_any_command(self):
        calls = []

        def runner(command, stdin=None, capture=False):
            calls.append(command)
            return config_view(server="https://prod.example:6443")

        with self.assertRaises(tool.SparkError):
            tool.SparkTest(CONFIG, runner=runner).status()
        self.assertEqual(len(calls), 1)
        self.assertIn("config", calls[0])

    def test_a_kubeconfig_without_a_base_namespace_is_refused(self):
        for namespace in (None, "default", ""):
            with self.assertRaises(tool.SparkError, msg=repr(namespace)):
                tool.SparkTest(CONFIG, runner=lambda *a, **k: config_view(namespace=namespace)).status()

    def test_missing_kubeconfig_never_falls_back(self):
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaises(tool.SparkError):
                tool.spark_env.kubeconfig_path({"SPARK_KUBECONFIG": str(Path(folder) / "absent.yaml")})

    def test_every_command_is_kubectl_with_the_spark_kubeconfig_and_an_owned_namespace(self):
        cluster = Cluster(job="complete", log=LOG_PASS)
        spark = make(cluster)
        with contextlib.redirect_stdout(io.StringIO()):
            spark.run("probe", "web", "HEAD", "", 60, keep=False, detach=False, summary_path=None)
        self.assertTrue(cluster.calls)
        for command, _ in cluster.calls:
            self.assertEqual(command[:3], ["kubectl", "--kubeconfig", str(CONFIG)], command)
            self.assertNotIn("helm", command)
            if "-n" in command:
                tool.check_namespace(BASE, command[command.index("-n") + 1])
            elif "config" not in command:
                self.assertTrue("namespace" in command or "resourcequota" in command, command)  # the base namespace's quota, or listing and creating the slot namespaces
        self.assertFalse([c for c, _ in cluster.calls if "delete" in c and "namespace" in c], "down never deletes a namespace")

    def test_a_missing_tool_is_reported(self):
        with self.assertRaises(tool.SparkError):
            tool.spark_env.require_tools(("definitely-not-installed",), which=lambda name: None)


class Rendering(unittest.TestCase):
    JOB = (ROOT / "deploy" / "spark" / "test-job.yaml").read_text()

    def test_a_valid_job(self):
        text = tool.render_job(self.JOB, "lib", SHA, "", 90)
        self.assertIn(f'value: "{SHA}"', text)
        self.assertIn("activeDeadlineSeconds: 5400", text)
        self.assertNotRegex(text, r"__[A-Z_]+__")

    def test_test_args_are_checked_and_limited_to_lib_and_web(self):
        self.assertIn('"--exact management::tests::x"', tool.render_job(self.JOB, "lib", SHA, "--exact management::tests::x", 60))
        tool.render_job(self.JOB, "web", SHA, "src/lib/review-history.test.ts", 60)
        for bad in ("$(id)", "a;b", "a && b", "`id`", 'a"b', "a'b", "a\nb", "a  b", "a|b", ">x", "${HOME}", "-- --x;"):
            with self.assertRaises(tool.SparkError, msg=repr(bad)):
                tool.render_job(self.JOB, "lib", SHA, bad, 60)
        for suite in ("integration", "checks", "all"):
            with self.assertRaises(tool.SparkError, msg=suite):
                tool.render_job(self.JOB, suite, SHA, "x", 60)

    def test_suite_commit_and_timeout_are_checked(self):
        for bad in ("", "rm", "LIB", "lib;x"):
            with self.assertRaises(tool.SparkError, msg=bad):
                tool.render_job(self.JOB, bad, SHA, "", 60)
        for bad in ("", "main", SHA[:12], SHA.upper(), SHA + "0", "x" * 40, None):
            with self.assertRaises(tool.SparkError, msg=repr(bad)):
                tool.render_job(self.JOB, "lib", bad, "", 60)
        for minutes in (0, -1, tool.MAX_TIMEOUT_MINUTES + 1):
            with self.assertRaises(tool.SparkError, msg=minutes):
                tool.render_job(self.JOB, "lib", SHA, "", minutes)

    def test_a_manifest_without_a_placeholder_or_with_a_leftover_is_refused(self):
        with self.assertRaises(tool.SparkError):
            tool.render_job(self.JOB.replace("__SHA__", "x"), "lib", SHA, "", 60)
        with self.assertRaises(tool.SparkError):
            tool.render_job(self.JOB + "\n# __OTHER__\n", "lib", SHA, "", 60)

    def test_the_run_script_is_valid_bash_and_prints_no_password(self):
        import subprocess
        script = ROOT / "deploy" / "spark" / "test-run.sh"
        subprocess.run(["bash", "-n", str(script)], check=True)
        text = script.read_text()
        self.assertNotRegex(text, r"(?m)^\s*(set -x|set -o xtrace)")
        self.assertNotIn("echo \"$PGPASSWORD\"", text)
        self.assertNotIn("printenv", text)
        self.assertRegex(text, r"main 2>&1 \| redact\nexit \"\$\{PIPESTATUS\[0\]\}\"")

    def test_the_password_secret_is_random_redacted_and_not_an_argument(self):
        first, second = json.loads(tool.superuser_secret()), json.loads(tool.superuser_secret())
        self.assertNotEqual(first["stringData"]["password"], second["stringData"]["password"])
        self.assertRegex(first["stringData"]["password"], r"^[0-9a-f]{48}$")
        self.assertEqual(first["stringData"]["username"], "postgres")
        self.assertEqual(spark_env.redact(f"url postgres://x:{first['stringData']['password']}@h"), "url postgres://x:[redacted]@h", "generated passwords are registered for redaction")


class Slots(unittest.TestCase):
    INFO = {"suite": "lib", "ref": "main", "sha": SHA}

    def acquire(self, cluster, name="new"):
        spark = make(cluster)
        return spark, spark.acquire(name, dict(self.INFO))

    def test_the_first_run_creates_one_slot_namespace_and_waits_for_its_policies(self):
        cluster = Cluster()
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s1")
        commands = cluster.commands()
        created = [c for c in commands if "create namespace" in c]
        self.assertEqual(created, [f"kubectl --kubeconfig {CONFIG} create namespace {BASE}-omg-test-s1"])
        self.assertLess(commands.index(created[0]), next(i for i, c in enumerate(commands) if "get networkpolicy" in c))
        self.assertLess(next(i for i, c in enumerate(commands) if "get networkpolicy" in c), next(i for i, c in enumerate(commands) if "create configmap omg-test-lease" in c))
        self.assertEqual(cluster.leases["s1"]["name"], "new")
        self.assertEqual(cluster.leases["s1"]["sha"], SHA)

    def test_a_free_existing_slot_is_preferred_to_creating_one(self):
        cluster = Cluster(namespaces={"s2"})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")
        self.assertFalse([c for c in cluster.commands() if "create namespace" in c])

    def test_the_second_slot_is_created_when_the_first_is_busy(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "other", "suite": "web", "ref": "main", "sha": SHA}})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")
        self.assertEqual(len([c for c in cluster.commands() if "create namespace" in c]), 1)

    def test_a_third_run_is_refused_with_a_clear_message_and_nothing_is_created(self):
        busy = {"suite": "lib", "ref": "main", "sha": SHA}
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": {"name": "one", **busy}, "s2": {"name": "two", **busy}})
        spark = make(cluster)
        with self.assertRaises(tool.SparkError) as caught:
            spark.acquire("three", dict(self.INFO))
        message = str(caught.exception)
        for needle in ("all 2 test slots are in use", "one (lib main", "two (lib main", "down"):
            self.assertIn(needle, message)
        self.assertFalse([c for c in cluster.commands() if "create" in c and "namespace" in c])
        self.assertEqual(set(cluster.leases), {"s1", "s2"})

    def test_a_lease_created_by_someone_else_in_the_meantime_moves_to_the_next_slot(self):
        class Racing(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                if "create configmap omg-test-lease" in " ".join(command) and "omg-test-s1" in " ".join(command) and "s1" not in self.leases:
                    self.leases["s1"] = {"name": "winner"}
                return super().__call__(command, stdin, capture)

        cluster = Racing(namespaces={"s1", "s2"})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")

    def test_a_duplicate_name_is_refused(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "same", "suite": "lib", "ref": "main", "sha": SHA}})
        with self.assertRaises(tool.SparkError) as caught:
            self.acquire(cluster, "same")
        self.assertIn("already exists", str(caught.exception))

    def test_a_forbidden_namespace_that_is_not_ready_is_retried_but_other_errors_are_not(self):
        class Slow(Cluster):
            attempts = 0

            def __call__(self, command, stdin=None, capture=False):
                if "get networkpolicy" in " ".join(command):
                    Slow.attempts += 1
                    if Slow.attempts < 3:
                        raise tool.SparkError("Forbidden: cannot list networkpolicies")
                return super().__call__(command, stdin, capture)

        _, slot = self.acquire(Slow())
        self.assertEqual(slot, "s1")
        with self.assertRaises(tool.SparkError):
            self.acquire(Cluster(fail_on="create namespace"))

    def test_a_namespace_that_never_gets_its_policies_is_not_used(self):
        class Never(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                if "get networkpolicy" in " ".join(command):
                    return "networkpolicy/a\n"
                return super().__call__(command, stdin, capture)

        with self.assertRaises(tool.SparkError) as caught:
            self.acquire(Never())
        self.assertIn("two network policies", str(caught.exception))


class Lifecycle(unittest.TestCase):
    def test_start_applies_the_cache_then_secrets_script_database_and_job_in_that_order(self):
        cluster = Cluster()
        spark = make(cluster)
        with contextlib.redirect_stdout(io.StringIO()):
            slot, resolved = spark.start("probe", "lib", "HEAD", "", 90)
        self.assertEqual(slot, "s1")
        commands = cluster.commands()
        order = [next(i for i, c in enumerate(commands) if needle in c) for needle in ("create configmap omg-test-lease", "apply -f -", "create -f -", "create configmap omg-test-script")]
        self.assertEqual(order, sorted(order))
        stdins = [data for _, data in cluster.calls if data]
        self.assertEqual(len(stdins), 4)
        self.assertIn("PersistentVolumeClaim", stdins[0])
        self.assertIn('"Secret"', stdins[1])
        self.assertIn("kind: Cluster", stdins[2])
        self.assertIn("kind: Job", stdins[3])
        self.assertIn(resolved.sha, stdins[3])

    def test_no_secret_is_ever_a_command_line_argument(self):
        cluster = Cluster()
        spark = make(cluster)
        with contextlib.redirect_stdout(io.StringIO()):
            spark.start("probe", "lib", "HEAD", "", 90)
        secret = json.loads(next(data for _, data in cluster.calls if data and '"Secret"' in data))["stringData"]["password"]
        self.assertFalse([c for c in cluster.commands() if secret in c])
        for _, data in cluster.calls:
            if data and secret in data:
                self.assertIn('"Secret"', data)

    def test_a_failed_setup_removes_the_run_and_frees_the_slot(self):
        cluster = Cluster(fail_on="kind: Job")
        cluster.fail_on = None

        class Failing(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                if stdin and "kind: Job" in stdin:
                    raise tool.SparkError("kubectl create failed (exit 1): exceeded quota")
                return super().__call__(command, stdin, capture)

        cluster = Failing()
        spark = make(cluster)
        out, err = quiet()
        with out, err, self.assertRaises(tool.SparkError):
            spark.start("probe", "lib", "HEAD", "", 90)
        self.assertEqual(cluster.leases, {}, "the lease is released")
        self.assertTrue([c for c in cluster.commands() if "delete job omg-test" in c])

    def test_down_removes_the_run_but_never_a_namespace_and_the_lease_goes_last(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "probe", "suite": "lib", "ref": "main", "sha": SHA}})
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).down("probe")
        commands = cluster.commands()
        deleted = [c for c in commands if " delete " in c]
        self.assertTrue(deleted[-1].endswith("delete configmap omg-test-lease --ignore-not-found"), deleted)
        self.assertTrue(any("delete cluster.postgresql.cnpg.io omg-test-db" in c for c in deleted))
        self.assertFalse([c for c in deleted if "namespace" in c or "persistentvolumeclaim" in c or "pvc" in c], "the cache volume is not deleted")
        self.assertEqual(cluster.leases, {})

    def test_down_keeps_the_lease_when_something_is_left_behind(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "probe"}}, leftovers=1)
        with self.assertRaises(tool.SparkError) as caught:
            make(cluster).down("probe")
        self.assertIn("omg-test-db-0", str(caught.exception))
        self.assertIn("s1", cluster.leases)

    def test_down_of_an_unknown_run_changes_nothing(self):
        cluster = Cluster(namespaces={"s1"})
        with self.assertRaises(tool.SparkError):
            make(cluster).down("ghost")
        self.assertFalse([c for c in cluster.commands() if " delete " in c])

    def test_purge_deletes_idle_slot_namespaces_only(self):
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": {"name": "busy", "suite": "lib", "ref": "main", "sha": SHA}})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).purge()
        deleted = [c for c in cluster.commands() if "delete namespace" in c]
        self.assertEqual(deleted, [f"kubectl --kubeconfig {CONFIG} delete namespace {BASE}-omg-test-s2 --wait=false"])
        self.assertIn("in use", out.getvalue())

    def test_status_lists_slots(self):
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": {"name": "busy", "suite": "lib", "ref": "main", "sha": SHA, "started": "10000"}})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).status()
        self.assertIn("s1: busy (lib main", out.getvalue())
        self.assertIn("s2: free", out.getvalue())

    def test_run_waits_prints_the_result_and_downs(self):
        cluster = Cluster(job="complete", log=LOG_PASS)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = make(cluster).run("probe", "all", "HEAD", "", 60, keep=False, detach=False, summary_path=None)
        self.assertEqual(code, 0)
        self.assertIn("test result: ok. 2441 passed; 0 failed; 9 ignored", out.getvalue())
        self.assertEqual(cluster.leases, {})

    def test_a_failing_run_exits_nonzero_names_the_failures_and_is_still_removed(self):
        cluster = Cluster(job="failed", log=LOG_FAIL.replace("\x1b", "\x1b"))
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(out):
            path = Path(folder) / "summary.json"
            code = make(cluster).run("probe", "all", "HEAD", "", 60, keep=False, detach=False, summary_path=str(path))
            summary = json.loads(path.read_text())
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(code, 1)
        self.assertEqual(summary["result"], "failed")
        self.assertEqual(summary["failed_tests"], ["bad::one", "bad::two", "src/a.test.ts > suite > fails"])
        self.assertIn("failing tests:", out.getvalue())
        self.assertIn("bad::two", out.getvalue())
        self.assertEqual(cluster.leases, {})

    def test_keep_and_detach_leave_the_run_in_place(self):
        cluster = Cluster(job="complete", log=LOG_PASS)
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).run("probe", "all", "HEAD", "", 60, keep=True, detach=False, summary_path=None)
        self.assertIn("s1", cluster.leases)
        cluster = Cluster()
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).run("probe", "all", "HEAD", "", 60, keep=False, detach=True, summary_path=None)
        self.assertIn("s1", cluster.leases)

    def test_a_run_that_has_not_finished_has_no_result(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "probe", "suite": "lib", "ref": "main", "sha": SHA}}, job="running")
        with self.assertRaises(tool.SparkError):
            make(cluster).result("probe")

    def test_a_job_that_ran_another_commit_is_refused(self):
        other = LOG_PASS.replace(f"source commit={SHA}", "source commit=" + "f" * 40)
        cluster = Cluster(namespaces={"s1"}, leases={"s1": {"name": "probe", "suite": "lib", "ref": "main", "sha": SHA}}, job="complete", log=other)
        with self.assertRaises(tool.SparkError):
            make(cluster).result("probe")

    def test_a_job_without_a_pod_names_the_warning_events(self):
        class NoPod(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                joined = " ".join(command)
                if "get pods -l job-name=omg-test" in joined:
                    return ""
                if "get events" in joined:
                    return "FailedCreate   exceeded quota: requests.cpu\n"
                return super().__call__(command, stdin, capture)

        cluster = NoPod(namespaces={"s1"}, leases={"s1": {"name": "probe", "suite": "lib", "ref": "main", "sha": SHA, "started": "10000"}}, job="running", pod_phase="Pending")
        now = [10_000]

        def sleep(seconds):
            now[0] += seconds

        spark = tool.SparkTest(CONFIG, runner=cluster, clock=lambda: now[0], sleep=sleep)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
            spark.wait("probe", "s1")
        self.assertIn("exceeded quota", str(caught.exception))


class Ledger(unittest.TestCase):
    ERROR = "kubectl create failed (exit 1): admission webhook denied the request: GlobalResourceQuota QuantityLedger x/y is not initialized"

    def test_a_ledger_that_is_not_ready_is_waited_for(self):
        attempts, waits = [], []

        def runner(command, stdin=None, capture=False):
            if "config" in command:
                return config_view()
            attempts.append(command)
            if len(attempts) < 4:
                raise tool.SparkError(self.ERROR)
            return "ok"

        spark = tool.SparkTest(CONFIG, runner=runner, sleep=waits.append)
        self.assertEqual(spark.kubectl(["create", "configmap", "x"], slot="s1"), "ok")
        self.assertEqual(len(waits), 3)

    def test_it_gives_up_and_other_errors_are_not_retried(self):
        waits = []

        def always(command, stdin=None, capture=False):
            raise tool.SparkError(self.ERROR if "config" not in command else "x")

        spark = tool.SparkTest(CONFIG, runner=lambda c, stdin=None, capture=False: config_view() if "config" in c else (_ for _ in ()).throw(tool.SparkError(self.ERROR)), sleep=waits.append)
        with self.assertRaises(tool.SparkError):
            spark.kubectl(["create", "configmap", "x"], slot="s1")
        self.assertEqual(len(waits), tool.LEDGER_RETRIES)
        waits.clear()
        spark = tool.SparkTest(CONFIG, runner=lambda c, stdin=None, capture=False: config_view() if "config" in c else (_ for _ in ()).throw(tool.SparkError("Forbidden")), sleep=waits.append)
        with self.assertRaises(tool.SparkError):
            spark.kubectl(["create", "configmap", "x"], slot="s1")
        self.assertEqual(waits, [])


class Quota(unittest.TestCase):
    def test_quantities(self):
        self.assertEqual([tool.quantity("requests.cpu", v) for v in ("1500m", "2", "0.5")], [1500, 2000, 500])
        self.assertEqual([tool.quantity("limits.memory", v) for v in ("8Gi", "512Mi", "1048576Ki", str(1024**3))], [8192, 512, 1024, 1024])
        self.assertEqual(tool.quantity("requests.storage", "30Gi"), 30720)
        self.assertEqual(tool.quantity("pods", "3"), 3)

    def test_run_needs_match_the_manifests(self):
        job = (ROOT / "deploy" / "spark" / "test-job.yaml").read_text()
        cluster = (ROOT / "deploy" / "spark" / "test-cluster.yaml").read_text()
        cache = (ROOT / "deploy" / "spark" / "test-cache.yaml").read_text()

        def resources(text, section, key):
            found = re.search(rf"{section}: \{{cpu: \"?([0-9a-zA-Z.]+)\"?, memory: ([0-9A-Za-z]+)\}}", text)
            return tool.quantity(f"{key}.cpu", found.group(1)), tool.quantity(f"{key}.memory", found.group(2))

        tests_requests = resources(job.split("name: tests")[1], "requests", "requests")
        tests_limits = resources(job.split("name: tests")[1], "limits", "limits")
        db_requests = resources(cluster, "requests", "requests")
        db_limits = resources(cluster, "limits", "limits")
        needs = tool.RUN_NEEDS
        self.assertEqual(needs["requests.cpu"], tests_requests[0] + db_requests[0])
        self.assertEqual(needs["requests.memory"], tests_requests[1] + db_requests[1])
        self.assertEqual(needs["limits.cpu"], tests_limits[0] + db_limits[0])
        self.assertEqual(needs["limits.memory"], tests_limits[1] + db_limits[1])
        self.assertEqual(needs["requests.storage"], tool.quantity("requests.storage", re.search(r"storage:\n    size: (\S+)", cluster).group(1)))
        self.assertEqual(tool.CACHE_NEEDS["requests.storage"], tool.quantity("requests.storage", re.search(r"requests: \{storage: (\S+)\}", cache).group(1)))

    def test_shortfalls(self):
        self.assertEqual(tool.quota_shortfalls(quota_json(), {**tool.RUN_NEEDS, **{"requests.storage": 9000}}), [])
        short = tool.quota_shortfalls(quota_json(**{"requests.cpu": "1250m", "pods": "2"}), tool.RUN_NEEDS)
        self.assertEqual(len(short), 2)
        self.assertIn("requests.cpu: needs 1750, 1250 available", short)
        self.assertEqual(tool.quota_shortfalls(json.dumps({"items": []}), tool.RUN_NEEDS), [])

    def test_a_run_that_does_not_fit_is_refused_before_anything_is_created(self):
        cluster = Cluster(quota=quota_json(**{"requests.memory": "4Gi"}))
        out, err = quiet()
        with out, err, self.assertRaises(tool.SparkError) as caught:
            make(cluster).start("probe", "lib", "HEAD", "", 60)
        self.assertIn("requests.memory", str(caught.exception))
        self.assertIn("shared Spark quota", str(caught.exception))
        self.assertEqual(cluster.leases, {})
        self.assertFalse([c for c in cluster.commands() if "create -f -" in c or "apply" in c or "create configmap omg-test-script" in c])


class Reading(unittest.TestCase):
    def test_a_passing_log(self):
        parsed = tool.parse_log(LOG_PASS)
        self.assertEqual(parsed["suites"]["lib"], {"passed": 2441, "failed": 0, "ignored": 9, "targets": 1})
        self.assertEqual(parsed["suites"]["integration"], {"passed": 15, "failed": 0, "ignored": 1, "targets": 2})
        self.assertEqual(parsed["suites"]["web"], {"passed": 1903, "failed": 0, "ignored": 0, "targets": 1})
        self.assertEqual(parsed["commit"], SHA)
        self.assertEqual(parsed["done"], {"exit": 0, "seconds": 3300})
        self.assertEqual(len(parsed["steps"]), 4)
        self.assertEqual(len(parsed["results"]), 3)
        summary = tool.build_summary("probe", "all", "main", SHA, parsed, "complete", {}, 3310)
        self.assertEqual((summary["result"], summary["exit_code"], summary["duration_seconds"], summary["wall_seconds"]), ("passed", 0, 3300, 3310))
        self.assertEqual(summary["counts"], {"passed": 2441 + 15 + 1903, "failed": 0, "ignored": 10})
        self.assertEqual(summary["failed_steps"], [])
        json.dumps(summary)

    def test_a_sharded_lib_run_is_checked_against_the_listed_inventory(self):
        shard = "test result: ok. {p} passed; 0 failed; {i} ignored; 0 measured; 0 filtered out; finished in 1.00s"
        good = "::omg-test:: step-start suite=lib name=shard-0\n" + shard.format(p=600, i=10) + "\n::omg-test:: step-start suite=lib name=shard-1\n" + shard.format(p=700, i=1) + "\n"
        parsed = tool.parse_log("::omg-test:: lib-inventory tests=1311\n" + good + "::omg-test:: done exit=0 seconds=9\n")
        summary = tool.build_summary("p", "lib", "main", SHA, parsed, "complete", {}, 9)
        self.assertEqual((summary["result"], summary["lib_tests_listed"]), ("passed", 1311))
        short = tool.parse_log("::omg-test:: lib-inventory tests=2000\n" + good + "::omg-test:: done exit=0 seconds=9\n")
        summary = tool.build_summary("p", "lib", "main", SHA, short, "complete", {}, 9)
        self.assertEqual(summary["result"], "failed")
        self.assertIn("lists 2000 tests but the shards account for 1311", summary["note"])

    def test_a_failing_log(self):
        parsed = tool.parse_log(LOG_FAIL)
        self.assertEqual(parsed["failed_tests"], ["bad::one", "bad::two", "src/a.test.ts > suite > fails"])
        self.assertEqual(parsed["suites"]["lib"]["failed"], 2)
        self.assertEqual(parsed["suites"]["web"], {"passed": 4, "failed": 1, "ignored": 1902, "targets": 1})
        summary = tool.build_summary("probe", "all", "main", SHA, parsed, "failed", {}, 61)
        self.assertEqual((summary["result"], summary["exit_code"]), ("failed", 1))
        self.assertEqual(summary["failed_steps"], ["lib/lib", "web/vitest"])

    def test_a_passing_log_of_a_failed_job_is_still_failed(self):
        summary = tool.build_summary("probe", "lib", "main", SHA, tool.parse_log(LOG_PASS), "failed", {}, 5)
        self.assertEqual(summary["result"], "failed")

    def test_a_run_killed_before_its_end_marker_reports_the_pod(self):
        parsed = tool.parse_log("::omg-test:: step-start suite=lib name=lib\nrunning 5 tests\n")
        summary = tool.build_summary("probe", "lib", "main", SHA, parsed, "failed", {"exit_code": 137, "reason": "OOMKilled"}, 120)
        self.assertEqual((summary["result"], summary["exit_code"], summary["pod_reason"]), ("failed", 137, "OOMKilled"))
        self.assertIn("end marker", summary["note"])
        self.assertEqual(tool.build_summary("probe", "lib", "main", SHA, parsed, "failed", {}, 1)["exit_code"], 1, "an unknown exit is a failure, never 0")

    def test_the_report_has_the_result_lines_the_failures_and_the_json(self):
        parsed = tool.parse_log(LOG_FAIL)
        report = tool.format_report(tool.build_summary("probe", "all", "main", SHA, parsed, "failed", {}, 61), parsed)
        lines = report.splitlines()
        self.assertTrue(lines[0].startswith("test result: FAILED. 8 passed; 2 failed"))
        self.assertEqual(json.loads(lines[-1])["suite"], "all")
        self.assertEqual(json.dumps(json.loads(lines[-1]), separators=(",", ":")), lines[-1], "the JSON line is compact")

    def test_markers_and_noise(self):
        self.assertEqual(tool.parse_marker("::omg-test:: step-end suite=lib name=a-b exit=3 seconds=4"), ("step-end", {"suite": "lib", "name": "a-b", "exit": "3", "seconds": "4"}))
        self.assertEqual(tool.parse_marker("::omg-test::"), ("", {}))
        self.assertEqual(tool.parse_log("")["suites"], {})

    def test_job_phases(self):
        running = json.dumps({"status": {"conditions": []}})
        self.assertEqual(tool.job_phase(json.dumps({"status": {"conditions": [{"type": "Complete", "status": "True"}]}})), "succeeded")
        self.assertEqual(tool.job_phase(json.dumps({"status": {"conditions": [{"type": "Failed", "status": "True"}]}})), "failed")
        self.assertEqual(tool.job_phase(running, {"status": {"phase": "Pending"}}), "starting")
        self.assertEqual(tool.job_phase(running, {"status": {"phase": "Running"}}, "lib/lib"), "running lib/lib")
        self.assertEqual(tool.job_phase(running), "starting")

    def test_summary_files_are_private_regular_files(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / "real"
            target.write_text("keep")
            link = Path(folder) / "link"
            link.symlink_to(target)
            with self.assertRaises(OSError):
                tool.write_summary(str(link), {"a": 1})
            self.assertEqual(target.read_text(), "keep")
            path = Path(folder) / "ok.json"
            tool.write_summary(str(path), {"a": 1})
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_elapsed_text(self):
        self.assertEqual(tool.elapsed_text(75), "1m15s")
        self.assertEqual(tool.elapsed_text(3725), "1h02m05s")
        self.assertEqual(tool.elapsed_text(-5), "0m00s")


if __name__ == "__main__":
    unittest.main()

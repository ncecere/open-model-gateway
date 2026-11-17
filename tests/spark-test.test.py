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
NONCE = "00112233aabbccdd"
# Refs resolve to a fixed commit: the tests never read this repository's history.
spark_env.resolve_ref = lambda ref, runner=None, repo=None: spark_env.Ref(ref, SHA, False)

SERVER = "https://bitop-spark.taila06ed.ts.net:6443"
CONFIG = Path("/k/spark.yaml")
BASE = "coder-03"
LABELS = {tool.MANAGED_LABEL: tool.MANAGED_VALUE}


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


def marker(kind, nonce=NONCE, **fields):
    return f"::omg-test::{nonce}:: {kind}" + "".join(f" {k}={v}" for k, v in fields.items())


class Cluster:
    """A fake `run`: models the slot namespaces (labels, terminating), their lease ConfigMaps and one job, and records every call."""

    def __init__(self, namespaces=(), leases=None, job="running", leftovers=0, fail_on=None, log="", pod=None, quota=None, foreign=(), terminating=()):
        self.calls = []
        self.namespaces = set(namespaces)
        self.foreign = set(foreign)  # exist without our label
        self.terminating = set(terminating)
        self.leases = dict(leases or {})  # slot -> data
        self.job = job
        self.leftovers = leftovers
        self.fail_on = fail_on
        self.log = log
        self.pod = pod
        self.quota = quota if quota is not None else quota_json()

    def exists(self, slot):
        return slot in self.namespaces or slot in self.foreign or slot in self.terminating

    def __call__(self, command, stdin=None, capture=False):
        self.calls.append((list(command), stdin))
        joined = " ".join(command)
        if self.fail_on and self.fail_on in joined:
            raise tool.SparkError(f"{spark_env.describe(command)} failed (exit 1): boom")
        if "config view" in joined:
            return config_view()
        ns = re.search(r"-n (\S+)", joined)
        slot = ns.group(1).rsplit("-", 1)[1] if ns else None
        m = re.search(r"get namespace (\S+) --ignore-not-found", joined)
        if m:
            s = m.group(1).rsplit("-", 1)[1]
            if not self.exists(s):
                return ""
            return json.dumps({"metadata": {"name": m.group(1), "labels": {} if s in self.foreign else dict(LABELS)}, "status": {"phase": "Terminating" if s in self.terminating else "Active"}})
        if "create -f -" in joined and stdin and '"kind": "Namespace"' in stdin:
            document = json.loads(stdin)
            self.namespaces.add(document["metadata"]["name"].rsplit("-", 1)[1])
            assert document["metadata"]["labels"] == LABELS
            return ""
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
            if self.job is None:
                return ""
            conditions = {"complete": [{"type": "Complete", "status": "True"}], "failed": [{"type": "Failed", "status": "True", "reason": "BackoffLimitExceeded"}], "deadline": [{"type": "Failed", "status": "True", "reason": "DeadlineExceeded"}], "running": []}[self.job]
            return json.dumps({"status": {"conditions": conditions, "startTime": "2026-11-16T10:00:00Z", "completionTime": "2026-11-16T10:10:05Z"}})
        if "get pods -l job-name=omg-test" in joined:
            if self.pod == "none":
                return ""
            status = {"phase": "Running", "containerStatuses": [{"name": "tests", "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]}
            status.update(self.pod or {})
            return json.dumps({"items": [{"status": status}]})
        if "exec job/omg-test" in joined:
            return "lib/lib\n"
        if "logs job/omg-test" in joined:
            return self.log
        if "get events" in joined:
            return "FailedScheduling   0/1 nodes: Insufficient cpu\n"
        if "get resourcequota" in joined:
            return self.quota
        if "get persistentvolumeclaim omg-test-cache" in joined:
            return ""
        if "get pods,persistentvolumeclaims" in joined:
            return "persistentvolumeclaim/omg-test-cache\n" + "".join(f"persistentvolumeclaim/omg-test-db-{i}\n" for i in range(self.leftovers))
        return ""

    def commands(self):
        return [" ".join(command) for command, _ in self.calls]


class Primed(Cluster):
    """A cluster whose leases carry the nonce the test log was written with (the real nonce is random per run)."""

    def __call__(self, command, stdin=None, capture=False):
        result = super().__call__(command, stdin, capture)
        if "create configmap omg-test-lease" in " ".join(command):
            self.leases["s1"]["nonce"] = NONCE
        return result


def lease(name="probe", suite="lib", nonce=NONCE, **extra):
    return {"name": name, "suite": suite, "ref": "main", "sha": SHA, "nonce": nonce, "started": "10000", "started_ms": "10000000", **extra}


def make(cluster, clock=None, sleep=None):
    ticks = iter(range(10_000, 10_000_000, 1))
    return tool.SparkTest(CONFIG, runner=cluster, clock=clock or (lambda: next(ticks)), sleep=sleep or (lambda seconds: None))


def quiet():
    return contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO())


def log_for(suite="lib", *, nonce=NONCE, passed=2456, ignored=9, failed=0, extra="", steps=None, done=True):
    """A log with the markers and result lines of a run of the suite; the pieces a test breaks are parameters."""
    out = [marker("run", nonce, suite=suite, commit=SHA), marker("source", nonce, commit=SHA)]
    parts = tool.SUITE_PARTS[suite]
    for part in parts:
        names = list(tool.REQUIRED_STEPS.get(part, ()))
        if part == "lib":
            names += [f"shard-{i}" for i in range(tool.LIB_SHARDS)]
        for name in names:
            if steps is not None and f"{part}/{name}" not in steps:
                continue
            out.append(marker("step-start", nonce, suite=part, name=name))
            if part == "web" and name == "vitest":
                out.append(f"      Tests  {passed} passed ({passed})" if part != "x" else "")
            elif part in ("lib", "integration") and (name.startswith("shard-") or name in ("targets", "doctests")):
                each = passed // 4 if part == "lib" else passed
                out.append(f"test result: ok. {each} passed; {failed} failed; {ignored if name == 'shard-0' else 0} ignored; 0 measured; 0 filtered out; finished in 1.00s")
            out.append(marker("step-end", nonce, suite=part, name=name, exit=0, seconds=1))
    if "lib" in parts:
        out.append(marker("lib-inventory", nonce, tests=(passed // 4) * 4 + (ignored if "lib" in parts else 0)))
    out.append(extra)
    if done:
        out.append(marker("done", nonce, exit=0, seconds=9))
    return "\n".join(out) + "\n"


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
        cluster = Primed(job="complete", log=log_for("web", passed=1903))
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).run("probe", "web", "HEAD", "", 60, keep=False, detach=False, summary_path=None)
        for command, _ in cluster.calls:
            self.assertEqual(command[:3], ["kubectl", "--kubeconfig", str(CONFIG)], command)
            self.assertNotIn("helm", command)
            if "-n" in command:
                tool.check_namespace(BASE, command[command.index("-n") + 1])
            elif "config" not in command:
                # cluster-wide only: the base namespace's quota, and getting or creating one of the two slot namespaces by name
                named = [w for w in command if re.fullmatch(rf"{BASE}-omg-test-s[12]", w)]
                self.assertTrue("resourcequota" in command or named or ("create" in command and "-f" in command), command)
        self.assertFalse([c for c, _ in cluster.calls if "delete" in c and "namespace" in c], "down never deletes a namespace")

    def test_a_namespace_without_our_label_is_never_used_stopped_or_deleted(self):
        for action in (lambda s: s.status(), lambda s: s.down("probe"), lambda s: s.purge(), lambda s: s.acquire("probe", {"suite": "lib"}), lambda s: s.find("probe")):
            cluster = Cluster(foreign={"s1"}, leases={"s1": lease()})
            with contextlib.redirect_stdout(io.StringIO()):
                try:
                    action(make(cluster))
                except tool.SparkError as error:
                    self.assertIn("managed-by", str(error))
            self.assertFalse([c for c in cluster.commands() if " delete " in c or "create configmap" in c], cluster.commands())

    def test_slot_namespaces_are_created_with_the_label(self):
        cluster = Cluster()
        make(cluster).acquire("probe", {"suite": "lib"})
        create = [(c, d) for c, d in cluster.calls if d and '"Namespace"' in d]
        self.assertEqual(len(create), 1)
        self.assertEqual(json.loads(create[0][1])["metadata"]["labels"], LABELS)

    def test_a_namespace_being_deleted_gives_a_retry_message(self):
        cluster = Cluster(terminating={"s1"})
        with self.assertRaises(tool.SparkError) as caught:
            make(cluster).acquire("probe", {"suite": "lib"})
        self.assertIn("being deleted", str(caught.exception))
        self.assertIn("retry", str(caught.exception))

    def test_a_missing_tool_is_reported(self):
        with self.assertRaises(tool.SparkError):
            tool.spark_env.require_tools(("definitely-not-installed",), which=lambda name: None)


class Rendering(unittest.TestCase):
    JOB = (ROOT / "deploy" / "spark" / "test-job.yaml").read_text()

    def test_a_valid_job(self):
        text = tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=NONCE)
        self.assertIn(f'value: "{SHA}"', text)
        self.assertIn(f'value: "{NONCE}"', text)
        self.assertIn("activeDeadlineSeconds: 5400", text)
        self.assertIn('cpu: "1800m", memory: 10Gi', text)
        self.assertNotRegex(text, r"__[A-Z_]+__")
        self.assertIn('{name: OMG_JOBS, value: "2"}', text)
        self.assertIn('{name: OMG_TEST_THREADS, value: "4"}', text)
        self.assertIn('{name: OMG_WEB_WORKERS, value: "1"}', text)
        self.assertIn('{name: OMG_FRESH, value: "0"}', text)

    def test_the_password_is_not_in_the_environment(self):
        text = tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=NONCE)
        self.assertNotIn("PGPASSWORD", text)
        self.assertNotIn("secretKeyRef", text)
        self.assertIn("secretName: omg-test-pg-superuser", text)

    def test_fast_raises_the_limit_and_what_follows_it_but_not_the_request(self):
        text = tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=NONCE, cpu_limit_m=5500)
        self.assertIn('limits: {cpu: "5500m", memory: 10Gi}', text)
        self.assertIn('requests: {cpu: "1500m", memory: 6Gi}', text)
        self.assertIn('{name: OMG_JOBS, value: "4"}', text)
        self.assertIn('{name: OMG_TEST_THREADS, value: "8"}', text)
        self.assertIn('{name: OMG_WEB_WORKERS, value: "4"}', text)
        self.assertEqual(tool.cpu_plan(1800), {"jobs": 2, "threads": 4, "workers": 1})
        self.assertEqual(tool.cpu_plan(6000), {"jobs": 4, "threads": 8, "workers": 4})
        for bad in (1000, 6001, "5000", None):
            with self.assertRaises(tool.SparkError, msg=repr(bad)):
                tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=NONCE, cpu_limit_m=bad)

    def test_fresh_and_the_nonce(self):
        self.assertIn('{name: OMG_FRESH, value: "1"}', tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=NONCE, fresh=True))
        for bad in ("x", NONCE + "0", NONCE.upper(), ""):
            if bad == "":
                continue
            with self.assertRaises(tool.SparkError, msg=bad):
                tool.render_job(self.JOB, "lib", SHA, "", 90, nonce=bad)
        self.assertNotEqual(tool.render_job(self.JOB, "lib", SHA, "", 90), tool.render_job(self.JOB, "lib", SHA, "", 90), "a random nonce per rendering")

    def test_test_args_are_checked_and_limited_to_lib_and_web(self):
        self.assertIn('"--exact management::tests::x"', tool.render_job(self.JOB, "lib", SHA, "--exact management::tests::x", 60))
        tool.render_job(self.JOB, "web", SHA, "src/lib/review-history.test.ts", 60)
        for bad in ("$(id)", "a;b", "a && b", "`id`", 'a"b', "a'b", "a\nb", "a  b", "a|b", ">x", "${HOME}", "-- --x;"):
            with self.assertRaises(tool.SparkError, msg=repr(bad)):
                tool.render_job(self.JOB, "lib", SHA, bad, 60)
        for suite in ("integration", "checks", "all"):
            with self.assertRaises(tool.SparkError, msg=suite):
                tool.render_job(self.JOB, suite, SHA, "x", 60)

    def test_arguments_that_do_not_run_tests_are_refused(self):
        for bad in ("--list", "--help", "-h", "--version", "--format=json", "--format terse", "x --list", "--show-output", "--shuffle", "--shuffle-seed=3", "--report-time", "--ensure-time",
                    "--watch", "--ui", "-u", "--update", "--reporter=json", "--coverage"):
            for suite in ("lib", "web"):
                with self.assertRaises(tool.SparkError, msg=f"{suite} {bad}"):
                    tool.validate_test_args(bad, suite)
        for good in ("--exact a::b", "--ignored a_b", "metrics_export::tests", "src/x.test.ts", "--nocapture a"):
            tool.validate_test_args(good, "lib")

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

    def test_default_deadlines_per_suite(self):
        self.assertEqual(tool.DEFAULT_TIMEOUT_MINUTES, {"web": 30, "checks": 45, "integration": 60, "lib": 120, "all": 180})
        cluster = Cluster()
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).run("probe", "web", "HEAD", "", None, keep=False, detach=True, summary_path=None)
        job = next(d for _, d in cluster.calls if d and "kind: Job" in d)
        self.assertIn("activeDeadlineSeconds: 1800", job)

    def test_a_manifest_without_a_placeholder_or_with_a_leftover_is_refused(self):
        with self.assertRaises(tool.SparkError):
            tool.render_job(self.JOB.replace("__SHA__", "x"), "lib", SHA, "", 60)
        with self.assertRaises(tool.SparkError):
            tool.render_job(self.JOB + "\n# __OTHER__\n", "lib", SHA, "", 60)

    def test_the_password_secret_is_random_redacted_and_not_an_argument(self):
        first, second = json.loads(tool.superuser_secret()), json.loads(tool.superuser_secret())
        self.assertNotEqual(first["stringData"]["password"], second["stringData"]["password"])
        self.assertRegex(first["stringData"]["password"], r"^[0-9a-f]{48}$")
        self.assertEqual(first["stringData"]["username"], "postgres")
        self.assertEqual(spark_env.redact(f"url postgres://x:{first['stringData']['password']}@h"), "url postgres://x:[redacted]@h", "generated passwords are registered for redaction")


class Slots(unittest.TestCase):
    INFO = {"suite": "lib", "ref": "main", "sha": SHA, "nonce": NONCE}

    def acquire(self, cluster, name="new", info=None):
        spark = make(cluster)
        return spark, spark.acquire(name, dict(info or self.INFO))

    def test_the_first_run_creates_one_slot_namespace_and_waits_for_its_policies(self):
        cluster = Cluster()
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s1")
        commands = cluster.commands()
        created = [i for i, (c, d) in enumerate(cluster.calls) if d and '"Namespace"' in d]
        self.assertEqual(len(created), 1)
        self.assertLess(created[0], next(i for i, c in enumerate(commands) if "get networkpolicy" in c))
        self.assertLess(next(i for i, c in enumerate(commands) if "get networkpolicy" in c), next(i for i, c in enumerate(commands) if "create configmap omg-test-lease" in c))
        self.assertEqual(cluster.leases["s1"]["name"], "new")
        self.assertEqual(cluster.leases["s1"]["sha"], SHA)

    def test_a_free_existing_slot_is_preferred_to_creating_one(self):
        cluster = Cluster(namespaces={"s2"})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")
        self.assertFalse([d for _, d in cluster.calls if d and '"Namespace"' in d])

    def test_the_second_slot_is_created_when_the_first_is_busy(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease("other", "web")})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")
        self.assertEqual(len([d for _, d in cluster.calls if d and '"Namespace"' in d]), 1)

    def test_a_third_run_is_refused_with_a_clear_message_and_nothing_is_created(self):
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": lease("one"), "s2": lease("two")})
        with self.assertRaises(tool.SparkError) as caught:
            make(cluster).acquire("three", dict(self.INFO))
        message = str(caught.exception)
        for needle in ("all 2 test slots are in use", "one (lib main", "two (lib main", "down"):
            self.assertIn(needle, message)
        self.assertFalse([c for c in cluster.commands() if "create" in c])
        self.assertEqual(set(cluster.leases), {"s1", "s2"})

    def test_a_lease_created_by_someone_else_in_the_meantime_moves_to_the_next_slot(self):
        class Racing(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                joined = " ".join(command)
                if "create configmap omg-test-lease" in joined and "omg-test-s1" in joined and "s1" not in self.leases:
                    self.leases["s1"] = lease("winner", nonce="ffffffffffffffff")
                return super().__call__(command, stdin, capture)

        cluster = Racing(namespaces={"s1", "s2"})
        _, slot = self.acquire(cluster)
        self.assertEqual(slot, "s2")

    def test_a_duplicate_name_is_refused(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease("same")})
        with self.assertRaises(tool.SparkError) as caught:
            self.acquire(cluster, "same")
        self.assertIn("already exists", str(caught.exception))

    def test_the_same_name_started_twice_at_once_gives_way_for_the_later_lease(self):
        earlier = lease("same", nonce="aaaaaaaaaaaaaaaa", started_ms="1000")

        class Simultaneous(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                result = super().__call__(command, stdin, capture)
                if "create configmap omg-test-lease" in " ".join(command) and "omg-test-s1" in " ".join(command):
                    self.leases["s2"] = earlier  # the other process leased slot s2 just before
                return result

        cluster = Simultaneous(namespaces={"s1", "s2"})
        with self.assertRaises(tool.SparkError) as caught:
            self.acquire(cluster, "same", info={**self.INFO, "nonce": "bbbbbbbbbbbbbbbb"})
        self.assertIn("same time", str(caught.exception))
        self.assertNotIn("s1", cluster.leases, "the later lease is released")
        self.assertEqual(cluster.leases["s2"], earlier)

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
            self.acquire(Cluster(fail_on="create -f -"))

    def test_a_namespace_that_never_gets_its_policies_is_not_used(self):
        class Never(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                if "get networkpolicy" in " ".join(command):
                    return "networkpolicy/a\n"
                return super().__call__(command, stdin, capture)

        with self.assertRaises(tool.SparkError) as caught:
            self.acquire(Never())
        self.assertIn("two network policies", str(caught.exception))


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
        self.assertEqual([tool.quantity("requests.cpu", v) for v in ("1500m", "2", "0.5", "1e3m")], [1500, 2000, 500, 1000])
        self.assertEqual([tool.quantity("limits.memory", v) for v in ("8Gi", "512Mi", "1048576Ki", str(1024**3), "24G", "512M", "1.5Gi")], [8192, 512, 1024, 1024, 22888, 488, 1536])
        self.assertEqual(tool.quantity("requests.storage", "20Gi"), 20480)
        self.assertEqual(tool.quantity("requests.storage", "100G"), 95367)
        self.assertEqual(tool.quantity("pods", "3"), 3)
        for bad in ("", "x", "5Zi", "1.2.3"):
            with self.assertRaises(tool.SparkError, msg=bad):
                tool.quantity("limits.memory", bad)

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
        self.assertEqual(tests_limits[0], tool.DEFAULT_CPU_LIMIT_M)
        self.assertEqual(db_limits[0], tool.DB_CPU_LIMIT_M)
        self.assertEqual(needs["limits.memory"], tests_limits[1] + db_limits[1])
        self.assertEqual(needs["requests.storage"], tool.quantity("requests.storage", re.search(r"storage:\n    size: (\S+)", cluster).group(1)))
        self.assertEqual(tool.CACHE_NEEDS["requests.storage"], tool.quantity("requests.storage", re.search(r"requests: \{storage: (\S+)\}", cache).group(1)))

    def test_shortfalls(self):
        self.assertEqual(tool.quota_shortfalls(quota_json(), {**tool.RUN_NEEDS, **{"requests.storage": 9000}}), [])
        short = tool.quota_shortfalls(quota_json(**{"requests.cpu": "1250m", "pods": "2"}), tool.RUN_NEEDS)
        self.assertEqual(len(short), 2)
        self.assertIn("requests.cpu: needs 1750, 1250 available", short)
        self.assertEqual(tool.quota_shortfalls(json.dumps({"items": []}), tool.RUN_NEEDS), [])

    def test_the_smallest_room_wins_when_several_objects_define_a_key(self):
        two = json.dumps({"items": [{"status": {"hard": {"requests.cpu": "4"}, "used": {"requests.cpu": "0"}}}, {"status": {"hard": {"requests.cpu": "2"}, "used": {"requests.cpu": "1500m"}}}]})
        self.assertEqual(tool.quota_free(two, ["requests.cpu"]), {"requests.cpu": 500})
        self.assertEqual(tool.quota_free(json.dumps({"items": list(reversed(json.loads(two)["items"]))}), ["requests.cpu"]), {"requests.cpu": 500})

    def test_a_run_that_does_not_fit_is_refused_before_anything_is_created(self):
        cluster = Cluster(quota=quota_json(**{"requests.memory": "4Gi"}))
        out, err = quiet()
        with out, err, self.assertRaises(tool.SparkError) as caught:
            make(cluster).start("probe", "lib", "HEAD", "", 60)
        self.assertIn("requests.memory", str(caught.exception))
        self.assertIn("shared Spark quota", str(caught.exception))
        self.assertEqual(cluster.leases, {})
        self.assertFalse([c for c in cluster.commands() if "apply" in c or "create configmap omg-test-script" in c])

    def test_fast_uses_the_free_limit_quota_and_refuses_when_it_does_not_fit(self):
        self.assertEqual(tool.fast_cpu_limit(8000), 6000)
        self.assertEqual(tool.fast_cpu_limit(5000), 1800 + 5000 - 2800)
        self.assertIsNone(tool.fast_cpu_limit(3200))
        self.assertIsNone(tool.fast_cpu_limit(2800))
        cluster = Cluster(quota=quota_json(**{"limits.cpu": "5"}))
        with contextlib.redirect_stdout(io.StringIO()):
            slot, _ = make(cluster).start("probe", "lib", "HEAD", "", 60, fast=True)
        job = next(d for _, d in cluster.calls if d and "kind: Job" in d)
        self.assertIn('limits: {cpu: "4000m", memory: 10Gi}', job)
        self.assertIn('requests: {cpu: "1500m", memory: 6Gi}', job)
        self.assertEqual(cluster.leases[slot]["fast_cpu_limit_m"], "4000")
        full = Cluster(quota=quota_json(**{"limits.cpu": "3"}))
        with self.assertRaises(tool.SparkError) as caught:
            make(full).start("probe", "lib", "HEAD", "", 60, fast=True)
        self.assertIn("--fast does not fit", str(caught.exception))
        self.assertEqual(full.leases, {})


class Lifecycle(unittest.TestCase):
    def test_start_applies_the_cache_then_secrets_script_database_and_job_in_that_order(self):
        cluster = Cluster()
        with contextlib.redirect_stdout(io.StringIO()):
            slot, resolved = make(cluster).start("probe", "lib", "HEAD", "", 90)
        self.assertEqual(slot, "s1")
        commands = cluster.commands()
        order = [next(i for i, c in enumerate(commands) if needle in c) for needle in ("create configmap omg-test-lease", "apply -f -", "create configmap omg-test-script")]
        self.assertEqual(order, sorted(order))
        stdins = [data for _, data in cluster.calls if data and '"Namespace"' not in data]
        self.assertEqual(len(stdins), 4)
        self.assertIn("PersistentVolumeClaim", stdins[0])
        self.assertIn('"Secret"', stdins[1])
        self.assertIn("kind: Cluster", stdins[2])
        self.assertIn("kind: Job", stdins[3])
        self.assertIn(resolved.sha, stdins[3])
        self.assertRegex(cluster.leases["s1"]["nonce"], r"^[0-9a-f]{16}$")
        self.assertIn(f'value: "{cluster.leases["s1"]["nonce"]}"', stdins[3])

    def test_the_lease_records_the_narrowing_and_the_print_says_partial(self):
        cluster = Cluster()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).start("probe", "lib", "HEAD", "--ignored x", 90, fresh=True)
        self.assertEqual(cluster.leases["s1"]["test_args"], "--ignored x")
        self.assertEqual(cluster.leases["s1"]["fresh"], "1")
        self.assertIn("PARTIAL", out.getvalue())

    def test_no_secret_is_ever_a_command_line_argument(self):
        cluster = Cluster()
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).start("probe", "lib", "HEAD", "", 90)
        secret = json.loads(next(data for _, data in cluster.calls if data and '"Secret"' in data))["stringData"]["password"]
        self.assertFalse([c for c in cluster.commands() if secret in c])
        for _, data in cluster.calls:
            if data and secret in data:
                self.assertIn('"Secret"', data)

    def test_a_failed_setup_removes_the_run_and_frees_the_slot(self):
        class Failing(Cluster):
            def __call__(self, command, stdin=None, capture=False):
                if stdin and "kind: Job" in stdin:
                    raise tool.SparkError("kubectl create failed (exit 1): exceeded quota")
                return super().__call__(command, stdin, capture)

        cluster = Failing()
        out, err = quiet()
        with out, err, self.assertRaises(tool.SparkError):
            make(cluster).start("probe", "lib", "HEAD", "", 90)
        self.assertEqual(cluster.leases, {}, "the lease is released")
        self.assertTrue([c for c in cluster.commands() if "delete job omg-test" in c])

    def test_down_removes_the_run_but_never_a_namespace_and_the_lease_goes_last(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()})
        with contextlib.redirect_stdout(io.StringIO()):
            make(cluster).down("probe")
        deleted = [c for c in cluster.commands() if " delete " in c]
        self.assertTrue(deleted[-1].endswith("delete configmap omg-test-lease --ignore-not-found"), deleted)
        self.assertTrue(any("delete cluster.postgresql.cnpg.io omg-test-db" in c for c in deleted))
        self.assertFalse([c for c in deleted if "namespace" in c or "persistentvolumeclaim" in c or "pvc" in c], "the cache volume is not deleted")
        self.assertEqual(cluster.leases, {})

    def test_down_keeps_the_lease_when_something_is_left_behind(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, leftovers=1)
        with self.assertRaises(tool.SparkError) as caught:
            make(cluster).down("probe")
        self.assertIn("omg-test-db-0", str(caught.exception))
        self.assertIn("s1", cluster.leases)

    def test_down_of_an_unknown_run_changes_nothing_and_is_not_an_error(self):
        cluster = Cluster(namespaces={"s1"})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).down("ghost")
        self.assertIn("nothing to remove", out.getvalue())
        self.assertFalse([c for c in cluster.commands() if " delete " in c])

    def test_purge_deletes_idle_slot_namespaces_only(self):
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": lease("busy")})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).purge()
        deleted = [c for c in cluster.commands() if "delete namespace" in c]
        self.assertEqual(deleted, [f"kubectl --kubeconfig {CONFIG} delete namespace {BASE}-omg-test-s2 --wait=false"])
        self.assertIn("in use", out.getvalue())

    def test_purge_skips_a_namespace_that_is_already_being_deleted(self):
        cluster = Cluster(terminating={"s1"})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).purge()
        self.assertFalse([c for c in cluster.commands() if "delete namespace" in c])
        self.assertIn("already being deleted", out.getvalue())

    def test_status_lists_slots(self):
        cluster = Cluster(namespaces={"s1", "s2"}, leases={"s1": lease("busy", test_args="--exact a")})
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            make(cluster).status()
        self.assertIn("s1: busy (lib main", out.getvalue())
        self.assertIn("--test-args '--exact a'", out.getvalue())
        self.assertIn("s2: free", out.getvalue())

    def test_run_waits_prints_the_result_and_downs(self):
        cluster = Primed(job="complete", log=log_for("web", passed=1903))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = make(cluster).run("probe", "web", "HEAD", "", 60, keep=False, detach=False, summary_path=None)
        self.assertEqual(code, 0)
        self.assertIn("RESULT: passed: web of HEAD", out.getvalue())
        self.assertIn("Tests  1903 passed (1903)", out.getvalue())
        self.assertEqual(cluster.leases, {})

    def _run_with_log(self, log, suite="lib", job="complete", test_args="", pod=None, keep=False, log_path=None):
        cluster = Primed(job=job, log=log, pod=pod)
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(out):
            summary = Path(folder) / "summary.json"
            tool.LOG_DIR = Path(folder) / "logs"
            code = make(cluster).run("probe", suite, "HEAD", test_args, 60, keep=keep, detach=False, summary_path=str(summary), log_path=log_path)
            data = json.loads(summary.read_text())
            logs = sorted(p.name for p in tool.LOG_DIR.glob("*")) if tool.LOG_DIR.exists() else []
            self.assertEqual(stat.S_IMODE(summary.stat().st_mode), 0o600)
        return code, data, out.getvalue(), cluster, logs

    def test_a_failing_run_exits_one_names_the_failures_and_saves_a_log_before_it_is_removed(self):
        log = log_for("lib") + "\ntest bad::one ... FAILED\n---- bad::one stdout ----\nthread 'bad::one' panicked at x.rs:1:1:\nassertion failed\n\nfailures:\n    bad::one\n"
        failing = log.replace(marker("step-end", suite="lib", name="shard-1", exit=0, seconds=1), marker("step-end", suite="lib", name="shard-1", exit=101, seconds=1))
        code, summary, printed, cluster, logs = self._run_with_log(failing)
        self.assertEqual(code, 1)
        self.assertEqual(summary["result"], "failed")
        self.assertEqual(summary["failed_tests"], ["bad::one"])
        self.assertIn("failure output (sanitised, bounded):", printed)
        self.assertIn("assertion failed", printed)
        self.assertEqual(logs, [f"probe-{SHA[:12]}.log"])
        self.assertEqual(cluster.leases, {}, "the run is removed after the log is saved")

    def test_a_killed_run_is_not_a_failing_test(self):
        code, summary, printed, cluster, _ = self._run_with_log(log_for("lib", done=False), job="failed", pod={"containerStatuses": [{"name": "tests", "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]})
        self.assertEqual(code, 2)
        self.assertEqual(summary["result"], "killed")
        self.assertEqual(summary["pod_reason"], "OOMKilled")
        self.assertIn("RESULT: killed", printed)

    def test_an_evicted_pod_and_a_deadline_are_recorded(self):
        _, summary, _, _, _ = self._run_with_log(log_for("lib", done=False), job="failed", pod={"reason": "Evicted", "containerStatuses": []})
        self.assertEqual((summary["result"], summary["pod_reason"]), ("killed", "Evicted"))
        _, summary, _, _, _ = self._run_with_log(log_for("lib", done=False), job="deadline", pod={"containerStatuses": []})
        self.assertEqual((summary["result"], summary["pod_reason"]), ("killed", "DeadlineExceeded"))

    def test_a_narrowed_run_is_partial_and_never_exit_zero(self):
        log = log_for("web", passed=20)
        code, summary, printed, _, _ = self._run_with_log(log, suite="web", test_args="deployment-preview-focus")
        self.assertEqual(code, 3)
        self.assertTrue(summary["partial"])
        self.assertEqual(summary["test_args"], "deployment-preview-focus")
        self.assertEqual(summary["result"], "passed")
        self.assertIn("RESULT: passed (partial)", printed)
        self.assertTrue(any("partial" in note for note in summary["notes"]))

    def test_a_run_that_executed_no_tests_is_failed(self):
        zero = log_for("web", passed=0)
        code, summary, printed, _, _ = self._run_with_log(zero, suite="web")
        self.assertEqual((code, summary["result"]), (1, "failed"))
        self.assertIn("suite web executed no tests", summary["notes"])
        self.assertIn("RESULT: failed", printed)
        # a log without any result line at all
        bare = "\n".join([marker("source", commit=SHA), marker("done", exit=0, seconds=1)]) + "\n"
        code, summary, _, _, _ = self._run_with_log(bare, suite="lib")
        self.assertEqual((code, summary["result"]), (1, "failed"))
        self.assertTrue(any("did not run" in n for n in summary["notes"]))
        # a filter that matches nothing: libtest runs 0 tests
        filtered = log_for("lib", passed=0, ignored=0, steps=["lib/npm-ci", "lib/build"]).replace(marker("done", exit=0, seconds=9), "") \
            + marker("step-start", suite="lib", name="filtered") + "\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 2465 filtered out; finished in 0.00s\n" \
            + marker("step-end", suite="lib", name="filtered", exit=0, seconds=0) + "\n" + marker("done", exit=0, seconds=3) + "\n"
        filtered = filtered.replace(marker("lib-inventory", tests=0) + "\n", "")
        code, summary, _, _, _ = self._run_with_log(filtered, suite="lib", test_args="no_such_test")
        self.assertEqual((code, summary["result"]), (1, "failed"))
        self.assertIn("suite lib executed no tests", summary["notes"])

    def test_a_step_that_did_not_run_fails_an_exit_zero_run(self):
        log = log_for("integration", passed=170, ignored=10, steps=["integration/npm-ci", "integration/targets"])
        code, summary, _, _, _ = self._run_with_log(log, suite="integration")
        self.assertEqual((code, summary["result"]), (1, "failed"))
        self.assertIn("step integration/tools did not run", summary["notes"])

    def test_a_setup_failure_reports_the_reason_not_a_stopped_pod(self):
        log = "\n".join([marker("run", suite="lib", commit=SHA), "cannot fetch " + SHA + " from https://github.com/x/y.git: push the branch first", marker("step-end", suite="setup", name="source", exit=1, seconds=2),
                         marker("setup-failed"), "\x1b]0;evil title\x07\x1b[31mred\x1b[0m"]) + "\n"
        code, summary, printed, _, _ = self._run_with_log(log, job="failed")
        self.assertEqual((code, summary["result"]), (2, "setup failed"))
        self.assertIn("RESULT: setup failed", printed)
        self.assertIn("push the branch first", printed)
        self.assertNotIn("pod was stopped", printed)
        self.assertNotIn("\x1b", printed)
        self.assertNotIn("evil title\x07", printed)

    def test_the_log_is_saved_for_a_pass_only_when_asked(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "run.log"
            _, _, _, _, logs = self._run_with_log(log_for("web", passed=1903), suite="web", log_path=str(path))
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertNotIn("\x1b", path.read_text())
        _, _, _, _, logs = self._run_with_log(log_for("web", passed=1903), suite="web")
        self.assertEqual(logs, [])

    def test_keep_and_detach_leave_the_run_in_place_and_detach_is_not_a_pass(self):
        cluster = Primed(job="complete", log=log_for("web", passed=1903))
        with contextlib.redirect_stdout(io.StringIO()):
            code = make(cluster).run("probe", "web", "HEAD", "", 60, keep=True, detach=False, summary_path=None)
        self.assertIn("s1", cluster.leases)
        cluster = Cluster()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = make(cluster).run("probe", "all", "HEAD", "", 60, keep=False, detach=True, summary_path=None)
        self.assertEqual(code, tool.EXIT_STARTED)
        self.assertNotEqual(code, 0)
        self.assertIn("nothing is known about the outcome", out.getvalue())
        self.assertIn("s1", cluster.leases)

    def test_a_bad_summary_path_is_refused_before_anything_starts(self):
        cluster = Cluster()
        with tempfile.TemporaryDirectory() as folder:
            for bad in (str(Path(folder) / "no" / "such" / "dir" / "s.json"), folder):
                with self.assertRaises(tool.SparkError, msg=bad):
                    make(cluster).run("probe", "web", "HEAD", "", 60, keep=False, detach=True, summary_path=bad)
        self.assertEqual(cluster.calls, [], "not one command ran")

    def test_a_run_that_has_not_finished_has_no_result(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job="running")
        with self.assertRaises(tool.SparkError):
            make(cluster).result("probe")

    def test_a_job_that_ran_another_commit_is_refused(self):
        other = log_for("lib").replace(f"commit={SHA}", "commit=" + "f" * 40)
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job="complete", log=other)
        with self.assertRaises(tool.SparkError):
            make(cluster).result("probe")

    def test_an_error_while_waiting_keeps_the_run_and_says_so(self):
        cluster = Cluster(job="running")
        spark = make(cluster)
        err = io.StringIO()

        def broken(*args, **kwargs):
            raise tool.SparkError("boom")

        spark.wait = broken
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err), self.assertRaises(tool.SparkError):
            spark.run("probe", "web", "HEAD", "", 60, keep=False, detach=False, summary_path=None)
        self.assertIn("run kept: `down probe`", err.getvalue())
        self.assertIn("s1", cluster.leases)


class Waiting(unittest.TestCase):
    def clock(self):
        now = [10_000]

        def sleep(seconds):
            now[0] += seconds

        return (lambda: now[0]), sleep

    def waiter(self, cluster):
        clock, sleep = self.clock()
        return tool.SparkTest(CONFIG, runner=cluster, clock=clock, sleep=sleep)

    def test_a_pod_that_cannot_pull_its_image_fails_the_wait_with_the_reason_and_events(self):
        pending = {"phase": "Pending", "containerStatuses": [{"name": "tests", "state": {"waiting": {"reason": "ImagePullBackOff", "message": "Back-off pulling image x"}}}]}
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job="running", pod=pending)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
            self.waiter(cluster).wait("probe", "s1", lease())
        message = str(caught.exception)
        for needle in ("ImagePullBackOff", "FailedScheduling", "run kept: `down probe`"):
            self.assertIn(needle, message)

    def test_unschedulable_and_unbound_volumes_are_stuck_too(self):
        for condition in ({"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/1 nodes: Insufficient cpu"},
                          {"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "pod has unbound immediate PersistentVolumeClaims"}):
            cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job="running", pod={"phase": "Pending", "containerStatuses": [], "conditions": [condition]})
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
                self.waiter(cluster).wait("probe", "s1", lease())
            self.assertIn("Unschedulable", str(caught.exception))
        self.assertIsNone(tool.stuck_reason({"status": {"phase": "Running"}}))
        self.assertIsNone(tool.stuck_reason({"status": {"phase": "Pending", "containerStatuses": [{"state": {"waiting": {"reason": "ContainerCreating"}}}]}}))

    def test_a_brief_stall_does_not_fail_the_wait(self):
        calls = {"n": 0}

        class Slowly(Cluster):
            def __call__(inner, command, stdin=None, capture=False):
                if "get pods -l job-name=omg-test" in " ".join(command):
                    calls["n"] += 1
                    inner.job = "complete" if calls["n"] > 5 else "running"
                return super().__call__(command, stdin, capture)

        cluster = Slowly(namespaces={"s1"}, leases={"s1": lease()}, job="running", pod={"phase": "Pending", "containerStatuses": [{"state": {"waiting": {"reason": "ErrImagePull"}}}]})
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.waiter(cluster).wait("probe", "s1", lease()), "succeeded")

    def test_a_vanished_job_ends_the_wait(self):
        class Vanishing(Cluster):
            polls = 0

            def __call__(inner, command, stdin=None, capture=False):
                if "get job/omg-test" in " ".join(command):
                    Vanishing.polls += 1
                    inner.job = "running" if Vanishing.polls < 3 else None
                return super().__call__(command, stdin, capture)

        cluster = Vanishing(namespaces={"s1"}, leases={"s1": lease()}, job="running")
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
            self.waiter(cluster).wait("probe", "s1", lease())
        self.assertIn("vanished", str(caught.exception))

    def test_a_job_missing_on_the_first_poll_ends_the_wait(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job=None)
        now = [10_000]
        def bounded_sleep(seconds):
            now[0] += seconds
            self.assertLess(now[0], 10_120, "missing Job kept waiting")
        waiter = tool.SparkTest(CONFIG, runner=cluster, clock=lambda: now[0], sleep=bounded_sleep)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(tool.SparkError, "vanished"):
            waiter.wait("probe", "s1", lease())

    def test_a_lease_that_is_gone_or_belongs_to_another_run_ends_the_wait(self):
        for replacement in (None, lease("probe", nonce="ffffffffffffffff"), lease("other"), {**lease(), "sha": "f" * 40}):
            leases = {} if replacement is None else {"s1": replacement}
            cluster = Cluster(namespaces={"s1"}, leases=leases, job="complete", log=log_for("web", passed=1903))
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError, msg=str(replacement)) as caught:
                self.waiter(cluster).wait("probe", "s1", lease())
            self.assertIn("no longer belongs", str(caught.exception))

    def test_a_few_transient_errors_are_tolerated_and_many_are_not(self):
        class Flaky(Cluster):
            failures = 0

            def __call__(inner, command, stdin=None, capture=False):
                if "get job/omg-test" in " ".join(command) and Flaky.failures < inner.limit:
                    Flaky.failures += 1
                    raise tool.SparkError("kubectl get failed (exit 1): connection reset")
                return super().__call__(command, stdin, capture)

        cluster = Flaky(namespaces={"s1"}, leases={"s1": lease()}, job="complete")
        cluster.limit = 3
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.waiter(cluster).wait("probe", "s1", lease()), "succeeded")
        Flaky.failures = 0
        cluster = Flaky(namespaces={"s1"}, leases={"s1": lease()}, job="complete")
        cluster.limit = 99
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
            self.waiter(cluster).wait("probe", "s1", lease())
        self.assertIn("consecutive kubectl errors", str(caught.exception))

    def test_a_job_without_a_pod_names_the_warning_events(self):
        cluster = Cluster(namespaces={"s1"}, leases={"s1": lease()}, job="running", pod="none")
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(tool.SparkError) as caught:
            self.waiter(cluster).wait("probe", "s1", lease())
        self.assertIn("FailedScheduling", str(caught.exception))


class Reading(unittest.TestCase):
    def test_a_passing_log(self):
        parsed = tool.parse_log(log_for("lib"), NONCE)
        self.assertEqual(parsed["suites"]["lib"], {"passed": 2456, "failed": 0, "ignored": 9, "targets": 4})
        self.assertEqual(parsed["commit"], SHA)
        self.assertEqual(parsed["done"], {"exit": 0, "seconds": 9})
        summary = tool.build_summary("probe", "lib", "main", SHA, parsed, "complete", {}, 3310)
        self.assertEqual((summary["result"], summary["exit_code"], summary["duration_seconds"], summary["wall_seconds"], summary["partial"]), ("passed", 0, 9, 3310, False))
        self.assertEqual(summary["counts"], {"passed": 2456, "failed": 0, "ignored": 9})
        self.assertEqual(summary["lib_tests_listed"], 2465)
        json.dumps(summary)

    def test_every_suite_lists_what_it_does_not_cover(self):
        for suite in tool.SUITES:
            covered = tool.build_summary("probe", suite, "main", SHA, tool.parse_log(log_for(suite, passed=1904 if suite == "web" else 2456), NONCE), "complete", {}, 1)["not_covered"]
            joined = " ".join(covered)
            for needle in ("amd64", "browser suite", "container and staging"):
                self.assertIn(needle, joined, suite)
        self.assertIn("filestore_s3", " ".join(tool.not_covered("integration")))
        self.assertIn("backup", " ".join(tool.not_covered("integration")))
        self.assertIn("npm run test:supply-chain", " ".join(tool.not_covered("web")))
        self.assertIn("npm run test:structure", " ".join(tool.not_covered("all")))
        self.assertNotIn("test:supply-chain", " ".join(tool.not_covered("lib")))

    def test_a_sharded_lib_run_is_checked_against_the_listed_inventory(self):
        good = log_for("lib")
        self.assertEqual(tool.build_summary("p", "lib", "main", SHA, tool.parse_log(good, NONCE), "complete", {}, 9)["result"], "passed")
        short = good.replace("lib-inventory tests=2465", "lib-inventory tests=2600")
        summary = tool.build_summary("p", "lib", "main", SHA, tool.parse_log(short, NONCE), "complete", {}, 9)
        self.assertEqual((summary["result"], summary["exit_code"]), ("failed", 1))
        self.assertIn("lists 2600 tests but the shards account for 2465", " ".join(summary["notes"]))

    def test_markers_without_the_nonce_or_malformed_are_ignored_never_a_traceback(self):
        forged = [
            "::omg-test:: done exit=0 seconds=1",
            marker("done", nonce="ffffffffffffffff", exit=0, seconds=1),
            f"::omg-test::{NONCE}:: done exit=abc seconds=1",
            f"::omg-test::{NONCE}:: done exit= seconds=1",
            f"::omg-test::{NONCE}:: done",
            f"::omg-test::{NONCE}:: done exit=0 seconds=99999999999",
            f"::omg-test::{NONCE}:: step-end suite=lib name=../x exit=0 seconds=1",
            f"::omg-test::{NONCE}:: step-end suite=lib name=ok exit=0 seconds=1 extra",
            f"::omg-test::{NONCE}:: step-end suite=lib name=ok exit=0 seconds=1 bad=\x1b[31m",
            f"::omg-test::{NONCE}:: unknown-kind a=b",
            f"::omg-test::{NONCE}::done exit=0 seconds=1",
            f" ::omg-test::{NONCE}:: done exit=0 seconds=1",
            f"::omg-test::{NONCE}:: source commit=nothex",
        ]
        parsed = tool.parse_log("\n".join(forged) + "\n", NONCE)
        self.assertIsNone(parsed["done"])
        self.assertEqual(parsed["steps"], [])
        self.assertIsNone(parsed["commit"])
        self.assertEqual(tool.build_summary("p", "lib", "main", SHA, parsed, "complete", {}, 5)["result"], "killed")
        self.assertIsNone(tool.parse_marker("::omg-test:: done exit=0 seconds=1", "not-a-nonce"))
        self.assertIsNone(tool.parse_marker(f"::omg-test::{NONCE}:: done exit=0 seconds=1", ""))
        self.assertEqual(tool.parse_marker(f"::omg-test::{NONCE}:: done exit=0 seconds=1", NONCE), ("done", {"exit": "0", "seconds": "1"}))

    def test_a_forged_result_line_cannot_change_an_exit_status(self):
        # a failing run (the step exited 101) whose output also claims success stays failed
        log = log_for("web", passed=1903).replace(marker("step-end", suite="web", name="vitest", exit=0, seconds=1), "test result: ok. 99 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n" + marker("step-end", suite="web", name="vitest", exit=1, seconds=1))
        summary = tool.build_summary("p", "web", "main", SHA, tool.parse_log(log, NONCE), "complete", {}, 5)
        self.assertEqual(summary["result"], "failed")

    def test_result_lines_are_anchored_and_only_captured_numbers_are_printed(self):
        lines = [
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s",
            "xx test result: ok. 700 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: ok. 1234567890 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s; <script>alert(1)</script>",
            "      Tests  3 passed (3) and then some",
            "ATests  9 passed (9)",
            " FAILURE  not-a-fail",
        ]
        parsed = tool.parse_log(marker("step-start", suite="lib", name="shard-0") + "\n" + "\n".join(lines) + "\n", NONCE)
        self.assertEqual(parsed["results"], ["test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"])
        self.assertEqual(parsed["suites"]["lib"]["passed"], 5)
        self.assertEqual(parsed["vitest"], [])
        self.assertEqual(parsed["failed_tests"], [])

    def test_test_names_are_printable_ascii_bounded_and_escape_free(self):
        hostile = "test " + "a" * 500 + " ... FAILED"
        osc = "test bad\x1b]8;;http://evil/\x07name\x1b]8;;\x07 ... FAILED"
        csi = "test \x1b[31mred\x1b[0m::t ... FAILED"
        dcs = "test x\x1bPpayload\x1b\\y ... FAILED"
        unicode = "test café‮::t ... FAILED"
        parsed = tool.parse_log("\n".join([hostile, osc, csi, dcs, unicode, " FAIL  src/a.test.ts > \x1b[1mbold\x1b[0m > t"]) + "\n", NONCE)
        for name in parsed["failed_tests"]:
            self.assertLessEqual(len(name), tool.NAME_LIMIT)
            self.assertRegex(name, r"^[ -~]*$")
        self.assertIn("bad" + "name", parsed["failed_tests"])
        self.assertIn("red::t", parsed["failed_tests"])
        self.assertIn("xy", parsed["failed_tests"])
        self.assertIn("src/a.test.ts > bold > t", parsed["failed_tests"])
        self.assertEqual(tool.sanitize("a\x1b]0;title\x1b\\b\x1b]0;unterminated"), "ab")
        self.assertEqual(tool.sanitize("\x9b31mz\x00\x7f"), "31mz", "8-bit controls are dropped as non-printable")

    def test_a_forged_marker_in_the_tail_of_a_setup_failure_is_not_shown(self):
        log = "\n".join(["::omg-test:: step-end suite=x name=y exit=0 seconds=0", "plain line", marker("setup-failed")]) + "\n"
        parsed = tool.parse_log(log, NONCE)
        self.assertTrue(parsed["setup_failed"])
        self.assertEqual(parsed["tail"], ["plain line"])

    def test_a_failing_log(self):
        log = log_for("lib") + "test bad::one ... FAILED\ntest bad::two ... FAILED\n"
        log = log.replace(marker("step-end", suite="lib", name="shard-0", exit=0, seconds=1), marker("step-end", suite="lib", name="shard-0", exit=101, seconds=1))
        summary = tool.build_summary("probe", "lib", "main", SHA, tool.parse_log(log, NONCE), "failed", {}, 61)
        self.assertEqual((summary["result"], tool.result_exit(summary)), ("failed", 1))
        self.assertEqual(summary["failed_steps"], ["lib/shard-0"])
        self.assertEqual(summary["failed_tests"], ["bad::one", "bad::two"])

    def test_a_passing_log_of_a_failed_job_is_still_failed(self):
        summary = tool.build_summary("probe", "lib", "main", SHA, tool.parse_log(log_for("lib"), NONCE), "failed", {}, 5)
        self.assertEqual(summary["result"], "failed")

    def test_a_run_killed_before_its_end_marker_reports_the_pod(self):
        parsed = tool.parse_log(marker("step-start", suite="lib", name="lib") + "\nrunning 5 tests\n", NONCE)
        summary = tool.build_summary("probe", "lib", "main", SHA, parsed, "failed", {"exit_code": 137, "reason": "OOMKilled"}, 120)
        self.assertEqual((summary["result"], summary["exit_code"], summary["pod_reason"]), ("killed", 137, "OOMKilled"))
        self.assertEqual(tool.result_exit(summary), 2)
        self.assertEqual(tool.build_summary("probe", "lib", "main", SHA, parsed, "failed", {}, 1)["exit_code"], 1, "an unknown exit is a failure, never 0")

    def test_failure_blocks_are_bounded_and_sanitised(self):
        body = "".join(f"line {i} \x1b[31mred\x1b[0m\n" for i in range(100))
        text = f"failures:\n\n---- a::b stdout ----\n{body}\n---- c::d stdout ----\nother\n\nfailures:\n    a::b\n"
        blocks = tool.failure_blocks(text, ["a::b"])
        self.assertEqual([name for name, _ in blocks], ["a::b"])
        self.assertEqual(len(blocks[0][1]), tool.FAILURE_BLOCK_LINES)
        self.assertEqual(blocks[0][1][0], "line 0 red")
        self.assertEqual(tool.failure_blocks(text, ["nothing"]), [])

    def test_markers_and_noise(self):
        self.assertEqual(tool.parse_log("", NONCE)["suites"], {})

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
            with self.assertRaises(tool.SparkError):
                tool.write_summary(str(link), {"a": 1})
            with self.assertRaises(tool.SparkError):
                tool.check_writable(str(link))
            self.assertEqual(target.read_text(), "keep")
            path = Path(folder) / "ok.json"
            tool.write_summary(str(path), {"a": 1})
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_existing_output_files_become_private_before_writing(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "existing"
            for operation in ("check", "write"):
                with self.subTest(operation=operation):
                    path.write_text("keep")
                    path.chmod(0o644)
                    if operation == "check":
                        tool.check_writable(path)
                        self.assertEqual(path.read_text(), "keep")
                    else:
                        tool.write_private(path, "new")
                        self.assertEqual(path.read_text(), "new")
                    self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_a_saved_log_is_sanitised_and_redacted(self):
        secret = json.loads(tool.superuser_secret())["stringData"]["password"]
        text = f"ok \x1b[31mred\x1b[0m \x1b]0;t\x07 postgres://postgres:{secret}@h/db\n"
        saved = tool.sanitized_log(text)
        self.assertNotIn(secret, saved)
        self.assertNotIn("\x1b", saved)
        self.assertIn("ok red", saved)

    def test_elapsed_text(self):
        self.assertEqual(tool.elapsed_text(75), "1m15s")
        self.assertEqual(tool.elapsed_text(3725), "1h02m05s")
        self.assertEqual(tool.elapsed_text(-5), "0m00s")


class Main(unittest.TestCase):
    def test_an_unexpected_exception_is_exit_two_without_a_traceback(self):
        err = io.StringIO()
        original = tool.SparkTest.status
        tool.SparkTest.status = lambda self, name=None: (_ for _ in ()).throw(ZeroDivisionError("bug \x1b[31m"))
        with tempfile.TemporaryDirectory() as folder:
            config = Path(folder) / "spark.yaml"
            config.write_text("x")
            old = os.environ.get("SPARK_KUBECONFIG")
            os.environ["SPARK_KUBECONFIG"] = str(config)
            try:
                with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as caught:
                    tool.main(["status"])
            finally:
                tool.SparkTest.status = original
                if old is None:
                    del os.environ["SPARK_KUBECONFIG"]
                else:
                    os.environ["SPARK_KUBECONFIG"] = old
        self.assertEqual(caught.exception.code, 2)
        self.assertIn("internal error (ZeroDivisionError)", err.getvalue())
        self.assertNotIn("Traceback", err.getvalue())
        self.assertNotIn("\x1b", err.getvalue())

    def test_the_exit_statuses_are_distinct(self):
        self.assertEqual([tool.EXIT_PASSED, tool.EXIT_FAILED, tool.EXIT_ERROR, tool.EXIT_PARTIAL, tool.EXIT_STARTED], [0, 1, 2, 3, 4])
        doc = tool.__doc__
        for fragment in ("0 every suite passed in full", "1 a suite failed", "2 an error of the tool", "3 passed but partial", "4 `run --detach`"):
            self.assertIn(fragment, doc)


if __name__ == "__main__":
    unittest.main()

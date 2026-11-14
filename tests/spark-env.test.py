"""Spark test-platform script safety tests. No cluster, registry or network access."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import re
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("spark_env", ROOT / "scripts" / "spark-env.py")
spark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(spark)

SERVER = "https://bitop-spark.taila06ed.ts.net:6443"
DIGEST = "sha256:" + "a" * 64
ARM_DIGEST = "sha256:" + "b" * 64


def config_view(server=SERVER, namespace="coder-03"):
    context = {"cluster": "c", "user": "u"}
    if namespace is not None:
        context["namespace"] = namespace
    return json.dumps({
        "clusters": [{"name": "c", "cluster": {"server": server}}],
        "contexts": [{"name": "c", "context": context}],
        "current-context": "c",
    })


def index(*platforms):
    manifests = [{"digest": ARM_DIGEST, "platform": {"os": os, "architecture": arch}} for os, arch in platforms]
    manifests.append({"digest": "sha256:" + "c" * 64, "platform": {"os": "unknown", "architecture": "unknown"}})
    return {"manifests": manifests}


class Recorder:
    """A fake `run`: records every command and its stdin, answers a few read-only queries."""

    def __init__(self, exists=False):
        self.calls = []
        self.exists = exists

    def __call__(self, command, input=None, capture=False):
        self.calls.append((list(command), input))
        joined = " ".join(command)
        if "delete namespace" in joined:
            self.exists = False
        if "config view" in joined:
            return config_view()
        if "get namespace" in joined and joined.endswith("-o name"):
            return "namespace/x\n" if self.exists else ""
        if "get networkpolicy" in joined:
            return "networkpolicy/a\nnetworkpolicy/b\n"
        return ""

    def commands(self):
        return [" ".join(command) for command, _ in self.calls]


class Names(unittest.TestCase):
    def test_valid_and_invalid_names(self):
        for name in ("t599", "my-branch-1", "ab", "a" * 20):
            self.assertEqual(spark.validate_name(name), name)
        for name in ("", "a", "a" * 21, "UPPER", "-x", "x-", "a_b", "a/b", "a.b", "x y", "../x", "a\n"):
            with self.assertRaises(spark.SparkError, msg=repr(name)):
                spark.validate_name(name)

    def test_namespace_is_base_omg_name(self):
        self.assertEqual(spark.namespace_for("coder-03", "t599"), "coder-03-omg-t599")

    def test_only_environment_namespaces_are_accepted(self):
        spark.check_namespace("coder-03", "coder-03-omg-t599")
        for namespace in ("coder-03", "coder-03-omg", "coder-03-omg-", "coder-03-other", "coder-02-omg-x", "default", "kube-system", "coder-03-omg-UP", "coder-03-omg-x y", "coder-03-omg-" + "a" * 21):
            with self.assertRaises(spark.SparkError, msg=namespace):
                spark.check_namespace("coder-03", namespace)


class Kubeconfig(unittest.TestCase):
    def test_missing_file_is_refused_and_never_falls_back(self):
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaises(spark.SparkError):
                spark.kubeconfig_path({"SPARK_KUBECONFIG": str(Path(folder) / "absent.yaml")})
            with self.assertRaises(spark.SparkError):
                spark.kubeconfig_path({"HOME": folder})

    def test_default_and_override_paths(self):
        with tempfile.TemporaryDirectory() as folder:
            default = Path(folder) / ".kube" / "spark-k3s.yaml"
            default.parent.mkdir()
            default.write_text("x")
            self.assertEqual(spark.kubeconfig_path({"HOME": folder}), default)
            other = Path(folder) / "other.yaml"
            other.write_text("x")
            self.assertEqual(spark.kubeconfig_path({"SPARK_KUBECONFIG": str(other), "HOME": folder}), other)
            # The ambient KUBECONFIG (which may name production) is never consulted.
            self.assertEqual(spark.kubeconfig_path({"KUBECONFIG": "/etc/prod.yaml", "HOME": folder}), default)

    def test_only_the_spark_cluster_is_accepted(self):
        self.assertEqual(spark.base_namespace(config_view()), "coder-03")
        for text in (config_view(server="https://203.0.113.9:6443"), config_view(server="https://bitop-spark.taila06ed.ts.net.evil.example:6443"), config_view(server="https://evil.example:6443/bitop-spark.taila06ed.ts.net"),
                     config_view(namespace=None), config_view(namespace="default"), "{}", "not json"):
            with self.assertRaises(spark.SparkError, msg=text[:60]):
                spark.base_namespace(text)


class Commands(unittest.TestCase):
    def test_every_command_carries_the_spark_kubeconfig(self):
        cfg = Path("/home/u/.kube/spark-k3s.yaml")
        kubectl = spark.kubectl_command(cfg, "coder-03", ["get", "pods"], namespace="coder-03-omg-t599")
        self.assertEqual(kubectl, ["kubectl", "--kubeconfig", str(cfg), "-n", "coder-03-omg-t599", "get", "pods"])
        helm = spark.helm_command(cfg, "coder-03", ["install", "omg", "chart"], namespace="coder-03-omg-t599")
        self.assertEqual(helm[:5], ["helm", "--kubeconfig", str(cfg), "-n", "coder-03-omg-t599"])
        self.assertEqual(spark.kubectl_command(cfg, "coder-03", ["get", "namespace"]), ["kubectl", "--kubeconfig", str(cfg), "get", "namespace"])

    def test_foreign_namespaces_cannot_be_targeted(self):
        cfg = Path("/k")
        for namespace in ("default", "coder-03", "kube-system", "coder-03-omg"):
            with self.assertRaises(spark.SparkError):
                spark.kubectl_command(cfg, "coder-03", ["get", "pods"], namespace=namespace)
            with self.assertRaises(spark.SparkError):
                spark.helm_command(cfg, "coder-03", ["list"], namespace=namespace)

    def test_subprocesses_start_only_in_run(self):
        source = (ROOT / "scripts" / "spark-env.py").read_text()
        self.assertEqual(len(re.findall(r"subprocess\.(?:run|Popen|call|check_output|check_call)", source)), 2, "run() and forward() only")
        self.assertNotRegex(source, r"shell\s*=\s*True")
        self.assertNotIn("os.system", source)
        # No tool is named outside the two command builders.
        self.assertEqual(len(re.findall(r'"kubectl"|"helm"', source)), 2)


class Images(unittest.TestCase):
    def test_release_tags_keep_their_name_and_other_refs_use_the_commit(self):
        sha = "0123456789abcdef" * 2 + "01234567"
        self.assertEqual(spark.image_tag("v0.16.0", sha, is_tag=True), "v0.16.0")
        self.assertEqual(spark.image_tag("v1.2.3-rc.1", sha, is_tag=True), "v1.2.3-rc.1")
        self.assertEqual(spark.image_tag("issue/599-spark-env", sha, is_tag=False), "dev-0123456789ab")
        self.assertEqual(spark.image_tag("v0.16.0", sha, is_tag=False), "dev-0123456789ab", "a branch named like a release is not a release")
        self.assertEqual(spark.image_tag("nightly", sha, is_tag=True), "dev-0123456789ab", "only release-shaped tags are published as tags")

    def test_arm64_must_be_present(self):
        self.assertEqual(spark.arm64_manifest(index(("linux", "amd64"), ("linux", "arm64"))), ARM_DIGEST)
        for missing in (index(("linux", "amd64")), index(("windows", "arm64")), {}, {"manifests": []}):
            with self.assertRaises(spark.SparkError):
                spark.arm64_manifest(missing)

    def test_registry_lookup_returns_the_index_digest(self):
        responses = {
            "https://ghcr.io/token?service=ghcr.io&scope=repository:ncecere/open-model-gateway:pull": (200, {}, json.dumps({"token": "t"})),
            "https://ghcr.io/v2/ncecere/open-model-gateway/manifests/v0.16.0": (200, {"Docker-Content-Digest": DIGEST}, json.dumps(index(("linux", "arm64")))),
        }
        seen = []

        def fetch(url, headers):
            seen.append((url, dict(headers)))
            return responses[url]

        self.assertEqual(spark.lookup_image("v0.16.0", fetch), (DIGEST, ARM_DIGEST))
        self.assertIn("Bearer t", seen[1][1]["Authorization"])
        self.assertNotIn("Authorization", seen[0][1], "anonymous pull")

    def test_a_missing_image_is_a_clear_error(self):
        def fetch(url, headers):
            return (200, {}, json.dumps({"token": "t"})) if "token" in url else (404, {}, "{}")

        with self.assertRaisesRegex(spark.SparkError, "branch-images"):
            spark.lookup_image("dev-0123456789ab", fetch)

    def test_a_malformed_digest_header_is_refused(self):
        def fetch(url, headers):
            return (200, {}, json.dumps({"token": "t"})) if "token" in url else (200, {"Docker-Content-Digest": "sha256:zz"}, json.dumps(index(("linux", "arm64"))))

        with self.assertRaises(spark.SparkError):
            spark.lookup_image("v0.16.0", fetch)


class Manifests(unittest.TestCase):
    def test_image_substitution_is_complete_and_strict(self):
        image = f"ghcr.io/ncecere/open-model-gateway@{DIGEST}"
        for name in ("bootstrap-job.yaml", "demo.yaml"):
            text = spark.render((ROOT / "deploy" / "spark" / name).read_text(), image)
            self.assertNotIn("__IMAGE__", text)
            self.assertIn(image, text)
        with self.assertRaises(spark.SparkError):
            spark.render("image: __IMAGE__", "bad image")
        with self.assertRaises(spark.SparkError):
            spark.render("image: x", image)

    def test_demo_pod_is_loopback_only_and_not_exposed(self):
        demo = (ROOT / "deploy" / "spark" / "demo.yaml").read_text()
        values = (ROOT / "deploy" / "spark" / "values.yaml").read_text()
        self.assertIn('GATEWAY_LISTEN, value: "127.0.0.1:3000"', demo)
        self.assertIn("127.0.0.1:18084", demo)
        for text in (demo, values, (ROOT / "deploy" / "spark" / "bootstrap-job.yaml").read_text()):
            self.assertNotRegex(text, r"(?m)^kind: (Service|Ingress)\b")
            self.assertNotRegex(text, r"hostNetwork|hostPort|hostPath|privileged:\s*true|NodePort|LoadBalancer")
        self.assertIn("storageClass: local-path", values)
        self.assertIn("postgresql:17.", values)
        self.assertIn("database: gateway_enterprise_demo", values)

    def test_secrets_are_basic_auth_and_distinct(self):
        text = spark.role_secrets()
        data = json.loads(text)
        names = [item["metadata"]["name"] for item in data["items"]]
        self.assertEqual(names, ["omg-migrator-role", "omg-runtime-role"])
        passwords = [item["stringData"]["password"] for item in data["items"]]
        self.assertEqual(len(set(passwords)), 2)
        self.assertTrue(all(re.fullmatch(r"[0-9a-f]{48}", password) for password in passwords))
        self.assertEqual([item["stringData"]["username"] for item in data["items"]], ["gateway_migrator", "gateway_runtime"])
        self.assertTrue(all(item["type"] == "kubernetes.io/basic-auth" for item in data["items"]))


class Workflow(unittest.TestCase):
    def setUp(self):
        quiet = contextlib.redirect_stdout(io.StringIO())
        quiet.__enter__()
        self.addCleanup(quiet.__exit__, None, None, None)

    def up(self, recorder, name="t599", ref="v0.16.0", lookup=None):
        env = spark.Spark(Path("/k/spark.yaml"), recorder)
        resolved = spark.Ref(name=ref, sha="0123456789abcdef" * 2 + "01234567", is_tag=True)
        with mock.patch.object(spark, "resolve_ref", return_value=resolved), \
             mock.patch.object(spark, "lookup_image", lookup or mock.Mock(return_value=(DIGEST, ARM_DIGEST))), \
             mock.patch.object(spark, "extract_ref", return_value=Path("/tmp/x")), \
             mock.patch.object(spark.time, "sleep"):
            env.up(name, ref)
        return recorder.commands()

    def test_up_follows_the_invariant_order(self):
        recorder = Recorder()
        commands = self.up(recorder)
        position = lambda needle: next(i for i, command in enumerate(commands) if needle in command)
        order = [position("create namespace coder-03-omg-t599"), position("get networkpolicy"), position("apply -f -"), position("install omg"),
                 position("create configmap omg-demo-scripts"), position("wait --for=condition=complete job/omg-bootstrap-demo"), position("rollout status deployment/omg-demo")]
        self.assertEqual(order, sorted(order))
        self.assertLess(position("install omg"), position("wait --for=condition=complete"))
        self.assertTrue(all(command.split()[0] in ("kubectl", "helm") and "--kubeconfig /k/spark.yaml" in command for command in commands))
        install = commands[position("install omg")]
        self.assertIn("--wait-for-jobs", install)
        self.assertIn(f"image.digest={DIGEST}", install)
        self.assertNotIn("serve", install)

    def test_up_waits_through_the_rights_delay_of_a_new_namespace(self):
        recorder = Recorder()
        answers = iter([spark.SparkError("kubectl failed with exit 1: Error from server (Forbidden): networkpolicies"), "networkpolicy/a\n", "networkpolicy/a\nnetworkpolicy/b\n"])
        original = recorder.__call__

        def run(command, input=None, capture=False):
            if "get networkpolicy" in " ".join(command):
                recorder.calls.append((list(command), input))
                answer = next(answers)
                if isinstance(answer, Exception):
                    raise answer
                return answer
            return original(command, input, capture)

        env = spark.Spark(Path("/k/spark.yaml"), run)
        resolved = spark.Ref("v0.16.0", "0123456789abcdef" * 2 + "01234567", True)
        with mock.patch.object(spark, "resolve_ref", return_value=resolved), mock.patch.object(spark, "lookup_image", return_value=(DIGEST, ARM_DIGEST)), \
             mock.patch.object(spark, "extract_ref"), mock.patch.object(spark.time, "sleep"):
            env.up("t599", "v0.16.0")
        self.assertEqual(sum("get networkpolicy" in c for c in recorder.commands()), 3)
        self.assertTrue(any("install omg" in c for c in recorder.commands()))

    def test_up_does_not_swallow_other_errors_while_waiting(self):
        def run(command, input=None, capture=False):
            joined = " ".join(command)
            if "config view" in joined:
                return config_view()
            if "get namespace" in joined:
                return ""
            if "get networkpolicy" in joined:
                raise spark.SparkError("kubectl failed with exit 1: connection refused")
            return ""

        resolved = spark.Ref("v0.16.0", "0123456789abcdef" * 2 + "01234567", True)
        with mock.patch.object(spark, "resolve_ref", return_value=resolved), mock.patch.object(spark, "lookup_image", return_value=(DIGEST, ARM_DIGEST)), \
             mock.patch.object(spark, "extract_ref"), mock.patch.object(spark.time, "sleep"), self.assertRaisesRegex(spark.SparkError, "connection refused"):
            spark.Spark(Path("/k/spark.yaml"), run).up("t599", "v0.16.0")

    def test_up_never_puts_a_secret_in_a_command_line(self):
        recorder = Recorder()
        self.up(recorder)
        secrets_input = [data for command, data in recorder.calls if data and "stringData" in data]
        self.assertEqual(len(secrets_input), 1)
        for password in re.findall(r'"password": "([0-9a-f]+)"', secrets_input[0]):
            self.assertFalse(any(password in " ".join(command) for command, _ in recorder.calls))

    def test_up_refuses_an_image_without_arm64_before_touching_the_cluster(self):
        recorder = Recorder()
        with self.assertRaises(spark.SparkError):
            self.up(recorder, lookup=mock.Mock(side_effect=spark.SparkError("no linux/arm64")))
        self.assertFalse([c for c in recorder.commands() if " create " in c or " apply " in c or "install" in c])

    def test_up_refuses_an_existing_environment(self):
        recorder = Recorder(exists=True)
        with self.assertRaises(spark.SparkError):
            self.up(recorder)
        self.assertFalse([c for c in recorder.commands() if " create " in c or " apply " in c or "install" in c])

    def test_down_deletes_only_the_environment_namespace(self):
        recorder = Recorder(exists=True)
        with mock.patch.object(spark.time, "sleep"):
            spark.Spark(Path("/k/spark.yaml"), recorder).down("t599")
        deletes = [c for c in recorder.commands() if " delete " in c]
        self.assertEqual(len(deletes), 1)
        self.assertIn("delete namespace coder-03-omg-t599 --wait=false", deletes[0])
        with self.assertRaises(spark.SparkError):
            spark.Spark(Path("/k/spark.yaml"), recorder).down("../coder-03")
        with self.assertRaises(spark.SparkError):
            spark.Spark(Path("/k/spark.yaml"), Recorder(exists=False)).down("t599")

    def test_forward_targets_the_demo_pod_in_the_environment_namespace(self):
        recorder = Recorder(exists=True)
        command = spark.Spark(Path("/k/spark.yaml"), recorder).forward_command("t599")
        self.assertEqual(command, ["kubectl", "--kubeconfig", "/k/spark.yaml", "-n", "coder-03-omg-t599", "port-forward", "--address", "127.0.0.1", "deployment/omg-demo", "3000:3000", "18084:18084"])

    def test_a_foreign_cluster_stops_everything(self):
        calls = []

        def run(command, input=None, capture=False):
            calls.append(command)
            return config_view(server="https://203.0.113.9:6443") if "config view" in " ".join(command) else ""

        with self.assertRaises(spark.SparkError):
            spark.Spark(Path("/k/spark.yaml"), run).status("t599")
        self.assertEqual(len(calls), 1)


class Cli(unittest.TestCase):
    def test_main_refuses_without_the_spark_kubeconfig_and_never_runs_a_tool(self):
        with tempfile.TemporaryDirectory() as folder, mock.patch.dict(spark.os.environ, {"SPARK_KUBECONFIG": str(Path(folder) / "none.yaml")}), \
             mock.patch.object(spark, "run") as run:
            for argv in (["up", "t599", "--ref", "main"], ["status"], ["down", "t599"], ["forward", "t599"]):
                with self.assertRaises(SystemExit) as raised:
                    spark.main(argv)
                self.assertIn("missing", str(raised.exception))
            run.assert_not_called()


class Usage(unittest.TestCase):
    def test_creds_lists_the_passwordless_personas_without_secrets(self):
        text = spark.creds_text("coder-03-omg-t599")
        for email in ("operator@demo.invalid", "auditor@demo.invalid", "alex@demo.invalid", "blair@demo.invalid", "unentitled@demo.invalid"):
            self.assertIn(email, text)
        self.assertIn("127.0.0.1:3000", text)
        self.assertNotRegex(text, r"(?i)password:|secret")


if __name__ == "__main__":
    unittest.main()

"""Spark test-platform script safety tests. No cluster, registry or external network access."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import socket
import signal
import shlex
import sys
import subprocess
import tempfile
import unittest
from unittest import mock
import urllib.error

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("spark_env", ROOT / "scripts" / "spark-env.py")
spark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(spark)

SERVER = "https://bitop-spark.taila06ed.ts.net:6443"
SHA = "0123456789abcdef" * 2 + "01234567"
CONFIG = Path("/k/spark.yaml")


def sha256(body):
    return "sha256:" + hashlib.sha256(body).hexdigest()


def config_view(server=SERVER, namespace="coder-03"):
    context = {"cluster": "c", "user": "u"}
    if namespace is not None:
        context["namespace"] = namespace
    return json.dumps({"clusters": [{"name": "c", "cluster": {"server": server}}], "contexts": [{"name": "c", "context": context}], "current-context": "c"})


class Registry:
    """A fake ghcr.io: an index with an arm64 child whose configuration carries a revision label."""

    def __init__(self, revision=SHA, arch="arm64", header=None, redirect_blob=False, owner="bitop-dev", tag="dev-x-arm64"):
        self.config = json.dumps({"config": {"Labels": {"org.opencontainers.image.revision": revision}} if revision else {}}).encode()
        self.manifest = json.dumps({"config": {"digest": sha256(self.config)}}).encode()
        self.index = json.dumps({"manifests": [
            {"digest": sha256(self.manifest), "platform": {"os": "linux", "architecture": arch}},
            {"digest": sha256(b"attestation"), "platform": {"os": "unknown", "architecture": "unknown"}}]}).encode()
        self.digest = sha256(self.index)
        self.header = header
        self.seen = []
        base = f"https://ghcr.io/v2/{owner}/open-model-gateway"
        self.urls = {f"{base}/manifests/{tag}": self.index, f"{base}/manifests/{sha256(self.manifest)}": self.manifest, f"{base}/blobs/{sha256(self.config)}": self.config}

    def __call__(self, url, headers):
        self.seen.append((url, dict(headers)))
        if "/token" in url:
            return 200, {}, json.dumps({"token": "t0ken-value"}).encode()
        if url not in self.urls:
            return 404, {}, b""
        extra = {"docker-content-digest": self.header} if self.header and "manifests/dev" in url else {}
        return 200, extra, self.urls[url]


class Recorder:
    """A fake `run`: records every command and its stdin, answers read-only queries, can fail on demand."""

    def __init__(self, exists=False, job="complete", fail_on=None, interrupt_on=None, namespaces=""):
        self.calls = []
        self.exists = exists
        self.job = job
        self.fail_on = fail_on
        self.interrupt_on = interrupt_on
        self.namespaces = namespaces

    def __call__(self, command, stdin=None, capture=False):
        self.calls.append((list(command), stdin))
        joined = " ".join(command)
        if self.interrupt_on and self.interrupt_on in joined:
            raise KeyboardInterrupt
        if self.fail_on and self.fail_on in joined:
            raise spark.SparkError(f"{spark.describe(command)} failed (exit 1): boom")
        if "delete namespace" in joined:
            self.exists = False
        if "config view" in joined:
            return config_view()
        if "get namespace" in joined and joined.endswith("-o name"):
            if joined.endswith("get namespace -o name"):
                return self.namespaces
            return "namespace/x\n" if self.exists else ""
        if "get networkpolicy" in joined:
            return "networkpolicy/a\nnetworkpolicy/b\n"
        if "get job/" in joined:
            status = {"complete": "Complete", "failed": "Failed", "running": None}[self.job]
            return json.dumps({"status": {"conditions": [{"type": status, "status": "True"}] if status else []}})
        return ""

    def commands(self):
        return [" ".join(command) for command, _ in self.calls]

    def index(self, needle, stdin=None):
        """Position of the one call containing needle (and, if given, stdin text)."""
        found = [i for i, (command, data) in enumerate(self.calls) if needle in " ".join(command) and (stdin is None or stdin in (data or ""))]
        if len(found) != 1:
            raise AssertionError(f"expected exactly one {needle!r} {stdin or ''} call, found {len(found)}: {self.commands()}")
        return found[0]


def silent():
    return contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO())


class LoopbackPorts(unittest.TestCase):
    def test_time_wait_does_not_block_a_new_forward(self):
        # The server actively closes, leaving its local endpoint in TIME_WAIT.
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
            listener.listen()
            with socket.create_connection(("127.0.0.1", port), timeout=2) as client:
                connection, _ = listener.accept()
                connection.close()
                self.assertEqual(client.recv(1), b"")
        # Prove the fixture really prevents a bind without SO_REUSEADDR.
        with socket.socket() as plain:
            with self.assertRaises(OSError):
                plain.bind(("127.0.0.1", port))
        self.assertTrue(spark.port_is_free(port))

    def test_active_listener_is_refused_even_when_it_allows_reuse(self):
        for address in ("127.0.0.1", "0.0.0.0"):
            with self.subTest(address=address), socket.socket() as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind((address, 0))
                listener.listen()
                self.assertFalse(spark.port_is_free(listener.getsockname()[1]))


class SidecarProbe(unittest.TestCase):
    def test_node_entrypoint_through_regular_and_configmap_paths(self):
        script = ROOT / "deploy/spark/loopback-probe.mjs"
        with tempfile.TemporaryDirectory() as folder:
            mount = Path(folder)
            revision = mount / "..revision"
            revision.mkdir()
            (revision / script.name).symlink_to(script)
            (mount / "..data").symlink_to(revision.name)
            entry = mount / script.name
            entry.symlink_to(Path("..data") / script.name)
            for path in (script, entry):
                with self.subTest(entry=path):
                    def probe(argument):
                        return subprocess.run(["node", str(path), argument], timeout=5, check=False).returncode

                    with socket.socket() as listener:
                        listener.bind(("127.0.0.1", 0))
                        listener.listen()
                        port = str(listener.getsockname()[1])
                        self.assertEqual(probe(port), 0)
                    self.assertEqual(probe(port), 1)
                    self.assertEqual(probe("invalid"), 1)


class ForwardLifecycle(unittest.TestCase):
    # No kubectl or cluster: each wrapper owns one disposable Python listener.
    CHILD = """
import os, select, signal, socket, sys
if sys.argv[2] == 'ignore':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', int(sys.argv[1])))
s.listen()
print(str(os.getpid()) + " " + str(s.getsockname()[1]), flush=True)
# The test owns the write end of stdin. EOF releases even an orphaned fixture.
select.select([sys.stdin], [], [], 20)
"""

    def wrapper(self, port, mode="normal"):
        code = f"""
import contextlib, importlib.util, sys
spec = importlib.util.spec_from_file_location('spark_env', {str(ROOT / 'scripts/spark-env.py')!r})
spark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(spark)
env = spark.Spark('/unused')
env.environment = lambda name: 'coder-03-omg-t599'
env.forward_command = lambda name: [sys.executable, '-u', '-c', {self.CHILD!r}, {str(port)!r}, {mode!r}]
spark.DEMO_PORTS = ({port},)
with contextlib.redirect_stdout(sys.stderr):
    env.forward('t599')
"""
        return subprocess.Popen([sys.executable, "-u", "-c", code], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, start_new_session=True)

    def ready(self, wrapper):
        # select bounds the handshake even if the wrapper fails before spawning.
        import select
        self.assertTrue(select.select([wrapper.stdout], [], [], 5)[0], "child startup timed out")
        line = wrapper.stdout.readline()
        self.assertEqual(len(line.split()), 2, f"child did not start: {line!r}")
        return tuple(map(int, line.split()))

    def stop_wrapper(self, wrapper):
        if wrapper.poll() is None:
            wrapper.kill()
        # communicate closes the pipe, including after a failed readiness handshake.
        wrapper.communicate(timeout=5)

    def test_parent_only_signals_reap_owned_child_and_allow_immediate_reforward(self):
        for signum, mode in ((signal.SIGINT, "normal"), (signal.SIGTERM, "normal"), (signal.SIGTERM, "ignore"),
                             (signal.SIGHUP, "normal")):
            with self.subTest(signal=signum, mode=mode), socket.socket() as unrelated:
                unrelated.bind(("127.0.0.1", 0))
                unrelated.listen()
                sentinel = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
                self.addCleanup(sentinel.wait, timeout=5)
                self.addCleanup(sentinel.terminate)
                wrapper = self.wrapper(0, mode)
                try:
                    child_pid, port = self.ready(wrapper)
                    self.assertFalse(spark.port_is_free(port))
                    wrapper.send_signal(signum)  # PID only, never its process group.
                    if mode == "ignore":
                        wrapper.send_signal(signum)
                        wrapper.send_signal(signum)
                    self.assertEqual(wrapper.wait(timeout=9), 0 if signum == signal.SIGINT else 128 + signum)
                    with self.assertRaises(ProcessLookupError):
                        os.kill(child_pid, 0)  # gone, including no unreaped zombie
                    self.assertIsNone(sentinel.poll(), "an unrelated disposable process survives")
                    self.assertFalse(spark.port_is_free(unrelated.getsockname()[1]))
                    replacement = self.wrapper(port)
                    try:
                        self.ready(replacement)
                        replacement.send_signal(signal.SIGTERM)
                        self.assertEqual(replacement.wait(timeout=9), 143)
                    finally:
                        self.stop_wrapper(replacement)
                finally:
                    self.stop_wrapper(wrapper)

    def test_fixture_releases_port_if_wrapper_cleanup_is_interrupted(self):
        wrapper = self.wrapper(0, "ignore")
        try:
            _, port = self.ready(wrapper)
        finally:
            self.stop_wrapper(wrapper)
        self.assertTrue(spark.port_is_free(port))

    def test_real_occupied_port_is_refused_before_any_child_starts(self):
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]
            wrapper = self.wrapper(port)
            try:
                stdout, stderr = wrapper.communicate(timeout=5)
                self.assertEqual(wrapper.returncode, 1)
                self.assertEqual(stdout, "", "the disposable child did not start")
                self.assertIn(f"local port {port} is already in use", stderr)
                self.assertNotIn("Forwarding ", stderr)
                self.assertFalse(spark.port_is_free(port))
            finally:
                self.stop_wrapper(wrapper)

    def test_signal_during_spawn_still_reaps_the_child_and_restores_handlers(self):
        real_popen = subprocess.Popen
        owned = []
        handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}

        def spawn_then_signal(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            owned.append(child)
            os.kill(os.getpid(), signal.SIGTERM)
            return child

        try:
            with mock.patch.object(spark.subprocess, "Popen", side_effect=spawn_then_signal):
                with self.assertRaises(SystemExit) as raised:
                    spark.launch_forward([sys.executable, "-c", "import time; time.sleep(60)"])
            self.assertEqual(raised.exception.code, 143)
            self.assertIsNotNone(owned[0].returncode, "the child was waited on")
            with self.assertRaises(ChildProcessError):
                os.waitpid(owned[0].pid, os.WNOHANG)
            self.assertEqual({sig: signal.getsignal(sig) for sig in handlers}, handlers)
        finally:
            for child in owned:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)

    def test_natural_exit_status_is_preserved_and_signal_handlers_are_restored(self):
        handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        for code in (0, 7):
            result = spark.launch_forward([sys.executable, "-c", f"raise SystemExit({code})"])
            self.assertEqual(result.returncode, code)
        self.assertEqual({sig: signal.getsignal(sig) for sig in handlers}, handlers)
        with self.assertRaises(FileNotFoundError):
            spark.launch_forward(["/no-such-spark-forward-test-executable"])
        self.assertEqual({sig: signal.getsignal(sig) for sig in handlers}, handlers)


class Names(unittest.TestCase):
    def test_valid_and_invalid_names(self):
        for name in ("t599", "my-branch-1", "ab", "a" * 20):
            self.assertEqual(spark.validate_name(name), name)
        for name in ("", "a", "a" * 21, "UPPER", "-x", "x-", "a_b", "a/b", "a.b", "x y", "../x", "a\n"):
            with self.assertRaises(spark.SparkError, msg=repr(name)):
                spark.validate_name(name)

    def test_the_test_runner_prefix_is_reserved(self):
        # <base>-omg-test-<slot> belongs to scripts/spark-test.py: this script never creates, targets or lists it.
        for name in ("test-s1", "test-anything"):
            with self.assertRaises(spark.SparkError, msg=name):
                spark.validate_name(name)
            with self.assertRaises(spark.SparkError, msg=name):
                spark.check_namespace("coder-03", f"coder-03-omg-{name}")
        spark.validate_name("tester")
        spark.check_namespace("coder-03", "coder-03-omg-tester")

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
        for namespace in ("default", "coder-03", "kube-system", "coder-03-omg"):
            with self.assertRaises(spark.SparkError):
                spark.kubectl_command(CONFIG, "coder-03", ["get", "pods"], namespace=namespace)
            with self.assertRaises(spark.SparkError):
                spark.helm_command(CONFIG, "coder-03", ["list"], namespace=namespace)
        env = spark.Spark(CONFIG, Recorder())
        for method in (env.create_namespace, env.delete_namespace, env.namespace_exists):
            for namespace in ("default", "coder-03", "coder-03-omg", "coder-02-omg-x"):
                with self.assertRaises(spark.SparkError, msg=f"{method.__name__} {namespace}"):
                    method(namespace)
        self.assertFalse([c for c in env.runner.commands() if "create" in c or "delete" in c or "get namespace default" in c])


class ChildProcesses(unittest.TestCase):
    HOSTILE = {"PATH": "/usr/bin", "HOME": "/home/u", "LANG": "C.UTF-8", "LC_ALL": "C", "KUBECONFIG": "/etc/prod.yaml", "HELM_KUBEAPISERVER": "https://evil.example", "HELM_KUBETOKEN": "x",
               "HELM_KUBEASUSER": "admin", "HELM_KUBECAFILE": "/tmp/ca", "HELM_NAMESPACE": "kube-system", "HELM_KUBECONTEXT": "prod", "KUBECTL_KUBERC": "true", "GIT_DIR": "/elsewhere",
               "KUBECACHEDIR": "/tmp/x", "HTTPS_PROXY": "http://proxy:3128", "SSL_CERT_FILE": "/etc/ssl/c.pem", "SECRET_TOKEN": "s3"}

    def test_the_child_environment_is_an_allowlist(self):
        child = spark.child_environment(self.HOSTILE)
        self.assertEqual(child, {"PATH": "/usr/bin", "HOME": "/home/u", "LANG": "C.UTF-8", "LC_ALL": "C", "HTTPS_PROXY": "http://proxy:3128", "SSL_CERT_FILE": "/etc/ssl/c.pem", "KUBECTL_KUBERC": "false"})

    def test_run_starts_children_with_the_scrubbed_environment(self):
        with mock.patch.dict(spark.os.environ, self.HOSTILE, clear=True), mock.patch.object(spark.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "out", "")) as started:
            self.assertEqual(spark.run(["kubectl", "get", "pods"], capture=True), "out")
            self.assertEqual(started.call_args.kwargs["env"], spark.child_environment(self.HOSTILE))
            for name in ("KUBECONFIG", "HELM_KUBEAPISERVER", "HELM_KUBETOKEN", "HELM_NAMESPACE", "GIT_DIR", "SECRET_TOKEN"):
                self.assertNotIn(name, started.call_args.kwargs["env"])
            self.assertEqual(started.call_args.kwargs["env"]["KUBECTL_KUBERC"], "false")

    def test_forward_uses_the_same_scrubbed_environment(self):
        recorder = Recorder(exists=True)
        env = spark.Spark(CONFIG, recorder, port_free=lambda port: True)
        with mock.patch.dict(spark.os.environ, self.HOSTILE, clear=True), mock.patch.object(spark.subprocess, "Popen") as started, contextlib.redirect_stdout(io.StringIO()):
            started.return_value.wait.return_value = 0
            started.return_value.poll.return_value = 0
            env.forward("t599")
        self.assertEqual(started.call_args.kwargs["env"], spark.child_environment(self.HOSTILE))
        self.assertNotIn("HELM_KUBEAPISERVER", started.call_args.kwargs["env"])

    def test_a_failure_reports_the_redacted_stderr_not_the_argv(self):
        password = json.loads(spark.role_secrets())["items"][0]["stringData"]["password"]
        stderr = "x" * 3000 + f" error: invalid Secret {password} Authorization: Bearer abcdefghijklmnop password=hunter22"
        with mock.patch.object(spark.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", stderr)):
            with self.assertRaises(spark.SparkError) as raised:
                spark.run(["kubectl", "--kubeconfig", "/k", "-n", "coder-03-omg-x", "create", "-f", "-"], stdin=f"{password}")
        message = str(raised.exception)
        for leaked in (password, "abcdefghijklmnop", "hunter22"):
            self.assertNotIn(leaked, message)
        self.assertIn("[redacted]", message)
        self.assertLessEqual(len(message), 2200)
        self.assertTrue(message.startswith("kubectl create failed (exit 1)"))

    def test_a_missing_tool_is_a_clear_error(self):
        with mock.patch.object(spark.subprocess, "run", side_effect=FileNotFoundError):
            with self.assertRaisesRegex(spark.SparkError, "helm is not installed"):
                spark.run(["helm", "list"])
        with self.assertRaisesRegex(spark.SparkError, "helm"):
            spark.require_tools(("kubectl", "helm"), which=lambda name: None if name == "helm" else "/bin/" + name)


class Images(unittest.TestCase):
    def test_registry_selection_preserves_old_releases_without_fallback(self):
        for tag, owner in (("v0.17.0", "ncecere"), ("v0.18.0", "ncecere"), ("v0.18.0-rc.1", "ncecere"),
                           ("v0.18.1", "bitop-dev"), ("v0.19.0-rc.1", "bitop-dev"), ("v1.0.0", "bitop-dev"),
                           ("dev-x-arm64", "bitop-dev")):
            with self.subTest(tag=tag):
                registry = Registry(owner=owner, tag=tag)
                self.assertEqual(spark.lookup_image(tag, SHA, registry), registry.digest)
                self.assertIn(f"scope=repository:{owner}/open-model-gateway:pull", registry.seen[0][0])
                self.assertTrue(all(f"/v2/{owner}/open-model-gateway/" in url for url, _ in registry.seen[1:]))
        registry = Registry(owner="ncecere")
        with self.assertRaises(spark.SparkError):
            spark.lookup_image("dev-x-arm64", SHA, registry)
        self.assertFalse(any("ncecere" in url for url, _ in registry.seen), "never retry the old registry")

    def test_release_tags_keep_their_name_and_other_refs_use_the_commit(self):
        self.assertEqual(spark.image_tag("v0.16.0", SHA, is_tag=True), "v0.16.0")
        self.assertEqual(spark.image_tag("v1.2.3-rc.1", SHA, is_tag=True), "v1.2.3-rc.1")
        self.assertEqual(spark.image_tag("issue/599-spark-env", SHA, is_tag=False), "dev-0123456789ab-arm64")
        self.assertEqual(spark.image_tag("v0.16.0", SHA, is_tag=False), "dev-0123456789ab-arm64", "a branch named like a release is not a release")
        self.assertEqual(spark.image_tag("nightly", SHA, is_tag=True), "dev-0123456789ab-arm64", "only release-shaped tags are published as tags")

    def test_arm64_must_be_present(self):
        digest = "sha256:" + "b" * 64
        self.assertEqual(spark.arm64_manifest({"manifests": [{"digest": digest, "platform": {"os": "linux", "architecture": "arm64"}}]}), digest)
        for missing in ({"manifests": [{"digest": digest, "platform": {"os": "linux", "architecture": "amd64"}}]}, {"manifests": [{"digest": digest, "platform": {"os": "windows", "architecture": "arm64"}}]}, {}, {"manifests": []}):
            with self.assertRaises(spark.SparkError):
                spark.arm64_manifest(missing)

    def test_lookup_returns_the_index_digest_computed_from_the_bytes(self):
        registry = Registry()
        self.assertEqual(spark.lookup_image("dev-x-arm64", SHA, registry), registry.digest)
        self.assertNotIn("Authorization", registry.seen[0][1], "anonymous token request")
        self.assertTrue(all(headers["Authorization"] == "Bearer t0ken-value" for _, headers in registry.seen[1:]))
        # An absent header is fine; a header that disagrees with the bytes is refused.
        registry = Registry(header=sha256(b"something else"))
        with self.assertRaisesRegex(spark.SparkError, "digest header"):
            spark.lookup_image("dev-x-arm64", SHA, registry)
        registry = Registry()
        registry = Registry(header=registry.digest)
        self.assertEqual(spark.lookup_image("dev-x-arm64", SHA, registry), registry.digest)

    def test_a_tampered_body_is_refused(self):
        registry = Registry()
        base = "https://ghcr.io/v2/bitop-dev/open-model-gateway"
        registry.urls[f"{base}/blobs/{sha256(registry.config)}"] = registry.config + b" "
        with self.assertRaisesRegex(spark.SparkError, "configuration"):
            spark.lookup_image("dev-x-arm64", SHA, registry)
        registry = Registry()
        registry.urls[f"{base}/manifests/{sha256(registry.manifest)}"] = registry.manifest + b" "
        with self.assertRaisesRegex(spark.SparkError, "arm64 manifest"):
            spark.lookup_image("dev-x-arm64", SHA, registry)

    def test_the_revision_label_must_be_the_commit(self):
        for registry in (Registry(revision="f" * 40), Registry(revision=None)):
            with self.assertRaisesRegex(spark.SparkError, "built from"):
                spark.lookup_image("dev-x-arm64", SHA, registry)

    def test_an_image_without_arm64_or_a_missing_tag_is_a_clear_error(self):
        with self.assertRaisesRegex(spark.SparkError, "linux/arm64"):
            spark.lookup_image("dev-x-arm64", SHA, Registry(arch="amd64"))
        with self.assertRaisesRegex(spark.SparkError, "branch-images"):
            spark.lookup_image("dev-0123456789ab-arm64", SHA, Registry())

    def test_redirects_are_followed_without_the_bearer_token(self):
        calls = []

        class Opener:
            def open(self, request, timeout=None):
                calls.append((request.full_url, request.get_header("Authorization")))
                if len(calls) == 1:
                    raise urllib.error.HTTPError(request.full_url, 307, "redirect", {"Location": "https://cdn.example/blob?sig=1"}, None)
                return mock.MagicMock(__enter__=lambda self: self, __exit__=lambda *a: False, status=200, headers={"X-A": "b"}, read=lambda: b"bytes")

        status, headers, body = spark.fetch_url("https://ghcr.io/v2/x/blobs/sha256:a", {"Authorization": "Bearer secret-token"}, opener=Opener())
        self.assertEqual((status, body, headers), (200, b"bytes", {"x-a": "b"}))
        self.assertEqual(calls, [("https://ghcr.io/v2/x/blobs/sha256:a", "Bearer secret-token"), ("https://cdn.example/blob?sig=1", None)])

    def test_redirects_to_plain_http_or_in_a_loop_are_refused(self):
        class Plain:
            def open(self, request, timeout=None):
                raise urllib.error.HTTPError(request.full_url, 302, "r", {"Location": "http://cdn.example/x"}, None)

        class Loop:
            def open(self, request, timeout=None):
                raise urllib.error.HTTPError(request.full_url, 302, "r", {"Location": "https://cdn.example/x"}, None)

        for opener in (Plain(), Loop()):
            with self.assertRaises(spark.SparkError):
                spark.fetch_url("https://ghcr.io/x", {}, opener=opener)


class Manifests(unittest.TestCase):
    def test_image_substitution_is_complete_and_strict(self):
        image = f"ghcr.io/bitop-dev/open-model-gateway@sha256:{'a' * 64}"
        for name in ("bootstrap-job.yaml", "demo.yaml"):
            text = spark.render((ROOT / "deploy" / "spark" / name).read_text(), image)
            self.assertNotIn("__IMAGE__", text)
            self.assertIn(image, text)
        for bad in ("bad image", "ghcr.io/bitop-dev/open-model-gateway:v0.16.0", "ghcr.io/bitop-dev/open-model-gateway"):
            with self.assertRaises(spark.SparkError, msg=bad):
                spark.render("image: __IMAGE__", bad)
        with self.assertRaises(spark.SparkError):
            spark.render("image: x", image)

    def test_secrets_are_basic_auth_and_distinct(self):
        data = json.loads(spark.role_secrets())
        self.assertEqual([item["metadata"]["name"] for item in data["items"]], ["omg-migrator-role", "omg-runtime-role"])
        passwords = [item["stringData"]["password"] for item in data["items"]]
        self.assertEqual(len(set(passwords)), 2)
        self.assertTrue(all(re.fullmatch(r"[0-9a-f]{48}", password) for password in passwords))
        self.assertEqual([item["stringData"]["username"] for item in data["items"]], ["gateway_migrator", "gateway_runtime"])
        self.assertTrue(all(item["type"] == "kubernetes.io/basic-auth" for item in data["items"]))

    def test_job_states(self):
        self.assertEqual(spark.job_state(json.dumps({"status": {"conditions": [{"type": "Complete", "status": "True"}]}})), "complete")
        self.assertEqual(spark.job_state(json.dumps({"status": {"conditions": [{"type": "Failed", "status": "True"}]}})), "failed")
        for text in (json.dumps({"status": {}}), json.dumps({"status": {"conditions": [{"type": "Complete", "status": "False"}]}}), "not json", "{}"):
            self.assertEqual(spark.job_state(text), "running")


# git sets GIT_DIR and GIT_INDEX_FILE when it runs a hook, and `git -C folder` does not
# override them: the fixture below and spark-env.py's own git calls would then commit into,
# tag and re-initialise that repository instead of the temporary one (#596 follow-up).
for _name in [name for name in os.environ if name.startswith("GIT_")]:
    del os.environ[_name]


class InheritedGit(unittest.TestCase):
    def test_no_git_variable_reaches_the_fixtures(self):
        self.assertEqual([name for name in os.environ if name.startswith("GIT_")], [])

    def test_the_fixture_repository_is_its_own(self):
        with tempfile.TemporaryDirectory() as sentinel, tempfile.TemporaryDirectory() as folder:
            git_repository(sentinel)
            before = subprocess.run(["git", "-C", sentinel, "for-each-ref"], check=True, capture_output=True, text=True).stdout
            os.environ["GIT_DIR"] = str(Path(sentinel) / ".git")
            try:
                with self.assertRaises(AssertionError):
                    self.assertEqual([name for name in os.environ if name.startswith("GIT_")], [])
            finally:
                del os.environ["GIT_DIR"]
            git_repository(folder)
            self.assertTrue((Path(folder) / ".git").is_dir())
            after = subprocess.run(["git", "-C", sentinel, "for-each-ref"], check=True, capture_output=True, text=True).stdout
            self.assertEqual(before, after)


def git_repository(folder):
    def git(*args):
        return subprocess.run(["git", "-C", folder, *args], check=True, capture_output=True, text=True,
                              env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}).stdout.strip()
    git("init", "-q", "-b", "main")
    chart = Path(folder) / "deploy/helm/open-model-gateway"
    chart.mkdir(parents=True)
    (chart / "Chart.yaml").write_text("name: x\n")
    (Path(folder) / "scripts").mkdir()
    (Path(folder) / "scripts/demo-oidc.mjs").write_text("// issuer\n")
    (Path(folder) / "unrelated.txt").write_text("not extracted\n")
    git("add", "-A")
    git("commit", "-q", "-m", "one")
    first = git("rev-parse", "HEAD")
    git("tag", "v1.2.3")
    (Path(folder) / "unrelated.txt").write_text("two\n")
    git("commit", "-q", "-am", "two")
    git("branch", "feature/x")
    git("tag", "-a", "-m", "annotated", "v1.2.4")
    return git, first


class Refs(unittest.TestCase):
    def test_refs_resolve_with_real_git(self):
        with tempfile.TemporaryDirectory() as folder:
            git, first = git_repository(folder)
            head = git("rev-parse", "HEAD")
            self.assertEqual(spark.resolve_ref("main", repo=folder), spark.Ref("main", head, False))
            self.assertEqual(spark.resolve_ref("feature/x", repo=folder), spark.Ref("feature/x", head, False))
            self.assertEqual(spark.resolve_ref("v1.2.3", repo=folder), spark.Ref("v1.2.3", first, True))
            self.assertEqual(spark.resolve_ref("v1.2.4", repo=folder), spark.Ref("v1.2.4", head, True), "an annotated tag resolves to its commit")
            self.assertEqual(spark.resolve_ref(head[:12], repo=folder).sha, head)
            with self.assertRaisesRegex(spark.SparkError, "not a commit"):
                spark.resolve_ref("no-such-branch", repo=folder)

    def test_hostile_refs_never_reach_git(self):
        runner = mock.Mock()
        for ref in ("", "-x", "--upload-pack=evil", "-", "a b", "a\n", "x;y", "a..b", "refs/heads/../x", "main@{upstream}", "$(id)", "a`b`", "~1", "^x", ":x", "a:b", "a*", "a?", "a[", "a\\b"):
            with self.assertRaises(spark.SparkError, msg=repr(ref)):
                spark.resolve_ref(ref, runner=runner)
        runner.assert_not_called()

    def test_extract_unpacks_only_the_chart_and_the_issuer_of_that_commit(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as out:
            _, first = git_repository(folder)
            spark.extract_ref(first, out, repo=folder)
            self.assertTrue((Path(out) / "deploy/helm/open-model-gateway/Chart.yaml").is_file())
            self.assertEqual((Path(out) / "scripts/demo-oidc.mjs").read_text(), "// issuer\n")
            self.assertEqual(sorted(p.name for p in Path(out).iterdir()), ["deploy", "ref.tar", "scripts"])

    def test_extraction_needs_tarfile_filters(self):
        with mock.patch.object(spark.tarfile, "data_filter", create=True), mock.patch.object(spark, "tarfile") as tar:
            del tar.data_filter
            with self.assertRaisesRegex(spark.SparkError, "Python 3.12"):
                spark.extract_ref(SHA, "/tmp", runner=mock.Mock())


class Workflow(unittest.TestCase):
    RESOLVED = spark.Ref("v0.16.0", SHA, True)

    def setUp(self):
        out, err = silent()
        out.__enter__()
        err.__enter__()
        self.addCleanup(out.__exit__, None, None, None)
        self.addCleanup(err.__exit__, None, None, None)
        self.digest = Registry().digest

    def patches(self, **overrides):
        values = {"resolve_ref": mock.Mock(return_value=self.RESOLVED), "lookup_image": mock.Mock(return_value=self.digest), "extract_ref": mock.Mock(), "time": mock.Mock(),
                  **overrides}
        stack = contextlib.ExitStack()
        for name, value in values.items():
            stack.enter_context(mock.patch.object(spark, name, value))
        self.addCleanup(stack.close)
        return values

    def up(self, recorder, name="t599", ref="v0.16.0", **overrides):
        values = self.patches(**overrides)
        spark.Spark(CONFIG, recorder).up(name, ref)
        return values

    def test_up_follows_the_invariant_order_exactly(self):
        recorder = Recorder()
        self.up(recorder)
        order = [recorder.index("create namespace coder-03-omg-t599"), recorder.index("get networkpolicy"), recorder.index("create -f -", "stringData"),
                 recorder.index("create configmap omg-spark-env"), recorder.index("install omg"), recorder.index("create configmap omg-demo-scripts"),
                 recorder.index("apply -f -", "kind: Job"), recorder.index("get job/omg-bootstrap-demo"), recorder.index("apply -f -", "kind: Deployment"),
                 recorder.index("rollout status deployment/omg-demo")]
        self.assertEqual(order, sorted(set(order)), recorder.commands())
        self.assertEqual(len(recorder.calls) - 2, len(order) - 0 + 0, "no other cluster commands")  # config view + namespace-exists check
        self.assertTrue(all(c.split()[0] in ("kubectl", "helm") and "--kubeconfig /k/spark.yaml" in c for c in recorder.commands()))

    def test_roles_are_created_not_applied_and_no_secret_is_in_a_command_line(self):
        recorder = Recorder()
        self.up(recorder)
        secret_calls = [(command, data) for command, data in recorder.calls if data and "stringData" in data]
        self.assertEqual(len(secret_calls), 1)
        self.assertEqual(secret_calls[0][0][5], "create")
        for password in re.findall(r'"password": "([0-9a-f]+)"', secret_calls[0][1]):
            self.assertFalse(any(password in " ".join(command) for command, _ in recorder.calls))
            self.assertFalse(any(password in (data or "") for command, data in recorder.calls if data is not secret_calls[0][1]))
        self.assertFalse([data for command, data in recorder.calls if "apply" in command and "stringData" in (data or "")])

    def test_everything_is_pinned_by_digest(self):
        recorder = Recorder()
        self.up(recorder)
        image = f"ghcr.io/ncecere/open-model-gateway@{self.digest}"
        install = recorder.commands()[recorder.index("install omg")]
        self.assertIn(f"image.digest={self.digest}", install)
        self.assertIn("image.repository=ghcr.io/ncecere/open-model-gateway", install)
        self.assertNotIn("image.tag", install)
        for kind in ("kind: Job", "kind: Deployment"):
            data = recorder.calls[recorder.index("apply -f -", kind)][1]
            images = [i for i in re.findall(r"image: (?:&\w+ )?(\S+)", data) if not i.startswith("*")]
            self.assertIn(image, images)
            self.assertTrue(all("@sha256:" in found for found in images), images)
        self.assertIn(image, recorder.calls[recorder.index("create configmap omg-spark-env")][0][-1])

    def test_up_validates_the_name_before_anything_else(self):
        for name in ("../coder-03", "UP", "a", "-x", "x y"):
            recorder = Recorder()
            values = self.patches()
            with self.assertRaises(spark.SparkError, msg=name):
                spark.Spark(CONFIG, recorder).up(name, "v0.16.0")
            values["resolve_ref"].assert_not_called()
            values["lookup_image"].assert_not_called()
            self.assertFalse([c for c in recorder.commands() if "create" in c or "install" in c or "apply" in c], name)

    def test_up_checks_the_commit_and_the_image_before_touching_the_cluster(self):
        recorder = Recorder()
        values = self.patches(lookup_image=mock.Mock(side_effect=spark.SparkError("no linux/arm64")))
        with self.assertRaisesRegex(spark.SparkError, "arm64"):
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        values["lookup_image"].assert_called_once_with("v0.16.0", SHA)
        self.assertFalse([c for c in recorder.commands() if " create " in c or " apply " in c or "install" in c])

    def test_up_refuses_an_existing_environment(self):
        recorder = Recorder(exists=True)
        with self.assertRaises(spark.SparkError):
            self.up(recorder)
        self.assertFalse([c for c in recorder.commands() if " create " in c or " apply " in c or "install" in c])

    def test_up_waits_through_the_rights_delay_of_a_new_namespace(self):
        recorder = Recorder()
        answers = iter([spark.SparkError("kubectl get failed (exit 1): Error from server (Forbidden): networkpolicies"), "networkpolicy/a\n", "networkpolicy/a\nnetworkpolicy/b\n"])

        def run(command, stdin=None, capture=False):
            if "get networkpolicy" in " ".join(command):
                recorder.calls.append((list(command), stdin))
                answer = next(answers)
                if isinstance(answer, Exception):
                    raise answer
                return answer
            return recorder(command, stdin, capture)

        self.patches()
        spark.Spark(CONFIG, run).up("t599", "v0.16.0")
        self.assertEqual(sum("get networkpolicy" in c for c in recorder.commands()), 3)
        self.assertLess(recorder.commands().index(next(c for c in recorder.commands() if "get networkpolicy" in c)), recorder.index("install omg"))

    def test_up_does_not_swallow_other_errors_while_waiting(self):
        recorder = Recorder(fail_on="get networkpolicy")
        with self.assertRaisesRegex(spark.SparkError, "boom"):
            self.up(recorder)
        self.assertFalse([c for c in recorder.commands() if "install" in c])

    def test_a_failure_after_the_namespace_leaves_instructions_and_stops(self):
        for fail in ("install omg", "create -f -", "rollout status"):
            recorder = Recorder(fail_on=fail)
            self.patches()
            err = io.StringIO()
            with contextlib.redirect_stderr(err), self.assertRaises(spark.SparkError):
                spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
            self.assertIn("coder-03-omg-t599", err.getvalue(), fail)
            self.assertIn("left behind", err.getvalue(), fail)
            self.assertIn("status t599", err.getvalue(), fail)
            self.assertIn("down t599", err.getvalue(), fail)
        self.assertFalse([c for c in Recorder(fail_on="install omg").commands() if "install" in c])

    def test_log_hint_preserves_a_kubeconfig_path_containing_spaces(self):
        env = spark.Spark(Path("/k/Spark Config/config"), Recorder())
        command = shlex.split(env.logs_hint("coder-03-omg-t599", "deployment/omg-demo"))
        self.assertEqual(command, ["kubectl", "--kubeconfig", "/k/Spark Config/config", "-n",
                                   "coder-03-omg-t599", "logs", "deployment/omg-demo", "--all-containers"])

    def test_rollout_timeout_points_to_scoped_sidecar_status_and_events(self):
        recorder = Recorder(fail_on="rollout status")
        self.patches()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(spark.SparkError) as raised:
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        message = str(raised.exception)
        self.assertIn("init/sidecar startup status and pod events", message)
        self.assertIn("kubectl --kubeconfig /k/spark.yaml -n coder-03-omg-t599 describe pods -l app.kubernetes.io/name=omg-demo", message)
        self.assertIn("logs deployment/omg-demo --all-containers", message)
        self.assertIn("--from-file=loopback-probe.mjs=", " ".join(recorder.commands()))

    def test_a_failure_before_the_namespace_says_nothing_is_left(self):
        recorder = Recorder()
        self.patches(lookup_image=mock.Mock(side_effect=spark.SparkError("no image")))
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(spark.SparkError):
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        self.assertNotIn("left behind", err.getvalue())

    def test_ctrl_c_leaves_instructions_and_propagates(self):
        recorder = Recorder(interrupt_on="install omg")
        self.patches()
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(KeyboardInterrupt):
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        self.assertIn("left behind", err.getvalue())
        self.assertFalse([c for c in recorder.commands() if "apply" in c])

    def test_a_failed_bootstrap_job_stops_at_once_and_says_how_to_read_its_logs(self):
        recorder = Recorder(job="failed")
        self.patches()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(spark.SparkError) as raised:
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        self.assertIn("logs job/omg-bootstrap-demo", str(raised.exception))
        self.assertIn("--kubeconfig /k/spark.yaml -n coder-03-omg-t599", str(raised.exception))
        self.assertEqual(sum("get job/omg-bootstrap-demo" in c for c in recorder.commands()), 1)
        self.assertFalse([data for _, data in recorder.calls if "kind: Deployment" in (data or "")], "the gateway is not started after a failed bootstrap")

    def test_a_running_bootstrap_job_times_out_without_starting_the_gateway(self):
        recorder = Recorder(job="running")
        self.patches()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaisesRegex(spark.SparkError, "did not finish"):
            spark.Spark(CONFIG, recorder).up("t599", "v0.16.0")
        self.assertFalse([data for _, data in recorder.calls if "kind: Deployment" in (data or "")])

    def test_down_deletes_only_the_environment_namespace(self):
        recorder = Recorder(exists=True)
        with mock.patch.object(spark.time, "sleep"):
            spark.Spark(CONFIG, recorder).down("t599")
        deletes = [c for c in recorder.commands() if " delete " in c]
        self.assertEqual(len(deletes), 1)
        self.assertIn("delete namespace coder-03-omg-t599 --wait=false", deletes[0])
        for bad in ("../coder-03", "UP"):
            with self.assertRaises(spark.SparkError):
                spark.Spark(CONFIG, Recorder(exists=True)).down(bad)
        absent = Recorder(exists=False)
        with self.assertRaises(spark.SparkError):
            spark.Spark(CONFIG, absent).down("t599")
        self.assertFalse([c for c in absent.commands() if " delete " in c])

    def test_status_lists_only_environment_namespaces(self):
        listing = "namespace/coder-03\nnamespace/coder-03-omg-test-s1\nnamespace/coder-03-omg-b\nnamespace/coder-02-omg-x\nnamespace/coder-03-omgx\nnamespace/coder-03-omg-a\nnamespace/coder-03-other\nnamespace/default\n"
        recorder = Recorder(namespaces=listing)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            spark.Spark(CONFIG, recorder).status()
        self.assertEqual(out.getvalue().split(), ["coder-03-omg-a", "coder-03-omg-b"])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            spark.Spark(CONFIG, Recorder(namespaces="namespace/coder-03\n")).status()
        self.assertEqual(out.getvalue().strip(), "no environments")

    def test_status_of_an_environment_reads_only_inside_its_namespace(self):
        recorder = Recorder(exists=True)
        with contextlib.redirect_stdout(io.StringIO()):
            spark.Spark(CONFIG, recorder).status("t599")
        reads = [c for c in recorder.commands() if "get pods" in c or "get configmap" in c]
        self.assertEqual(len(reads), 2)
        self.assertTrue(all("-n coder-03-omg-t599" in c for c in reads))
        with self.assertRaises(spark.SparkError):
            spark.Spark(CONFIG, Recorder(exists=False)).status("t599")


class Forward(unittest.TestCase):
    def forward(self, recorder=None, returncode=0, free=lambda port: True, interrupt=False):
        launched = mock.Mock(side_effect=KeyboardInterrupt if interrupt else None, return_value=subprocess.CompletedProcess([], returncode))
        env = spark.Spark(CONFIG, recorder or Recorder(exists=True), launcher=launched, port_free=free)
        out = io.StringIO()
        try:
            with contextlib.redirect_stdout(out):
                env.forward("t599")
        finally:
            self.out, self.launched = out.getvalue(), launched

    def test_forward_targets_the_demo_pod_in_the_environment_namespace(self):
        command = spark.Spark(CONFIG, Recorder(exists=True)).forward_command("t599")
        self.assertEqual(command, ["kubectl", "--kubeconfig", "/k/spark.yaml", "-n", "coder-03-omg-t599", "port-forward", "--address", "127.0.0.1", "deployment/omg-demo", "3000:3000", "18084:18084"])
        self.forward()
        self.launched.assert_called_once_with(command)
        self.assertIn("127.0.0.1:3000", self.out)

    def test_forward_requires_an_existing_environment(self):
        launched = mock.Mock()
        with self.assertRaises(spark.SparkError):
            spark.Spark(CONFIG, Recorder(exists=False), launcher=launched, port_free=lambda port: True).forward("t599")
        launched.assert_not_called()

    def test_a_busy_port_is_named_and_nothing_starts_or_is_announced(self):
        for busy in (3000, 18084):
            with self.assertRaises(spark.SparkError) as raised:
                self.forward(free=lambda port: port != busy)
            self.assertIn(str(busy), str(raised.exception))
            self.launched.assert_not_called()
            self.assertEqual(self.out, "", "no banner before the ports are known to be free")

    def test_a_failed_port_forward_is_an_error(self):
        with self.assertRaisesRegex(spark.SparkError, "status 1"):
            self.forward(returncode=1)

    def test_ctrl_c_is_a_normal_stop(self):
        self.forward(interrupt=True)

    def test_the_port_probe_sees_a_listener(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            self.assertFalse(spark.port_is_free(listener.getsockname()[1]))


class Cli(unittest.TestCase):
    def test_main_refuses_without_the_spark_kubeconfig_and_never_runs_a_tool(self):
        with tempfile.TemporaryDirectory() as folder, mock.patch.dict(spark.os.environ, {"SPARK_KUBECONFIG": str(Path(folder) / "none.yaml")}), \
             mock.patch.object(spark, "run") as run, mock.patch.object(spark, "launch") as launch:
            for argv in (["up", "t599", "--ref", "main"], ["status"], ["down", "t599"], ["forward", "t599"], ["creds", "t599"]):
                with self.assertRaises(SystemExit) as raised:
                    spark.main(argv)
                self.assertIn("missing", str(raised.exception))
            run.assert_not_called()
            launch.assert_not_called()

    def kubeconfig(self, folder):
        path = Path(folder) / "spark.yaml"
        path.write_text("x")
        return mock.patch.dict(spark.os.environ, {"SPARK_KUBECONFIG": str(path)})

    def test_missing_tools_are_reported_before_the_cluster_is_touched(self):
        with tempfile.TemporaryDirectory() as folder, self.kubeconfig(folder), mock.patch.object(spark.shutil, "which", return_value=None), \
             mock.patch.object(spark, "run") as run:
            for argv in (["up", "t599", "--ref", "main"], ["status"]):
                with self.assertRaisesRegex(SystemExit, "kubectl"):
                    spark.main(argv)
            run.assert_not_called()
        with tempfile.TemporaryDirectory() as folder, self.kubeconfig(folder), mock.patch.object(spark.shutil, "which", side_effect=lambda n: None if n == "helm" else "/bin/" + n), \
             mock.patch.object(spark, "run") as run:
            with self.assertRaisesRegex(SystemExit, "helm"):
                spark.main(["up", "t599", "--ref", "main"])
            run.assert_not_called()

    def test_ctrl_c_exits_130_without_a_traceback(self):
        with tempfile.TemporaryDirectory() as folder, self.kubeconfig(folder), mock.patch.object(spark.shutil, "which", return_value="/bin/x"), \
             mock.patch.object(spark.Spark, "up", side_effect=KeyboardInterrupt), contextlib.redirect_stderr(io.StringIO()) as err:
            with self.assertRaises(SystemExit) as raised:
                spark.main(["up", "t599", "--ref", "main"])
        self.assertEqual(raised.exception.code, 130)
        self.assertIn("interrupted", err.getvalue())

    def test_up_needs_a_python_with_tar_filters(self):
        with tempfile.TemporaryDirectory() as folder, self.kubeconfig(folder), mock.patch.object(spark.shutil, "which", return_value="/bin/x"), \
             mock.patch.object(spark, "require_tarfile_filters", side_effect=spark.SparkError("Python 3.12")), mock.patch.object(spark.Spark, "up") as up:
            with self.assertRaisesRegex(SystemExit, "3.12"):
                spark.main(["up", "t599", "--ref", "main"])
            up.assert_not_called()

    def test_every_subcommand_documents_itself(self):
        for argv in (["-h"], ["up", "-h"], ["status", "-h"], ["forward", "-h"], ["creds", "-h"], ["down", "-h"]):
            out = io.StringIO()
            with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as raised:
                spark.main(argv)
            self.assertEqual(raised.exception.code, 0)
            self.assertGreater(len(out.getvalue()), 60, argv)
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            spark.main(["-h"])
        for command in ("up", "status", "forward", "creds", "down"):
            self.assertRegex(out.getvalue(), rf"{command}\s+\S")

    def test_creds_lists_the_passwordless_personas_without_secrets(self):
        text = spark.creds_text("coder-03-omg-t599")
        for email in ("operator@demo.invalid", "auditor@demo.invalid", "alex@demo.invalid", "blair@demo.invalid", "unentitled@demo.invalid"):
            self.assertIn(email, text)
        self.assertIn("127.0.0.1:3000", text)
        self.assertNotRegex(text, r"(?i)password:|secret")


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Run any ref of this repository on the Spark k3s test platform, then remove it.

TEST ONLY. Plain kubectl on a developer machine may target production, so this script
passes the Spark kubeconfig explicitly to every kubectl and helm call, refuses to run
unless that file exists and names the Spark server, runs every tool with a scrubbed
environment, and only ever creates, targets or deletes namespaces named
<base>-omg-<name>. See AGENTS.md "Running a branch on the cluster".

  up <name> --ref <branch|commit|release tag>   create the environment (about a minute)
  status [name]                                 list environments, or show one
  forward <name>                                port-forward the demo (loopback only)
  creds <name>                                  the passwordless demo accounts
  down <name>                                   delete the namespace and everything in it

The image, chart and demo issuer come from the ref; the Spark glue in deploy/spark comes
from this checkout. Environments have no time limit: only `down` removes them. Order, as
the invariants require: role Secrets, CloudNativePG Cluster and the chart's migrate Job
(migrate + runtime grants), bootstrap-demo, then the gateway.
"""
import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import shlex
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "spark"
# The only API server this script talks to. A kubeconfig for any other cluster is refused.
SPARK_HOST = "bitop-spark.taila06ed.ts.net"
IMAGE_REPOSITORY = "ghcr.io/bitop-dev/open-model-gateway"
LEGACY_IMAGE_REPOSITORY = "ghcr.io/ncecere/open-model-gateway"
REGISTRY = "https://ghcr.io"
NAME = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,18}[a-z0-9])")
BASE = re.compile(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?")
REF = re.compile(r"[A-Za-z0-9][A-Za-z0-9._/@+-]*")
RELEASE = re.compile(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?")
SHA256 = re.compile(r"sha256:[0-9a-f]{64}")
# <base>-omg-test-<slot> namespaces belong to scripts/spark-test.py (the suites on Spark); this script never
# creates, lists or touches them, and that script never touches <base>-omg-<name> environments.
RESERVED_PREFIX = "test-"
DEMO_PORTS = (3000, 18084)
BOOTSTRAP_JOB = "omg-bootstrap-demo"
ACCOUNTS = (
    ("Platform Admin", "operator@demo.invalid"),
    ("Platform Auditor", "auditor@demo.invalid"),
    ("Team Admin (Alex)", "alex@demo.invalid"),
    ("Platform User (Blair)", "blair@demo.invalid"),
    ("Unentitled (authenticates, platform access denied)", "unentitled@demo.invalid"),
)

# What a child process may inherit. Everything else is dropped, because kubectl and helm
# read many variables that override --kubeconfig or the server (HELM_KUBEAPISERVER,
# HELM_KUBETOKEN, HELM_KUBEASUSER, HELM_KUBECAFILE, HELM_NAMESPACE, KUBECONFIG, KUBECACHEDIR,
# GIT_DIR, ...). Each name below is needed by at least one tool:
#   PATH            find kubectl, helm, git, tar
#   HOME            kubectl and helm cache directories, git's configuration
#   LANG, LC_*      locale of the tools' messages
#   TMPDIR          temporary files (helm, git)
#   SSL_CERT_FILE, SSL_CERT_DIR   trust roots, should the API server certificate need them
#   HTTPS_PROXY, NO_PROXY (and lower case)   reaching the API server through an egress proxy
CHILD_ENVIRONMENT = ("PATH", "HOME", "LANG", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy")
# kubectl 1.33+ reads ~/.kube/kuberc, whose defaults can change flags such as the server.
# This is its documented off switch (verified against kubectl 1.36 with a kuberc that set a default).
FIXED_ENVIRONMENT = {"KUBECTL_KUBERC": "false"}
# Generated passwords of this process, removed from anything printed.
_REGISTERED_SECRETS = []


class SparkError(RuntimeError):
    pass


@dataclass(frozen=True)
class Ref:
    name: str
    sha: str
    is_tag: bool


def child_environment(environ=None):
    """The environment of every child process: an allowlist, never a copy of ours."""
    environ = os.environ if environ is None else environ
    child = {k: v for k, v in environ.items() if k in CHILD_ENVIRONMENT or k.startswith("LC_")}
    child.update(FIXED_ENVIRONMENT)
    return child


def redact(text, extra=()):
    """Remove generated passwords and credential-shaped values from text we are about to print."""
    for secret in [*_REGISTERED_SECRETS, *extra]:
        if secret:
            text = text.replace(secret, "[redacted]")
    text = re.sub(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}", r"\1[redacted]", text)
    return re.sub(r"(?i)((?:password|token|secret)[\"']?\s*[:=]\s*[\"']?)[^\s\"',}]{4,}", r"\1[redacted]", text)


def validate_name(name):
    if not NAME.fullmatch(name or ""):
        raise SparkError("name must be 2-20 characters: lowercase letters, digits and inner dashes")
    if name.startswith(RESERVED_PREFIX):
        raise SparkError(f"names starting with {RESERVED_PREFIX!r} are reserved for scripts/spark-test.py")
    return name


def namespace_for(base, name):
    return f"{base}-omg-{validate_name(name)}"


def check_namespace(base, namespace):
    prefix = f"{base}-omg-"
    if not namespace.startswith(prefix) or not NAME.fullmatch(namespace[len(prefix):]) or namespace[len(prefix):].startswith(RESERVED_PREFIX):
        raise SparkError(f"refusing namespace {namespace!r}: only {prefix}<name> environments are touched")
    return namespace


def kubeconfig_path(environ):
    """The Spark kubeconfig: SPARK_KUBECONFIG, else ~/.kube/spark-k3s.yaml (as the kspark command).

    The ambient KUBECONFIG and the default context are never consulted.
    """
    home = environ.get("HOME") or str(Path.home())
    path = Path(environ.get("SPARK_KUBECONFIG") or Path(home) / ".kube" / "spark-k3s.yaml")
    if not path.is_file():
        raise SparkError(f"Spark kubeconfig {path} is missing; refusing to fall back to any other cluster")
    return path


def base_namespace(config_view_json):
    """Check `kubectl config view --minify -o json` names the Spark server; return its namespace."""
    try:
        view = json.loads(config_view_json)
        server = view["clusters"][0]["cluster"]["server"]
        base = view["contexts"][0]["context"].get("namespace")
        host = re.fullmatch(r"https://([^/:]+)(?::\d+)?/?", server)
    except (ValueError, KeyError, IndexError, TypeError, AttributeError):
        raise SparkError("the kubeconfig could not be read; refusing to continue") from None
    if not host or host.group(1) != SPARK_HOST:
        raise SparkError(f"the kubeconfig does not point at {SPARK_HOST}; refusing to continue")
    if not base or base == "default" or not BASE.fullmatch(base):
        raise SparkError("the Spark kubeconfig has no base namespace; refusing to continue")
    return base


def kubectl_command(config, base, args, namespace=None):
    command = ["kubectl", "--kubeconfig", str(config)]
    if namespace is not None:
        command += ["-n", check_namespace(base, namespace)]
    return command + list(args)


def helm_command(config, base, args, namespace):
    return ["helm", "--kubeconfig", str(config), "-n", check_namespace(base, namespace)] + list(args)


def launch(command, stdin=None, capture=False, pipe_stderr=False):
    """Run a finite command with the scrubbed environment."""
    return subprocess.run(command, input=stdin, text=True, stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if pipe_stderr else None, env=child_environment())


def launch_forward(command):
    """Own exactly one forward child; parent-only signals cannot leave its listener behind."""
    received = None
    child = None

    def stopping(signum, _frame):
        nonlocal received
        if received is None:
            received = signum

    # SIGHUP too: the child has its own session, so a closed terminal reaches only this process.
    handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        # Record signals before spawning so a signal during Popen cannot lose the child handle.
        for sig in handlers:
            signal.signal(sig, stopping)
        child = subprocess.Popen(command, env=child_environment(), start_new_session=True)
        while received is None:
            try:
                code = child.wait(timeout=0.1)
                break
            except subprocess.TimeoutExpired:
                continue
        else:
            code = None
    finally:
        try:
            if child is not None and child.poll() is None:
                # Signal the owned process handle only, never a PID search or a process group.
                child.terminate()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    try:
                        child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        raise SparkError("port-forward child did not exit after SIGKILL") from None
        finally:
            for sig, handler in handlers.items():
                signal.signal(sig, handler)
    if received == signal.SIGINT:
        raise KeyboardInterrupt
    if received is not None:
        raise SystemExit(128 + received)
    return subprocess.CompletedProcess(command, code)


def describe(command):
    """A short, secret-free name of a command for error messages: tool and verb, no stdin."""
    words, skip = [], 0
    for word in command[1:]:
        if skip:
            skip -= 1
        elif word in ("--kubeconfig", "-n"):
            skip = 1
        elif not word.startswith("-"):
            words.append(word)
    return " ".join([command[0], *words[:3]])


def run(command, stdin=None, capture=False):
    try:
        result = launch(command, stdin=stdin, capture=capture, pipe_stderr=True)
    except FileNotFoundError:
        raise SparkError(f"{command[0]} is not installed") from None
    stderr = redact(result.stderr or "").strip()
    if result.returncode:
        raise SparkError(f"{describe(command)} failed (exit {result.returncode}): {stderr[-2000:] or 'no error output'}")
    if stderr:
        print(stderr, file=sys.stderr)
    return result.stdout if capture else ""


def require_tools(names, which=None):
    which = which or shutil.which
    missing = [name for name in names if not which(name)]
    if missing:
        raise SparkError(f"{', '.join(missing)} not found on PATH; install it before using the Spark platform")


def require_tarfile_filters():
    if not hasattr(tarfile, "data_filter"):
        raise SparkError("this Python's tarfile has no extraction filters; use Python 3.12 or newer")


def image_tag(ref, sha, is_tag):
    """Release images are multi-arch under their own tag; branch-images.yml publishes the arm64 dev image as dev-<sha12>-arm64."""
    return ref if is_tag and RELEASE.fullmatch(ref) else f"dev-{sha[:12]}-arm64"


def image_repository(tag):
    """Published releases through v0.18.0 remain in the original account; never fall back on lookup failure."""
    if RELEASE.fullmatch(tag):
        version = tuple(int(part) for part in tag[1:].split("-")[0].split("."))
        if version <= (0, 18, 0):
            return LEGACY_IMAGE_REPOSITORY
    return IMAGE_REPOSITORY


def resolve_ref(ref, runner=run, repo=ROOT):
    if not REF.fullmatch(ref or "") or ".." in ref:
        raise SparkError(f"unusable ref {ref!r}")
    try:
        sha = runner(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"], capture=True).strip()
    except SparkError:
        raise SparkError(f"{ref!r} is not a commit, branch or tag of this clone (fetch it first)") from None
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise SparkError(f"{ref!r} did not resolve to a commit")
    try:
        runner(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"refs/tags/{ref}"], capture=True)
        is_tag = True
    except SparkError:
        is_tag = False
    return Ref(ref, sha, is_tag)


def extract_ref(sha, destination, runner=run, repo=ROOT):
    """Unpack the chart and the demo issuer of the commit into destination."""
    require_tarfile_filters()
    archive = Path(destination) / "ref.tar"
    runner(["git", "-C", str(repo), "archive", "--format=tar", "-o", str(archive), sha, "deploy/helm/open-model-gateway", "scripts/demo-oidc.mjs"])
    with tarfile.open(archive) as tar:
        tar.extractall(destination, filter="data")


def arm64_manifest(index):
    for manifest in index.get("manifests") or []:
        platform = manifest.get("platform") or {}
        if platform.get("os") == "linux" and platform.get("architecture") == "arm64" and SHA256.fullmatch(manifest.get("digest", "")):
            return manifest["digest"]
    raise SparkError("the image has no linux/arm64 manifest (the Spark is arm64; see branch-images.yml)")


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def fetch_url(url, headers, opener=None, hops=3):
    """GET with redirects followed by hand, so the Authorization header never leaves the registry.

    Returns (status, lower-case headers, body bytes).
    """
    opener = opener or urllib.request.build_opener(_NoRedirect)
    headers = dict(headers)
    for _ in range(hops + 1):
        try:
            with opener.open(urllib.request.Request(url, headers=headers), timeout=30) as response:
                return response.status, {k.lower(): v for k, v in response.headers.items()}, response.read()
        except urllib.error.HTTPError as error:
            location = error.headers.get("Location") if error.headers else None
            if error.code in (301, 302, 303, 307, 308) and location:
                url = urllib.parse.urljoin(url, location)
                if not url.startswith("https://"):
                    raise SparkError("the registry redirected to a non-HTTPS address") from None
                headers.pop("Authorization", None)
                continue
            return error.code, {k.lower(): v for k, v in (error.headers or {}).items()}, b""
    raise SparkError("the registry redirected too many times")


def _sha256(body):
    return "sha256:" + hashlib.sha256(body).hexdigest()


def lookup_image(tag, expected_revision, fetch=fetch_url):
    """Return the index digest of the published image, after checking it from the registry itself.

    Anonymous pull. Digests are computed from the bytes received, the image has a linux/arm64
    manifest, and its revision label is the commit the ref resolved to (a local tag can point
    elsewhere than the commit the CI built).
    """
    repository = image_repository(tag)
    name = repository.removeprefix("ghcr.io/")
    status, _, body = fetch(f"{REGISTRY}/token?service=ghcr.io&scope=repository:{name}:pull", {})
    try:
        auth = {"Authorization": "Bearer " + json.loads(body)["token"]}
    except (ValueError, KeyError, TypeError):
        raise SparkError(f"could not get an anonymous pull token from ghcr.io (HTTP {status})") from None
    accept = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json"
    manifest_types = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json"

    def get(path, headers):
        status, received, content = fetch(f"{REGISTRY}/v2/{name}/{path}", {**auth, **headers})
        if status != 200:
            raise SparkError(f"{repository} {path} is not available (HTTP {status}); push the branch and wait for branch-images.yml")
        return received, content

    headers, body = get(f"manifests/{tag}", {"Accept": accept})
    digest = _sha256(body)
    if headers.get("docker-content-digest", digest) != digest:
        raise SparkError("the registry's digest header does not match the manifest it sent")
    try:
        child = arm64_manifest(json.loads(body))
        _, manifest = get(f"manifests/{child}", {"Accept": manifest_types})
        if _sha256(manifest) != child:
            raise SparkError("the arm64 manifest does not match its digest")
        config_digest = json.loads(manifest)["config"]["digest"]
        _, config = get(f"blobs/{config_digest}", {})
        if _sha256(config) != config_digest:
            raise SparkError("the image configuration does not match its digest")
        revision = (json.loads(config).get("config", {}).get("Labels") or {}).get("org.opencontainers.image.revision")
    except (ValueError, KeyError, TypeError, AttributeError):
        raise SparkError("the registry returned a malformed manifest") from None
    if revision != expected_revision:
        raise SparkError(f"{repository}:{tag} was built from {revision or 'an unknown revision'}, not {expected_revision}")
    return digest


def render(text, image):
    """Substitute the one placeholder; the image must be a plain pinned reference."""
    if "__IMAGE__" not in text:
        raise SparkError("manifest has no image placeholder")
    if not re.fullmatch(r"[a-z0-9./-]+@sha256:[0-9a-f]{64}", image):
        raise SparkError(f"unusable image reference {image!r}: it must be pinned by digest")
    return text.replace("__IMAGE__", image)


def role_secrets():
    """Both database role Secrets as one JSON List. Created over stdin; never an argument."""
    items = [{"apiVersion": "v1", "kind": "Secret", "metadata": {"name": f"omg-{role}-role"}, "type": "kubernetes.io/basic-auth",
              "stringData": {"username": f"gateway_{role}", "password": secrets.token_hex(24)}} for role in ("migrator", "runtime")]
    _REGISTERED_SECRETS.extend(item["stringData"]["password"] for item in items)
    return json.dumps({"apiVersion": "v1", "kind": "List", "items": items})


def job_state(job_json):
    """complete, failed or running, from `kubectl get job -o json`."""
    try:
        conditions = json.loads(job_json)["status"].get("conditions") or []
    except (ValueError, KeyError, TypeError, AttributeError):
        return "running"
    for kind, state in (("Complete", "complete"), ("Failed", "failed")):
        if any(c.get("type") == kind and c.get("status") == "True" for c in conditions):
            return state
    return "running"


def port_is_free(port):
    with socket.socket() as probe:
        # Match the forward listener: TIME_WAIT is reusable, an active listener is not.
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


def creds_text(namespace):
    rows = "\n".join(f"  {title:<52} {email}" for title, email in ACCOUNTS)
    return (f"{namespace}: the demo is passwordless; there are no stored credentials.\n"
            "Run `forward`, open http://127.0.0.1:3000, choose SSO sign-in and pick an account:\n"
            f"{rows}\n"
            "The issuer on 127.0.0.1:18084 signs in anyone who can reach it: keep it loopback and port-forward only.\n")


class Spark:
    def __init__(self, config, runner=None, launcher=None, port_free=None):
        self.config = Path(config)
        self.runner = runner or run
        self.launcher = launcher or launch_forward
        self.port_free = port_free or port_is_free
        self._base = None
        self.touched = None

    @property
    def base(self):
        if self._base is None:
            self._base = base_namespace(self.runner(kubectl_command(self.config, None, ["config", "view", "--minify", "-o", "json"]), capture=True))
        return self._base

    def kubectl(self, args, namespace=None, stdin=None, capture=False):
        return self.runner(kubectl_command(self.config, self.base, args, namespace), stdin=stdin, capture=capture)

    def helm(self, args, namespace):
        return self.runner(helm_command(self.config, self.base, args, namespace))

    def namespace_exists(self, namespace):
        check_namespace(self.base, namespace)
        return bool(self.kubectl(["get", "namespace", namespace, "--ignore-not-found", "-o", "name"], capture=True).strip())

    def create_namespace(self, namespace):
        self.kubectl(["create", "namespace", check_namespace(self.base, namespace)])

    def delete_namespace(self, namespace):
        # No --wait: the wait is a cluster-wide namespace watch, which the Spark credentials may not open.
        self.kubectl(["delete", "namespace", check_namespace(self.base, namespace), "--wait=false"])

    def environment(self, name):
        namespace = namespace_for(self.base, name)
        if not self.namespace_exists(namespace):
            raise SparkError(f"no environment {name!r} ({namespace})")
        return namespace

    def logs_hint(self, namespace, target):
        return shlex.join(kubectl_command(self.config, self.base, ["logs", target, "--all-containers"], namespace))

    def wait_for_job(self, namespace, job, polls=100):
        for _ in range(polls):
            state = job_state(self.kubectl(["get", f"job/{job}", "-o", "json"], namespace=namespace, capture=True))
            if state == "complete":
                return
            if state == "failed":
                raise SparkError(f"job {job} failed; read its logs with: {self.logs_hint(namespace, f'job/{job}')}")
            time.sleep(3)
        raise SparkError(f"job {job} did not finish in {polls * 3} seconds; read its logs with: {self.logs_hint(namespace, f'job/{job}')}")

    def up(self, name, ref):
        namespace = namespace_for(self.base, name)
        resolved = resolve_ref(ref)
        tag = image_tag(resolved.name, resolved.sha, resolved.is_tag)
        digest = lookup_image(tag, resolved.sha)
        repository = image_repository(tag)
        image = f"{repository}@{digest}"
        if self.namespace_exists(namespace):
            raise SparkError(f"{namespace} already exists; run `down {name}` first")
        job, demo = (render((DEPLOY / file).read_text(), image) for file in ("bootstrap-job.yaml", "demo.yaml"))
        print(f"{namespace}: {resolved.name} {resolved.sha[:12]} -> {repository}:{tag} ({digest[:19]}...)")
        try:
            with tempfile.TemporaryDirectory() as source:
                extract_ref(resolved.sha, source)
                self.deploy(namespace, resolved, image, digest, source, job, demo)
        except BaseException:
            if self.touched:
                print(f"{namespace} may have been created and is left behind. `status {name}` inspects it; `down {name}` removes it.", file=sys.stderr)
            raise
        print(f"ready. Next: spark-env.py forward {name}, then open http://127.0.0.1:3000 (spark-env.py creds {name}). Remove with: spark-env.py down {name}")

    def deploy(self, namespace, resolved, image, digest, source, job, demo):
        self.touched = namespace
        self.create_namespace(namespace)
        # Permissions arrive in seconds and the isolation policies a little later: wait for both.
        for _ in range(60):
            try:
                if len(self.kubectl(["get", "networkpolicy", "-o", "name"], namespace=namespace, capture=True).split()) >= 2:
                    break
            except SparkError as error:
                if "Forbidden" not in str(error):  # rights for a new namespace arrive a second or two late
                    raise
            time.sleep(2)
        else:
            raise SparkError(f"{namespace} never received its two network policies; refusing to deploy without isolation")
        # `create`, not `apply`: apply would copy the passwords into the last-applied-configuration annotation.
        self.kubectl(["create", "-f", "-"], namespace=namespace, stdin=role_secrets())
        self.kubectl(["create", "configmap", "omg-spark-env", f"--from-literal=ref={resolved.name}", f"--from-literal=sha={resolved.sha}",
                      f"--from-literal=image={image}"], namespace=namespace)
        # CloudNativePG Cluster (PostgreSQL 17) and the explicit migrate Job: migrate, then runtime grants.
        self.helm(["install", "omg", f"{source}/deploy/helm/open-model-gateway", "-f", str(DEPLOY / "values.yaml"),
                   "--set", f"image.repository={image.split('@')[0]}", "--set", f"image.digest={digest}", "--wait", "--wait-for-jobs", "--timeout", "10m", "--hide-notes"], namespace)
        self.kubectl(["create", "configmap", "omg-demo-scripts", f"--from-file=demo-oidc.mjs={source}/scripts/demo-oidc.mjs",
                      f"--from-file=pg-forward.mjs={DEPLOY / 'pg-forward.mjs'}",
                      f"--from-file=loopback-probe.mjs={DEPLOY / 'loopback-probe.mjs'}"], namespace=namespace)
        self.kubectl(["apply", "-f", "-"], namespace=namespace, stdin=job)
        self.wait_for_job(namespace, BOOTSTRAP_JOB)
        self.kubectl(["apply", "-f", "-"], namespace=namespace, stdin=demo)
        try:
            self.kubectl(["rollout", "status", "deployment/omg-demo", "--timeout=300s"], namespace=namespace)
        except SparkError as error:
            inspect = shlex.join(kubectl_command(self.config, self.base,
                                 ["describe", "pods", "-l", "app.kubernetes.io/name=omg-demo"], namespace))
            raise SparkError(f"{error}; inspect init/sidecar startup status and pod events with: {inspect}; "
                             f"read container logs with: {self.logs_hint(namespace, 'deployment/omg-demo')}") from None

    def status(self, name=None):
        if name is None:
            prefix = f"namespace/{self.base}-omg-"
            names = sorted(line.removeprefix("namespace/") for line in self.kubectl(["get", "namespace", "-o", "name"], capture=True).split()
                           if line.startswith(prefix) and not line.startswith(prefix + RESERVED_PREFIX))
            print("\n".join(names) if names else "no environments")
            return
        namespace = self.environment(name)
        print(self.kubectl(["get", "configmap", "omg-spark-env", "--ignore-not-found", "-o", "jsonpath={.data.ref} {.data.sha} {.data.image}{\"\\n\"}"], namespace=namespace, capture=True), end="")
        print(self.kubectl(["get", "pods,jobs,clusters.postgresql.cnpg.io"], namespace=namespace, capture=True), end="")

    def forward_command(self, name):
        ports = [f"{port}:{port}" for port in DEMO_PORTS]
        return kubectl_command(self.config, self.base, ["port-forward", "--address", "127.0.0.1", "deployment/omg-demo", *ports], namespace_for(self.base, name))

    def forward(self, name):
        self.environment(name)
        for port in DEMO_PORTS:
            if not self.port_free(port):
                raise SparkError(f"local port {port} is already in use (only one environment can be forwarded at a time); stop what holds it and retry")
        print("Forwarding http://127.0.0.1:3000 (dashboard and API) and 127.0.0.1:18084 (demo issuer); Ctrl-C stops. Loopback only.")
        try:
            code = self.launcher(self.forward_command(name)).returncode
        except KeyboardInterrupt:
            return
        if code:
            raise SparkError(f"kubectl port-forward exited with status {code}")

    def creds(self, name):
        print(creds_text(self.environment(name)), end="")

    def down(self, name):
        namespace = self.environment(name)
        self.delete_namespace(namespace)
        for _ in range(60):
            if not self.namespace_exists(namespace):
                print(f"deleted {namespace}. Creating pods can fail for about two minutes after a delete (`QuantityLedger ... is not initialized`); wait and retry.")
                return
            time.sleep(3)
        raise SparkError(f"{namespace} is still terminating after 3 minutes; check `status`")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    up = commands.add_parser("up", help="create an environment from a ref", description="Create <name> from a branch, commit or release tag of this clone.")
    up.add_argument("name", help="2-20 characters: lowercase letters, digits, dashes")
    up.add_argument("--ref", required=True, help="branch, commit or release tag of this clone")
    status = commands.add_parser("status", help="list environments, or show one", description="Without a name, list the environments; with one, show its ref, pods, jobs and database.")
    status.add_argument("name", nargs="?", help="environment name")
    for command, text in (("forward", "port-forward the demo to 127.0.0.1:3000 and :18084"), ("creds", "show the passwordless demo accounts"),
                          ("down", "delete the environment's namespace")):
        commands.add_parser(command, help=text, description=text).add_argument("name", help="environment name")
    args = parser.parse_args(argv)
    try:
        spark = Spark(kubeconfig_path(os.environ))
        require_tools(("kubectl", "helm", "git") if args.command == "up" else ("kubectl",))
        if args.command == "up":
            require_tarfile_filters()
            spark.up(args.name, args.ref)
        elif args.command == "status":
            spark.status(args.name)
        else:
            getattr(spark, args.command)(args.name)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        raise SystemExit(130) from None
    except SparkError as error:
        raise SystemExit(str(error)) from None


if __name__ == "__main__":
    main()

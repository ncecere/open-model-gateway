#!/usr/bin/env python3
"""Run any ref of this repository on the Spark k3s test platform, then remove it.

TEST ONLY. Plain kubectl on a developer machine may target production, so this script
passes the Spark kubeconfig explicitly to every kubectl and helm call, refuses to run
unless that file exists and names the Spark server, and only ever creates, targets or
deletes namespaces named <base>-omg-<name>. See AGENTS.md "Running a branch on the cluster".

  up <name> --ref <branch|commit|release tag>   create the environment (about 3 minutes)
  status [name]                                 list environments, or show one
  forward <name>                                port-forward the demo (loopback only)
  creds <name>                                  the passwordless demo accounts
  down <name>                                   delete the namespace and everything in it

The image, chart and demo issuer come from the ref; the Spark glue in deploy/spark comes
from this checkout. Order, as the invariants require: role Secrets, CloudNativePG Cluster
and the chart's migrate Job (migrate + runtime grants), bootstrap-demo, then the gateway.
"""
import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "spark"
# The only API server this script talks to. A kubeconfig for any other cluster is refused.
SPARK_HOST = "bitop-spark.taila06ed.ts.net"
IMAGE_REPOSITORY = "ghcr.io/ncecere/open-model-gateway"
REGISTRY = "https://ghcr.io"
NAME = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,18}[a-z0-9])")
BASE = re.compile(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?")
REF = re.compile(r"[A-Za-z0-9][A-Za-z0-9._/@+-]*")
RELEASE = re.compile(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?")
SHA256 = re.compile(r"sha256:[0-9a-f]{64}")
DEMO_PORTS = ("3000:3000", "18084:18084")
ACCOUNTS = (
    ("Platform Admin", "operator@demo.invalid"),
    ("Platform Auditor", "auditor@demo.invalid"),
    ("Team Admin (Alex)", "alex@demo.invalid"),
    ("Platform User (Blair)", "blair@demo.invalid"),
    ("Unentitled (authenticates, platform access denied)", "unentitled@demo.invalid"),
)


class SparkError(RuntimeError):
    pass


@dataclass(frozen=True)
class Ref:
    name: str
    sha: str
    is_tag: bool


def validate_name(name):
    if not NAME.fullmatch(name or ""):
        raise SparkError("name must be 2-20 characters: lowercase letters, digits and inner dashes")
    return name


def namespace_for(base, name):
    return f"{base}-omg-{validate_name(name)}"


def check_namespace(base, namespace):
    prefix = f"{base}-omg-"
    if not namespace.startswith(prefix) or not NAME.fullmatch(namespace[len(prefix):]):
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


def run(command, input=None, capture=False):
    environment = {k: v for k, v in os.environ.items() if k not in ("KUBECONFIG", "HELM_KUBECONTEXT")}
    try:
        result = subprocess.run(command, input=input, text=True, capture_output=capture, check=True, env=environment)
    except subprocess.CalledProcessError as error:
        # Never echo stdin: it can carry the generated role passwords.
        raise SparkError(f"{command[0]} failed with exit {error.returncode}: {error.stderr.strip() if error.stderr else ' '.join(command[:6])}") from None
    except FileNotFoundError:
        raise SparkError(f"{command[0]} is not installed") from None
    return result.stdout if capture else ""


def image_tag(ref, sha, is_tag):
    """Release tags are published under their own name; every other ref as dev-<sha12>."""
    return ref if is_tag and RELEASE.fullmatch(ref) else f"dev-{sha[:12]}"


def resolve_ref(ref, runner=run):
    if not REF.fullmatch(ref or "") or ".." in ref:
        raise SparkError(f"unusable ref {ref!r}")
    try:
        sha = runner(["git", "-C", str(ROOT), "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"], capture=True).strip()
    except SparkError:
        raise SparkError(f"{ref!r} is not a commit, branch or tag of this clone (fetch it first)") from None
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise SparkError(f"{ref!r} did not resolve to a commit")
    try:
        runner(["git", "-C", str(ROOT), "rev-parse", "--verify", "--quiet", f"refs/tags/{ref}"], capture=True)
        is_tag = True
    except SparkError:
        is_tag = False
    return Ref(ref, sha, is_tag)


def extract_ref(sha, destination, runner=run):
    """Unpack the chart and the demo issuer of the ref into destination."""
    archive = Path(destination) / "ref.tar"
    runner(["git", "-C", str(ROOT), "archive", "--format=tar", "-o", str(archive), sha, "deploy/helm/open-model-gateway", "scripts/demo-oidc.mjs"])
    with tarfile.open(archive) as tar:
        tar.extractall(destination, filter="data")


def arm64_manifest(index):
    for manifest in index.get("manifests") or []:
        platform = manifest.get("platform") or {}
        if platform.get("os") == "linux" and platform.get("architecture") == "arm64" and SHA256.fullmatch(manifest.get("digest", "")):
            return manifest["digest"]
    raise SparkError("the image has no linux/arm64 manifest (the Spark is arm64; see branch-images.yml)")


def fetch_url(url, headers):
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, dict(response.headers), response.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), ""


def lookup_image(tag, fetch=fetch_url):
    """Return (index digest, linux/arm64 manifest digest) of the published image, anonymously."""
    name = IMAGE_REPOSITORY.removeprefix("ghcr.io/")
    status, _, body = fetch(f"{REGISTRY}/token?service=ghcr.io&scope=repository:{name}:pull", {})
    try:
        token = json.loads(body)["token"]
    except (ValueError, KeyError, TypeError):
        raise SparkError(f"could not get an anonymous pull token from ghcr.io (HTTP {status})") from None
    accept = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json"
    status, headers, body = fetch(f"{REGISTRY}/v2/{name}/manifests/{tag}", {"Authorization": f"Bearer {token}", "Accept": accept})
    if status != 200:
        raise SparkError(f"{IMAGE_REPOSITORY}:{tag} is not published (HTTP {status}); push the branch and wait for branch-images.yml")
    digest = {k.lower(): v for k, v in headers.items()}.get("docker-content-digest") or "sha256:" + hashlib.sha256(body.encode()).hexdigest()
    if not SHA256.fullmatch(digest):
        raise SparkError("the registry returned a malformed digest")
    try:
        index = json.loads(body)
    except ValueError:
        raise SparkError("the registry returned a malformed manifest") from None
    return digest, arm64_manifest(index)


def render(text, image):
    """Substitute the one placeholder; the image must be a plain pinned reference."""
    if "__IMAGE__" not in text:
        raise SparkError("manifest has no image placeholder")
    if not re.fullmatch(r"[a-z0-9./-]+(?::[A-Za-z0-9._-]+)?(?:@sha256:[0-9a-f]{64})?", image):
        raise SparkError(f"unusable image reference {image!r}")
    return text.replace("__IMAGE__", image)


def role_secrets():
    """Both database role Secrets as one JSON List. Applied over stdin; never an argument."""
    items = [{"apiVersion": "v1", "kind": "Secret", "metadata": {"name": f"omg-{role}-role"}, "type": "kubernetes.io/basic-auth",
              "stringData": {"username": f"gateway_{role}", "password": secrets.token_hex(24)}} for role in ("migrator", "runtime")]
    return json.dumps({"apiVersion": "v1", "kind": "List", "items": items})


def creds_text(namespace):
    rows = "\n".join(f"  {title:<52} {email}" for title, email in ACCOUNTS)
    return (f"{namespace}: the demo is passwordless; there are no stored credentials.\n"
            "Run `forward`, open http://127.0.0.1:3000, choose SSO sign-in and pick an account:\n"
            f"{rows}\n"
            "The issuer on 127.0.0.1:18084 signs in anyone who can reach it: keep it loopback and port-forward only.\n")


class Spark:
    def __init__(self, config, runner=run):
        self.config = Path(config)
        self.runner = runner
        self._base = None

    @property
    def base(self):
        if self._base is None:
            self._base = base_namespace(self.runner(kubectl_command(self.config, None, ["config", "view", "--minify", "-o", "json"]), capture=True))
        return self._base

    def kubectl(self, args, namespace=None, input=None, capture=False):
        return self.runner(kubectl_command(self.config, self.base, args, namespace), input=input, capture=capture)

    def helm(self, args, namespace):
        return self.runner(helm_command(self.config, self.base, args, namespace))

    def environment(self, name):
        namespace = namespace_for(self.base, name)
        if not self.kubectl(["get", "namespace", namespace, "--ignore-not-found", "-o", "name"], capture=True).strip():
            raise SparkError(f"no environment {name!r} ({namespace})")
        return namespace

    def up(self, name, ref):
        namespace = namespace_for(self.base, name)
        resolved = resolve_ref(ref)
        tag = image_tag(resolved.name, resolved.sha, resolved.is_tag)
        digest, _ = lookup_image(tag)
        image = f"{IMAGE_REPOSITORY}@{digest}"
        if self.kubectl(["get", "namespace", namespace, "--ignore-not-found", "-o", "name"], capture=True).strip():
            raise SparkError(f"{namespace} already exists; run `down {name}` first")
        job, demo = (render((DEPLOY / file).read_text(), image) for file in ("bootstrap-job.yaml", "demo.yaml"))
        print(f"{namespace}: {resolved.name} {resolved.sha[:12]} -> {IMAGE_REPOSITORY}:{tag} ({digest[:19]}...)")
        with tempfile.TemporaryDirectory() as source:
            extract_ref(resolved.sha, source)
            self.kubectl(["create", "namespace", namespace])
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
            self.kubectl(["apply", "-f", "-"], namespace=namespace, input=role_secrets())
            self.kubectl(["create", "configmap", "omg-spark-env", f"--from-literal=ref={resolved.name}", f"--from-literal=sha={resolved.sha}",
                          f"--from-literal=image={image}"], namespace=namespace)
            # CloudNativePG Cluster (PostgreSQL 17) and the explicit migrate Job: migrate, then runtime grants.
            self.helm(["install", "omg", f"{source}/deploy/helm/open-model-gateway", "-f", str(DEPLOY / "values.yaml"),
                       "--set", f"image.digest={digest}", "--set", f"image.tag={tag}", "--wait", "--wait-for-jobs", "--timeout", "10m", "--hide-notes"], namespace)
            self.kubectl(["create", "configmap", "omg-demo-scripts", f"--from-file=demo-oidc.mjs={source}/scripts/demo-oidc.mjs",
                          f"--from-file=pg-forward.mjs={DEPLOY / 'pg-forward.mjs'}"], namespace=namespace)
            self.kubectl(["apply", "-f", "-"], namespace=namespace, input=job)
            self.kubectl(["wait", "--for=condition=complete", "job/omg-bootstrap-demo", "--timeout=300s"], namespace=namespace)
            self.kubectl(["apply", "-f", "-"], namespace=namespace, input=demo)
            self.kubectl(["rollout", "status", "deployment/omg-demo", "--timeout=300s"], namespace=namespace)
        print(f"ready. Next: spark-env.py forward {name}, then open http://127.0.0.1:3000 (spark-env.py creds {name}). Remove with: spark-env.py down {name}")

    def status(self, name=None):
        if name is None:
            prefix = f"namespace/{self.base}-omg-"
            names = sorted(line.removeprefix("namespace/") for line in self.kubectl(["get", "namespace", "-o", "name"], capture=True).split() if line.startswith(prefix))
            print("\n".join(names) if names else "no environments")
            return
        namespace = self.environment(name)
        print(self.kubectl(["get", "configmap", "omg-spark-env", "--ignore-not-found", "-o", "jsonpath={.data.ref} {.data.sha} {.data.image}{\"\\n\"}"], namespace=namespace, capture=True), end="")
        print(self.kubectl(["get", "pods,jobs,clusters.postgresql.cnpg.io"], namespace=namespace, capture=True), end="")

    def forward_command(self, name):
        return kubectl_command(self.config, self.base, ["port-forward", "--address", "127.0.0.1", "deployment/omg-demo", *DEMO_PORTS], namespace_for(self.base, name))

    def forward(self, name):
        self.environment(name)
        print("Forwarding http://127.0.0.1:3000 (dashboard and API) and 127.0.0.1:18084 (demo issuer); Ctrl-C stops. Loopback only.")
        try:
            subprocess.run(self.forward_command(name), check=False)
        except KeyboardInterrupt:
            pass

    def creds(self, name):
        print(creds_text(self.environment(name)), end="")

    def down(self, name):
        namespace = self.environment(name)
        # No --wait: the wait is a cluster-wide namespace watch, which the Spark credentials may not open.
        self.kubectl(["delete", "namespace", namespace, "--wait=false"])
        for _ in range(60):
            if not self.kubectl(["get", "namespace", namespace, "--ignore-not-found", "-o", "name"], capture=True).strip():
                print(f"deleted {namespace}")
                return
            time.sleep(3)
        raise SparkError(f"{namespace} is still terminating after 3 minutes; check `status`")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    up = commands.add_parser("up")
    up.add_argument("name")
    up.add_argument("--ref", required=True, help="branch, commit or release tag of this clone")
    status = commands.add_parser("status")
    status.add_argument("name", nargs="?")
    for command in ("forward", "creds", "down"):
        commands.add_parser(command).add_argument("name")
    args = parser.parse_args(argv)
    try:
        spark = Spark(kubeconfig_path(os.environ))
        if args.command == "up":
            spark.up(args.name, args.ref)
        elif args.command == "status":
            spark.status(args.name)
        else:
            getattr(spark, args.command)(args.name)
    except SparkError as error:
        raise SystemExit(str(error)) from None


if __name__ == "__main__":
    main()

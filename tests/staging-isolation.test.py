"""Exercise Git, BuildKit and Compose with disposable inputs only."""
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
STAGING = runpy.run_path(str(ROOT / "scripts/staging.py"))
DEMO = runpy.run_path(str(ROOT / "scripts/demo-runtime.py"))["COMPOSE"]


def clean_env(directory):
    return {"PATH": os.environ["PATH"], "HOME": str(directory), "COMPOSE_DISABLE_ENV_FILE": "1"}


def render(command, directory, env=None):
    result = subprocess.run([*command, "config", "--format", "json"], cwd=directory,
                            env=clean_env(directory) if env is None else env, capture_output=True, text=True, check=True)
    return json.loads(result.stdout)


class IsolationTests(unittest.TestCase):
    def assert_isolated(self, staging, demo):
        self.assertNotEqual(staging["name"], demo["name"], "shared project")
        for model in (staging, demo):
            mounts = [m for m in model["services"]["postgres"]["volumes"]
                      if m["target"] == "/var/lib/postgresql/data"]
            self.assertEqual(len(mounts), 1, "missing PostgreSQL data mount")
            self.assertEqual(mounts[0]["type"], "volume")
            volume = model["volumes"][mounts[0]["source"]]
            self.assertTrue(volume["name"])
            self.assertFalse(volume.get("external"), "database volume is externally managed")
            self.assertEqual(volume.get("driver", "local"), "local")
            self.assertFalse(volume.get("driver_opts"), "database volume redirects storage")
        staging_names = {v["name"] for v in staging["volumes"].values()}
        demo_names = {v["name"] for v in demo["volumes"].values()}
        self.assertFalse(staging_names & demo_names, "shared volume")

    def test_resolved_compose_storage(self):
        with tempfile.TemporaryDirectory() as folder:
            state = Path(folder)
            shutil.copyfile(STAGING["DEPLOY"] / "staging.env.example", state / "staging.env")
            demo = render(DEMO, state)
            # Capture the CLI default and its files without querying running containers.
            argv = ["staging.py", "status", "--state-dir", str(state)]
            with patch.dict(os.environ, clean_env(state), clear=True), patch.object(sys, "argv", argv), patch.object(subprocess, "run") as run:
                STAGING["main"]()
            invocation = run.call_args.args[0]
            self.assertEqual(invocation[-1], "ps")
            command = invocation[:-1]
            env = run.call_args.kwargs["env"]
            staging = render(command, state, env)
            self.assert_isolated(staging, demo)

            manifest = ["docker", "compose", "-f", str(STAGING["DEPLOY"] / "compose.yaml")]
            self.assert_isolated(render(manifest, state), demo)

            override = state / "collision.yaml"
            demo_mount = next(m for m in demo["services"]["postgres"]["volumes"]
                              if m["target"] == "/var/lib/postgresql/data")
            demo_volume = demo["volumes"][demo_mount["source"]]["name"]
            override.write_text("volumes:\n  database:\n    name: " + demo_volume + "\n")
            with self.assertRaisesRegex(AssertionError, "shared volume"):
                self.assert_isolated(render([*command, "-f", str(override)], state, env), demo)

            with self.assertRaisesRegex(AssertionError, "shared project"):
                self.assert_isolated(render([*manifest, "-p", demo["name"]], state), demo)

            result = subprocess.run([sys.executable, "-I", str(ROOT / "scripts/staging.py"), "init",
                                     "--project", demo["name"], "--state-dir", str(state / "unused")],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("demo projects are never accepted", result.stderr)
            self.assertFalse((state / "unused").exists())

    def test_local_artifacts(self):
        with tempfile.TemporaryDirectory() as folder:
            context = Path(folder) / "context"
            context.mkdir()
            for name in (".gitignore", ".dockerignore"):
                shutil.copyfile(ROOT / name, context / name)
            # The nested marker and its control sit in an allowed build subtree. Neutral filenames
            # ensure only the directory policy can exclude the nested marker.
            markers = [".local/staging/probe", "apps/web/public/.local/probe",
                       ".local/staging/secrets/runtime_password",
                       ".local/staging/backups/gateway-20000101T000000Z-00000000.dump"]
            control = "apps/web/public/probe"
            for name in [*markers, control]:
                path = context / name
                path.parent.mkdir(exist_ok=True, parents=True)
                path.write_text("harmless fixture\n")
            git_env = {"PATH": os.environ["PATH"], "HOME": folder, "GIT_CONFIG_NOSYSTEM": "1"}
            subprocess.run(["git", "init", "-q", "--template="], cwd=context, env=git_env, check=True)
            for name in [*markers, control]:
                result = subprocess.run(["git", "check-ignore", "-q", "--", name], cwd=context, env=git_env)
                self.assertEqual(result.returncode, 1 if name == control else 0, name)

            (context / "Cargo.toml").write_text("harmless fixture\n")
            for index, (name, included) in enumerate([("Dockerfile", control),
                                                      ("deploy/loadtest/Dockerfile", "Cargo.toml")]):
                with self.subTest(dockerfile=name):
                    dockerfile = context / name
                    dockerfile.parent.mkdir(parents=True, exist_ok=True)
                    dockerfile.write_text("FROM scratch\nCOPY . /\n")
                    specific = ROOT / (name + ".dockerignore")
                    if specific.exists():
                        shutil.copyfile(specific, context / (name + ".dockerignore"))
                    output = Path(folder) / f"export-{index}"
                    subprocess.run(["docker", "buildx", "build", "--network=none", "-f", str(dockerfile),
                                    "--output", f"type=local,dest={output}", str(context)], check=True)
                    self.assertEqual((output / included).read_bytes(), (context / included).read_bytes())
                    for marker in markers:
                        self.assertFalse((output / marker).exists(), marker)


if __name__ == "__main__":
    unittest.main()

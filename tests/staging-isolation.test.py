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


def render(command, directory):
    env = {"PATH": os.environ["PATH"], "HOME": str(directory), "COMPOSE_DISABLE_ENV_FILE": "1"}
    result = subprocess.run([*command, "config", "--format", "json"], cwd=directory,
                            env=env, capture_output=True, text=True, check=True)
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
            (state / "staging.env").touch()
            demo = render(DEMO, state)
            # Capture the CLI default and its files without querying running containers.
            argv = ["staging.py", "status", "--state-dir", str(state)]
            with patch.object(sys, "argv", argv), patch.object(subprocess, "run") as run:
                STAGING["main"]()
            invocation = run.call_args.args[0]
            self.assertEqual(invocation[-1], "ps")
            command = invocation[:-1]
            staging = render(command, state)
            self.assert_isolated(staging, demo)

            manifest = ["docker", "compose", "-f", str(STAGING["DEPLOY"] / "compose.yaml")]
            self.assert_isolated(render(manifest, state), demo)

            override = state / "collision.yaml"
            demo_mount = next(m for m in demo["services"]["postgres"]["volumes"]
                              if m["target"] == "/var/lib/postgresql/data")
            demo_volume = demo["volumes"][demo_mount["source"]]["name"]
            override.write_text("volumes:\n  database:\n    name: " + demo_volume + "\n")
            with self.assertRaisesRegex(AssertionError, "shared volume"):
                self.assert_isolated(render([*command, "-f", str(override)], state), demo)

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
            specific_ignore = ROOT / "Dockerfile.dockerignore"
            if specific_ignore.exists():
                shutil.copyfile(specific_ignore, context / specific_ignore.name)
            # The nested marker and its control sit in an allowed build subtree. Neutral filenames
            # ensure only the directory policy can exclude the nested marker.
            markers = [".local/staging/probe", "apps/web/public/.local/probe"]
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

            (context / "Dockerfile").write_text("FROM scratch\nCOPY . /\n")
            output = Path(folder) / "export"
            subprocess.run(["docker", "buildx", "build", "--network=none", "--output",
                            f"type=local,dest={output}", str(context)], check=True)
            self.assertEqual((output / control).read_bytes(), (context / control).read_bytes())
            for name in markers:
                self.assertFalse((output / name).exists(), name)


if __name__ == "__main__":
    unittest.main()

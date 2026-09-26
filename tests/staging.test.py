"""Host-side staging safety tests. No Docker, real secrets or demo DB access."""
import importlib.util
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("staging", ROOT / "scripts" / "staging.py")
staging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(staging)


class StagingTests(unittest.TestCase):
    def test_secret_generation_is_private_separate_and_never_overwritten(self):
        with tempfile.TemporaryDirectory() as folder:
            state = Path(folder) / "state"
            staging.create_state(state)
            self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE((state / "secrets").stat().st_mode), 0o700)
            passwords = [(state / "secrets" / (role + "_password")).read_text() for role in ("postgres", "migrator", "runtime")]
            self.assertEqual(len(set(passwords)), 3)
            for password in passwords:
                self.assertRegex(password, r"^[a-f0-9]{64}\n$")
            for role in ("migrator", "runtime"):
                path = state / "secrets" / (role + "_database_url")
                self.assertTrue(path.read_text().startswith("postgres://gateway_" + role + ":"))
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o444)
            with self.assertRaises(FileExistsError):
                staging.create_state(state)
            self.assertEqual(passwords[0], (state / "secrets" / "postgres_password").read_text())
            config = staging.settings(state)
            self.assertEqual(config["GATEWAY_PUBLIC_URL"], "https://localhost:18443")
            self.assertEqual(config["STAGING_BIND_ADDRESS"], "127.0.0.1")
            self.assertNotIn("GATEWAY_OIDC_ISSUER", config)
            self.assertNotIn("STAGING_OPENAI_ENABLED", config)

    def test_overlays_are_explicit_not_enabled_by_placeholder_values(self):
        base = staging.compose(Path("/state"), "omg-staging-ci", {})
        self.assertEqual(base.count("-f"), 1)
        enabled = staging.compose(Path("/state"), "omg-staging-ci", {"STAGING_OIDC_ENABLED": "1", "STAGING_OIDC_CONFIDENTIAL": "1", "STAGING_ANTHROPIC_ENABLED": "1"})
        self.assertEqual(enabled.count("-f"), 4)
        self.assertNotIn(str(staging.DEPLOY / "compose.openai.yaml"), enabled)
        with self.assertRaises(ValueError):
            staging.compose(Path("/state"), "omg-staging-ci", {"STAGING_OIDC_CONFIDENTIAL": "1"})

    def test_demo_project_names_are_rejected_before_any_state_change(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "must-not-exist"
            result = subprocess.run([sys.executable, str(ROOT / "scripts/staging.py"), "init", "--project", "open-model-gateway", "--state-dir", str(path)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(path.exists())

    def test_init_refuses_existing_symlink(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / "target"
            target.mkdir()
            link = Path(folder) / "link"
            link.symlink_to(target, target_is_directory=True)
            with self.assertRaises(FileExistsError):
                staging.create_state(link)
            self.assertEqual(list(target.iterdir()), [])


if __name__ == "__main__":
    unittest.main()

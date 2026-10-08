import contextlib
import importlib.util
import io
from pathlib import Path
import stat
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("demo_runtime", Path(__file__).resolve().parents[1] / "scripts/demo-runtime.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RuntimeInitialization(unittest.TestCase):
    def setup_checkout(self, root):
        root = Path(root)
        (root / "deploy/staging").mkdir(parents=True)
        (root / "deploy/staging/runtime-grants.sql").write_text("BEGIN; COMMIT;")
        (root / "deploy/staging/verify-privileges.sql").write_text("BEGIN; ROLLBACK;")
        (root / ".env.demo.example").write_text("DATABASE_URL=unused\nGATEWAY_ENV=development\n")
        return root / ".local/enterprise-rebuild/demo-runtime.env"

    def test_existing_file_is_preserved_without_any_sql(self):
        with tempfile.TemporaryDirectory() as root:
            destination = self.setup_checkout(root)
            destination.parent.mkdir(parents=True)
            destination.write_text("preserve me")
            with mock.patch.multiple(module, ROOT=Path(root), DESTINATION=destination), mock.patch.object(module, "sql") as sql:
                with self.assertRaises(RuntimeError):
                    module.main()
                sql.assert_not_called()
            self.assertEqual(destination.read_text(), "preserve me")

    def test_existing_role_is_never_reset(self):
        with tempfile.TemporaryDirectory() as root:
            destination = self.setup_checkout(root)
            with mock.patch.multiple(module, ROOT=Path(root), DESTINATION=destination), mock.patch.object(module, "sql", side_effect=["gateway_enterprise_demo", "enterprise_v1", "t", "t"]) as sql:
                with self.assertRaises(RuntimeError):
                    module.main()
                self.assertEqual(sql.call_count, 4)
            self.assertFalse(destination.exists())

    def test_new_environment_is_private_and_credentials_not_printed(self):
        with tempfile.TemporaryDirectory() as root:
            destination = self.setup_checkout(root)
            output = io.StringIO()
            with mock.patch.multiple(module, ROOT=Path(root), DESTINATION=destination), mock.patch.object(module, "sql", side_effect=["gateway_enterprise_demo", "enterprise_v1", "t", "f", "", "", ""]), mock.patch.object(module.secrets, "token_urlsafe", return_value="fixture-not-a-real-password"), contextlib.redirect_stdout(output):
                module.main()
            self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
            self.assertIn("gateway_runtime", destination.read_text())
            self.assertNotIn("fixture-not-a-real-password", output.getvalue())

    def test_partial_failure_preserves_private_credential_for_explicit_recovery(self):
        with tempfile.TemporaryDirectory() as root:
            destination = self.setup_checkout(root)
            with mock.patch.multiple(module, ROOT=Path(root), DESTINATION=destination), mock.patch.object(module, "sql", side_effect=["gateway_enterprise_demo", "enterprise_v1", "t", "f", RuntimeError("SQL failed")]):
                with self.assertRaises(RuntimeError):
                    module.main()
            self.assertTrue(destination.exists())
            self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)

    def test_wrong_database_is_rejected_without_new_environment(self):
        with tempfile.TemporaryDirectory() as root:
            destination = self.setup_checkout(root)
            with mock.patch.multiple(module, ROOT=Path(root), DESTINATION=destination), mock.patch.object(module, "sql", return_value="legacy") as sql:
                with self.assertRaises(RuntimeError):
                    module.main()
                self.assertEqual(sql.call_count, 1)
            self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()

"""Host-side load-test runner safety tests. No Docker or database access."""
import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("loadtest", ROOT / "scripts" / "loadtest.py")
loadtest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(loadtest)


def sample_report(label="empty-1r-pgbouncer-60", violations=(), consistent=True):
    summary = {"count": 10, "mean_ms": 1.0, "p50_ms": 1.0, "p95_ms": 2.0, "p99_ms": 3.0}
    return {
        "config": {"label": label, "rate": 60.0},
        "throughput_ok_per_s": 59.9,
        "latency_ms": {"all": {"p50_ms": 63.6, "p95_ms": 67.7, "p99_ms": 69.8}},
        "gateway_overhead_ms": {"p50_ms": 10.6, "p99_ms": 15.2},
        "gateway": {"admission": {"phase=total,outcome=admitted": summary,
                                  "phase=locks,outcome=admitted": summary},
                    "settlement": {"phase=total,outcome=settled": summary}},
        "totals": {"by_status": {"200": 2700}},
        "violations": list(violations),
        "budget_verify": {"consistent": consistent},
        "stack": {"replicas": 1, "via": "pgbouncer"},
    }


class LoadtestTests(unittest.TestCase):
    def test_only_throwaway_database_names(self):
        for name in ("omg_loadtest", "omg_loadtest_seeded", "omg_loadtest2"):
            self.assertEqual(loadtest.check_db_name(name), name)
        for name in ("gateway", "gateway_enterprise_demo", "postgres", "omg_load", "omg_loadtest;drop",
                     "OMG_LOADTEST", "omg_loadtest-x"):
            with self.assertRaises(SystemExit):
                loadtest.check_db_name(name)

    def test_compose_always_uses_the_loadtest_project(self):
        command = loadtest.compose_command(Path("/state"), "up", "-d", profiles=("tools",))
        self.assertEqual(command[:4], ["docker", "compose", "-p", "omg-loadtest"])
        self.assertIn(str(loadtest.DEPLOY / "compose.yaml"), command)
        self.assertEqual(command[-4:], ["--profile", "tools", "up", "-d"])
        compose = (loadtest.DEPLOY / "compose.yaml").read_text()
        self.assertIn("name: omg-loadtest", compose)
        for port in loadtest.FORBIDDEN_PORTS:
            self.assertNotIn(f":{port}:", compose)
        # No Docker Hub images (docs/verification.md).
        for line in compose.splitlines():
            if line.strip().startswith("image:") and "OMG_LOADTEST_IMAGE" not in line:
                self.assertRegex(line, r"image: (mirror\.gcr\.io|ghcr\.io)/")

    def test_second_project_is_isolated_by_slot(self):
        try:
            loadtest.configure("omg-loadtest-p1", 1)
            command = loadtest.compose_command(Path("/state"), "down")
            self.assertEqual(command[:4], ["docker", "compose", "-p", "omg-loadtest-p1"])
            env = loadtest.slot_env()
            self.assertEqual(env["OMG_LOADTEST_POSTGRES_PORT"], "54469")
            self.assertEqual([env[f"OMG_LOADTEST_GATEWAY_PORT_{i}"] for i in (1, 2, 3)],
                             ["18311", "18312", "18313"])
            self.assertEqual(env["OMG_LOADTEST_SUBNET"], "10.213.48")
            self.assertTrue(str(loadtest.state_dir()).endswith(".local/omg-loadtest-p1")
                            or "OMG_LOADTEST_STATE" in loadtest.os.environ)
            self.assertIn(loadtest.IMAGE, ("omg-loadtest-p1:local", loadtest.os.environ.get("OMG_LOADTEST_IMAGE")))
            for project, slot in (("omg-loadtest-p1", 0), ("omg-loadtest", 1), ("gateway", 1),
                                  ("omg-loadtest-P1", 1), ("omg-loadtest-p1", 10)):
                with self.assertRaises(SystemExit):
                    loadtest.configure(project, slot)
            compose = (loadtest.DEPLOY / "compose.yaml").read_text()
            self.assertIn("${OMG_LOADTEST_SUBNET:-10.213.47}.0/24", compose)
            self.assertIn("${OMG_LOADTEST_GATEWAY_PORT_3:-18303}", compose)
        finally:
            loadtest.configure()
        self.assertEqual(loadtest.slot_env()["OMG_LOADTEST_POSTGRES_PORT"], "54369")

    def test_state_is_private_and_stable(self):
        with tempfile.TemporaryDirectory() as folder:
            state = Path(folder) / "loadtest"
            env = loadtest.create_state(state)
            self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE((state / "stack.env").stat().st_mode), 0o600)
            self.assertEqual(len(set(env.values())), 3)
            for value in env.values():
                self.assertRegex(value, r"^[0-9a-f]{48}$")
            self.assertEqual(loadtest.create_state(state), env)
            noprep = (state / "pgbouncer-noprepared.ini").read_text()
            self.assertIn("max_prepared_statements = 0", noprep)
            self.assertIn("pool_mode = transaction", noprep)
            self.assertEqual((state / "userlist.txt").read_text(), "")

    def test_write_if_changed_keeps_unchanged_files(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "userlist.txt"
            loadtest.write_if_changed(path, "a\n")
            before = path.stat().st_ino
            loadtest.write_if_changed(path, "a\n")
            self.assertEqual(path.stat().st_ino, before)
            loadtest.write_if_changed(path, "b\n")
            self.assertEqual(path.read_text(), "b\n")

    def test_report_rows_flag_failed_invariants(self):
        table = loadtest.render([sample_report(), sample_report("bad", violations=["x"]),
                                 sample_report("unverified", consistent=False)])
        lines = table.splitlines()
        self.assertEqual(len(lines), 5)
        self.assertIn("| empty-1r-pgbouncer-60 | 1 | pgbouncer | 60 | 59.9 | 63.6 / 67.7 / 69.8 |", lines[2])
        self.assertTrue(lines[2].endswith("| 200:2700 | ok |"))
        self.assertTrue(lines[3].endswith("| FAIL |"))
        self.assertTrue(lines[4].endswith("| FAIL |"))
        json.dumps(sample_report())


if __name__ == "__main__":
    unittest.main()

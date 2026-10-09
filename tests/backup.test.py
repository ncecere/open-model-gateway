"""Backup/restore safety tests.

Unit tests use a fake PostgreSQL runner (no database, Docker or secrets).
The optional integration test runs only when BACKUP_TEST_DATABASE_URL names a
loopback *disposable* server (e.g. the 54339 regression cluster); it creates
and drops only its own omg_backup_* databases. With BACKUP_TEST_PG_CONTAINER
the PostgreSQL tools run inside that container and the URL is resolved there.
BACKUP_TEST_GATEWAY_BINARY optionally checks lineage against a real binary.
"""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("backup", ROOT / "scripts" / "backup.py")
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)

LINEAGE = [{"version": 1, "checksum": "aa" * 48}, {"version": 2, "checksum": "bb" * 48}]


def lineage_json(migrations=LINEAGE, success=True, family="enterprise_v1"):
    return json.dumps({
        "migrations": [dict(m, success=success) for m in migrations],
        "schema_family": family,
        "server_version": "17.0",
        "database": "gateway",
    })


class FakePostgres:
    def __init__(self, lineages=None, empty="0", exists="0", fail_restore=False):
        self.lineages = list(lineages or [lineage_json(), lineage_json()])
        self.empty = empty
        self.exists = exists
        self.fail_restore = fail_restore
        self.calls = []

    def query(self, sql, database=None):
        self.calls.append(("query", sql.split()[0:3], database))
        if "json_build_object" in sql:
            return self.lineages.pop(0) if len(self.lineages) > 1 else self.lineages[0]
        if "pg_database WHERE datname" in sql:
            return self.exists
        if "pg_class" in sql:
            return self.empty
        if "ledger entries" in sql:
            return "3 ledger entries, 2 executions"
        return ""

    def dump(self, output):
        self.calls.append(("dump",))
        output.write(b"PGDMP fake archive")

    def list_archive(self, archive):
        self.calls.append(("list", archive.read(5)))

    def restore(self, archive, database):
        self.calls.append(("restore", database))
        if self.fail_restore:
            raise backup.BackupError("pg_restore failed with exit status 1")


class UnitTests(unittest.TestCase):
    def test_password_never_reaches_argv(self):
        env = backup.connection_env("postgres://migrator:s%40cret@db.internal:6543/gateway?sslmode=verify-full")
        self.assertEqual(env["PGPASSWORD"], "s@cret")
        self.assertEqual(env["PGSSLMODE"], "verify-full")
        self.assertEqual((env["PGHOST"], env["PGPORT"], env["PGDATABASE"]), ("db.internal", "6543", "gateway"))
        for container in (None, "pg"):
            command, process_env = backup.Postgres(env, container).command(["psql"], database="other")
            self.assertNotIn("s@cret", " ".join(command))
            self.assertEqual(process_env["PGPASSWORD"], "s@cret")
            self.assertEqual(process_env["PGDATABASE"], "other")
        command, _ = backup.Postgres(env, "pg").command(["psql"])
        self.assertEqual(command[:3], ["docker", "exec", "-i"])
        self.assertIn("PGPASSWORD", command)
        for bad in ("mysql://u:p@h/db", "postgres://u:p@h/", "postgres://u:p@h/db?host=/tmp"):
            with self.assertRaises(backup.BackupError) as raised:
                backup.connection_env(bad)
            self.assertNotIn(":p@", str(raised.exception))

    def test_target_names_are_explicit_identifiers_and_never_protected(self):
        self.assertEqual(backup.valid_database_name("gateway_restore_2026"), "gateway_restore_2026")
        for bad in ("", "Gateway", "x;DROP", 'a"b', "postgres", "template1", "gateway_enterprise_demo", "gateway_demo", "a" * 64):
            with self.assertRaises(backup.BackupError):
                backup.valid_database_name(bad)

    def test_backup_writes_private_checksummed_manifest_with_lineage(self):
        with tempfile.TemporaryDirectory() as folder:
            pg = FakePostgres()
            manifest_path = backup.backup(pg, Path(folder) / "out")
            manifest = json.loads(manifest_path.read_text())
            dump = manifest_path.parent / manifest["file"]
            self.assertEqual(stat.S_IMODE(dump.stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(manifest_path.stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE((Path(folder) / "out").stat().st_mode), 0o700)
            self.assertEqual(manifest["sha256"], backup.sha256(dump))
            self.assertEqual(manifest["migrations"], LINEAGE)
            self.assertEqual(manifest["latest_version"], 2)
            self.assertEqual(manifest["format"], "omg-backup-v1")
            self.assertIn("-v0002-", manifest["file"])
            self.assertIn(("list", b"PGDMP"), pg.calls)
            backup.verify(manifest_path)

    def test_dirty_or_changing_lineage_leaves_no_files(self):
        for lineages in ([lineage_json(success=False)], [lineage_json(family="other")], [lineage_json(), lineage_json(LINEAGE[:1])]):
            with tempfile.TemporaryDirectory() as folder:
                with self.assertRaises(backup.BackupError):
                    backup.backup(FakePostgres(lineages), Path(folder))
                self.assertEqual(list(Path(folder).iterdir()), [])

    def test_verify_detects_tampering_missing_files_and_path_escape(self):
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(FakePostgres(), Path(folder))
            manifest = json.loads(manifest_path.read_text())
            dump = Path(folder) / manifest["file"]
            dump.chmod(0o600)
            with dump.open("ab") as handle:
                handle.write(b"x")
            with self.assertRaises(backup.BackupError):
                backup.verify(manifest_path)
            dump.unlink()
            with self.assertRaises(backup.BackupError):
                backup.verify(manifest_path)
            for name in ("../escape.dump", ".hidden", "a/b.dump"):
                bad = Path(folder) / "bad.json"
                bad.write_text(json.dumps(dict(manifest, file=name)))
                with self.assertRaises(backup.BackupError):
                    backup.verify(bad)

    def test_restore_requires_matching_lineage(self):
        manifest = {"migrations": LINEAGE, "latest_version": 2}
        backup.check_compatible(manifest, expected_migrations=LINEAGE)
        backup.check_compatible(manifest, expected_version=2)
        for kwargs in ({"expected_migrations": LINEAGE[:1]}, {"expected_migrations": [LINEAGE[0], dict(LINEAGE[1], checksum="cc" * 48)]}, {"expected_version": 3}, {}):
            with self.assertRaises(backup.BackupError):
                backup.check_compatible(manifest, **kwargs)

    def test_restore_refuses_non_empty_or_existing_targets_without_touching_data(self):
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(FakePostgres(), Path(folder))
            pg = FakePostgres(empty="4")
            with self.assertRaises(backup.BackupError) as raised:
                backup.restore(pg, manifest_path, "gateway_restored", expected_migrations=LINEAGE)
            self.assertIn("not empty", str(raised.exception))
            self.assertFalse([c for c in pg.calls if c[0] == "restore"])
            self.assertFalse([c for c in pg.calls if c[1][:1] == ["DROP"]])
            pg = FakePostgres(exists="1")
            with self.assertRaises(backup.BackupError):
                backup.restore(pg, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE)
            self.assertFalse([c for c in pg.calls if c[1][:1] in (["CREATE"], ["DROP"])])
            # Lineage mismatch is rejected before any database change.
            pg = FakePostgres()
            with self.assertRaises(backup.BackupError):
                backup.restore(pg, manifest_path, "gateway_restored", create=True, expected_version=9)
            self.assertFalse([c for c in pg.calls if c[0] == "query"])

    def test_failed_restore_drops_only_the_database_it_created(self):
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(FakePostgres(), Path(folder))
            pg = FakePostgres(fail_restore=True)
            with self.assertRaises(backup.BackupError):
                backup.restore(pg, manifest_path, "gateway_new", create=True, expected_migrations=LINEAGE)
            self.assertIn(("query", ["DROP", "DATABASE", '"gateway_new"'], None), pg.calls)
            pg = FakePostgres(fail_restore=True)
            with self.assertRaises(backup.BackupError):
                backup.restore(pg, manifest_path, "gateway_existing", expected_migrations=LINEAGE)
            self.assertFalse([c for c in pg.calls if c[1][:1] == ["DROP"]])
            restored, counts = backup.restore(FakePostgres(), manifest_path, "gateway_ok", create=True, expected_migrations=LINEAGE)
            self.assertEqual(restored["latest_version"], 2)
            self.assertIn("ledger entries", counts)

    def test_binary_lineage_comes_from_schema_version(self):
        with tempfile.TemporaryDirectory() as folder:
            fake = Path(folder) / "gateway"
            fake.write_text("#!/bin/sh\ntest \"$1\" = schema-version || exit 2\necho '" + json.dumps({"latest_version": 2, "migrations": LINEAGE}) + "'\n")
            fake.chmod(0o700)
            self.assertEqual(backup.binary_lineage(fake), LINEAGE)
            with self.assertRaises(backup.BackupError):
                backup.binary_lineage(Path(folder) / "missing")

    def test_cli_failures_never_print_the_url(self):
        env = {"PATH": "/nonexistent", "OMG_BACKUP_URL": "postgres://u:topsecret@127.0.0.1:1/db"}
        with tempfile.TemporaryDirectory() as folder:
            result = subprocess.run([sys.executable, str(ROOT / "scripts/backup.py"), "backup", "--url-env", "OMG_BACKUP_URL", "--out-dir", folder], capture_output=True, text=True, env=env)
            self.assertEqual(result.returncode, 1)
            self.assertNotIn("topsecret", result.stdout + result.stderr)
            self.assertIn("not found", result.stderr)
            self.assertEqual([p for p in Path(folder).iterdir() if p.suffix == ".dump"], [])


@unittest.skipUnless(os.environ.get("BACKUP_TEST_DATABASE_URL"), "set BACKUP_TEST_DATABASE_URL to a disposable loopback server")
class DisposableDatabaseTests(unittest.TestCase):
    def setUp(self):
        url = os.environ["BACKUP_TEST_DATABASE_URL"]
        if urlsplit(url).hostname not in ("127.0.0.1", "::1", "localhost"):
            self.skipTest("integration test only runs against a loopback server")
        self.env = backup.connection_env(url)
        self.pg = backup.Postgres(self.env, os.environ.get("BACKUP_TEST_PG_CONTAINER"))
        suffix = secrets.token_hex(6)
        self.source, self.target = f"omg_backup_src_{suffix}", f"omg_backup_dst_{suffix}"
        binary = os.environ.get("BACKUP_TEST_GATEWAY_BINARY")
        self.lineage = backup.binary_lineage(binary) if binary else LINEAGE
        self.pg.query(f'CREATE DATABASE "{self.source}";')
        rows = ",".join(f"({m['version']},'m{m['version']}',true,decode('{m['checksum']}','hex'),1)" for m in self.lineage)
        self.pg.query(f"""CREATE TABLE _sqlx_migrations(version bigint PRIMARY KEY, description text NOT NULL, installed_on timestamptz NOT NULL DEFAULT now(), success boolean NOT NULL, checksum bytea NOT NULL, execution_time bigint NOT NULL);
            INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES {rows};
            CREATE TABLE installation(singleton boolean PRIMARY KEY, schema_family text NOT NULL);
            INSERT INTO installation VALUES(true,'enterprise_v1');
            CREATE TABLE inference_executions(id uuid PRIMARY KEY);
            CREATE TABLE monetary_ledger(id uuid PRIMARY KEY, execution_id uuid REFERENCES inference_executions(id), amount_microusd bigint);
            INSERT INTO inference_executions VALUES(gen_random_uuid()),(gen_random_uuid());
            INSERT INTO monetary_ledger SELECT gen_random_uuid(),id,11 FROM inference_executions;""", self.source)

    def tearDown(self):
        for name in (self.source, self.target):
            self.pg.query(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE);')

    def test_backup_verify_restore_and_refusals(self):
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(source, Path(folder))
            manifest, _ = backup.verify(manifest_path, source)
            self.assertEqual(manifest["migrations"], self.lineage)
            with self.assertRaises(backup.BackupError):
                backup.restore(self.pg, manifest_path, self.target, create=True, expected_version=manifest["latest_version"] + 1)
            self.assertEqual(self.pg.query(f"SELECT count(*) FROM pg_database WHERE datname='{self.target}';"), "0")
            restored, counts = backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage)
            self.assertEqual(restored["migrations"], self.lineage)
            self.assertEqual(counts, "2 ledger entries, 2 executions")
            self.assertEqual(self.pg.query("SELECT sum(amount_microusd) FROM monetary_ledger;", self.target), "22")
            # The now-populated target and the source are refused; data is untouched.
            for name in (self.target, self.source):
                with self.assertRaises(backup.BackupError):
                    backup.restore(self.pg, manifest_path, name, expected_migrations=self.lineage)
            self.assertEqual(self.pg.query("SELECT count(*) FROM monetary_ledger;", self.target), "2")
            with self.assertRaises(backup.BackupError):
                backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage)


if __name__ == "__main__":
    unittest.main()

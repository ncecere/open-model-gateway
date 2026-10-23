"""Backup/restore safety tests.

Unit tests use a fake PostgreSQL runner (no database, Docker or secrets).
The optional integration test runs only when BACKUP_TEST_DATABASE_URL names a
loopback *disposable* server (e.g. the 54339 regression cluster); it creates
and drops only its own omg_backup_* databases. With BACKUP_TEST_PG_CONTAINER
the PostgreSQL tools run inside that container and the URL is resolved there.
BACKUP_TEST_GATEWAY_BINARY optionally checks lineage against a real binary.
"""
from datetime import datetime, timedelta, timezone
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock
from urllib.parse import parse_qsl, quote, unquote, urlsplit
from xml.sax.saxutils import escape

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
    def __init__(self, lineages=None, empty="0", exists="0", fail_restore=False, file_key_ids=None, payload=b"PGDMP fake archive", snoop=None):
        self.lineages = list(lineages or [lineage_json(), lineage_json()])
        self.empty = empty
        self.exists = exists
        self.fail_restore = fail_restore
        self.file_key_ids = file_key_ids  # None: no stored_files table
        self.payload = payload
        self.snoop = snoop  # directory listed while the dump is being written
        self.seen_during_dump = None
        self.restored = None
        self.calls = []
        self.statements = []

    def query(self, sql, database=None):
        self.calls.append(("query", sql.split()[0:3], database))
        self.statements.append(sql)
        if "to_regclass" in sql:
            return "t" if self.file_key_ids is not None else "f"
        if "FROM stored_files" in sql:
            return json.dumps(self.file_key_ids)
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
        half = len(self.payload) // 2
        output.write(self.payload[:half])
        output.flush()
        if self.snoop is not None:
            self.seen_during_dump = sorted(p.name for p in Path(self.snoop).iterdir())
        output.write(self.payload[half:])

    def list_archive(self, archive):
        self.calls.append(("list", archive.read(5)))

    def restore(self, archive, database):
        self.calls.append(("restore", database))
        self.restored = archive.read()
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

    def test_create_copies_template0_not_template1(self):
        # template1 may carry extensions (pgvector on some clusters); a copy of it is never empty.
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(FakePostgres(), Path(folder))
            pg = FakePostgres()
            backup.restore(pg, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE)
        self.assertIn('CREATE DATABASE "gateway_restored" TEMPLATE template0;', pg.statements)

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


def age_identity(folder, name="identity.txt"):
    """A fresh age identity file and its public key. age is required, never skipped."""
    path = Path(folder) / name
    try:
        subprocess.run(["age-keygen", "-o", str(path)], check=True, capture_output=True)
        public = subprocess.run(["age-keygen", "-y", str(path)], check=True, capture_output=True, text=True).stdout.strip()
    except FileNotFoundError:
        raise AssertionError("age and age-keygen must be installed for the backup tests (CI: sudo apt-get install -y age)") from None
    return path, public


class EncryptedBackupTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.keys = Path(self.folder.name) / "keys"
        self.keys.mkdir()
        self.identity, self.recipient = age_identity(self.keys)
        self.other_identity, _ = age_identity(self.keys, "other.txt")
        self.out = Path(self.folder.name) / "out"

    def test_encrypted_backup_streams_without_plaintext_and_round_trips(self):
        payload = b"PGDMP" + os.urandom(3 * (1 << 20) + 12345)  # several pipe chunks
        pg = FakePostgres(payload=payload, snoop=self.out)
        manifest_path = backup.backup(pg, self.out, recipients=[self.recipient])
        manifest = json.loads(manifest_path.read_text())
        archive = self.out / manifest["file"]
        self.assertTrue(manifest["file"].endswith(".dump.age"))
        self.assertEqual(manifest_path.name, manifest["file"] + ".manifest.json")
        # Only ciphertext ever existed on disk, during and after the dump.
        self.assertEqual(pg.seen_during_dump, [archive.name])
        self.assertEqual(sorted(p.name for p in self.out.iterdir()), sorted([archive.name, manifest_path.name]))
        self.assertEqual(stat.S_IMODE(archive.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(manifest_path.stat().st_mode), 0o600)
        ciphertext = archive.read_bytes()
        self.assertTrue(ciphertext.startswith(b"age-encryption.org/v1"))
        self.assertNotIn(payload[:64], ciphertext)
        self.assertEqual(manifest["sha256"], hashlib.sha256(payload).hexdigest())
        self.assertEqual(manifest["bytes"], len(payload))
        self.assertEqual(manifest["encryption"], {
            "tool": "age", "recipients": [self.recipient],
            "ciphertext_sha256": hashlib.sha256(ciphertext).hexdigest(), "ciphertext_bytes": len(ciphertext)})
        self.assertEqual(manifest["file_encryption_key_ids"], [])
        self.assertNotIn("AGE-SECRET-KEY", manifest_path.read_text())
        self.assertIn(("list", b"PGDMP"), pg.calls)  # pg_restore --list saw the stream
        # verify: ciphertext only without an identity, plaintext + archive listing with one.
        backup.verify(manifest_path)
        check = FakePostgres()
        backup.verify(manifest_path, check, identity=self.identity)
        self.assertIn(("list", b"PGDMP"), check.calls)
        with self.assertRaises(backup.BackupError) as raised:
            backup.verify(manifest_path, identity=self.other_identity)
        self.assertIn("wrong identity", str(raised.exception))
        with self.assertRaises(backup.BackupError) as raised:
            backup.restore(FakePostgres(), manifest_path, "gateway_restored", expected_migrations=LINEAGE)
        self.assertIn("--identity", str(raised.exception))
        target = FakePostgres()
        with self.assertRaises(backup.BackupError):
            backup.restore(target, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE, identity=self.other_identity)
        self.assertFalse([c for c in target.calls if c[0] == "restore" or c[1][:1] == ["CREATE"]])
        restored, counts = backup.restore(target, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE, identity=self.identity)
        self.assertEqual(target.restored, payload)
        self.assertEqual(restored["latest_version"], 2)
        self.assertEqual(sorted(p.name for p in self.out.iterdir()), sorted([archive.name, manifest_path.name]))

    def test_tampered_ciphertext_is_rejected(self):
        manifest_path = backup.backup(FakePostgres(), self.out, recipients=[self.recipient])
        manifest = json.loads(manifest_path.read_text())
        archive = self.out / manifest["file"]
        data = bytearray(archive.read_bytes())
        data[-5] ^= 0x01
        archive.write_bytes(bytes(data))
        with self.assertRaises(backup.BackupError) as raised:
            backup.verify(manifest_path, identity=self.identity)
        self.assertIn("checksum", str(raised.exception))
        # Even with a manifest rewritten to match, age authentication fails.
        manifest["encryption"].update(ciphertext_sha256=hashlib.sha256(data).hexdigest(), ciphertext_bytes=len(data))
        manifest_path.write_text(json.dumps(manifest))
        backup.verify(manifest_path)  # ciphertext checksum alone cannot tell
        with self.assertRaises(backup.BackupError):
            backup.verify(manifest_path, identity=self.identity)
        pg = FakePostgres()
        with self.assertRaises(backup.BackupError):
            backup.restore(pg, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE, identity=self.identity)
        self.assertFalse([c for c in pg.calls if c[0] == "restore" or c[1][:1] == ["CREATE"]])

    def test_archive_replaced_between_passes_never_completes_a_restore(self):
        manifest_path = backup.backup(FakePostgres(payload=b"PGDMP first" * 1000), self.out, recipients=[self.recipient])
        other = backup.backup(FakePostgres(payload=b"PGDMP other" * 1000), Path(self.folder.name) / "other", recipients=[self.recipient])
        archive = self.out / json.loads(manifest_path.read_text())["file"]
        swapped = other.parent / json.loads(other.read_text())["file"]
        original = backup.verify_plaintext

        def verify_then_swap(*args, **kwargs):
            original(*args, **kwargs)
            archive.write_bytes(swapped.read_bytes())
        pg = FakePostgres()
        with mock.patch.object(backup, "verify_plaintext", verify_then_swap):
            with self.assertRaises(backup.BackupError) as raised:
                backup.restore(pg, manifest_path, "gateway_new", create=True, expected_migrations=LINEAGE, identity=self.identity)
        self.assertIn("changed since it was verified", str(raised.exception))
        self.assertNotEqual(pg.restored, b"PGDMP other" * 1000)  # the tail was held back
        self.assertIn(("query", ["DROP", "DATABASE", '"gateway_new"'], None), pg.calls)

    def test_file_store_key_ids_are_recorded_and_required_for_restore(self):
        manifest_path = backup.backup(FakePostgres(file_key_ids=["k2026a", "k2027a"]), self.out, recipients=[self.recipient])
        self.assertEqual(json.loads(manifest_path.read_text())["file_encryption_key_ids"], ["k2026a", "k2027a"])
        secret_value = "c2VjcmV0LWtleS1tYXRlcmlhbC1uZXZlci1wcmludGVk"
        restore = lambda pg, **kw: backup.restore(pg, manifest_path, "gateway_restored", create=True, expected_migrations=LINEAGE, identity=self.identity, **kw)
        pg = FakePostgres()
        with self.assertRaises(backup.BackupError) as raised:
            restore(pg)
        self.assertIn("--file-keys-env", str(raised.exception))
        self.assertEqual(pg.calls, [])
        with mock.patch.dict(os.environ, {"OMG_TEST_FILE_KEYS": f"k2027a:{secret_value}"}):
            with self.assertRaises(backup.BackupError) as raised:
                restore(pg, file_keys_env="OMG_TEST_FILE_KEYS")
            self.assertIn("k2026a", str(raised.exception))
            self.assertNotIn(secret_value, str(raised.exception))
            self.assertEqual(pg.calls, [])
        for value in ("", "k2026a", f"bad id!:{secret_value}"):
            with mock.patch.dict(os.environ, {"OMG_TEST_FILE_KEYS": value}):
                with self.assertRaises(backup.BackupError) as raised:
                    restore(pg, file_keys_env="OMG_TEST_FILE_KEYS")
                self.assertNotIn(secret_value, str(raised.exception))
        with self.assertRaises(backup.BackupError):
            restore(pg, file_keys_env="lowercase")
        self.assertEqual(pg.calls, [])
        with mock.patch.dict(os.environ, {"OMG_TEST_FILE_KEYS": f"k2028a:{secret_value}, k2027a:{secret_value},k2026a:{secret_value}"}):
            restore(FakePostgres(), file_keys_env="OMG_TEST_FILE_KEYS")
        restore(FakePostgres(), skip_file_key_check=True)

    def test_recipients_are_public_keys_from_flags_and_files(self):
        _, second = age_identity(self.keys, "second.txt")
        recipients = self.keys / "recipients.txt"
        recipients.write_text(f"# backup recipients\n\n{second}\n{self.recipient}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeKeyForParsingOnly ops@example\n")
        self.assertEqual(backup.parse_recipients([self.recipient], [recipients]),
                         [self.recipient, second, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeKeyForParsingOnly"])
        secret_line = self.identity.read_text().strip().splitlines()[-1]
        self.assertTrue(secret_line.startswith("AGE-SECRET-KEY-"))
        recipients.write_text(secret_line + "\n")
        with self.assertRaises(backup.BackupError) as raised:
            backup.parse_recipients([], [recipients])
        self.assertNotIn(secret_line, str(raised.exception))
        recipients.write_text("# nothing\n")
        with self.assertRaises(backup.BackupError):
            backup.parse_recipients([], [recipients])

    def test_failed_encryption_leaves_no_files(self):
        bogus = "age1" + "q" * 58  # well-formed shape, invalid checksum: age rejects it
        with self.assertRaises(backup.BackupError) as raised:
            backup.backup(FakePostgres(payload=b"PGDMP" + b"x" * (2 << 20)), self.out, recipients=[bogus])
        self.assertIn("age could not encrypt", str(raised.exception))
        self.assertEqual(list(self.out.iterdir()), [])


class FakeS3:
    """In-process S3 (PUT/GET/HEAD/DELETE, ListObjectsV2) that verifies SigV4 signatures."""
    ACCESS = "AKIDBACKUPTESTONLY"
    SECRET = "fake/Secret+Key/never-printed-7f3c9a"

    def __init__(self, page_size=2, token=None):
        self.objects, self.log, self.problems, self.fail = {}, [], [], {}
        self.page_size, self.token = page_size, token
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_PUT(self):
                outer.handle(self, "PUT")

            def do_GET(self):
                outer.handle(self, "GET")

            def do_HEAD(self):
                outer.handle(self, "HEAD")

            def do_DELETE(self):
                outer.handle(self, "DELETE")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.endpoint = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def client(self):
        return backup.S3(self.endpoint, "us-east-1", backup.Credentials(self.ACCESS, self.SECRET))

    def keys(self, bucket="backups"):
        return sorted(k for b, k in self.objects if b == bucket)

    def signature_problem(self, request, method, body):
        match = re.fullmatch(r"AWS4-HMAC-SHA256 Credential=([^/]+)/(\d{8})/([a-z0-9-]+)/s3/aws4_request, SignedHeaders=([a-z0-9;-]+), Signature=([0-9a-f]{64})", request.headers.get("Authorization", ""))
        if not match or match.group(1) != self.ACCESS:
            return "missing or malformed SigV4 Authorization header"
        payload = request.headers.get("x-amz-content-sha256")
        if payload != hashlib.sha256(body).hexdigest():
            return "x-amz-content-sha256 missing or not the body hash"
        names = match.group(4).split(";")
        if not {"host", "x-amz-date", "x-amz-content-sha256"} <= set(names) or names != sorted(names):
            return "required headers are not signed"
        if self.token and ("x-amz-security-token" not in names or request.headers.get("x-amz-security-token") != self.token):
            return "session token not sent or not signed"
        path, _, query = request.path.partition("?")
        canonical_query = "&".join(f"{k}={v}" for k, v in sorted((quote(k, safe="-_.~"), quote(v, safe="-_.~")) for k, v in parse_qsl(query, keep_blank_values=True)))
        headers = "".join(f"{n}:{' '.join(request.headers.get(n, '').split())}\n" for n in names)
        canonical = "\n".join([method, path, canonical_query, headers, ";".join(names), payload])
        scope = f"{match.group(2)}/{match.group(3)}/s3/aws4_request"
        to_sign = "\n".join(["AWS4-HMAC-SHA256", request.headers.get("x-amz-date", ""), scope, hashlib.sha256(canonical.encode()).hexdigest()])
        key = ("AWS4" + self.SECRET).encode()
        for part in (match.group(2), match.group(3), "s3", "aws4_request"):
            key = hmac.new(key, part.encode(), hashlib.sha256).digest()
        if not hmac.compare_digest(hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest(), match.group(5)):
            return "signature does not match the request"
        return None

    def reply(self, request, status, body=b"", length=None):
        request.send_response(status)
        request.send_header("Content-Length", str(len(body) if length is None else length))
        request.end_headers()
        if body and request.command != "HEAD":
            request.wfile.write(body)

    def error(self, request, status, code):
        self.reply(request, status, f"<Error><Code>{code}</Code><Message>{self.SECRET} must never be echoed</Message></Error>".encode())

    def handle(self, request, method):
        length = int(request.headers.get("Content-Length") or 0)
        body = request.rfile.read(length) if length else b""
        path, _, query = request.path.partition("?")
        bucket, _, key = unquote(path).lstrip("/").partition("/")
        self.log.append((method, key))
        problem = self.signature_problem(request, method, body)
        if problem:
            self.problems.append(problem)
            return self.error(request, 403, "SignatureDoesNotMatch")
        for (fail_method, suffix), status in self.fail.items():
            if fail_method == method and key.endswith(suffix):
                return self.error(request, status, "InternalError")
        if method == "PUT":
            self.objects[(bucket, key)] = body
            return self.reply(request, 200)
        if method == "DELETE":
            self.objects.pop((bucket, key), None)
            return self.reply(request, 204)
        if method == "GET" and not key:
            params = dict(parse_qsl(query, keep_blank_values=True))
            assert params.get("list-type") == "2"
            prefix, after = params.get("prefix", ""), params.get("continuation-token", "")
            names = [k for k in self.keys(bucket) if k.startswith(prefix) and k > after
                     and not (params.get("delimiter") and params["delimiter"] in k[len(prefix):])]
            page, more = names[:self.page_size], len(names) > self.page_size
            xml = '<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
            xml += f"<IsTruncated>{'true' if more else 'false'}</IsTruncated>"
            xml += "".join(f"<Contents><Key>{escape(k)}</Key><Size>{len(self.objects[(bucket, k)])}</Size></Contents>" for k in page)
            if more:
                xml += f"<NextContinuationToken>{escape(page[-1])}</NextContinuationToken>"
            return self.reply(request, 200, (xml + "</ListBucketResult>").encode())
        if (bucket, key) not in self.objects:
            return self.error(request, 404, "NoSuchKey")
        data = self.objects[(bucket, key)]
        return self.reply(request, 200, data, len(data))


class ObjectStoreTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.root = Path(self.folder.name)
        self.identity, self.recipient = age_identity(self.root)
        self.s3 = FakeS3()
        self.addCleanup(self.s3.close)
        self.client = self.s3.client()

    def encrypted(self, minutes=0, out="local"):
        now = datetime(2026, 10, 13, 2, 0, tzinfo=timezone.utc) + timedelta(minutes=minutes)
        return backup.backup(FakePostgres(payload=b"PGDMP" + os.urandom(4096)), self.root / out, now=now, recipients=[self.recipient])

    def test_sigv4_matches_aws_documented_examples(self):
        # https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html
        credentials = backup.Credentials("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
        now = datetime(2013, 5, 24, tzinfo=timezone.utc)
        get = backup.sigv4_headers("GET", "examplebucket.s3.amazonaws.com", "/test.txt", {}, {"Range": "bytes=0-9"}, backup.EMPTY_SHA256, credentials, "us-east-1", now)
        self.assertEqual(get["authorization"], "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41")
        listing = backup.sigv4_headers("GET", "examplebucket.s3.amazonaws.com", "/", {"max-keys": "2", "prefix": "J"}, {}, backup.EMPTY_SHA256, credentials, "us-east-1", now)
        self.assertTrue(listing["authorization"].endswith("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"))
        self.assertEqual(listing["x-amz-date"], "20130524T000000Z")

    def test_upload_puts_archive_before_manifest_then_downloads_and_verifies(self):
        manifest_path = self.encrypted()
        name = json.loads(manifest_path.read_text())["file"]
        backup.upload(self.client, manifest_path, "backups", "gateway/prod/")
        self.assertEqual(self.s3.problems, [])
        archive_key, manifest_key = f"gateway/prod/{name}", f"gateway/prod/{name}.manifest.json"
        self.assertEqual(self.s3.log, [("HEAD", manifest_key), ("PUT", archive_key), ("HEAD", archive_key), ("PUT", manifest_key), ("HEAD", manifest_key)])
        self.assertEqual(self.s3.objects[("backups", archive_key)], (manifest_path.parent / name).read_bytes())
        self.assertEqual(self.s3.objects[("backups", manifest_key)], manifest_path.read_bytes())
        with self.assertRaises(backup.BackupError) as raised:
            backup.upload(self.client, manifest_path, "backups", "gateway/prod/")
        self.assertIn("already exists", str(raised.exception))
        generations, other = backup.remote_generations(self.client, "backups", "gateway/prod/")
        self.assertEqual([(g["archive"], g["archive_bytes"]) for g in generations], [(name, (manifest_path.parent / name).stat().st_size)])
        self.assertEqual(other, [])
        out = self.root / "restore-host"
        downloaded = backup.download(self.client, "backups", "gateway/prod/", name, out)
        self.assertEqual(downloaded.name, name + ".manifest.json")
        for path in out.iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        backup.verify(downloaded, FakePostgres(), identity=self.identity)
        with self.assertRaises(backup.BackupError) as raised:
            backup.download(self.client, "backups", "gateway/prod/", name + ".manifest.json", out)
        self.assertIn("refusing to overwrite", str(raised.exception))
        self.assertEqual(self.s3.problems, [])

    def test_download_rejects_a_changed_remote_archive(self):
        manifest_path = self.encrypted()
        name = json.loads(manifest_path.read_text())["file"]
        backup.upload(self.client, manifest_path, "backups", "")
        self.s3.objects[("backups", name)] += b"!"
        out = self.root / "restore-host"
        with self.assertRaises(backup.BackupError) as raised:
            backup.download(self.client, "backups", "", name, out)
        self.assertIn("checksum", str(raised.exception))
        self.assertEqual(list(out.iterdir()), [])

    def test_interrupted_upload_never_publishes_a_manifest(self):
        manifest_path = self.encrypted()
        self.s3.fail[("PUT", ".age")] = 500
        with self.assertRaises(backup.BackupError) as raised:
            backup.upload(self.client, manifest_path, "backups", "gateway/")
        self.assertIn("HTTP 500 (InternalError)", str(raised.exception))
        self.assertNotIn(self.s3.SECRET, str(raised.exception))
        self.assertNotIn("127.0.0.1", str(raised.exception))
        self.assertFalse([k for k in self.s3.keys() if k.endswith(".manifest.json")])
        self.assertFalse([m for m, k in self.s3.log if m == "PUT" and k.endswith(".manifest.json")])

    def test_plaintext_and_oversized_uploads_are_refused(self):
        plain = backup.backup(FakePostgres(), self.root / "plain")
        with self.assertRaises(backup.BackupError) as raised:
            backup.upload(self.client, plain, "backups", "")
        self.assertIn("unencrypted", str(raised.exception))
        self.assertEqual(self.s3.log, [])
        backup.upload(self.client, plain, "backups", "", allow_plaintext=True)
        self.assertEqual(len(self.s3.keys()), 2)
        self.s3.log.clear()
        with mock.patch.object(backup, "MAX_SINGLE_PUT", 16):
            with self.assertRaises(backup.BackupError) as raised:
                backup.upload(self.client, self.encrypted(), "backups", "")
        self.assertIn("5 GiB", str(raised.exception))
        self.assertEqual(self.s3.log, [])

    def test_prune_keeps_the_newest_and_deletes_only_with_yes(self):
        names = []
        for minutes in (0, 60, 120):
            manifest_path = self.encrypted(minutes)
            backup.upload(self.client, manifest_path, "backups", "gw/")
            names.append(json.loads(manifest_path.read_text())["file"])
        newest = names[-1]
        orphan = "gateway-20200101T000000Z-v0002-deadbeef.dump.age"  # interrupted upload
        broken = "gateway-20210101T000000Z-v0002-0badf00d.dump.age"  # manifest is not a manifest
        self.s3.objects.update({
            ("backups", "gw/" + orphan): b"ciphertext", ("backups", "gw/notes.txt"): b"keep me",
            ("backups", "gw/nested/" + names[0]): b"elsewhere",
            ("backups", "gw/" + broken): b"x", ("backups", "gw/" + broken + ".manifest.json"): b"not json"})
        before = dict(self.s3.objects)
        lines = []
        planned = backup.prune(self.client, "backups", "gw/", 1, out=lines.append)
        self.assertEqual([g["archive"] for g in planned], [names[1], names[0]])
        self.assertEqual(self.s3.objects, before)
        self.assertFalse([m for m, _ in self.s3.log if m == "DELETE"])
        self.assertIn("Dry run", "\n".join(lines))
        with self.assertRaises(backup.BackupError):
            backup.prune(self.client, "backups", "gw/", 0, yes=True)
        self.s3.log.clear()
        backup.prune(self.client, "backups", "gw/", 1, yes=True, out=lines.append)
        deletes = [k for m, k in self.s3.log if m == "DELETE"]
        self.assertEqual(deletes, [f"gw/{names[1]}.manifest.json", f"gw/{names[1]}", f"gw/{names[0]}.manifest.json", f"gw/{names[0]}"])
        self.assertEqual(self.s3.keys(), sorted([
            f"gw/{newest}", f"gw/{newest}.manifest.json", f"gw/{orphan}", "gw/notes.txt", f"gw/nested/{names[0]}",
            f"gw/{broken}", f"gw/{broken}.manifest.json"]))
        self.assertEqual(backup.prune(self.client, "backups", "gw/", 1, yes=True, out=lines.append), [])
        self.assertEqual(self.s3.problems, [])

    def test_prune_never_counts_a_truncated_archive_toward_keep(self):
        names = []
        for minutes in (0, 60):
            manifest_path = self.encrypted(minutes)
            backup.upload(self.client, manifest_path, "backups", "gw/")
            names.append(json.loads(manifest_path.read_text())["file"])
        older, newest = names
        self.s3.objects[("backups", "gw/" + newest)] = self.s3.objects[("backups", "gw/" + newest)][:-7]  # truncated
        lines = []
        self.assertEqual(backup.prune(self.client, "backups", "gw/", 1, yes=True, out=lines.append), [])
        self.assertFalse([m for m, _ in self.s3.log if m == "DELETE"])
        self.assertIn(("backups", f"gw/{older}"), self.s3.objects)  # the only restorable generation survives
        self.assertIn("1 incomplete or unrecognized manifest(s)", "\n".join(lines))

    def test_locations_and_endpoints_are_validated_without_echoing_secrets(self):
        self.assertEqual(backup.parse_target("s3://backups"), ("backups", ""))
        self.assertEqual(backup.parse_target("s3://backups/gw/prod/"), ("backups", "gw/prod/"))
        for bad in ("https://backups/x", "s3://Bad_Bucket/x", "s3://backups/../x", "s3://backups/x?y=1", "s3://"):
            with self.assertRaises(backup.BackupError):
                backup.parse_target(bad)
        credentials = backup.Credentials("AKID", "topsecret")
        self.assertNotIn("topsecret", repr(credentials))
        for bad in ("http://rustfs.internal:9000", "https://user:topsecret@rustfs.internal", "https://rustfs.internal/path", "ftp://x", "https://x:notaport"):
            with self.assertRaises(backup.BackupError) as raised:
                backup.S3(bad, "us-east-1", credentials)
            self.assertNotIn("topsecret", str(raised.exception))
            self.assertNotIn("rustfs.internal", str(raised.exception))
        backup.S3("https://rustfs.internal:9000", "us-east-1", credentials)
        backup.S3("http://127.0.0.1:9000", "us-east-1", credentials)

    def test_cli_never_prints_credentials(self):
        self.s3 = FakeS3(token="session-token-never-printed")
        self.addCleanup(self.s3.close)
        manifest_path = self.encrypted()
        env = dict(os.environ, AWS_ACCESS_KEY_ID=FakeS3.ACCESS, AWS_SECRET_ACCESS_KEY=FakeS3.SECRET, AWS_SESSION_TOKEN="session-token-never-printed")
        common = ["--endpoint", self.s3.endpoint, "--region", "us-east-1"]
        cli = lambda *args, env=env: subprocess.run([sys.executable, str(ROOT / "scripts/backup.py"), *args], capture_output=True, text=True, env=env)
        runs = [cli("upload", str(manifest_path), "--to", "s3://backups/gw", *common),
                cli("list", "--from", "s3://backups/gw", *common),
                cli("prune", "--from", "s3://backups/gw", "--keep", "1", *common)]
        for result in runs:
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(json.loads(manifest_path.read_text())["file"], runs[1].stdout)
        self.assertEqual(self.s3.problems, [])
        self.s3.fail[("GET", "")] = 403
        runs.append(cli("list", "--from", "s3://backups/gw", *common))
        self.assertEqual(runs[-1].returncode, 1)
        self.assertIn("HTTP 403", runs[-1].stderr)
        runs.append(cli("list", "--from", "s3://backups/gw", *common, env=dict(env, AWS_SECRET_ACCESS_KEY="")))
        self.assertIn("AWS_SECRET_ACCESS_KEY", runs[-1].stderr)
        for result in runs:
            output = result.stdout + result.stderr
            for secret in (FakeS3.ACCESS, FakeS3.SECRET, "session-token-never-printed", self.s3.endpoint):
                self.assertNotIn(secret, output)


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

    def test_reapply_revocations_from_the_old_database_after_a_restore(self):
        # #146: a restore brings back keys, grants, sessions and invitations
        # revoked, and users, keys, service accounts and workspaces disabled,
        # after the backup. Re-apply them from the old database.
        revocable = ["api_keys", "workspace_membership_grants", "platform_role_grants", "workspace_invitations", "admin_api_tokens"]
        schema = ["ALTER TABLE installation ADD COLUMN id uuid NOT NULL DEFAULT '11111111-1111-4111-8111-111111111111';",
                  "CREATE TABLE browser_sessions(token_hash bytea PRIMARY KEY, revoked_at timestamptz);",
                  "INSERT INTO browser_sessions SELECT sha256(g::text::bytea), NULL FROM generate_series(1,3) g;",
                  "CREATE TABLE users(id uuid PRIMARY KEY, disabled_at timestamptz, disable_reason text, cleanup_due_at timestamptz);",
                  "CREATE TABLE service_accounts(id uuid PRIMARY KEY, disabled_at timestamptz);",
                  "CREATE TABLE workspaces(id uuid PRIMARY KEY, disabled_at timestamptz);"]
        for table in revocable:
            extra = ", disabled_at timestamptz" if table == "api_keys" else ""
            schema += [f"CREATE TABLE {table}(id uuid PRIMARY KEY, revoked_at timestamptz{extra});",
                       f"INSERT INTO {table}(id) SELECT gen_random_uuid() FROM generate_series(1,3);"]
        for table in ["users", "service_accounts", "workspaces"]:
            schema.append(f"INSERT INTO {table}(id) SELECT gen_random_uuid() FROM generate_series(1,3);")
        # Revoked before the backup: the restore already carries it, and its time must not move.
        schema.append("UPDATE api_keys SET revoked_at=now()-interval '1 day' WHERE id=(SELECT min(id::text)::uuid FROM api_keys);")
        self.pg.query("\n".join(schema), self.source)
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        target = backup.Postgres(dict(self.env, PGDATABASE=self.target), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            backup.restore(self.pg, backup.backup(source, Path(folder)), self.target, create=True, expected_migrations=self.lineage)
        # After the backup the old database revokes and disables one more row each, and suspends a user.
        last = "(SELECT max(id::text)::uuid FROM {t})"
        after = [f"UPDATE {t} SET revoked_at=now() WHERE id={last.format(t=t)};" for t in revocable]
        after.append("UPDATE browser_sessions SET revoked_at=now() WHERE token_hash=(SELECT token_hash FROM browser_sessions ORDER BY token_hash DESC LIMIT 1);")
        after += [f"UPDATE {t} SET disabled_at=now() WHERE id={last.format(t=t)};" for t in ["api_keys", "service_accounts", "workspaces"]]
        after.append(f"UPDATE users SET disabled_at=now(), disable_reason='admin_suspension', cleanup_due_at=now()+interval '30 days' WHERE id={last.format(t='users')};")
        self.pg.query("\n".join(after), self.source)
        labels = [f"{t}.revoked_at" for t in revocable + ["browser_sessions"]] + [f"{t}.disabled_at" for t in ["users", "api_keys", "service_accounts", "workspaces"]]
        def state(pg):
            out = {}
            for table in revocable + ["browser_sessions"]:
                key = "encode(token_hash,'hex')" if table == "browser_sessions" else "id::text"
                out[f"{table}.revoked_at"] = pg.query(f"SELECT coalesce(string_agg({key}||'@'||revoked_at::text, ',' ORDER BY 1),'') FROM {table} WHERE revoked_at IS NOT NULL;")
            for table in ["users", "api_keys", "service_accounts", "workspaces"]:
                cols = "disabled_at::text||coalesce(disable_reason,'')||coalesce(cleanup_due_at::text,'')" if table == "users" else "disabled_at::text"
                out[f"{table}.disabled_at"] = pg.query(f"SELECT coalesce(string_agg(id::text||'@'||{cols}, ',' ORDER BY 1),'') FROM {table} WHERE disabled_at IS NOT NULL;")
            return out
        before, lines = state(target), []
        self.assertEqual(backup.reapply_revocations(source, target, yes=False, out=lines.append), {label: 1 for label in labels})
        self.assertEqual(state(target), before, "a dry run changes nothing")
        self.assertIn("Dry run", "\n".join(lines))
        # One transaction: a failure in any table leaves every table as it was.
        target.query("ALTER TABLE workspaces ADD CONSTRAINT refuse_disable CHECK (disabled_at IS NULL);")
        with self.assertRaises(backup.BackupError):
            backup.reapply_revocations(source, target, yes=True, out=lines.append)
        self.assertEqual(state(target), before, "a failed apply rolls back every table")
        target.query("ALTER TABLE workspaces DROP CONSTRAINT refuse_disable;")
        self.assertEqual(backup.reapply_revocations(source, target, yes=True, out=lines.append), {label: 1 for label in labels})
        self.assertEqual(state(target), state(source), "the same rows, with the same times, reasons and cleanup dates")
        self.assertEqual(backup.reapply_revocations(source, target, yes=True, out=lines.append), {label: 0 for label in labels})
        target.query("UPDATE installation SET id='22222222-2222-4222-8222-222222222222';")
        with self.assertRaises(backup.BackupError) as raised:
            backup.reapply_revocations(source, target, yes=True, out=lines.append)
        self.assertIn("different installation", str(raised.exception))

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

    def test_encrypted_backup_verify_restore_with_file_key_check(self):
        self.pg.query("""CREATE TABLE stored_files(id uuid PRIMARY KEY, encryption_key_id text NOT NULL, deleted_at timestamptz);
            INSERT INTO stored_files VALUES(gen_random_uuid(),'k2026a',NULL),(gen_random_uuid(),'k2026a',NULL),(gen_random_uuid(),'kold',now());""", self.source)
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            identity, recipient = age_identity(folder)
            wrong, _ = age_identity(folder, "wrong.txt")
            out = Path(folder) / "out"
            manifest_path = backup.backup(source, out, recipients=[recipient])
            manifest = json.loads(manifest_path.read_text())
            self.assertEqual(sorted(p.name for p in out.iterdir()), sorted([manifest["file"], manifest_path.name]))
            self.assertTrue(manifest["file"].endswith(".dump.age"))
            self.assertEqual(manifest["file_encryption_key_ids"], ["k2026a"])
            self.assertEqual(manifest["migrations"], self.lineage)
            backup.verify(manifest_path, source, identity=identity)
            with self.assertRaises(backup.BackupError):
                backup.verify(manifest_path, source, identity=wrong)
            restore = lambda **kw: backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage, identity=identity, **kw)
            with self.assertRaises(backup.BackupError):
                restore()
            with mock.patch.dict(os.environ, {"OMG_TEST_FILE_KEYS": "k2027a:" + "A" * 43 + "="}):
                with self.assertRaises(backup.BackupError):
                    restore(file_keys_env="OMG_TEST_FILE_KEYS")
            with self.assertRaises(backup.BackupError):
                backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage, identity=wrong, skip_file_key_check=True)
            self.assertEqual(self.pg.query(f"SELECT count(*) FROM pg_database WHERE datname='{self.target}';"), "0")
            with mock.patch.dict(os.environ, {"OMG_TEST_FILE_KEYS": "k2027a:" + "A" * 43 + "=,k2026a:" + "B" * 43 + "="}):
                restored, counts = restore(file_keys_env="OMG_TEST_FILE_KEYS")
            self.assertEqual(restored["migrations"], self.lineage)
            self.assertEqual(counts, "2 ledger entries, 2 executions")
            self.assertEqual(self.pg.query("SELECT sum(amount_microusd) FROM monetary_ledger;", self.target), "22")
            self.assertEqual(self.pg.query("SELECT count(*) FROM stored_files WHERE deleted_at IS NULL;", self.target), "2")
            self.assertEqual(sorted(p.name for p in out.iterdir()), sorted([manifest["file"], manifest_path.name]))

    def test_encrypted_archive_swapped_between_passes_fails_in_real_pg_restore(self):
        # The second decrypt pass holds back the last block; real pg_restore must fail
        # at the early end of file, and the created target must be dropped.
        self.pg.query("INSERT INTO monetary_ledger SELECT gen_random_uuid(), NULL, g FROM generate_series(1, 20000) g;", self.source)
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            identity, recipient = age_identity(folder)
            first = backup.backup(source, Path(folder) / "first", recipients=[recipient])
            self.pg.query("INSERT INTO monetary_ledger SELECT gen_random_uuid(), NULL, 1 FROM generate_series(1, 500) g;", self.source)
            second = backup.backup(source, Path(folder) / "second", recipients=[recipient])
            archive = first.parent / json.loads(first.read_text())["file"]
            replacement = second.parent / json.loads(second.read_text())["file"]
            original = backup.verify_plaintext

            def verify_then_swap(*args, **kwargs):
                result = original(*args, **kwargs)
                archive.write_bytes(replacement.read_bytes())
                return result
            with mock.patch.object(backup, "verify_plaintext", verify_then_swap):
                with self.assertRaises(backup.BackupError) as raised:
                    backup.restore(self.pg, first, self.target, create=True, expected_migrations=self.lineage, identity=identity, skip_file_key_check=True)
            self.assertIn("changed since it was verified", str(raised.exception))
            self.assertEqual(self.pg.query(f"SELECT count(*) FROM pg_database WHERE datname='{self.target}';"), "0")

    def test_extensions_from_the_cluster_template_are_not_backed_up(self):
        # A cluster whose template1 adds extensions (e.g. pgvector) puts them in every new
        # database. The gateway schema needs none, and the owner restoring on a clean host
        # usually cannot create them, so a backup must not carry them.
        self.pg.query("CREATE EXTENSION pg_trgm;", self.source)
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(source, Path(folder))
            archive = manifest_path.parent / json.loads(manifest_path.read_text())["file"]
            command, env = source.command(["pg_restore", "--list"])
            with archive.open("rb") as stdin:
                listing = subprocess.run(command, env=env, stdin=stdin, capture_output=True, text=True, check=True).stdout
            self.assertNotIn(" EXTENSION ", listing)
            self.assertIn(" TABLE public monetary_ledger", listing)
            restored, _ = backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage)
            self.assertEqual(self.pg.query("SELECT count(*) FROM pg_extension WHERE extname='pg_trgm';", self.target), "0")

    def test_partitioned_history_with_nested_validators_restores(self):
        # P6: history tables are partitioned and CHECK validators call other
        # validators schema-qualified (pg_restore loads with an empty
        # search_path); row counts per partition survive the round trip.
        self.pg.query("""CREATE FUNCTION public.base_ok(v bigint) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT v>=0 $$;
            CREATE FUNCTION public.amount_ok(v bigint) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN public.base_ok(v); END $$;
            CREATE TABLE history(id uuid, at timestamptz NOT NULL, amount bigint CHECK(public.amount_ok(amount)), PRIMARY KEY(id,at)) PARTITION BY RANGE(at);
            CREATE TABLE history_p_legacy PARTITION OF history FOR VALUES FROM (MINVALUE) TO ('2026-11-01');
            CREATE TABLE history_p2026_11 PARTITION OF history FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
            INSERT INTO history SELECT gen_random_uuid(),'2026-10-31'::timestamptz+make_interval(hours=>i),i FROM generate_series(1,48) i;""", self.source)
        source = backup.Postgres(dict(self.env, PGDATABASE=self.source), self.pg.container)
        with tempfile.TemporaryDirectory() as folder:
            manifest_path = backup.backup(source, Path(folder))
            backup.restore(self.pg, manifest_path, self.target, create=True, expected_migrations=self.lineage)
            counts = "SELECT string_agg(t||'='||n, ',' ORDER BY t) FROM (SELECT tableoid::regclass::text t,count(*) n FROM history GROUP BY 1) x;"
            self.assertEqual(self.pg.query(counts, self.target), self.pg.query(counts, self.source))
            self.assertEqual(self.pg.query("SELECT count(*) FROM pg_inherits WHERE inhparent='history'::regclass;", self.target), "2")
            self.assertEqual(self.pg.query("SELECT count(*) FROM history;", self.target), "48")


if __name__ == "__main__":
    unittest.main()

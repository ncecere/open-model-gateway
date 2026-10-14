#!/usr/bin/env python3
"""Gateway database backup, verification and guarded restore (docs/operations.md).

backup   pg_dump (custom format, no owner/ACLs) + SHA-256 + JSON manifest that
         records the applied migration lineage (version + checksum) and the
         file-store encryption key ids that stored files reference. With
         --encrypt-to/--encrypt-to-file the dump is streamed through `age` and
         the plaintext dump is never written to disk.
verify   Re-check a manifest's checksum/size and that the archive is readable.
         Encrypted backups: the ciphertext always; the plaintext with --identity.
restore  Load a verified backup into an explicitly named, EMPTY database only.
         The backup's lineage must exactly match the gateway binary that will
         serve it (`open-model-gateway schema-version`), or an explicitly
         expected latest version. It never drops, cleans or overwrites data.
upload   Copy a backup to S3-compatible storage: the archive first, then the
         manifest, so a manifest only exists once its archive is complete.
list     List backup generations under an s3:// prefix, newest first.
download Fetch a generation (manifest + archive) and verify its checksum.
prune    Plan (and with --yes, perform) deleting all but the newest N generations.

Connection URLs are read from an environment variable (default DATABASE_URL)
or a file, passed to PostgreSQL tools through PG* environment variables and
never printed or placed on a command line. `--pg-container NAME` runs the
PostgreSQL client tools inside that container with `docker exec`; the URL is
then resolved from inside the container. Object-store credentials come only
from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (and optionally
AWS_SESSION_TOKEN); requests are signed with AWS Signature Version 4.
"""
import argparse
import hashlib
import hmac
import http.client
import io
import json
import os
from pathlib import Path
import re
import secrets
import ssl
import subprocess
import sys
import threading
from datetime import datetime, timezone
from urllib.parse import parse_qs, quote, unquote, urlsplit
import xml.etree.ElementTree as ElementTree

MANIFEST_FORMAT = "omg-backup-v1"
SCHEMA_FAMILY = "enterprise_v1"
IDENTIFIER = re.compile(r"[a-z_][a-z0-9_]{0,62}")
# Never restore into system databases or the local demo databases.
PROTECTED_DATABASES = {"postgres", "template0", "template1", "gateway_demo", "gateway_enterprise_demo"}
PG_ENV = ("PGHOST", "PGPORT", "PGUSER", "PGPASSWORD", "PGDATABASE", "PGSSLMODE", "PGSSLROOTCERT")
CHUNK = 1 << 20

AGE_MISSING = "age not found; install age (https://age-encryption.org) to create or read encrypted backups"
AGE_RECIPIENT = re.compile(r"age1[0-9a-z]{20,}|ssh-(?:ed25519|rsa) [A-Za-z0-9+/]+={0,3}")
KEY_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,31}")
ENV_NAME = re.compile(r"[A-Z_][A-Z0-9_]{0,127}")
HEX64 = re.compile(r"[0-9a-f]{64}")
GENERATION = r"gateway-(\d{8}T\d{6}Z)-v\d{4}-[0-9a-f]{8}\.dump(?:\.age)?"
ARCHIVE_NAME = re.compile(GENERATION)
MANIFEST_SUFFIX = ".manifest.json"
MANIFEST_NAME = re.compile(GENERATION + re.escape(MANIFEST_SUFFIX))
BUCKET = re.compile(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]")
PREFIX_SEGMENT = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._=-]{0,127}")
REGION = re.compile(r"[a-z0-9][a-z0-9-]{0,62}")
LOOPBACK = {"127.0.0.1", "::1", "localhost"}
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
# Single-request PUT limit of S3; multipart upload is not implemented.
MAX_SINGLE_PUT = 5 * 1024 ** 3
MAX_MANIFEST_BYTES = 1 << 20


class BackupError(Exception):
    """Operator-facing failure. Messages never contain URLs or secrets."""


class ManifestError(BackupError):
    """The manifest itself is malformed (as opposed to an I/O failure)."""


def connection_env(url):
    """Translate a postgres:// URL into PG* variables (password never on argv)."""
    parts = urlsplit(url)
    if parts.scheme not in ("postgres", "postgresql"):
        raise BackupError("database URL must use postgres:// or postgresql://")
    database = unquote(parts.path.lstrip("/"))
    if not parts.hostname or not database:
        raise BackupError("database URL must include a host and database name")
    env = {"PGHOST": parts.hostname, "PGPORT": str(parts.port or 5432), "PGDATABASE": database}
    if parts.username:
        env["PGUSER"] = unquote(parts.username)
    if parts.password is not None:
        env["PGPASSWORD"] = unquote(parts.password)
    query = parse_qs(parts.query)
    unsupported = set(query) - {"sslmode", "sslrootcert"}
    if unsupported:
        raise BackupError("unsupported database URL query parameters: " + ", ".join(sorted(unsupported)))
    for name, variable in (("sslmode", "PGSSLMODE"), ("sslrootcert", "PGSSLROOTCERT")):
        if query.get(name):
            env[variable] = query[name][-1]
    return env


def read_url(args):
    if args.url_file:
        value = Path(args.url_file).read_text().strip()
    else:
        value = os.environ.get(args.url_env, "").strip()
    if not value:
        raise BackupError(f"no database URL: set {args.url_env} or pass --url-file")
    return value


def valid_database_name(name):
    if not IDENTIFIER.fullmatch(name or ""):
        raise BackupError("target database must be a lowercase identifier ([a-z_][a-z0-9_]*, max 63)")
    if name in PROTECTED_DATABASES:
        raise BackupError(f"refusing protected database {name}")
    return name


class Postgres:
    """PostgreSQL client tools on the host or inside a named container."""

    def __init__(self, env, container=None):
        self.env = env
        self.container = container

    def command(self, tool, database=None):
        child = dict(self.env)
        if database:
            child["PGDATABASE"] = database
        process_env = {k: v for k, v in os.environ.items() if k not in PG_ENV}
        process_env.update(child)
        prefix = []
        if self.container:
            prefix = ["docker", "exec", "-i"]
            for name in sorted(child):
                prefix += ["-e", name]  # value is inherited from process_env, never argv
            prefix.append(self.container)
        return prefix + tool, process_env

    def run(self, tool, database=None, **kwargs):
        command, env = self.command(tool, database)
        try:
            return subprocess.run(command, env=env, check=True, **kwargs)
        except FileNotFoundError as error:
            raise BackupError(f"{command[0]} not found; install PostgreSQL client tools or use --pg-container") from error
        except subprocess.CalledProcessError as error:
            raise BackupError(f"{tool[0]} failed with exit status {error.returncode}") from None

    def query(self, sql, database=None):
        result = self.run(["psql", "--no-psqlrc", "-X", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1"], database, input=sql, text=True, capture_output=True)
        return result.stdout.strip()

    def dump(self, output):
        self.run(["pg_dump", *PG_DUMP_OPTIONS], stdout=output)

    def list_archive(self, archive):
        self.run(["pg_restore", "--list"], stdin=archive, stdout=subprocess.DEVNULL)

    def restore(self, archive, database):
        self.run(["pg_restore", "--exit-on-error", "--single-transaction", "--no-owner", "--no-privileges", "--dbname", database], database, stdin=archive)


LINEAGE_SQL = """SELECT json_build_object(
  'migrations', coalesce((SELECT json_agg(json_build_object('version',version,'checksum',encode(checksum,'hex'),'success',success) ORDER BY version) FROM _sqlx_migrations),'[]'::json),
  'schema_family', (SELECT schema_family FROM installation WHERE singleton),
  'server_version', current_setting('server_version'),
  'database', current_database());"""

# The gateway schema uses no extensions. `--extension=plpgsql` selects only the built-in one, so
# extensions a cluster's template1 adds to every database (pgvector, for example) are left out:
# the owner restoring on a clean host usually may not create them.
PG_DUMP_OPTIONS = ["--format=custom", "--no-owner", "--no-privileges", "--extension=plpgsql"]

EMPTY_SQL = """SELECT
 (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%')
 + (SELECT count(*) FROM pg_namespace WHERE nspname NOT IN ('public','pg_catalog','information_schema') AND nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp%')
 + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public')
 + (SELECT count(*) FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='public')
 + (SELECT count(*) FROM pg_extension WHERE extname<>'plpgsql');"""

STORED_FILES_EXISTS_SQL = "SELECT to_regclass('public.stored_files') IS NOT NULL;"
FILE_KEY_IDS_SQL = "SELECT coalesce(json_agg(DISTINCT encryption_key_id ORDER BY encryption_key_id),'[]'::json) FROM stored_files WHERE deleted_at IS NULL;"


def read_lineage(pg, database=None):
    try:
        value = json.loads(pg.query(LINEAGE_SQL, database))
        migrations = [{"version": int(m["version"]), "checksum": str(m["checksum"]), "success": bool(m["success"])} for m in value["migrations"]]
    except (ValueError, TypeError, KeyError) as error:
        raise BackupError("could not read migration lineage") from error
    if not migrations or not all(m["success"] for m in migrations):
        raise BackupError("migration lineage is empty or dirty; refusing")
    if value.get("schema_family") != SCHEMA_FAMILY:
        raise BackupError("database is not an enterprise gateway installation")
    return {
        "schema_family": value["schema_family"],
        "latest_version": max(m["version"] for m in migrations),
        "migrations": [{"version": m["version"], "checksum": m["checksum"]} for m in migrations],
        "server_version": value.get("server_version"),
        "database": value.get("database"),
    }


def read_file_key_ids(pg):
    """Distinct key ids of live stored files (ids only); [] when the file store is unused."""
    if pg.query(STORED_FILES_EXISTS_SQL) != "t":
        return []
    try:
        ids = json.loads(pg.query(FILE_KEY_IDS_SQL))
    except ValueError as error:
        raise BackupError("could not read file-store encryption key ids") from error
    if not isinstance(ids, list) or not all(isinstance(i, str) and KEY_ID.fullmatch(i) for i in ids):
        raise BackupError("could not read file-store encryption key ids")
    return ids


def sha256(path):
    return file_digest(path)[0]


def file_digest(path):
    digest, size = hashlib.sha256(), 0
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(CHUNK), b""):
            digest.update(block)
            size += len(block)
    return digest.hexdigest(), size


# --- Streaming helpers: plaintext moves between processes through pipes only.

def write_all(fd, data):
    view = memoryview(data)
    while view:
        view = view[os.write(fd, view):]


class Producer:
    """Runs target(writer) in a thread; the caller reads the pipe's other end."""

    def __init__(self, target):
        self.fd, write_fd = os.pipe()
        self.error = None

        def run():
            try:
                with os.fdopen(write_fd, "wb") as output:
                    target(output)
            except BaseException as error:  # reported by finish()
                self.error = error

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def read(self):
        return os.read(self.fd, CHUNK)

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def finish(self):
        self.close()
        self.thread.join()
        if self.error is not None:
            raise self.error


class Consumer:
    """Runs target(reader, *args) in a thread; the caller writes to the pipe."""

    def __init__(self, target, *args):
        read_fd, self.fd = os.pipe()
        self.error = None

        def run():
            try:
                with os.fdopen(read_fd, "rb") as reader:
                    target(reader, *args)
            except BaseException as error:  # reported by finish()
                self.error = error

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def write(self, data):
        """False once the consumer has stopped reading."""
        if self.fd is None:
            return False
        try:
            write_all(self.fd, data)
            return True
        except BrokenPipeError:
            self.close()
            return False

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def finish(self):
        self.close()
        self.thread.join()
        if self.error is not None:
            raise self.error


def parse_recipients(values=(), files=()):
    """age recipients (public keys) from --encrypt-to values and recipients files."""
    found = [value.strip() for value in values or ()]
    for name in files or ():
        try:
            lines = Path(name).read_text().splitlines()
        except (OSError, UnicodeDecodeError):
            raise BackupError("age recipients file is unreadable") from None
        for line in lines:
            line = line.strip()
            if line and not line.startswith("#"):
                # ssh public keys: drop the trailing comment.
                found.append(" ".join(line.split()[:2]) if line.startswith("ssh-") else line)
    recipients = []
    for value in found:
        if not AGE_RECIPIENT.fullmatch(value):
            raise BackupError("invalid age recipient: expected an age1... public key or an ssh-ed25519/ssh-rsa public key")
        if value not in recipients:
            recipients.append(value)
    if files and not recipients:
        raise BackupError("no age recipients found")
    return recipients


def encrypted_dump(pg, archive, recipients):
    """pg_dump -> age -> archive, also feeding pg_restore --list; returns the plaintext (sha256, bytes)."""
    command = ["age"]
    for recipient in recipients:
        command += ["-r", recipient]
    digest, size = hashlib.sha256(), 0
    with archive.open("xb") as output:
        archive.chmod(0o600)
        try:
            age = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=output, stderr=subprocess.DEVNULL)
        except FileNotFoundError:
            raise BackupError(AGE_MISSING) from None
        producer = Producer(pg.dump)
        lister = Consumer(pg.list_archive)
        age_failed = False
        try:
            for block in iter(producer.read, b""):
                digest.update(block)
                size += len(block)
                try:
                    age.stdin.write(block)
                except BrokenPipeError:
                    age_failed = True
                    break
                lister.write(block)  # pg_restore --list stops after the table of contents
        finally:
            producer.close()  # an unfinished pg_dump gets EPIPE
            lister.close()
            try:
                age.stdin.close()
            except BrokenPipeError:
                age_failed = True
            status = age.wait()
            producer.thread.join()
            lister.thread.join()
        age_error = BackupError("age could not encrypt the backup; check the recipients")
        if age_failed:
            raise age_error  # pg_dump was stopped because age exited
        producer.finish()
        if status != 0:
            raise age_error
        lister.finish()
    return digest.hexdigest(), size


def age_decrypt(archive, identity, consume):
    """Stream-decrypt archive with age, calling consume(block); returns (sha256, bytes)."""
    identity = Path(identity)
    if not identity.is_file():
        raise BackupError("age identity file not found")
    digest, size = hashlib.sha256(), 0
    with archive.open("rb") as source:
        try:
            age = subprocess.Popen(["age", "--decrypt", "--identity", str(identity)], stdin=source, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        except FileNotFoundError:
            raise BackupError(AGE_MISSING) from None
        try:
            for block in iter(lambda: age.stdout.read(CHUNK), b""):
                digest.update(block)
                size += len(block)
                consume(block)
        finally:
            age.stdout.close()  # a consumer failure stops age with EPIPE
            status = age.wait()
    if status != 0:
        raise BackupError("age could not decrypt the backup (wrong identity or damaged archive)")
    return digest.hexdigest(), size


def backup(pg, out_dir, now=None, recipients=None):
    out_dir = Path(out_dir)
    out_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    now = now or datetime.now(timezone.utc)
    lineage = read_lineage(pg)
    key_ids = read_file_key_ids(pg)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    dump = out_dir / f"gateway-{stamp}-v{lineage['latest_version']:04d}-{secrets.token_hex(4)}.dump"
    if recipients:
        dump = dump.with_name(dump.name + ".age")
    manifest_path = dump.with_name(dump.name + MANIFEST_SUFFIX)
    try:
        if recipients:
            plain_sha, plain_bytes = encrypted_dump(pg, dump, recipients)
        else:
            with dump.open("xb") as output:
                dump.chmod(0o600)
                pg.dump(output)
        # The archive must describe the same lineage (no concurrent migrate).
        if read_lineage(pg)["migrations"] != lineage["migrations"]:
            raise BackupError("migration lineage changed during backup; retry")
        # Keys referenced before or after the dump: a superset of those in it.
        key_ids = sorted(set(key_ids) | set(read_file_key_ids(pg)))
        if recipients:
            if plain_bytes == 0:
                raise BackupError("pg_dump produced an empty archive")
            cipher_sha, cipher_bytes = file_digest(dump)
            details = {
                "bytes": plain_bytes,
                "sha256": plain_sha,
                "encryption": {"tool": "age", "recipients": list(recipients), "ciphertext_sha256": cipher_sha, "ciphertext_bytes": cipher_bytes},
            }
        else:
            if dump.stat().st_size == 0:
                raise BackupError("pg_dump produced an empty archive")
            with dump.open("rb") as archive:
                pg.list_archive(archive)
            details = {"bytes": dump.stat().st_size, "sha256": sha256(dump)}
        manifest = {
            "format": MANIFEST_FORMAT,
            "created_at": now.isoformat(),
            "file": dump.name,
            "file_encryption_key_ids": key_ids,
            "pg_dump_options": PG_DUMP_OPTIONS,
            **details,
            **lineage,
        }
        with manifest_path.open("x") as handle:
            manifest_path.chmod(0o600)
            handle.write(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    except BaseException:
        dump.unlink(missing_ok=True)
        manifest_path.unlink(missing_ok=True)
        raise
    return manifest_path


def parse_manifest(data):
    """Validate manifest JSON (bytes or str); raises ManifestError."""
    try:
        manifest = json.loads(data)
    except (ValueError, UnicodeDecodeError):
        raise ManifestError("manifest is unreadable or not JSON") from None
    if not isinstance(manifest, dict) or manifest.get("format") != MANIFEST_FORMAT:
        raise ManifestError("unsupported manifest format")
    name = manifest.get("file") or ""
    if not isinstance(name, str) or not name or "/" in name or "\\" in name or name.startswith("."):
        raise ManifestError("manifest file name is invalid")
    if not isinstance(manifest.get("migrations"), list) or not manifest["migrations"]:
        raise ManifestError("manifest has no migration lineage")
    encryption = manifest.get("encryption")
    if encryption is not None:
        if not (isinstance(encryption, dict) and encryption.get("tool") == "age"
                and isinstance(encryption.get("recipients"), list) and encryption["recipients"]
                and HEX64.fullmatch(str(encryption.get("ciphertext_sha256")))
                and isinstance(encryption.get("ciphertext_bytes"), int) and name.endswith(".age")):
            raise ManifestError("manifest encryption details are invalid")
    elif name.endswith(".age"):
        raise ManifestError("manifest names an encrypted archive but has no encryption details")
    ids = manifest.get("file_encryption_key_ids", [])
    if not isinstance(ids, list) or not all(isinstance(i, str) and KEY_ID.fullmatch(i) for i in ids):
        raise ManifestError("manifest file_encryption_key_ids is invalid")
    return manifest


def load_manifest(path):
    path = Path(path)
    try:
        data = path.read_bytes()
    except OSError as error:
        raise BackupError("manifest is unreadable or not JSON") from error
    manifest = parse_manifest(data)
    return manifest, path.parent / manifest["file"]


def stored_digest(manifest):
    """(sha256, bytes) of the archive as stored: ciphertext when encrypted."""
    encryption = manifest.get("encryption")
    if encryption:
        return encryption["ciphertext_sha256"], encryption["ciphertext_bytes"]
    return manifest.get("sha256"), manifest.get("bytes")


def verify_plaintext(manifest, archive, identity, pg=None):
    """Decrypt pass: plaintext SHA-256/size and (with pg) pg_restore --list, without writing plaintext."""
    listers = []

    def consume(block):
        if pg is not None:
            if not listers:  # started on the first plaintext block, not for a failed decrypt
                listers.append(Consumer(pg.list_archive))
            listers[0].write(block)
    try:
        digest, size = age_decrypt(archive, identity, consume)
    finally:
        for lister in listers:
            lister.close()
            lister.thread.join()
    if digest != manifest.get("sha256") or size != manifest.get("bytes"):
        raise BackupError("decrypted backup checksum or size does not match its manifest")
    for lister in listers:
        lister.finish()


def verify(manifest_path, pg=None, identity=None):
    manifest, dump = load_manifest(manifest_path)
    if not dump.is_file():
        raise BackupError("backup file named by the manifest is missing")
    expected_sha, expected_bytes = stored_digest(manifest)
    if dump.stat().st_size != expected_bytes or sha256(dump) != expected_sha:
        raise BackupError("backup checksum or size does not match its manifest")
    if manifest.get("encryption"):
        if identity is not None:
            verify_plaintext(manifest, dump, identity, pg)
    elif pg is not None:
        with dump.open("rb") as archive:
            pg.list_archive(archive)
    return manifest, dump


def binary_lineage(binary):
    try:
        result = subprocess.run([str(binary), "schema-version"], check=True, capture_output=True, text=True, timeout=60, env={"PATH": os.environ.get("PATH", "")})
        value = json.loads(result.stdout)
        return [{"version": int(m["version"]), "checksum": str(m["checksum"])} for m in value["migrations"]]
    except (OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError) as error:
        raise BackupError("could not read `schema-version` from the gateway binary") from error


def check_compatible(manifest, expected_migrations=None, expected_version=None):
    if expected_migrations is not None:
        if manifest["migrations"] != expected_migrations:
            latest = max((m["version"] for m in expected_migrations), default=None)
            raise BackupError(f"backup migration lineage (latest {manifest.get('latest_version')}) does not match the gateway binary (latest {latest}); restore with the matching release")
    elif expected_version is not None:
        if manifest.get("latest_version") != expected_version:
            raise BackupError(f"backup latest migration {manifest.get('latest_version')} does not match expected {expected_version}")
    else:
        raise BackupError("pass --gateway-binary or --expect-version")


def configured_file_key_ids(env_name):
    """Key ids (never values) from a `kid:base64key[,...]` variable."""
    if not ENV_NAME.fullmatch(env_name or ""):
        raise BackupError("--file-keys-env must name an environment variable ([A-Z_][A-Z0-9_]*)")
    value = os.environ.get(env_name, "")
    if not value.strip():
        raise BackupError(f"{env_name} is not set or empty")
    ids = set()
    for number, entry in enumerate(value.split(","), 1):
        kid, separator, key = entry.strip().partition(":")
        if not separator or not KEY_ID.fullmatch(kid) or not key.strip():
            raise BackupError(f"{env_name} entry {number} must be kid:base64key")
        ids.add(kid)
    return ids


def check_file_keys(manifest, env_name=None, skip=False):
    needed = manifest.get("file_encryption_key_ids") or []
    if not needed:
        return
    if env_name is None:
        if skip:
            return
        raise BackupError(
            f"stored files in this backup use file-store encryption key ids {', '.join(needed)}; "
            "pass --file-keys-env NAME (the variable holding the kid:base64key list) to confirm they are available, "
            "or --skip-file-key-check if stored files will not be recovered")
    missing = sorted(set(needed) - configured_file_key_ids(env_name))
    if missing:
        raise BackupError(f"file-store encryption key ids missing from {env_name}: {', '.join(missing)}; stored files would be unreadable")


def restore_encrypted(pg, manifest, archive, identity, target):
    """Second decrypt pass straight into pg_restore. The last block is held back
    until the plaintext checksum matches, so a changed archive never completes."""
    feed = Consumer(pg.restore, target)
    pending = [b""]

    def send(block):
        if not feed.write(block):
            feed.finish()
            raise BackupError("pg_restore stopped before the end of the archive")

    def consume(block):
        if pending[0]:
            send(pending[0])
        pending[0] = block

    try:
        digest, size = age_decrypt(archive, identity, consume)
        if digest != manifest["sha256"] or size != manifest["bytes"]:
            raise BackupError("decrypted backup changed since it was verified; restore aborted")
        send(pending[0])
    finally:
        feed.close()  # without the held-back block pg_restore fails and rolls back
        feed.thread.join()
    feed.finish()


def restore(pg, manifest_path, target, create=False, expected_migrations=None, expected_version=None,
            identity=None, file_keys_env=None, skip_file_key_check=False):
    target = valid_database_name(target)
    manifest, _ = load_manifest(manifest_path)
    encrypted = bool(manifest.get("encryption"))
    if encrypted and identity is None:
        raise BackupError("backup is encrypted; pass --identity with the age identity file")
    check_file_keys(manifest, file_keys_env, skip_file_key_check)
    manifest, dump = verify(manifest_path, pg, identity)
    check_compatible(manifest, expected_migrations, expected_version)
    created = False
    if create:
        if pg.query(f"SELECT count(*) FROM pg_database WHERE datname='{target}';") != "0":
            raise BackupError(f"database {target} already exists; --create only creates a new database")
        pg.query(f'CREATE DATABASE "{target}" TEMPLATE template0;')
        created = True
    try:
        if created:
            pg.query(f'REVOKE ALL ON DATABASE "{target}" FROM PUBLIC;')
        if pg.query(EMPTY_SQL, target) != "0":
            raise BackupError(f"database {target} is not empty; restore only into a new, empty database")
        if encrypted:
            restore_encrypted(pg, manifest, dump, identity, target)
        else:
            with dump.open("rb") as archive:
                pg.restore(archive, target)
        restored = read_lineage(pg, target)
        if restored["migrations"] != manifest["migrations"]:
            raise BackupError("restored migration lineage does not match the manifest")
        counts = pg.query("SELECT (SELECT count(*) FROM monetary_ledger)||' ledger entries, '||(SELECT count(*) FROM inference_executions)||' executions';", target)
    except BaseException:
        if created:
            # Only the database this invocation created, never a pre-existing one.
            try:
                pg.query(f'DROP DATABASE "{target}" WITH (FORCE);')
            except BackupError:
                pass
        raise
    return restored, counts


# --- S3-compatible object storage (path-style, AWS Signature Version 4, stdlib only).

class Credentials:
    __slots__ = ("access_key", "secret_key", "session_token")

    def __init__(self, access_key, secret_key, session_token=None):
        self.access_key, self.secret_key, self.session_token = access_key, secret_key, session_token

    def __repr__(self):
        return "Credentials(<redacted>)"

    @classmethod
    def from_env(cls):
        access = os.environ.get("AWS_ACCESS_KEY_ID", "").strip()
        secret = os.environ.get("AWS_SECRET_ACCESS_KEY", "").strip()
        if not access or not secret:
            raise BackupError("set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY for the object store")
        return cls(access, secret, os.environ.get("AWS_SESSION_TOKEN", "").strip() or None)


def uri_encode(value, safe="-_.~"):
    return quote(value, safe=safe)


def sigv4_headers(method, host, path, query, headers, payload_hash, credentials, region, now, service="s3"):
    """Headers (including Authorization) for an AWS SigV4-signed request.
    `path` is the already URI-encoded canonical path; every given header is signed."""
    amz_date = now.astimezone(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    signed = {name.lower(): " ".join(str(value).split()) for name, value in headers.items()}
    signed.update({"host": host, "x-amz-date": amz_date, "x-amz-content-sha256": payload_hash})
    if credentials.session_token:
        signed["x-amz-security-token"] = credentials.session_token
    names = sorted(signed)
    canonical_query = "&".join(f"{k}={v}" for k, v in sorted((uri_encode(k), uri_encode(v)) for k, v in (query or {}).items()))
    canonical_request = "\n".join([
        method, path, canonical_query,
        "".join(f"{name}:{signed[name]}\n" for name in names),
        ";".join(names), payload_hash])
    scope = f"{amz_date[:8]}/{region}/{service}/aws4_request"
    string_to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical_request.encode()).hexdigest()])
    key = ("AWS4" + credentials.secret_key).encode()
    for part in (amz_date[:8], region, service, "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, string_to_sign.encode(), hashlib.sha256).hexdigest()
    signed["authorization"] = f"AWS4-HMAC-SHA256 Credential={credentials.access_key}/{scope}, SignedHeaders={';'.join(names)}, Signature={signature}"
    return signed


def parse_target(value):
    """s3://bucket[/prefix] -> (bucket, prefix ending in '/' or '')."""
    parts = urlsplit(value or "")
    if parts.scheme != "s3" or not parts.netloc or parts.query or parts.fragment:
        raise BackupError("location must be s3://bucket[/prefix]")
    if not BUCKET.fullmatch(parts.netloc):
        raise BackupError("bucket name must be 3-63 characters of a-z 0-9 . -")
    segments = [s for s in parts.path.split("/") if s]
    if not all(PREFIX_SEGMENT.fullmatch(s) for s in segments):
        raise BackupError("prefix segments must be letters, digits, . _ = - (no '..')")
    return parts.netloc, "".join(s + "/" for s in segments)


class S3:
    """Minimal S3 client: no redirects, proxies or retries; one connection per request."""

    def __init__(self, endpoint, region, credentials, timeout=120):
        parts = urlsplit(endpoint or "")
        try:
            port = parts.port
        except ValueError:
            raise BackupError("--endpoint has an invalid port") from None
        if (parts.scheme not in ("https", "http") or not parts.hostname or parts.username or parts.password
                or parts.path not in ("", "/") or parts.query or parts.fragment):
            raise BackupError("--endpoint must be an https:// origin without path, credentials or query")
        if parts.scheme == "http" and parts.hostname not in LOOPBACK:
            raise BackupError("--endpoint must use https (http is accepted only for loopback hosts)")
        if not REGION.fullmatch(region or ""):
            raise BackupError("--region must be lowercase letters, digits and dashes")
        self.https, self.hostname, self.port = parts.scheme == "https", parts.hostname, port
        self.host = parts.netloc
        self.region, self.credentials, self.timeout = region, credentials, timeout

    def _connection(self):
        if self.https:
            return http.client.HTTPSConnection(self.hostname, self.port, timeout=self.timeout, context=ssl.create_default_context())
        return http.client.HTTPConnection(self.hostname, self.port, timeout=self.timeout)

    def _send(self, method, bucket, key, what, query=None, body=None, payload_hash=EMPTY_SHA256, length=None, content_type=None, ok=(200,)):
        path = "/" + bucket + ("/" + uri_encode(key, safe="/-_.~") if key else "")
        target = path
        if query:
            target += "?" + "&".join(f"{uri_encode(k)}={uri_encode(v)}" for k, v in sorted(query.items()))
        headers = {"content-type": content_type} if content_type else {}
        headers = sigv4_headers(method, self.host, path, query, headers, payload_hash, self.credentials, self.region, datetime.now(timezone.utc))
        connection = self._connection()
        try:
            try:
                connection.putrequest(method, target, skip_host=True, skip_accept_encoding=True)
                for name, value in headers.items():
                    connection.putheader(name, value)
                if length is not None:
                    connection.putheader("content-length", str(length))
                connection.endheaders()
                if body is not None:
                    for block in iter(lambda: body.read(CHUNK), b""):
                        connection.send(block)
                response = connection.getresponse()
            except (OSError, http.client.HTTPException):
                raise BackupError(f"S3 {what} failed: could not complete the request to the endpoint") from None
            if response.status not in ok:
                try:
                    detail = response.read(65536)
                except (OSError, http.client.HTTPException):
                    detail = b""
                code = re.search(rb"<Code>([A-Za-z]{1,64})</Code>", detail)
                raise BackupError(f"S3 {what} failed: HTTP {response.status}" + (f" ({code.group(1).decode()})" if code else ""))
            return connection, response
        except BaseException:
            connection.close()
            raise

    def _read(self, response, what, sink):
        try:
            for block in iter(lambda: response.read(CHUNK), b""):
                sink(block)
        except (OSError, http.client.HTTPException):
            raise BackupError(f"S3 {what} failed: the download was interrupted") from None

    def head(self, bucket, key):
        """Object size, or None when it does not exist."""
        connection, response = self._send("HEAD", bucket, key, f"HEAD {key.rsplit('/', 1)[-1]}", ok=(200, 404))
        try:
            response.read()
            if response.status == 404:
                return None
            length = response.getheader("content-length")
            if length is None or not length.isdigit():
                raise BackupError("S3 HEAD returned no object size")
            return int(length)
        finally:
            connection.close()

    def put(self, bucket, key, body, payload_hash, length, content_type):
        connection, response = self._send("PUT", bucket, key, f"PUT {key.rsplit('/', 1)[-1]}", body=body, payload_hash=payload_hash, length=length, content_type=content_type)
        try:
            response.read()
        finally:
            connection.close()

    def get(self, bucket, key, sink):
        what = f"GET {key.rsplit('/', 1)[-1]}"
        connection, response = self._send("GET", bucket, key, what)
        try:
            self._read(response, what, sink)
        finally:
            connection.close()

    def get_bytes(self, bucket, key, limit=MAX_MANIFEST_BYTES):
        data = bytearray()

        def sink(block):
            data.extend(block)
            if len(data) > limit:
                raise BackupError("S3 object is larger than expected for a manifest")
        self.get(bucket, key, sink)
        return bytes(data)

    def delete(self, bucket, key):
        connection, response = self._send("DELETE", bucket, key, f"DELETE {key.rsplit('/', 1)[-1]}", ok=(200, 204))
        try:
            response.read()
        finally:
            connection.close()

    def list(self, bucket, prefix):
        """Objects directly under prefix: [{key, size}] (ListObjectsV2, delimiter '/')."""
        objects, token = [], None
        for _ in range(10000):
            query = {"list-type": "2", "prefix": prefix, "delimiter": "/"}
            if token:
                query["continuation-token"] = token
            data = bytearray()
            connection, response = self._send("GET", bucket, "", "list", query=query)
            try:
                self._read(response, "list", data.extend)
            finally:
                connection.close()
            try:
                root = ElementTree.fromstring(bytes(data))
                for item in root.findall("{*}Contents"):
                    objects.append({"key": item.findtext("{*}Key"), "size": int(item.findtext("{*}Size"))})
                truncated = (root.findtext("{*}IsTruncated") or "").strip().lower() == "true"
                token = root.findtext("{*}NextContinuationToken")
            except (ElementTree.ParseError, TypeError, ValueError):
                raise BackupError("S3 list returned an unreadable response") from None
            if not truncated:
                return objects
            if not token:
                raise BackupError("S3 list is truncated without a continuation token")
        raise BackupError("S3 list returned too many pages")


def remote_generations(client, bucket, prefix):
    """(generations newest first, names of other objects). A generation is a manifest
    named <archive>.manifest.json; archive_bytes is None when the archive is absent."""
    objects = {}
    for item in client.list(bucket, prefix):
        if item["key"] and item["key"].startswith(prefix):
            objects[item["key"][len(prefix):]] = item["size"]
    generations, other = [], []
    for name in objects:
        match = MANIFEST_NAME.fullmatch(name)
        if match:
            archive = name[:-len(MANIFEST_SUFFIX)]
            generations.append({"manifest": name, "archive": archive, "stamp": match.group(1), "archive_bytes": objects.get(archive)})
        elif not (ARCHIVE_NAME.fullmatch(name) and name + MANIFEST_SUFFIX in objects):
            other.append(name)
    generations.sort(key=lambda g: (g["stamp"], g["manifest"]), reverse=True)
    return generations, sorted(other)


def upload(client, manifest_path, bucket, prefix, allow_plaintext=False):
    manifest_path = Path(manifest_path)
    try:
        manifest_bytes = manifest_path.read_bytes()
    except OSError:
        raise BackupError("manifest is unreadable or not JSON") from None
    manifest = parse_manifest(manifest_bytes)
    name = manifest["file"]
    archive = manifest_path.parent / name
    if not ARCHIVE_NAME.fullmatch(name):
        raise BackupError("archive name is not a gateway backup generation (gateway-<time>-v<version>-<id>.dump[.age])")
    if not manifest.get("encryption") and not allow_plaintext:
        raise BackupError("refusing to upload an unencrypted backup; create it with --encrypt-to, or pass --allow-plaintext")
    if not archive.is_file():
        raise BackupError("backup file named by the manifest is missing")
    expected_sha, expected_bytes = stored_digest(manifest)
    size = archive.stat().st_size
    if size != expected_bytes:
        raise BackupError("backup checksum or size does not match its manifest")
    if size > MAX_SINGLE_PUT:
        raise BackupError("archive is larger than 5 GiB, the single-request upload limit; multipart upload is not supported")
    digest = sha256(archive)
    if digest != expected_sha:
        raise BackupError("backup checksum or size does not match its manifest")
    archive_key, manifest_key = prefix + name, prefix + name + MANIFEST_SUFFIX
    if client.head(bucket, manifest_key) is not None:
        raise BackupError(f"{name}{MANIFEST_SUFFIX} already exists at the destination; refusing to overwrite")
    # Archive first: a manifest is only ever visible once its archive is complete.
    with archive.open("rb") as body:
        client.put(bucket, archive_key, body, digest, size, "application/octet-stream")
    if client.head(bucket, archive_key) != size:
        raise BackupError("uploaded archive size does not match; the manifest was not uploaded")
    client.put(bucket, manifest_key, io.BytesIO(manifest_bytes), hashlib.sha256(manifest_bytes).hexdigest(), len(manifest_bytes), "application/json")
    if client.head(bucket, manifest_key) != len(manifest_bytes):
        raise BackupError("uploaded manifest size does not match")
    return name


def exclusive_file(path):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        raise BackupError(f"{path.name} already exists in the output directory; refusing to overwrite") from None
    os.fchmod(fd, 0o600)
    return os.fdopen(fd, "wb")


def download(client, bucket, prefix, name, out_dir):
    manifest_name = name if name.endswith(MANIFEST_SUFFIX) else name + MANIFEST_SUFFIX
    if not MANIFEST_NAME.fullmatch(manifest_name):
        raise BackupError("name must be a backup generation (gateway-<time>-v<version>-<id>.dump[.age][.manifest.json])")
    archive_name = manifest_name[:-len(MANIFEST_SUFFIX)]
    data = client.get_bytes(bucket, prefix + manifest_name)
    manifest = parse_manifest(data)
    if manifest["file"] != archive_name:
        raise BackupError("the remote manifest names a different archive")
    expected_sha, expected_bytes = stored_digest(manifest)
    out_dir = Path(out_dir)
    out_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    archive_path, manifest_path = out_dir / archive_name, out_dir / manifest_name
    for path in (archive_path, manifest_path):
        if os.path.lexists(path):
            raise BackupError(f"{path.name} already exists in the output directory; refusing to overwrite")
    created = []
    try:
        with exclusive_file(archive_path) as output:
            created.append(archive_path)
            digest, size = hashlib.sha256(), [0]

            def sink(block):
                digest.update(block)
                size[0] += len(block)
                output.write(block)
            client.get(bucket, prefix + archive_name, sink)
        if digest.hexdigest() != expected_sha or size[0] != expected_bytes:
            raise BackupError("downloaded archive checksum or size does not match its manifest")
        with exclusive_file(manifest_path) as output:
            created.append(manifest_path)
            output.write(data)
    except BaseException:
        for path in created:
            path.unlink(missing_ok=True)
        raise
    return manifest_path


def prune(client, bucket, prefix, keep, yes=False, out=print):
    if keep < 1:
        raise BackupError("--keep must be at least 1")
    generations, other = remote_generations(client, bucket, prefix)
    recognized, skipped = [], [g["manifest"] for g in generations if g["archive_bytes"] is None]
    for generation in generations:
        if generation["archive_bytes"] is None:
            continue
        try:
            manifest = parse_manifest(client.get_bytes(bucket, prefix + generation["manifest"]))
        except ManifestError:
            skipped.append(generation["manifest"])
            continue
        # Only a complete generation counts toward --keep: a truncated or replaced
        # archive must never take the place of a restorable one.
        if manifest["file"] != generation["archive"] or generation["archive_bytes"] != stored_digest(manifest)[1]:
            skipped.append(generation["manifest"])
            continue
        recognized.append(generation)
    kept, doomed = recognized[:keep], recognized[keep:]
    if recognized and recognized[0] in doomed:  # defensive: never the newest generation
        raise BackupError("internal error: refusing to delete the newest generation")
    out(f"Keeping {len(kept)} newest generation(s):")
    for generation in kept:
        out(f"  keep    {generation['archive']}")
    out(f"{'Deleting' if yes else 'Would delete'} {len(doomed)} generation(s) (manifest first, then archive):")
    for generation in doomed:
        out(f"  delete  {generation['archive']}")
    if skipped or other:
        out(f"Left untouched: {len(skipped)} incomplete or unrecognized manifest(s), {len(other)} other object(s).")
    if not yes:
        if doomed:
            out("Dry run: nothing was deleted. Re-run with --yes to delete.")
        return doomed
    for generation in doomed:
        client.delete(bucket, prefix + generation["manifest"])
        client.delete(bucket, prefix + generation["archive"])
        out(f"Deleted {generation['archive']}")
    return doomed


# Rows whose revocation is final (migration 0039), with their key as text;
# `{a}` is the table alias prefix ("" or "t.").
REVOCABLE = [
    ("api_keys", "{a}id::text"),
    ("workspace_membership_grants", "{a}id::text"),
    ("platform_role_grants", "{a}id::text"),
    ("browser_sessions", "encode({a}token_hash,'hex')"),
    ("workspace_invitations", "{a}id::text"),
]


def reapply_revocations(source, target, yes=False, out=print):
    """Copy every revocation of the old (source) database onto the restored
    (target) one: a key, grant, session or invitation that the old database
    revoked but the restore brought back active gets the same `revoked_at`.
    Rows already revoked in the target keep their time; rows created after
    the backup don't exist in the target and are skipped. Dry run unless
    `yes`; applied in one transaction. Returns the count per table."""
    installation = "SELECT id FROM installation;"
    if source.query(installation) != target.query(installation):
        raise BackupError("the source and target are a different installation; refusing to copy revocations")
    tag = "omg_" + secrets.token_hex(8)
    counts, statements = {}, []
    for table, key in REVOCABLE:
        revoked = source.query(f"SELECT coalesce(json_agg(json_build_array({key.format(a='')}, revoked_at)),'[]') FROM {table} WHERE revoked_at IS NOT NULL;")
        match = f"{key.format(a='t.')}=r.key"
        pending = f"SELECT x->>0 AS key, (x->>1)::timestamptz AS revoked_at FROM json_array_elements(${tag}${revoked}${tag}$::json) x"
        counts[table] = int(target.query(f"SELECT count(*) FROM {table} t JOIN ({pending}) r ON {match} WHERE t.revoked_at IS NULL;"))
        statements.append(f"UPDATE {table} t SET revoked_at=r.revoked_at FROM ({pending}) r WHERE {match} AND t.revoked_at IS NULL;")
    verb = "Re-revoking" if yes else "Would re-revoke"
    for table, n in counts.items():
        out(f"{verb} {n} {table} row(s) revoked in the old database after the backup.")
    if not yes:
        out("Dry run: nothing changed. Re-run with --yes to apply.")
        return counts
    target.query("BEGIN;\n" + "\n".join(statements) + "\nCOMMIT;")
    return counts


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    def connection(p):
        p.add_argument("--url-env", default="DATABASE_URL", help="environment variable holding the database URL (default DATABASE_URL)")
        p.add_argument("--url-file", help="file containing the database URL (e.g. a migrator secret)")
        p.add_argument("--pg-container", help="run pg_dump/pg_restore/psql inside this container via docker exec")

    def storage(p, flag):
        p.add_argument(flag, dest="location", required=True, metavar="s3://BUCKET[/PREFIX]")
        p.add_argument("--endpoint", help="S3-compatible endpoint origin (default https://s3.<region>.amazonaws.com); path-style requests")
        p.add_argument("--region", default="us-east-1", help="signing region (default us-east-1)")

    p = sub.add_parser("backup", help="dump the database and write a manifest")
    connection(p)
    p.add_argument("--out-dir", required=True, type=Path)
    p.add_argument("--encrypt-to", action="append", default=[], metavar="RECIPIENT", help="age recipient public key (repeatable); the plaintext dump never touches disk")
    p.add_argument("--encrypt-to-file", action="append", default=[], metavar="FILE", help="file of age recipients, one per line")
    p = sub.add_parser("verify", help="check a manifest's checksum (and archive readability)")
    p.add_argument("manifest", type=Path)
    p.add_argument("--pg-container")
    p.add_argument("--no-archive-check", action="store_true", help="checksum only; do not run pg_restore --list")
    p.add_argument("--identity", type=Path, help="age identity file: also decrypt and check the plaintext of an encrypted backup")
    p = sub.add_parser("restore", help="restore into an explicitly named empty database")
    connection(p)
    p.add_argument("manifest", type=Path)
    p.add_argument("--target-db", required=True, help="explicit target database name (must be empty, or new with --create)")
    p.add_argument("--create", action="store_true", help="create the target database (refuses if it exists)")
    group = p.add_mutually_exclusive_group(required=True)
    group.add_argument("--gateway-binary", type=Path, help="gateway binary of the release that will serve the restore")
    group.add_argument("--expect-version", type=int, help="expected latest migration version (weaker than --gateway-binary)")
    p.add_argument("--identity", type=Path, help="age identity file (required for encrypted backups)")
    p.add_argument("--file-keys-env", metavar="NAME", help="variable holding the file-store key list (kid:base64key,...); every key id the backup references must be present")
    p.add_argument("--skip-file-key-check", action="store_true", help="restore even though file-store key ids were not checked")
    p = sub.add_parser("reapply-revocations", help="after a restore, re-revoke what the old database revoked since the backup (dry run without --yes)")
    connection(p)
    p.add_argument("--source-url-file", required=True, help="file containing the OLD (pre-restore) database URL; read only")
    p.add_argument("--yes", action="store_true", help="apply (one transaction)")
    p = sub.add_parser("upload", help="upload a backup (archive, then manifest) to S3-compatible storage")
    p.add_argument("manifest", type=Path)
    storage(p, "--to")
    p.add_argument("--allow-plaintext", action="store_true", help="also upload an unencrypted backup")
    p = sub.add_parser("list", help="list backup generations, newest first")
    storage(p, "--from")
    p = sub.add_parser("download", help="download and verify a generation")
    p.add_argument("name", help="manifest or archive name from `list`")
    storage(p, "--from")
    p.add_argument("--out-dir", required=True, type=Path)
    p = sub.add_parser("prune", help="delete all but the newest N generations (dry run without --yes)")
    storage(p, "--from")
    p.add_argument("--keep", type=int, required=True)
    p.add_argument("--yes", action="store_true", help="actually delete")
    args = parser.parse_args(argv)

    if args.command == "verify":
        pg = None if args.no_archive_check else Postgres({}, args.pg_container)
        manifest, dump = verify(args.manifest, pg, args.identity)
        if not manifest.get("encryption"):
            print(f"Verified {dump.name}: sha256 matches, latest migration {manifest['latest_version']}.")
        elif args.identity is None:
            print(f"Verified {dump.name}: ciphertext sha256 matches, latest migration {manifest['latest_version']}. Pass --identity to check the plaintext.")
        else:
            print(f"Verified {dump.name}: ciphertext and decrypted plaintext sha256 match, latest migration {manifest['latest_version']}.")
        return
    if args.command in ("upload", "list", "download", "prune"):
        bucket, prefix = parse_target(args.location)
        client = S3(args.endpoint or f"https://s3.{args.region}.amazonaws.com", args.region, Credentials.from_env())
        if args.command == "upload":
            name = upload(client, args.manifest, bucket, prefix, args.allow_plaintext)
            print(f"Uploaded {name} and its manifest to s3://{bucket}/{prefix}")
        elif args.command == "list":
            generations, other = remote_generations(client, bucket, prefix)
            for g in generations:
                state = "archive missing" if g["archive_bytes"] is None else f"{g['archive_bytes']} bytes, {'encrypted' if g['archive'].endswith('.age') else 'UNENCRYPTED'}"
                print(f"{g['archive']}  {state}")
            if not generations:
                print("No backup generations found.")
            if other:
                print(f"{len(other)} other object(s) under the prefix are not backup generations.")
        elif args.command == "download":
            path = download(client, bucket, prefix, args.name, args.out_dir)
            print(f"Downloaded and verified {path}")
        else:
            prune(client, bucket, prefix, args.keep, args.yes)
        return
    pg = Postgres(connection_env(read_url(args)), args.pg_container)
    if args.command == "reapply-revocations":
        source = Postgres(connection_env(Path(args.source_url_file).read_text().strip()), args.pg_container)
        reapply_revocations(source, pg, args.yes)
        return
    if args.command == "backup":
        recipients = parse_recipients(args.encrypt_to, args.encrypt_to_file)
        manifest = backup(pg, args.out_dir, recipients=recipients)
        if recipients:
            print(f"Encrypted backup and manifest written: {manifest}. Keep the age identity separate from the backups.")
        else:
            print(f"Backup and manifest written: {manifest}. Contains sensitive data: encrypt before off-host storage (--encrypt-to).")
    else:
        expected = binary_lineage(args.gateway_binary) if args.gateway_binary else None
        restored, counts = restore(pg, args.manifest, args.target_db, args.create, expected, args.expect_version,
                                   args.identity, args.file_keys_env, args.skip_file_key_check)
        print(f"Restored into {args.target_db}: latest migration {restored['latest_version']}, {counts}.")
        print("Next: apply deploy/staging/runtime-grants.sql as the migrator, start the matching release and check /health/ready.")


if __name__ == "__main__":
    try:
        main()
    except (BackupError, OSError) as error:
        message = str(error) if isinstance(error, BackupError) else type(error).__name__
        print(f"backup.py: {message}", file=sys.stderr)
        sys.exit(1)

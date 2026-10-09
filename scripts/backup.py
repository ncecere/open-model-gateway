#!/usr/bin/env python3
"""Gateway database backup, verification and guarded restore (docs/operations.md).

backup   pg_dump (custom format, no owner/ACLs) + SHA-256 + JSON manifest that
         records the applied migration lineage (version + checksum).
verify   Re-check a manifest's checksum/size and that the archive is readable.
restore  Load a verified backup into an explicitly named, EMPTY database only.
         The backup's lineage must exactly match the gateway binary that will
         serve it (`open-model-gateway schema-version`), or an explicitly
         expected latest version. It never drops, cleans or overwrites data.

Connection URLs are read from an environment variable (default DATABASE_URL)
or a file, passed to PostgreSQL tools through PG* environment variables and
never printed or placed on a command line. `--pg-container NAME` runs the
PostgreSQL client tools inside that container with `docker exec`; the URL is
then resolved from inside the container.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
from datetime import datetime, timezone
from urllib.parse import parse_qs, unquote, urlsplit

MANIFEST_FORMAT = "omg-backup-v1"
SCHEMA_FAMILY = "enterprise_v1"
IDENTIFIER = re.compile(r"[a-z_][a-z0-9_]{0,62}")
# Never restore into system databases or the local demo databases.
PROTECTED_DATABASES = {"postgres", "template0", "template1", "gateway_demo", "gateway_enterprise_demo"}
PG_ENV = ("PGHOST", "PGPORT", "PGUSER", "PGPASSWORD", "PGDATABASE", "PGSSLMODE", "PGSSLROOTCERT")
CHUNK = 1 << 20


class BackupError(Exception):
    """Operator-facing failure. Messages never contain URLs or secrets."""


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
        self.run(["pg_dump", "--format=custom", "--no-owner", "--no-privileges"], stdout=output)

    def list_archive(self, archive):
        self.run(["pg_restore", "--list"], stdin=archive, stdout=subprocess.DEVNULL)

    def restore(self, archive, database):
        self.run(["pg_restore", "--exit-on-error", "--single-transaction", "--no-owner", "--no-privileges", "--dbname", database], database, stdin=archive)


LINEAGE_SQL = """SELECT json_build_object(
  'migrations', coalesce((SELECT json_agg(json_build_object('version',version,'checksum',encode(checksum,'hex'),'success',success) ORDER BY version) FROM _sqlx_migrations),'[]'::json),
  'schema_family', (SELECT schema_family FROM installation WHERE singleton),
  'server_version', current_setting('server_version'),
  'database', current_database());"""

EMPTY_SQL = """SELECT
 (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%')
 + (SELECT count(*) FROM pg_namespace WHERE nspname NOT IN ('public','pg_catalog','information_schema') AND nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp%')
 + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public')
 + (SELECT count(*) FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='public')
 + (SELECT count(*) FROM pg_extension WHERE extname<>'plpgsql');"""


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


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(CHUNK), b""):
            digest.update(block)
    return digest.hexdigest()


def backup(pg, out_dir, now=None):
    out_dir = Path(out_dir)
    out_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    now = now or datetime.now(timezone.utc)
    lineage = read_lineage(pg)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    dump = out_dir / f"gateway-{stamp}-v{lineage['latest_version']:04d}-{secrets.token_hex(4)}.dump"
    manifest_path = dump.with_name(dump.name + ".manifest.json")
    try:
        with dump.open("xb") as output:
            dump.chmod(0o600)
            pg.dump(output)
        # The archive must describe the same lineage (no concurrent migrate).
        if read_lineage(pg)["migrations"] != lineage["migrations"]:
            raise BackupError("migration lineage changed during backup; retry")
        if dump.stat().st_size == 0:
            raise BackupError("pg_dump produced an empty archive")
        with dump.open("rb") as archive:
            pg.list_archive(archive)
        manifest = {
            "format": MANIFEST_FORMAT,
            "created_at": now.isoformat(),
            "file": dump.name,
            "bytes": dump.stat().st_size,
            "sha256": sha256(dump),
            "pg_dump_options": ["--format=custom", "--no-owner", "--no-privileges"],
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


def load_manifest(path):
    path = Path(path)
    try:
        manifest = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise BackupError("manifest is unreadable or not JSON") from error
    if not isinstance(manifest, dict) or manifest.get("format") != MANIFEST_FORMAT:
        raise BackupError("unsupported manifest format")
    name = manifest.get("file") or ""
    if not isinstance(name, str) or not name or "/" in name or "\\" in name or name.startswith("."):
        raise BackupError("manifest file name is invalid")
    if not isinstance(manifest.get("migrations"), list) or not manifest["migrations"]:
        raise BackupError("manifest has no migration lineage")
    return manifest, path.parent / name


def verify(manifest_path, pg=None):
    manifest, dump = load_manifest(manifest_path)
    if not dump.is_file():
        raise BackupError("backup file named by the manifest is missing")
    if dump.stat().st_size != manifest.get("bytes") or sha256(dump) != manifest.get("sha256"):
        raise BackupError("backup checksum or size does not match its manifest")
    if pg is not None:
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


def restore(pg, manifest_path, target, create=False, expected_migrations=None, expected_version=None):
    target = valid_database_name(target)
    manifest, dump = verify(manifest_path, pg)
    check_compatible(manifest, expected_migrations, expected_version)
    created = False
    if create:
        if pg.query(f"SELECT count(*) FROM pg_database WHERE datname='{target}';") != "0":
            raise BackupError(f"database {target} already exists; --create only creates a new database")
        pg.query(f'CREATE DATABASE "{target}";')
        created = True
    try:
        if created:
            pg.query(f'REVOKE ALL ON DATABASE "{target}" FROM PUBLIC;')
        if pg.query(EMPTY_SQL, target) != "0":
            raise BackupError(f"database {target} is not empty; restore only into a new, empty database")
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


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    def connection(p):
        p.add_argument("--url-env", default="DATABASE_URL", help="environment variable holding the database URL (default DATABASE_URL)")
        p.add_argument("--url-file", help="file containing the database URL (e.g. a migrator secret)")
        p.add_argument("--pg-container", help="run pg_dump/pg_restore/psql inside this container via docker exec")

    p = sub.add_parser("backup", help="dump the database and write a manifest")
    connection(p)
    p.add_argument("--out-dir", required=True, type=Path)
    p = sub.add_parser("verify", help="check a manifest's checksum (and archive readability)")
    p.add_argument("manifest", type=Path)
    p.add_argument("--pg-container")
    p.add_argument("--no-archive-check", action="store_true", help="checksum only; do not run pg_restore --list")
    p = sub.add_parser("restore", help="restore into an explicitly named empty database")
    connection(p)
    p.add_argument("manifest", type=Path)
    p.add_argument("--target-db", required=True, help="explicit target database name (must be empty, or new with --create)")
    p.add_argument("--create", action="store_true", help="create the target database (refuses if it exists)")
    group = p.add_mutually_exclusive_group(required=True)
    group.add_argument("--gateway-binary", type=Path, help="gateway binary of the release that will serve the restore")
    group.add_argument("--expect-version", type=int, help="expected latest migration version (weaker than --gateway-binary)")
    args = parser.parse_args(argv)

    if args.command == "verify":
        pg = None if args.no_archive_check else Postgres({}, args.pg_container)
        manifest, dump = verify(args.manifest, pg)
        print(f"Verified {dump.name}: sha256 matches, latest migration {manifest['latest_version']}.")
        return
    pg = Postgres(connection_env(read_url(args)), args.pg_container)
    if args.command == "backup":
        manifest = backup(pg, args.out_dir)
        print(f"Backup and manifest written: {manifest}. Contains sensitive data: encrypt before off-host storage.")
    else:
        expected = binary_lineage(args.gateway_binary) if args.gateway_binary else None
        restored, counts = restore(pg, args.manifest, args.target_db, args.create, expected, args.expect_version)
        print(f"Restored into {args.target_db}: latest migration {restored['latest_version']}, {counts}.")
        print("Next: apply deploy/staging/runtime-grants.sql as the migrator, start the matching release and check /health/ready.")


if __name__ == "__main__":
    try:
        main()
    except (BackupError, OSError) as error:
        message = str(error) if isinstance(error, BackupError) else type(error).__name__
        print(f"backup.py: {message}", file=sys.stderr)
        sys.exit(1)

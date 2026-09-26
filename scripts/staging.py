#!/usr/bin/env python3
"""Isolated single-host staging operations. Never targets the local demo stack."""
import argparse
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "staging"


def create_state(state):
    # Refuse existing state, including symlinks: init is not password rotation.
    state.parent.mkdir(parents=True, exist_ok=True)
    state.mkdir(mode=0o700)
    secret_dir = state / "secrets"
    secret_dir.mkdir(mode=0o700)
    values = {name: secrets.token_hex(32) for name in ("postgres_password", "migrator_password", "runtime_password")}
    for role in ("migrator", "runtime"):
        values[role + "_database_url"] = f"postgres://gateway_{role}:{values[role + '_password']}@postgres:5432/gateway?sslmode=disable"
    for name, value in values.items():
        path = secret_dir / name
        with path.open("x") as output:
            output.write(value + "\n")
        # Compose file secrets are bind mounts: uid/gid/mode remapping is not
        # portable. The 0700 host directories protect these 0444 files; the
        # non-root container can read only the explicitly mounted file.
        path.chmod(0o444)
    config = (DEPLOY / "staging.env.example").read_text()
    with (state / "staging.env").open("x") as output:
        output.write(config)
    (state / "staging.env").chmod(0o600)
    print("Created private staging state. No credentials were printed.")


def settings(state):
    result = {}
    for raw in (state / "staging.env").read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        name, separator, value = line.partition("=")
        if not separator or not re.fullmatch(r"[A-Z][A-Z0-9_]*", name):
            raise ValueError("Invalid staging.env entry")
        result[name] = value.strip().strip('"').strip("'")
    return result


def compose(state, project, config):
    command = ["docker", "compose", "--project-name", project, "--env-file", str(state / "staging.env"), "-f", str(DEPLOY / "compose.yaml")]
    overlays = {"STAGING_OIDC_ENABLED": "oidc", "STAGING_OIDC_CONFIDENTIAL": "oidc-secret", "STAGING_OPENAI_ENABLED": "openai", "STAGING_ANTHROPIC_ENABLED": "anthropic"}
    if config.get("STAGING_OIDC_CONFIDENTIAL") == "1" and config.get("STAGING_OIDC_ENABLED") != "1":
        raise ValueError("Confidential OIDC requires STAGING_OIDC_ENABLED=1")
    for flag, file in overlays.items():
        if config.get(flag) == "1":
            command += ["-f", str(DEPLOY / f"compose.{file}.yaml")]
    return command


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["init", "build", "db", "migrate", "up", "status", "down", "provision-user", "backup", "restore-check", "local-ca", "verify"])
    parser.add_argument("--state-dir", type=Path, default=ROOT / ".local" / "staging")
    parser.add_argument("--project", default="omg-staging")
    parser.add_argument("--email", help="Verified email for the first explicitly provisioned platform administrator")
    parser.add_argument("--backup", type=Path, help="Custom-format pg_dump file for a disposable restore check")
    args = parser.parse_args()
    if not re.fullmatch(r"omg-staging(?:-[a-z0-9-]+)?", args.project):
        parser.error("Project must be omg-staging or start with omg-staging-; demo projects are never accepted")
    state = args.state_dir.absolute()
    if args.command == "init":
        create_state(state)
        return
    config = settings(state)
    dc = compose(state, args.project, config)
    # Absolute paths avoid Compose's file-relative secret path ambiguity.
    env = os.environ.copy()
    env["STAGING_SECRETS_DIR"] = str(state / "secrets")
    for name, value in config.items():
        env[name] = value
    # Do not let an env-file entry redirect the secret mounts outside this state.
    env["STAGING_SECRETS_DIR"] = str(state / "secrets")

    def run(arguments, **kwargs):
        return subprocess.run(dc + arguments, env=env, check=True, **kwargs)

    def sql(statement):
        return run(["exec", "-T", "postgres", "psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-U", "gateway_bootstrap", "-d", "gateway"], input=statement, text=True, capture_output=True).stdout

    def backup():
        folder = state / "backups"
        folder.mkdir(mode=0o700, exist_ok=True)
        path = folder / (datetime.now(timezone.utc).strftime("gateway-%Y%m%dT%H%M%SZ-") + secrets.token_hex(4) + ".dump")
        try:
            with path.open("xb") as output:
                path.chmod(0o600)
                run(["exec", "-T", "postgres", "pg_dump", "-U", "gateway_bootstrap", "-d", "gateway", "--format=custom", "--no-owner", "--no-privileges"], stdout=output)
        except BaseException:
            path.unlink(missing_ok=True)
            raise
        print(f"Backup written to {path}. Contains sensitive data; use encrypted off-host storage.")
        return path

    if args.command == "build":
        run(["build", "gateway"])
    elif args.command == "db":
        run(["up", "-d", "--wait", "--wait-timeout", "120", "postgres"])
    elif args.command == "migrate":
        run(["run", "--rm", "migrate"])
        run(["run", "--rm", "grant-runtime"])
    elif args.command == "up":
        # Never runs a migration implicitly. Missing schema/permissions fail
        # readiness. Use db -> migrate -> up in the documented release flow.
        run(["up", "-d", "--wait", "--wait-timeout", "120", "gateway", "ingress"])
    elif args.command == "status":
        run(["ps"])
    elif args.command == "down":
        run(["down"])  # deliberately no --volumes
    elif args.command == "provision-user":
        if not args.email:
            parser.error("provision-user requires --email")
        run(["run", "--rm", "migrate", "provision-user", "--email", args.email, "--platform-admin"])
    elif args.command == "backup":
        backup()
    elif args.command == "restore-check":
        source = args.backup or backup()
        # Always a new disposable DB; never restores over gateway or a live DB.
        database = "gateway_restorecheck_" + secrets.token_hex(8)
        sql(f'CREATE DATABASE "{database}" OWNER gateway_migrator;\nREVOKE ALL ON DATABASE "{database}" FROM PUBLIC;')
        try:
            with source.open("rb") as dump:
                run(["exec", "-T", "postgres", "sh", "-c", 'set +x; export PGPASSWORD="$(cat /run/secrets/migrator_password)"; exec pg_restore --exit-on-error --no-owner --no-privileges -h 127.0.0.1 -U gateway_migrator -d "$1"', "restore-check", database], stdin=dump)
            run(["exec", "-T", "postgres", "psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-U", "gateway_bootstrap", "-d", database, "-c", "SELECT count(*) AS restored_migrations FROM public._sqlx_migrations; SELECT count(*) AS retained_ledger_entries FROM public.monetary_ledger;"])
            print("Disposable restore succeeded. This does not certify application-level recovery or off-host durability.")
        finally:
            sql(f'DROP DATABASE "{database}" WITH (FORCE);')
    elif args.command == "local-ca":
        if config.get("GATEWAY_HOST", "localhost") != "localhost":
            raise ValueError("local-ca is only for the localhost rehearsal")
        container = run(["ps", "-q", "ingress"], capture_output=True, text=True).stdout.strip()
        if not container:
            raise ValueError("Staging ingress is not running")
        subprocess.run(["docker", "cp", container + ":/data/caddy/pki/authorities/local/root.crt", str(state / "local-ca.crt")], check=True)
        print("Copied only the public local CA certificate. Use curl --cacert; no system trust settings changed.")
    elif args.command == "verify":
        run(["exec", "-T", "gateway", "curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:8080/health/ready"])
        run(["exec", "-T", "gateway", "sh", "-c", "test \"$(id -u)\" = 10001 && ! test -w /app && ! command -v node && ! command -v cargo"])
        run(["exec", "-T", "postgres", "psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-U", "gateway_bootstrap", "-d", "gateway"], input=(DEPLOY / "verify-privileges.sql").read_text(), text=True)
        print("Readiness, runtime image restrictions, and database privilege assertions passed.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        # Avoid printing command/environment/output that might contain a secret.
        print(f"Staging operation failed ({type(error).__name__}). Check the preceding service diagnostics; existing state was not reset.", file=sys.stderr)
        sys.exit(1)

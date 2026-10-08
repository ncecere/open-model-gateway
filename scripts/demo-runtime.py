#!/usr/bin/env python3
"""Initialize a NEW loopback demo runtime role, without printing its password.

Not a reset/repair script. Refuses an existing role or private environment file.
"""
import os
from pathlib import Path
import re
import secrets
import subprocess

ROOT = Path(__file__).resolve().parents[1]
COMPOSE = ["docker", "compose", "-p", "omg-enterprise-demo", "-f", str(ROOT / "deploy/demo/compose.yaml")]
DESTINATION = ROOT / ".local/enterprise-rebuild/demo-runtime.env"


def sql(statement):
    result = subprocess.run(COMPOSE + ["exec", "-T", "postgres", "psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-At", "-U", "gateway", "-d", "gateway_enterprise_demo"], input=statement, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError("Demo runtime SQL failed; credentials and SQL error bodies are not printed")
    return result.stdout.strip()


def main():
    if not DESTINATION.parent.resolve().is_relative_to(ROOT.resolve()):
        raise RuntimeError("Runtime environment must remain inside this checkout")
    if DESTINATION.exists():
        raise RuntimeError("Existing runtime environment is preserved; initialization refused")
    if sql("SELECT current_database()") != "gateway_enterprise_demo":
        raise RuntimeError("Not the dedicated enterprise demo database")
    if sql("SELECT schema_family FROM installation WHERE singleton") != "enterprise_v1":
        raise RuntimeError("Not an initialized enterprise installation")
    if sql("SELECT EXISTS(SELECT FROM audit_events WHERE action='installation.bootstrap_demo')") != "t":
        raise RuntimeError("Initialize the fresh demo explicitly before runtime provisioning")
    if sql("SELECT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime')") != "f":
        raise RuntimeError("Existing runtime role is preserved; initialization refused")
    password = secrets.token_urlsafe(32)
    template = (ROOT / ".env.demo.example").read_text()
    environment, count = re.subn(r"^DATABASE_URL=.*$", f"DATABASE_URL=postgres://gateway_runtime:{password}@127.0.0.1:54349/gateway_enterprise_demo", template, flags=re.MULTILINE)
    if count != 1:
        raise RuntimeError("Unexpected demo environment template")
    environment = environment.replace("GATEWAY_ENV_FILE=.env.demo.example", "GATEWAY_ENV_FILE=.local/enterprise-rebuild/demo-runtime.env")
    DESTINATION.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(DESTINATION, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as stream:
        stream.write(environment)
    # Save before database mutation so a partial initialization cannot lose the
    # credential. On any failure, preserve this file and investigate explicitly;
    # this script never resets an existing role or overwrites an environment.
    # SQL reaches psql through stdin, never a command argument or printed log.
    sql(f"BEGIN; CREATE ROLE gateway_runtime LOGIN PASSWORD '{password}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime; REVOKE ALL ON DATABASE gateway_enterprise_demo FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE gateway_enterprise_demo TO gateway_runtime; COMMIT;")
    sql((ROOT / "deploy/staging/runtime-grants.sql").read_text())
    sql((ROOT / "deploy/staging/verify-privileges.sql").read_text())
    print("Fresh demo runtime role verified; private environment saved to .local/enterprise-rebuild/demo-runtime.env (0600). No credentials printed.")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError) as error:
        raise SystemExit(str(error)) from None

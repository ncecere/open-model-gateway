#!/usr/bin/env python3
"""Laptop multi-replica load-test runner (docs/operations.md "Capacity baseline").

TEST ONLY. Drives deploy/loadtest/compose.yaml under the Compose project
`omg-loadtest` (or `--project omg-loadtest-<name> --slot N` for a second,
isolated stack: own containers, volume, image tag, state directory, host ports
and private subnet, so concurrent stacks never collide): PostgreSQL 17, PgBouncer (transaction mode), up to three
gateway replicas, the mock upstream and the load generator. It never touches
the demo stack (port 3000, PostgreSQL 54349), staging, or any database not
named omg_loadtest*. Generated passwords and results stay in .local/loadtest/.

    python3 scripts/loadtest.py up            # build image (if missing), start PG/PgBouncer/mock
    python3 scripts/loadtest.py reset-db      # drop/create omg_loadtest, migrate, runtime grants
    python3 scripts/loadtest.py seed --users 5000 --keys 20000 --history 0
    python3 scripts/loadtest.py gateways --replicas 3 --via pgbouncer
    python3 scripts/loadtest.py run --label 3r-pgb --rate 100 --duration 45
    python3 scripts/loadtest.py report        # markdown table of every saved result
    python3 scripts/loadtest.py baseline      # the full matrix used for the documented baseline
    python3 scripts/loadtest.py down          # remove containers, network and the data volume
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import secrets
import subprocess
import sys
import time
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "loadtest"
PROJECT = "omg-loadtest"
IMAGE = "omg-loadtest:local"
PROJECT_NAME = re.compile(r"^omg-loadtest(-[a-z0-9]{1,20})?$")
# Per-stack isolation (see configure): host ports and the private subnet move with the slot.
SLOT = 0
DEFAULT_DB = "omg_loadtest"
DB_NAME = re.compile(r"^omg_loadtest[a-z0-9_]{0,40}$")
GATEWAYS = ("gateway-1", "gateway-2", "gateway-3")
# Never used by this stack; refused if a caller tries to point at them.
FORBIDDEN_PORTS = {3000, 54339, 54349}


def configure(project=PROJECT, slot=0, image=None):
    """Select the Compose project and its isolation slot. The default project
    keeps slot 0 (ports 54369, 18301-18303; subnet 10.213.47.0/24; image
    omg-loadtest:local; .local/loadtest). Another project needs its own slot
    1-9: PostgreSQL 54369+100*slot, gateways 18301+10*slot.., subnet
    10.213.(47+slot).0/24, image <project>:local, state .local/<project>."""
    global PROJECT, IMAGE, SLOT
    if not PROJECT_NAME.match(project):
        raise SystemExit(f"refusing project {project!r}: only omg-loadtest or omg-loadtest-<name>")
    if not 0 <= slot <= 9 or (slot == 0) != (project == "omg-loadtest"):
        raise SystemExit("slot 0 is the default omg-loadtest project; other projects need --slot 1-9")
    PROJECT, SLOT = project, slot
    IMAGE = (image or os.environ.get("OMG_LOADTEST_IMAGE")
             or ("omg-loadtest:local" if slot == 0 else f"{project}:local"))


def slot_env():
    """Compose variables of the selected slot (host ports, subnet, image)."""
    ports = {"OMG_LOADTEST_POSTGRES_PORT": 54369 + 100 * SLOT}
    ports.update({f"OMG_LOADTEST_GATEWAY_PORT_{i}": 18300 + 10 * SLOT + i for i in (1, 2, 3)})
    if FORBIDDEN_PORTS & set(ports.values()):
        raise SystemExit("refusing a forbidden host port")
    env = {k: str(v) for k, v in ports.items()}
    env.update({"OMG_LOADTEST_SUBNET": f"10.213.{47 + SLOT}", "OMG_LOADTEST_IMAGE": IMAGE})
    return env


def state_dir():
    default = ROOT / ".local" / ("loadtest" if SLOT == 0 else PROJECT)
    return Path(os.environ.get("OMG_LOADTEST_STATE", default))


def check_db_name(name):
    if not DB_NAME.match(name):
        raise SystemExit(f"refusing database {name!r}: only throwaway omg_loadtest* names")
    return name


def create_state(state):
    """Generate passwords once (0600 files in a 0700 directory). Idempotent."""
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    env_file = state / "stack.env"
    if not env_file.exists():
        values = {name: secrets.token_hex(24) for name in (
            "OMG_LOADTEST_ADMIN_PASSWORD", "OMG_LOADTEST_MIGRATOR_PASSWORD", "OMG_LOADTEST_RUNTIME_PASSWORD")}
        with env_file.open("x") as output:
            output.write("".join(f"{k}={v}\n" for k, v in values.items()))
        env_file.chmod(0o600)
    env = read_env(env_file)
    # PgBouncer reads plaintext passwords so it can do SCRAM to the server.
    # The non-root PgBouncer container needs to read it; the directory is 0700.
    # Filled with the server's SCRAM secrets by `up` (see userlist_from_server).
    if not (state / "userlist.txt").exists():
        write_if_changed(state / "userlist.txt", "")
    write_if_changed(state / "pgbouncer-noprepared.ini", (DEPLOY / "pgbouncer.ini").read_text().replace(
        "max_prepared_statements = 200", "max_prepared_statements = 0"))
    (state / "results").mkdir(mode=0o700, exist_ok=True)
    return env


def write_if_changed(path, text, mode=0o644):
    """Rewrite only on change: a running PgBouncer bind-mounts these files and
    must never observe a truncated file."""
    if not path.exists() or path.read_text() != text:
        temporary = path.with_suffix(".tmp")
        temporary.write_text(text)
        temporary.chmod(mode)
        temporary.replace(path)


def read_env(path):
    values = {}
    for line in path.read_text().splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip()
    return values


def compose_command(state, *args, profiles=()):
    command = ["docker", "compose", "-p", PROJECT, "-f", str(DEPLOY / "compose.yaml"),
               "--env-file", str(state / "stack.env")]
    for profile in profiles:
        command += ["--profile", profile]
    return command + list(args)


class Stack:
    def __init__(self, state=None, db=DEFAULT_DB, extra_env=None):
        self.state = state or state_dir()
        self.db = check_db_name(db)
        self.env = create_state(self.state)
        self.extra_env = dict(extra_env or {})

    def process_env(self):
        env = dict(os.environ)
        env.update(slot_env())
        env.update({"OMG_LOADTEST_STATE_DIR": str(self.state), "OMG_LOADTEST_DB": self.db})
        env.update(self.extra_env)
        return env

    def compose(self, *args, profiles=(), capture=False, check=True, quiet=False):
        command = compose_command(self.state, *args, profiles=profiles)
        if not quiet:
            print("+", " ".join(a if "postgres://" not in a else "<database-url>" for a in command[8:]), file=sys.stderr)
        return subprocess.run(command, env=self.process_env(), cwd=ROOT, check=check, text=True,
                              stdout=subprocess.PIPE if capture else None,
                              stderr=subprocess.PIPE if capture else None)

    def url(self, role, host="postgres", port=5432, db=None):
        password = self.env[f"OMG_LOADTEST_{role.upper()}_PASSWORD"]
        return f"postgres://gateway_{role}:{password}@{host}:{port}/{db or self.db}"

    def psql(self, sql, db="postgres", user="loadtest_admin", capture=True, variables=()):
        """psql inside the postgres container over the local socket."""
        command = ["exec", "-T", "postgres", "psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-At",
                   "-U", user, "-d", db]
        for name, value in variables:
            command += ["-v", f"{name}={value}"]
        # SQL on stdin, so psql variables (:'name') are interpolated (not with -c).
        result = subprocess.run(compose_command(self.state, *command, "-f", "-"), env=self.process_env(),
                                cwd=ROOT, check=True, text=True, input=sql + "\n", capture_output=capture)
        return result.stdout.strip() if capture else ""


def cmd_up(stack, args):
    disk_guard()
    have_image = subprocess.run(["docker", "image", "inspect", IMAGE], capture_output=True).returncode == 0
    if args.build or not have_image:
        subprocess.run(["docker", "build", "-f", str(DEPLOY / "Dockerfile"), "-t", IMAGE, "."], cwd=ROOT, check=True)
    stack.compose("up", "-d", "--wait", "postgres")
    userlist_from_server(stack)
    stack.compose("up", "-d", "--wait", "postgres", "pgbouncer", "mock-upstream")


def userlist_from_server(stack):
    """PgBouncer userlist with the server's own SCRAM secrets. With plaintext
    passwords PgBouncer authenticates clients against a secret with its own
    salt and then cannot reuse the client's SCRAM keys for the server login
    (observed: intermittent "server login failed: password authentication
    failed"). Identical secrets make SCRAM pass-through work."""
    rows = stack.psql("SELECT rolname || ' ' || rolpassword FROM pg_authid "
                      "WHERE rolname IN ('gateway_runtime','gateway_migrator') ORDER BY 1")
    lines = []
    for line in rows.splitlines():
        role, secret = line.split(" ", 1)
        if not secret.startswith("SCRAM-SHA-256$"):
            raise SystemExit(f"{role} has no SCRAM secret")
        lines.append(f'"{role}" "{secret}"\n')
    if len(lines) != 2:
        raise SystemExit("load-test roles are missing; recreate the stack (down, up)")
    write_if_changed(stack.state / "userlist.txt", "".join(lines))


def cmd_reset_db(stack, args):
    db = stack.db
    stack.compose("stop", *GATEWAYS, check=False)
    stack.psql(f'DROP DATABASE IF EXISTS "{db}" WITH (FORCE)')
    stack.psql(f'CREATE DATABASE "{db}" OWNER gateway_migrator')
    stack.psql(";".join([
        f'REVOKE ALL ON DATABASE "{db}" FROM PUBLIC',
        f'GRANT CONNECT, TEMPORARY ON DATABASE "{db}" TO gateway_migrator',
        f'GRANT CONNECT ON DATABASE "{db}" TO gateway_runtime',
        "ALTER SCHEMA public OWNER TO gateway_migrator",
        "REVOKE ALL ON SCHEMA public FROM PUBLIC",
        "GRANT USAGE ON SCHEMA public TO gateway_runtime",
        f'ALTER ROLE gateway_runtime IN DATABASE "{db}" SET search_path = pg_catalog, public',
        "ALTER DEFAULT PRIVILEGES FOR ROLE gateway_migrator REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC",
    ]), db=db)
    # Migrations take a session advisory lock: always direct, never via PgBouncer.
    stack.compose("run", "--rm", "-e", f"DATABASE_URL={stack.url('migrator')}", "gateway-cli", "migrate",
                  profiles=("tools",))
    subprocess.run(compose_command(stack.state, "exec", "-T", "postgres", "psql", "--no-psqlrc", "-v",
                                   "ON_ERROR_STOP=1", "-q", "-U", "gateway_migrator", "-d", db, "-f",
                                   "/opt/loadtest/runtime-grants.sql"),
                   env=stack.process_env(), cwd=ROOT, check=True)


def cmd_seed(stack, args):
    disk_guard()
    command = ["run", "--rm", "loadgen", "seed", "--database-url", stack.url("migrator"),
               "--users", str(args.users), "--shared-workspaces", str(args.shared), "--keys", str(args.keys),
               "--history", str(args.history), "--history-unknown", str(args.history_unknown),
               "--history-days", str(args.history_days)]
    result = stack.compose(*command, profiles=("tools",), capture=True)
    sys.stderr.write(result.stderr[-2000:])
    report = json.loads(result.stdout)
    save(stack, f"seed-{report['reservations']}", report)
    print(json.dumps(report, indent=2))
    return report


def cmd_gateways(stack, args):
    via_direct = args.via == "direct"
    stack.extra_env.update({
        "OMG_LOADTEST_DB_HOST": "postgres" if via_direct else "pgbouncer",
        "OMG_LOADTEST_DB_PORT": "5432" if via_direct else "6432",
        "OMG_LOADTEST_POOL": str(args.pool),
        "OMG_LOADTEST_MAX_CONCURRENT": str(args.max_concurrent),
        "OMG_LOADTEST_ADMISSION_MODE": args.admission_mode,
        "OMG_LOADTEST_CONFIG_CACHE": getattr(args, "config_cache", "on"),
    })
    if args.prepared == "off":
        stack.extra_env["OMG_LOADTEST_PGBOUNCER_INI"] = str(stack.state / "pgbouncer-noprepared.ini")
    stack.compose("up", "-d", "--force-recreate", "--wait", "pgbouncer")
    active = GATEWAYS[:args.replicas]
    if GATEWAYS[args.replicas:]:
        # Never call `stop` without services: that would stop the whole stack.
        stack.compose("stop", *GATEWAYS[args.replicas:], check=False)
    stack.compose("up", "-d", "--force-recreate", "--wait", *active)
    settings = {"replicas": args.replicas, "via": args.via, "pool": args.pool,
                "max_concurrent": args.max_concurrent, "pgbouncer_prepared": args.prepared,
                "admission_mode": args.admission_mode,
                "config_cache": getattr(args, "config_cache", "on"),
                "commit_delay": os.environ.get("OMG_LOADTEST_COMMIT_DELAY", "0")}
    (stack.state / "gateways.json").write_text(json.dumps(settings))
    return settings


def active_settings(stack):
    path = stack.state / "gateways.json"
    if not path.exists():
        raise SystemExit("start gateways first: loadtest.py gateways --replicas N")
    return json.loads(path.read_text())


def cmd_run(stack, args):
    disk_guard()
    settings = active_settings(stack)
    replicas = GATEWAYS[:settings["replicas"]]
    stack.psql("SELECT pg_stat_statements_reset()")
    command = ["run", "--rm", "loadgen", "run",
               "--target", ",".join(f"http://{g}:8080" for g in replicas),
               "--metrics-url", ",".join(f"http://{g}:9464/metrics" for g in replicas),
               "--mock-url", "http://mock-upstream:8000",
               "--database-url", stack.url("runtime"),
               "--rate", str(args.rate), "--duration", str(args.duration), "--warmup", str(args.warmup),
               "--stream-ratio", str(args.stream_ratio), "--keys", str(args.keys),
               "--key-offset", str(getattr(args, "key_offset", 0)),
               "--max-tokens", str(args.max_tokens), "--timeout", str(args.timeout), "--label", args.label,
               "--readers", str(getattr(args, "readers", 0)), "--reader-days", str(getattr(args, "reader_days", 7))]
    started = time.monotonic()
    result = stack.compose(*command, profiles=("tools",), capture=True, check=False)
    sys.stderr.write(result.stderr[-4000:])
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError:
        raise SystemExit(f"loadgen failed (exit {result.returncode})")
    report["stack"] = dict(settings, **stack_facts(stack))
    # Before `budget verify`, whose full scan would otherwise top the list.
    report["top_queries"] = top_queries(stack, args.top)
    report["pgbouncer"] = pgbouncer_stats(stack) if settings.get("via") == "pgbouncer" else None
    report["budget_verify"] = budget_verify(stack)
    report["wall_seconds"] = round(time.monotonic() - started, 1)
    path = save(stack, args.label, report)
    print(path)
    problems = list(report.get("violations", []))
    if not report["budget_verify"].get("consistent"):
        problems.append("budget verify reported mismatches")
    if problems:
        raise SystemExit(f"INVARIANT VIOLATIONS: {problems}")
    return report


def budget_verify(stack):
    started = time.monotonic()
    result = stack.compose("run", "--rm", "-e", f"DATABASE_URL={stack.url('runtime')}", "gateway-cli",
                           "budget", "verify", profiles=("tools",), capture=True, check=False, quiet=True)
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError:
        return {"consistent": False, "error": result.stderr[-500:]}
    report["consistent"] = result.returncode == 0 and report.get("mismatch_count") == 0
    report["seconds"] = round(time.monotonic() - started, 2)
    return report


def top_queries(stack, limit):
    sql = ("SELECT coalesce(json_agg(q),'[]') FROM (SELECT left(regexp_replace(query,'\\s+',' ','g'),160) AS query,"
           " calls, round(total_exec_time::numeric,1) AS total_ms, round(mean_exec_time::numeric,3) AS mean_ms,"
           " rows, toplevel FROM pg_stat_statements WHERE dbid=(SELECT oid FROM pg_database WHERE datname=:'db')"
           f" ORDER BY total_exec_time DESC LIMIT {int(limit)}) q")
    try:
        return json.loads(stack.psql(sql, variables=(("db", stack.db),)))
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return []


def pgbouncer_stats(stack):
    """SHOW STATS for the load-test database (PgBouncer admin console, stats_users)."""
    password = stack.env["OMG_LOADTEST_MIGRATOR_PASSWORD"]
    command = compose_command(stack.state, "exec", "-T", "-e", f"PGPASSWORD={password}", "postgres", "psql",
                              "--no-psqlrc", "-h", "pgbouncer", "-p", "6432", "-U", "gateway_migrator",
                              "-d", "pgbouncer", "--csv", "-c", "SHOW STATS")
    try:
        lines = subprocess.run(command, env=stack.process_env(), cwd=ROOT, check=True, text=True,
                               capture_output=True).stdout.splitlines()
    except subprocess.CalledProcessError:
        return None
    header = lines[0].split(",") if lines else []
    for line in lines[1:]:
        values = dict(zip(header, line.split(",")))
        if values.get("database") == stack.db:
            return values
    return None


def stack_facts(stack):
    facts = {"db": stack.db, "image": IMAGE}
    try:
        info = subprocess.run(["docker", "info", "--format", "{{.NCPU}} {{.MemTotal}}"], capture_output=True,
                              text=True, check=True).stdout.split()
        facts["docker_cpus"], facts["docker_mem_gib"] = int(info[0]), round(int(info[1]) / 2**30, 1)
    except (subprocess.CalledProcessError, IndexError, ValueError):
        pass
    facts["host"] = f"{platform.system()} {platform.machine()}"
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True)
    facts["git"] = commit.stdout.strip()
    try:
        facts["db_size_bytes"] = int(stack.psql(f"SELECT pg_database_size('{stack.db}')"))
    except (subprocess.CalledProcessError, ValueError):
        pass
    try:
        # Cumulative since the database was created (reset-db): compare runs.
        facts["db_deadlocks"] = int(stack.psql(
            f"SELECT deadlocks FROM pg_stat_database WHERE datname='{stack.db}'"))
    except (subprocess.CalledProcessError, ValueError):
        pass
    return facts


def save(stack, label, report):
    safe = re.sub(r"[^A-Za-z0-9_.-]", "_", label)[:80]
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = stack.state / "results" / f"{stamp}-{safe}.json"
    path.write_text(json.dumps(report, indent=2))
    path.chmod(0o600)
    return path


def phase(report, family, series):
    summary = ((report.get("gateway") or {}).get(family) or {}).get(series) or {}
    return summary


def row(report):
    s = report.get("stack", {})
    lat = report["latency_ms"]["all"]
    overhead = report.get("gateway_overhead_ms") or {}
    adm = phase(report, "admission", "phase=total,outcome=admitted")
    lock = phase(report, "admission", "phase=locks,outcome=admitted")
    settle = phase(report, "settlement", "phase=total,outcome=settled")
    statuses = ", ".join(f"{k}:{v}" for k, v in sorted(report["totals"]["by_status"].items()))
    verified = "ok" if not report.get("violations") and (report.get("budget_verify") or {}).get("consistent") else "FAIL"
    f = lambda d, k: f"{d.get(k, 0):.1f}" if d else "-"
    line = (f"| {report['config'].get('label') or ''} | {s.get('replicas', '?')} | {s.get('via', '?')} | "
            f"{report['config']['rate']:.0f} | {report['throughput_ok_per_s']:.1f} | "
            f"{f(lat, 'p50_ms')} / {f(lat, 'p95_ms')} / {f(lat, 'p99_ms')} | "
            f"{f(overhead, 'p50_ms')} / {f(overhead, 'p99_ms')} | "
            f"{f(adm, 'p50_ms')} / {f(adm, 'p99_ms')} | {f(lock, 'p50_ms')} / {f(lock, 'p99_ms')} | "
            f"{f(settle, 'p50_ms')} / {f(settle, 'p99_ms')} | {statuses} | {verified} |")
    return line


def reader_cells(report):
    """Readers column: count, ok reads/s, ok latency p50/p99 and non-200 statuses."""
    r = report.get("reader")
    if not r:
        return "-"
    bad = ", ".join(f"{k}:{v}" for k, v in sorted(r.get("by_status", {}).items()) if k != "200")
    lat = r.get("latency") or {}
    return (f"{r['readers']}: {r.get('ok_per_s', 0):.1f}/s, {lat.get('p50_ms', 0):.0f} / {lat.get('p99_ms', 0):.0f} ms"
            + (f" ({bad})" if bad else ""))


HEADER = ("| run | replicas | DB path | offered/s | ok/s | client p50 / p95 / p99 ms | overhead p50 / p99 ms | "
          "admission p50 / p99 ms | lock wait p50 / p99 ms | settlement p50 / p99 ms | statuses | invariants |\n"
          "|---|---|---|---|---|---|---|---|---|---|---|---|")
READER_HEADER = (HEADER.split("\n")[0] + " readers: ok/s, p50 / p99 ms |\n" + HEADER.split("\n")[1] + "---|")


def render(reports):
    if any(r.get("reader") for r in reports):
        return "\n".join([READER_HEADER] + [f"{row(r)} {reader_cells(r)} |" for r in reports])
    return "\n".join([HEADER] + [row(r) for r in reports])


def cmd_report(stack, args):
    reports = []
    for path in sorted((stack.state / "results").glob("*.json")):
        data = json.loads(path.read_text())
        if "latency_ms" in data and (not args.match or args.match in path.name):
            reports.append(data)
    text = render(reports)
    if args.out:
        Path(args.out).write_text(text + "\n")
    print(text)


def cmd_down(stack, args):
    stack.compose("--profile", "tools", "down", "-v", "--remove-orphans", check=False)
    if not args.keep_image:
        subprocess.run(["docker", "image", "rm", IMAGE], check=False)


def disk_guard(minimum_gib=20):
    stats = os.statvfs("/")
    free = stats.f_bavail * stats.f_frsize / 2**30
    if free < minimum_gib:
        raise SystemExit(f"only {free:.1f} GiB free on /; refusing (need {minimum_gib} GiB)")


def cmd_baseline(stack, args):
    """The documented matrix: {1, 3} replicas x {pgbouncer, direct} x {empty, seeded history},
    each at a sub-saturation rate and an overload rate."""
    rates = [float(r) for r in args.rates.split(",")]
    for history in [int(h) for h in args.histories.split(",")]:
        stack.db = check_db_name(DEFAULT_DB)
        cmd_reset_db(stack, args)
        seed_args = argparse.Namespace(users=args.users, shared=args.shared, keys=args.seed_keys,
                                       history=history, history_unknown=min(args.history_unknown, history),
                                       history_days=120)
        cmd_seed(stack, seed_args)
        tag = f"seeded{history // 1000}k" if history else "empty"
        for replicas in (1, 3):
            for via in ("pgbouncer", "direct"):
                cmd_gateways(stack, argparse.Namespace(replicas=replicas, via=via, pool=args.pool,
                                                       max_concurrent=args.max_concurrent, prepared="on",
                                                       admission_mode="scoped", config_cache="on"))
                for rate in rates:
                    run_args = argparse.Namespace(
                        label=f"{tag}-{replicas}r-{via}-{rate:g}", rate=rate, duration=args.duration,
                        warmup=args.warmup, stream_ratio=0.5, keys=args.keys, max_tokens=16, timeout=60,
                        top=10)
                    try:
                        cmd_run(stack, run_args)
                    except SystemExit as failure:
                        print(f"run {run_args.label} failed: {failure}", file=sys.stderr)
                        if not args.keep_going:
                            raise


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--db", default=DEFAULT_DB, help="throwaway database name (omg_loadtest*)")
    parser.add_argument("--project", default=os.environ.get("OMG_LOADTEST_PROJECT", PROJECT),
                        help="Compose project: omg-loadtest (default) or omg-loadtest-<name> with --slot")
    parser.add_argument("--slot", type=int, default=int(os.environ.get("OMG_LOADTEST_SLOT", "0")),
                        help="isolation slot 1-9 for a non-default project (ports, subnet, image, state)")
    sub = parser.add_subparsers(dest="command", required=True)
    up = sub.add_parser("up")
    up.add_argument("--build", action="store_true", help="rebuild the image even if it exists")
    sub.add_parser("reset-db")
    seed = sub.add_parser("seed")
    seed.add_argument("--users", type=int, default=5000)
    seed.add_argument("--shared", type=int, default=2000)
    seed.add_argument("--keys", type=int, default=20000)
    seed.add_argument("--history", type=int, default=0)
    seed.add_argument("--history-unknown", type=int, default=0)
    seed.add_argument("--history-days", type=int, default=120)
    gw = sub.add_parser("gateways")
    gw.add_argument("--replicas", type=int, choices=(1, 2, 3), default=3)
    gw.add_argument("--via", choices=("pgbouncer", "direct"), default="pgbouncer")
    gw.add_argument("--pool", type=int, default=10)
    gw.add_argument("--max-concurrent", type=int, default=128)
    gw.add_argument("--admission-mode", choices=("scoped", "global"), default="scoped",
                    help="GATEWAY_ADMISSION_MODE of the replicas (global: the former installation lock)")
    gw.add_argument("--config-cache", choices=("on", "off"), default="on",
                    help="GATEWAY_CONFIG_CACHE of the replicas (off: every pre-admission read is live)")
    gw.add_argument("--prepared", choices=("on", "off"), default="on",
                    help="PgBouncer max_prepared_statements 200 (on) or 0 (off, decision gate D4 check)")
    run = sub.add_parser("run")
    run.add_argument("--label", required=True)
    run.add_argument("--rate", type=float, required=True)
    run.add_argument("--duration", type=float, default=45)
    run.add_argument("--warmup", type=float, default=5)
    run.add_argument("--stream-ratio", type=float, default=0.5)
    run.add_argument("--keys", type=int, default=2000)
    run.add_argument("--key-offset", type=int, default=0,
                     help="first seeded key index of the hot set (hot-workspace runs: seed --shared 1, "
                          "then the member/service keys from index 2 x users)")
    run.add_argument("--max-tokens", type=int, default=16)
    run.add_argument("--timeout", type=float, default=60)
    run.add_argument("--top", type=int, default=10)
    run.add_argument("--readers", type=int, default=0,
                     help="concurrent closed-loop report/usage/logs/me readers (seeded reader session)")
    run.add_argument("--reader-days", type=int, default=7, help="report window of the readers in days")
    report = sub.add_parser("report")
    report.add_argument("--match", default="")
    report.add_argument("--out")
    down = sub.add_parser("down")
    down.add_argument("--keep-image", action="store_true")
    base = sub.add_parser("baseline")
    base.add_argument("--rates", default="60,250")
    base.add_argument("--duration", type=float, default=45)
    base.add_argument("--warmup", type=float, default=5)
    base.add_argument("--users", type=int, default=5000)
    base.add_argument("--shared", type=int, default=2000)
    base.add_argument("--seed-keys", type=int, default=20000)
    base.add_argument("--keys", type=int, default=2000)
    base.add_argument("--histories", default="0,2000000",
                      help="settled history attempts per pass; 0 is the empty-history pass")
    base.add_argument("--history-unknown", type=int, default=1000)
    base.add_argument("--pool", type=int, default=10)
    base.add_argument("--max-concurrent", type=int, default=128)
    base.add_argument("--keep-going", action="store_true")
    args = parser.parse_args(argv)
    configure(args.project, args.slot)
    stack = Stack(db=args.db)
    {"up": cmd_up, "reset-db": cmd_reset_db, "seed": cmd_seed, "gateways": cmd_gateways, "run": cmd_run,
     "report": cmd_report, "down": cmd_down, "baseline": cmd_baseline}[args.command](stack, args)


if __name__ == "__main__":
    main()

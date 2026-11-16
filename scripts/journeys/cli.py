"""Command line: `python3 -I scripts/journeys run --base URL [--issuer URL] [--only NAME ...] [--summary PATH]`."""
import argparse
import ipaddress
import json
import os
import random
import sys
import urllib.parse

from .client import Http, Session
from .context import Env
from .recorder import render_table, summary_document
from .registry import load_all
from .runner import run_all

LOOPBACK_NAMES = {"localhost"}


def is_loopback(host):
    if host is None:
        return False
    if host.lower() in LOOPBACK_NAMES:
        return True
    try:
        return ipaddress.ip_address(host.strip("[]")).is_loopback
    except ValueError:
        return False


def origin_of(url):
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https") or not parts.hostname or parts.path not in ("", "/") or parts.query or parts.fragment or parts.username:
        raise ValueError("expected an http(s) origin without path, query or credentials")
    return parts.hostname, parts.port, f"{parts.scheme}://{parts.netloc}"


def build_parser():
    parser = argparse.ArgumentParser(prog="journeys", description="Real-path journeys against a forwarded demo environment (Spark or the local demo).")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="run journeys")
    run.add_argument("--base", required=True, help="gateway origin, e.g. http://127.0.0.1:3000")
    run.add_argument("--issuer", help="demo issuer origin, default: the one the gateway redirects to")
    run.add_argument("--only", nargs="+", metavar="NAME", help="run only these journeys, in this order")
    run.add_argument("--shuffle", action="store_true", help="run in a random order (proves order independence)")
    run.add_argument("--seed", type=int, help="seed for --shuffle")
    run.add_argument("--summary", metavar="PATH", help="write the JSON summary here")
    run.add_argument("--allow-remote", action="store_true", help="allow a base that is not loopback (never production)")
    sub.add_parser("list", help="list journey names")
    return parser


def preflight(env):
    """The target must be a ready gateway signing in through a demo issuer. Returns an error phrase or None."""
    probe = Session(env.http, "preflight")
    try:
        ready = probe.get("/health/ready", cookies=False)
        login = probe.get("/api/v1/auth/login", cookies=False)
    except OSError:
        return "the gateway is not reachable"
    if ready.status != 200:
        return "the gateway is not ready"
    location = login.header("location")
    if login.status not in (302, 303) or not location:
        return "the gateway offers no OIDC sign-in"
    parts = urllib.parse.urlsplit(location)
    if env.issuer_http is None and not is_loopback(parts.hostname):
        return "the sign-in issuer is not on loopback; this is not a demo environment"
    return None


def main(argv=None):
    args = build_parser().parse_args(argv)
    journeys = load_all()
    if args.command == "list":
        print("\n".join(sorted(journeys)))
        return 0
    try:
        host, _, base = origin_of(args.base)
        issuer_origin = origin_of(args.issuer)[2] if args.issuer else None
        issuer_host = origin_of(args.issuer)[0] if args.issuer else None
    except ValueError as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 2
    if not args.allow_remote and not (is_loopback(host) and (issuer_host is None or is_loopback(issuer_host))):
        print("refused: --base and --issuer must be loopback (127.0.0.1, ::1, localhost) unless --allow-remote is given.", file=sys.stderr)
        return 2
    names = args.only if args.only else sorted(journeys)
    unknown = [n for n in names if n not in journeys]
    if unknown or len(set(names)) != len(names):
        print("refused: unknown or repeated journey name; see `list`.", file=sys.stderr)
        return 2
    if args.shuffle:
        random.Random(args.seed).shuffle(names)
    env = Env(Http(base), Http(issuer_origin) if issuer_origin else None)
    label = urllib.parse.urlsplit(base).netloc
    print(f"journeys: about to modify the environment at {label} ({len(names)} journey{"" if len(names) == 1 else "s"}, run {env.run_id}).")
    print("They create throwaway workspaces, keys, invitations, requests and access reviews named journeys-<run>-*, and remove or disable them afterwards.")
    problem = preflight(env)
    if problem:
        print(f"refused: {problem}.", file=sys.stderr)
        return 2
    results = run_all(env, journeys, names)
    print(render_table(results))
    if args.summary:
        document = summary_document(label, results)
        fd = os.open(args.summary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        with os.fdopen(fd, "w") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")
        print(f"summary written to {args.summary}")
    return 0 if all(r.ok for r in results) else 1

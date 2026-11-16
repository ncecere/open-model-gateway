"""Command line: `python3 -I scripts/journeys run --base URL [--only NAME ...] [--summary PATH]`."""
import argparse
import datetime
import errno
import ipaddress
import json
import os
import random
import re
import stat
import sys
import time
import urllib.parse

from .client import Http, Session
from .context import Env
from .oidc import DEMO_PROOF, callback_origin, split_redirect
from .recorder import exception_type, render_table, summary_document
from .registry import load_all
from .runner import run_all

LOOPBACK_NAMES = {"localhost"}
LABEL = re.compile(r"[a-z0-9][a-z0-9.\-]{0,39}")
DEFAULT_TIMEOUT = 30
DEFAULT_DEADLINE = 1200


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
    """(host, port, origin) of an http(s) origin; refuses credentials, paths, queries and fragments."""
    parts = urllib.parse.urlsplit(url)
    if "@" in parts.netloc or parts.scheme not in ("http", "https") or not parts.hostname or parts.path not in ("", "/") or parts.query or parts.fragment:
        raise ValueError("expected an http(s) origin without credentials, path, query or fragment")
    return parts.hostname, parts.port, f"{parts.scheme}://{parts.netloc}"


def build_parser():
    parser = argparse.ArgumentParser(prog="journeys", description="Real-path journeys against a forwarded demo environment (Spark or the local demo).")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="run journeys (one run per environment at a time)")
    run.add_argument("--base", required=True, help="the gateway's public origin, e.g. http://127.0.0.1:3000")
    run.add_argument("--only", nargs="+", metavar="NAME", help="run only these journeys, in this order")
    run.add_argument("--shuffle", action="store_true", help="run in a random order (proves order independence)")
    run.add_argument("--seed", type=int, help="seed for --shuffle (default: a fresh one, printed and recorded)")
    run.add_argument("--summary", metavar="PATH", help="write the JSON summary here (mode 0600; a symlink is refused)")
    run.add_argument("--label", help="a short name for this run in the summary, e.g. v0.18.0-rc1")
    run.add_argument("--request-timeout", type=float, default=DEFAULT_TIMEOUT, metavar="SECONDS", help="per request (default %(default)s)")
    run.add_argument("--deadline", type=float, default=DEFAULT_DEADLINE, metavar="SECONDS", help="total run time before journeys stop starting (default %(default)s)")
    run.add_argument("--allow-remote", action="store_true", help="allow a non-loopback base (the gateway must still sign in through the demo issuer; never production)")
    sub.add_parser("list", help="list journey names")
    return parser


def preflight(env, allow_remote):
    """The target must be a ready gateway whose sign-in is the passwordless demo issuer.

    Returns an error phrase, or None after setting env.issuer_origin."""
    probe = Session(env.http, "preflight")
    try:
        ready = probe.get("/health/ready", cookies=False)
        login = probe.get("/api/v1/auth/login", cookies=False)
        if ready.status != 200:
            return "the gateway is not ready"
        location = login.header("location")
        if login.status not in (302, 303) or not location:
            return "the gateway offers no OIDC sign-in"
        issuer, target = split_redirect(location)
        host = urllib.parse.urlsplit(issuer).hostname
        if not allow_remote and not is_loopback(host):
            return "the sign-in issuer is not on loopback; this is not a demo environment"
        public = callback_origin(location)
        if public is not None and public != env.http.origin:
            return f"use {public}, the gateway's public origin, as --base"
        page = Http(issuer, env.http.timeout).request("GET", target)
        if page.status != 200 or DEMO_PROOF.encode() not in page.body:
            return "the sign-in issuer is not the passwordless demo issuer; this suite never signs in through a real identity provider"
    except Exception as exc:  # noqa: BLE001 - only an allow-listed type name is printed
        return f"the gateway could not be checked ({exception_type(exc)})"
    env.issuer_origin = issuer
    return None


def write_summary(path, document):
    """Write the summary with mode 0600, refusing a symlink and never truncating through one."""
    flags = os.O_WRONLY | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise OSError(errno.EINVAL, "not a regular file")
        os.ftruncate(fd, 0)
        with os.fdopen(fd, "w") as handle:
            fd = None
            json.dump(document, handle, indent=2)
            handle.write("\n")
    finally:
        if fd is not None:
            os.close(fd)


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    journeys = load_all()
    if args.command == "list":
        print("\n".join(sorted(journeys)))
        return 0
    if args.seed is not None and not args.shuffle:
        parser.error("--seed needs --shuffle")
    if args.label is not None and not LABEL.fullmatch(args.label):
        parser.error("--label may use lower-case letters, digits, '.' and '-' (40 characters at most)")
    if args.request_timeout <= 0 or args.deadline <= 0:
        parser.error("--request-timeout and --deadline must be positive")
    try:
        host, _, base = origin_of(args.base)
    except ValueError as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 2
    if not args.allow_remote and not is_loopback(host):
        print("refused: --base must be loopback (127.0.0.1, ::1, localhost) unless --allow-remote is given.", file=sys.stderr)
        return 2
    names = list(args.only) if args.only else sorted(journeys)
    unknown = [n for n in names if n not in journeys]
    if unknown or len(set(names)) != len(names):
        print("refused: unknown or repeated journey name; see `list`.", file=sys.stderr)
        return 2
    seed = None
    if args.shuffle:
        seed = args.seed if args.seed is not None else random.SystemRandom().randrange(1 << 31)
        random.Random(seed).shuffle(names)
    env = Env(Http(base, args.request_timeout))
    label = urllib.parse.urlsplit(base).netloc
    print(f"journeys: about to modify the environment at {label} ({len(names)} journey{'' if len(names) == 1 else 's'}, run {env.run_id}).")
    print("They create throwaway workspaces, keys, invitations, requests and access reviews named journeys-<run>-*, and remove or disable them afterwards. Run one suite per environment at a time.")
    if seed is not None:
        print(f"order: shuffled with seed {seed}")
    problem = preflight(env, args.allow_remote)
    if problem:
        print(f"refused: {problem}.", file=sys.stderr)
        return 2
    started = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat()
    deadline = time.monotonic() + args.deadline
    results, status = [], 0
    try:
        results = run_all(env, journeys, names, deadline)
    except KeyboardInterrupt:
        print("interrupted: summarising the journeys that finished.", file=sys.stderr)
        status = 130
    except Exception as exc:  # noqa: BLE001
        print(f"the run stopped early ({exception_type(exc)}).", file=sys.stderr)
        status = 1
    if results:
        print(render_table(results))
    if args.summary:
        meta = {"label": args.label, "started_at": started, "run_id": env.run_id, "order": names, "seed": seed}
        try:
            write_summary(args.summary, summary_document(label, results, meta))
            print(f"summary written to {args.summary}")
        except OSError:
            print("could not write the summary (the path must be writable and not a symlink).", file=sys.stderr)
            status = status or 1
    if status:
        return status
    return 0 if results and all(r.ok for r in results) else 1

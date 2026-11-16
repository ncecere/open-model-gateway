"""Journey suite tests (#592): recorder, redaction, cleanup, order independence, loopback refusal, feature map.

No cluster and no gateway: an in-process fake HTTP server stands in for the gateway and the demo issuer.
"""
import ast
import contextlib
import fnmatch
import io
import itertools
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.dont_write_bytecode = True

from journeys import cli, oidc, recorder, registry, runner  # noqa: E402
from journeys.client import Http, JourneyError, Response, Session, SetupError  # noqa: E402
from journeys.context import Ctx, Env, Secret  # noqa: E402

# Values the fake gateway puts in bodies, headers, cookies, URLs and exception text. None may ever be written.
PLANTED = {
    "key": "omg_" + "ab12cd34" * 4 + "." + "ef56ab78" * 8,
    "cookie": "sessionvalue" + "9f8e7d6c" * 3,
    "csrf": "csrfvalue" + "1a2b3c4d" * 3,
    "code": "authcode" + "5e6f7a8b" * 3,
    "email": "someone.private@example.test",
    "query": "tokenparam=" + "q" * 20,
    "id": "7b1f2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
}


class Fake:
    """A fake gateway and demo issuer on one loopback port. `app(method, path, headers, body)` answers."""

    def __init__(self, app):
        outer = self
        self.requests = []

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *args):
                pass

            def handle_one(self):
                length = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(length) if length else b""
                outer.requests.append((self.command, self.path))
                status, headers, payload = app(self.command, self.path, {k.lower(): v for k, v in self.headers.items()}, body)
                self.send_response(status)
                for key, value in headers:
                    self.send_header(key, value)
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = handle_one

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.base = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def json_response(status, value, headers=()):
    return status, [("content-type", "application/json"), *headers], json.dumps(value).encode()


class Gateway:
    """A tiny stateful stand-in: sign-in, `things` that journeys create and delete, and planted secrets in every answer."""

    def __init__(self, port_getter):
        self.things = {}
        self.next_id = 1
        self.port = port_getter
        self.sessions = set()

    def __call__(self, method, path, headers, body):
        base = f"http://127.0.0.1:{self.port()}"
        route = path.split("?")[0]
        leak = [("x-debug-token", PLANTED["key"]), ("x-request-url", f"{base}{route}?{PLANTED['query']}")]
        if route == "/health/ready":
            return json_response(200, {"checks": {"database": "ok"}})
        if route == "/api/v1/auth/login":
            return 303, [("location", f"{base}/authorize?state={PLANTED['code']}&{PLANTED['query']}"), ("set-cookie", "omg_oidc=pending; Path=/")], b""
        if route == "/authorize":
            page = f'<form method="post" action="/choose"><input type="hidden" name="state" value="{PLANTED["code"]}"></form>'
            return 200, [("content-type", "text/html")], page.encode()
        if route == "/choose":
            return 302, [("location", f"{base}/api/v1/auth/callback?code={PLANTED['code']}&state=x")], b""
        if route == "/api/v1/auth/callback":
            if getattr(self, "throttle_callback", 0):
                self.throttle_callback -= 1
                return 429, [("retry-after", "0")], b"{}"
            self.sessions.add(PLANTED["cookie"])
            return 303, [("location", "/"), ("set-cookie", f"omg_session={PLANTED['cookie']}; Path=/; HttpOnly"), ("set-cookie", f"omg_csrf={PLANTED['csrf']}; Path=/")], b""
        authed = PLANTED["cookie"] in headers.get("cookie", "")
        if route == "/api/v1/auth/logout":
            return (204, [("set-cookie", "omg_session=; Max-Age=0; Path=/")], b"") if authed else json_response(401, {"error": "Authentication required"})
        if route == "/api/v1/me":
            if not authed:
                return json_response(401, {"error": "Authentication required"})
            return json_response(200, {"user": {"email": PLANTED["email"], "id": PLANTED["id"]}, "token": PLANTED["key"]}, leak)
        if route == "/api/v1/things" and method == "POST":
            if not authed or headers.get("x-csrf-token") != PLANTED["csrf"]:
                return json_response(403, {"error": "csrf"})
            name = json.loads(body)["name"]
            thing = str(self.next_id)
            self.next_id += 1
            self.things[thing] = name
            return json_response(200, {"id": thing, "token": PLANTED["key"], "name": name}, leak)
        match = re.fullmatch(r"/api/v1/things/(\d+)", route)
        if match:
            thing = match.group(1)
            if method == "DELETE":
                self.things.pop(thing, None)
                return json_response(200, {"ok": True})
            return json_response(200 if thing in self.things else 404, {"name": self.things.get(thing), "secret": PLANTED["key"]}, leak)
        return json_response(404, {"error": {"message": f"nothing at {route} {PLANTED['key']}"}}, leak)


def start_gateway():
    holder = {}
    gateway = Gateway(lambda: holder["fake"].port)
    holder["fake"] = Fake(gateway)
    return gateway, holder["fake"]


def make_env(fake):
    return Env(Http(fake.base), run_id="abc123")


def add_thing_journey(registry_map, name):
    """A journey that creates a throwaway thing, checks it, and deletes it by deferred cleanup."""

    def run(t):
        operator = t.session("operator")
        created = operator.post("/api/v1/things", {"name": t.label("thing")})
        t.expect("thing.create", created, 200)
        thing = created.json()["id"]
        t.defer("delete thing", lambda: operator.delete(f"/api/v1/things/{thing}"))
        t.expect("thing.read", operator.get(f"/api/v1/things/{thing}"), 200)
        t.expect("thing.anonymous-refused", t.anonymous().post("/api/v1/things", {"name": "x"}), 403)
        t.ok("thing.name-carries-run-label", re.fullmatch(r"journeys-[0-9a-f]{6}-[a-z-]+-thing", operator.get(f"/api/v1/things/{thing}").json()["name"]) is not None)

    registry_map[name] = run


class RecorderTests(unittest.TestCase):
    def test_accepts_a_status_expectation_and_a_condition(self):
        rec = recorder.Recorder()
        self.assertTrue(rec.check("keys.create", 200, 200))
        self.assertFalse(rec.check("keys.refused", {401, 403}, 404))
        self.assertTrue(rec.assertion("keys.shape", True))
        self.assertEqual((rec.passed, rec.failed), (2, 1))
        failed = rec.checks[1].as_dict()
        self.assertEqual(failed, {"name": "keys.refused", "passed": False, "expected": [401, 403], "actual": 404, "note": "unexpected_status"})

    def test_names_come_from_a_closed_alphabet(self):
        rec = recorder.Recorder()
        for bad in ("Keys", "keys_create", "a@b.test", "keys?x=1", "keys/1", "a" * 80, "", "x" + "0123456789ab" * 2, "omg_" + "a" * 8, "key=value", "has\nnewline"):
            with self.assertRaises(ValueError, msg=bad):
                rec.check(bad, 200, 200)
        self.assertEqual(rec.checks, [])

    def test_statuses_must_be_statuses_never_responses_or_text(self):
        rec = recorder.Recorder()
        for expected, actual in ((200, "200"), ("200", 200), (200, True), (200, 99), (200, 600), ({200, "x"}, 200), (None, 200), ([], 200), (200, Response(200, {}, [], b"body"))):
            with self.assertRaises(TypeError, msg=repr((expected, actual))):
                rec.check("keys.create", expected, actual)
        self.assertEqual(rec.checks, [])

    def test_notes_come_from_a_fixed_vocabulary(self):
        rec = recorder.Recorder()
        for note in ("free text", "omg_" + "a" * 20, None, 3):
            with self.assertRaises(ValueError, msg=repr(note)):
                rec.check("keys.create", 200, 200, note=note)
            with self.assertRaises(ValueError, msg=repr(note)):
                rec.fail("keys.create", note)
        rec.fail("keys.create", "cleanup_failed")
        self.assertEqual(rec.checks[0].note, "cleanup_failed")

    def test_exception_types_are_named_only_from_an_allowlist(self):
        class Leaky(Exception):
            pass

        self.assertEqual(recorder.exception_type(Leaky(PLANTED["key"])), "Exception")
        self.assertEqual(recorder.exception_type(KeyError(PLANTED["key"])), "KeyError")

    def test_secret_wrapper_never_prints(self):
        secret = Secret(PLANTED["key"])
        for text in (repr(secret), str(secret), f"{secret}", f"{secret!r}", "%s" % secret, format(secret, ">40")):
            self.assertNotIn(PLANTED["key"], text)
        with self.assertRaises(TypeError):
            import pickle

            pickle.dumps(secret)
        self.assertEqual(secret.reveal(), PLANTED["key"])

    def test_responses_and_sessions_hide_their_contents_from_repr(self):
        response = Response(200, {"authorization": PLANTED["key"]}, [f"omg_session={PLANTED['cookie']}"], PLANTED["key"].encode())
        session = Session(Http("http://127.0.0.1:1"), "operator")
        session.store(response)
        for text in (repr(response), str(response), repr(session), str(session), f"{response}"):
            for value in PLANTED.values():
                self.assertNotIn(value, text)


class RedactionTests(unittest.TestCase):
    def run_cli(self, journeys, extra=()):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        out, err = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as folder, mock.patch.object(cli, "load_all", return_value=journeys):
            path = os.path.join(folder, "summary.json")
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = cli.main(["run", "--base", fake.base, "--summary", path, *extra])
            summary = Path(path).read_text() if os.path.exists(path) else ""
        return code, out.getvalue(), err.getvalue(), summary, gateway

    def test_no_planted_secret_reaches_the_summary_or_the_terminal(self):
        def leaky(t):
            operator = t.session("operator")
            me = operator.get("/api/v1/me")
            t.expect("me.read", me, 200)
            t.expect("me.should-fail", me, 404)  # a failing check prints expected and actual only
            t.ok("me.body-has-token", PLANTED["key"].encode() in me.body)
            t.expect("things.unknown", operator.get(f"/api/v1/nothing?{PLANTED['query']}"), 200)
            raise KeyError(PLANTED["key"] + PLANTED["email"])  # an exception message that carries secrets

        def broken_cleanup(t):
            t.defer("planted", lambda: (_ for _ in ()).throw(ValueError(PLANTED["cookie"])))
            t.ok("cleanup.registered", True)

        code, out, err, summary, _ = self.run_cli({"leaky": leaky, "broken-cleanup": broken_cleanup})
        self.assertEqual(code, 1)
        self.assertTrue(summary and out)
        for name, value in PLANTED.items():
            for stream_name, stream in (("summary", summary), ("stdout", out), ("stderr", err)):
                self.assertNotIn(value, stream, f"{name} leaked into {stream_name}")
        document = json.loads(summary)
        self.assertEqual(set(document), {"suite", "environment", "ok", "passed", "failed", "skipped", "journeys"})
        for journey in document["journeys"]:
            self.assertEqual(set(journey), {"journey", "ok", "passed", "failed", "skipped", "error", "duration_seconds", "checks"})
            for check in journey["checks"]:
                self.assertEqual(set(check), {"name", "passed", "expected", "actual", "note"})
        self.assertEqual(document["environment"], out.split("environment at ")[1].split(" ")[0])
        leaky_result = document["journeys"][0] if document["journeys"][0]["journey"] == "leaky" else document["journeys"][1]
        self.assertEqual(leaky_result["error"], "KeyError")

    def test_summary_lists_each_failed_check_with_expected_and_actual_status(self):
        def fail(t):
            t.expect("me.refused", t.anonymous().get("/api/v1/me"), 200)

        code, out, _, summary, _ = self.run_cli({"fail": fail})
        self.assertEqual(code, 1)
        self.assertIn("FAIL me.refused: expected 200, got 401", out)
        check = json.loads(summary)["journeys"][0]["checks"][0]
        self.assertEqual((check["expected"], check["actual"], check["passed"]), (200, 401, False))


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.gateway, self.fake = start_gateway()
        self.addCleanup(self.fake.close)
        self.env = make_env(self.fake)

    def test_cleanup_runs_in_reverse_order_after_a_failure_and_the_next_journey_still_runs(self):
        order = []

        def failing(t):
            t.defer("first", lambda: order.append("first"))
            t.defer("second", lambda: order.append("second"))
            raise SetupError("planned")

        def after(t):
            t.expect("me.read", t.session("alex").get("/api/v1/me"), 200)

        results = runner.run_all(self.env, {"failing": failing, "after": after}, ["failing", "after"])
        self.assertEqual(order, ["second", "first"])
        self.assertFalse(results[0].ok)
        self.assertEqual(results[0].error, "SetupError")
        self.assertTrue(results[1].ok)

    def test_a_failing_cleanup_is_a_failed_check_and_does_not_stop_the_other_undo_steps(self):
        ran = []

        def journey(t):
            t.defer("first", lambda: ran.append("first"))
            t.defer("broken", lambda: 1 / 0)
            t.defer("last", lambda: ran.append("last"))
            t.ok("work.done", True)

        result = runner.run_one(self.env, "journey", journey)
        self.assertEqual(ran, ["last", "first"])
        self.assertFalse(result.ok)
        failed = [c for c in result.recorder.checks if not c.passed]
        self.assertEqual([(c.name, c.note) for c in failed], [("cleanup broken", "cleanup_failed")])

    def test_cleanup_deletes_what_the_journey_created_even_when_a_check_fails(self):
        def journey(t):
            operator = t.session("operator")
            created = operator.post("/api/v1/things", {"name": t.label("thing")})
            thing = created.json()["id"]
            t.defer("delete thing", lambda: operator.delete(f"/api/v1/things/{thing}"))
            t.expect("thing.wrong-expectation", operator.get(f"/api/v1/things/{thing}"), 404)

        result = runner.run_one(self.env, "journey", journey)
        self.assertFalse(result.ok)
        self.assertEqual(self.gateway.things, {})

    def test_order_independence_and_rerun(self):
        journeys = {}
        for name in ("alpha", "bravo", "charlie"):
            add_thing_journey(journeys, name)
        outcomes = set()
        for order in itertools.permutations(journeys):
            results = runner.run_all(self.env, journeys, list(order))
            outcomes.add(tuple(sorted((r.name, r.recorder.passed, r.recorder.failed) for r in results)))
            self.assertTrue(all(r.ok for r in results), order)
            self.assertEqual(self.gateway.things, {}, order)
        self.assertEqual(len(outcomes), 1)
        again = runner.run_all(self.env, journeys, list(journeys))
        self.assertTrue(all(r.ok for r in again))
        self.assertEqual(self.gateway.things, {})

    def test_pooled_sessions_are_signed_out_at_the_end_of_a_run(self):
        journeys = {}
        add_thing_journey(journeys, "alpha")
        runner.run_all(self.env, journeys, ["alpha"])
        self.assertIn(("POST", "/api/v1/auth/logout"), self.fake.requests)
        self.assertEqual(self.env._pool, {})

    def test_shuffle_and_only_select_and_order_journeys(self):
        journeys = {}
        for name in ("alpha", "bravo", "charlie"):
            add_thing_journey(journeys, name)
        with mock.patch.object(cli, "load_all", return_value=journeys):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "charlie", "alpha"]), 0)
            table = out.getvalue().splitlines()
            rows = [line.split()[0] for line in table if line.split() and line.split()[0] in journeys]
            self.assertEqual(rows, ["charlie", "alpha"])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(cli.main(["run", "--base", self.fake.base, "--shuffle", "--seed", "3"]), 0)
            self.assertEqual(sorted(l.split()[0] for l in out.getvalue().splitlines() if l.split() and l.split()[0] in journeys), ["alpha", "bravo", "charlie"])

    def test_unknown_or_repeated_journey_names_are_refused(self):
        journeys = {}
        add_thing_journey(journeys, "alpha")
        with mock.patch.object(cli, "load_all", return_value=journeys), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "nope"]), 2)
            self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "alpha", "alpha"]), 2)


class SignInTests(unittest.TestCase):
    def test_sign_in_follows_the_redirects_and_retries_a_throttled_callback(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        gateway.throttle_callback = 2
        http = Http(fake.base)
        session = Session(http, "operator")
        with mock.patch.object(oidc.time, "sleep") as sleep:
            steps = oidc.sign_in(session, http, "operator")
        self.assertEqual({k: v.status for k, v in steps.items()}, {"login": 303, "authorize": 200, "choose": 302, "callback": 303})
        self.assertEqual(sleep.call_count, 2)
        self.assertTrue(session.signed_in)
        self.assertEqual(session.csrf, PLANTED["csrf"])
        self.assertEqual(session.get("/api/v1/me").status, 200)
        self.assertEqual(session.post("/api/v1/auth/logout").status, 204)
        self.assertFalse(session.signed_in)

    def test_unknown_persona_is_refused_before_any_request(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        http = Http(fake.base)
        with self.assertRaises(JourneyError):
            oidc.sign_in(Session(http, "x"), http, "mallory")
        self.assertEqual(fake.requests, [])


class LoopbackTests(unittest.TestCase):
    def refuse(self, *args):
        err = io.StringIO()
        with mock.patch.object(Http, "request", side_effect=AssertionError("network used")), mock.patch.object(cli, "load_all", return_value={}), contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            code = cli.main(["run", *args])
        return code, err.getvalue()

    def test_a_remote_base_is_refused_before_any_request(self):
        for base in ("http://example.com", "https://gateway.example.net:8443", "http://10.0.0.5:3000", "http://192.168.1.2", "http://[2001:db8::1]:3000", "http://127.0.0.1.example.com", "http://localhost.example.com"):
            code, err = self.refuse("--base", base)
            self.assertEqual(code, 2, base)
            self.assertIn("loopback", err)

    def test_a_remote_issuer_is_refused_too(self):
        code, err = self.refuse("--base", "http://127.0.0.1:3000", "--issuer", "http://issuer.example.com")
        self.assertEqual(code, 2)
        self.assertIn("loopback", err)

    def test_loopback_spellings_pass_the_gate(self):
        for base in ("http://127.0.0.1:3000", "http://localhost:3000", "http://[::1]:3000", "http://127.1.2.3"):
            host, _, origin = cli.origin_of(base)
            self.assertTrue(cli.is_loopback(host), base)

    def test_allow_remote_lifts_only_the_loopback_gate(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        with mock.patch.object(cli, "is_loopback", return_value=False), mock.patch.object(cli, "load_all", return_value={}), \
             contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(cli.main(["run", "--base", fake.base]), 2)  # refused: not loopback
            self.assertIn("loopback", err.getvalue())
            err.truncate(0)
            err.seek(0)
            code = cli.main(["run", "--base", fake.base, "--allow-remote"])
        self.assertEqual(code, 2)  # passes the gate but the issuer check still refuses a non-loopback sign-in
        self.assertIn("not on loopback", err.getvalue())
        self.assertIn("about to modify the environment", out.getvalue())

    def test_urls_with_credentials_paths_or_queries_are_refused(self):
        for base in ("http://user:pass@127.0.0.1:3000", "http://127.0.0.1:3000/api", "http://127.0.0.1:3000?x=1", "ftp://127.0.0.1", "127.0.0.1:3000"):
            code, err = self.refuse("--base", base)
            self.assertEqual(code, 2, base)

    def test_an_unready_gateway_or_a_non_demo_sign_in_is_refused(self):
        def unready(method, path, headers, body):
            return json_response(503, {}) if path == "/health/ready" else json_response(404, {})

        fake = Fake(unready)
        self.addCleanup(fake.close)
        err = io.StringIO()
        with mock.patch.object(cli, "load_all", return_value={}), contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(["run", "--base", fake.base]), 2)
        self.assertIn("not ready", err.getvalue())

        def real_idp(method, path, headers, body):
            if path == "/health/ready":
                return json_response(200, {})
            return 303, [("location", "https://login.example.com/authorize?x=1")], b""

        other = Fake(real_idp)
        self.addCleanup(other.close)
        err = io.StringIO()
        with mock.patch.object(cli, "load_all", return_value={}), contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(["run", "--base", other.base]), 2)
        self.assertIn("not a demo environment", err.getvalue())


# --- the feature map ---------------------------------------------------------------------------------------------

SUITE = ROOT / "scripts" / "journeys" / "suite"
RECORDING = {"expect", "status", "ok", "skip"}


def check_patterns():
    """{journey name: set of check-name patterns} read from the source; an f-string expression becomes '*'."""
    found = {}
    for path in sorted(SUITE.glob("*.py")):
        tree = ast.parse(path.read_text())
        for node in ast.walk(tree):
            if not isinstance(node, ast.FunctionDef):
                continue
            names = [
                d.args[0].value for d in node.decorator_list
                if isinstance(d, ast.Call) and getattr(d.func, "id", "") == "journey" and d.args and isinstance(d.args[0], ast.Constant)
            ]
            if not names:
                continue
            patterns = found.setdefault(names[0], set())
            for call in ast.walk(node):
                if isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr in RECORDING and call.args:
                    first = call.args[0]
                    if isinstance(first, ast.Constant) and isinstance(first.value, str):
                        patterns.add(first.value)
                    elif isinstance(first, ast.JoinedStr):
                        patterns.add("".join(v.value if isinstance(v, ast.Constant) else "*" for v in first.values))
                if isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr == "fail" and call.args and isinstance(call.args[0], ast.Constant):
                    patterns.add(call.args[0].value)
    return found


def feature_map():
    """Rows of docs/journeys.md's table: [(flow, journey or None, [check ids] or reason)]."""
    text = (ROOT / "docs" / "journeys.md").read_text()
    section = text.split("## Feature map", 1)[1]
    rows = []
    for line in section.splitlines():
        if not line.startswith("|") or line.startswith("| ---") or line.startswith("| Flow"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) != 3:
            raise AssertionError(f"feature map row has {len(cells)} cells: {line[:60]}")
        flow, journey, checks = cells
        if journey == "not covered":
            rows.append((flow, None, checks))
        else:
            rows.append((flow, journey.strip("`"), re.findall(r"`([^`]+)`", checks)))
    return rows


class FeatureMapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.journeys = registry.load_all()
        cls.patterns = check_patterns()
        cls.rows = feature_map()

    def test_every_journey_in_the_registry_is_in_the_map_and_the_other_way_round(self):
        mapped = {journey for _, journey, _ in self.rows if journey}
        self.assertEqual(sorted(set(self.journeys) - mapped), [], "a journey is missing from docs/journeys.md")
        self.assertEqual(sorted(mapped - set(self.journeys)), [], "docs/journeys.md names a journey that does not exist")
        self.assertEqual(set(self.patterns), set(self.journeys), "journey source and registry disagree")

    def test_every_check_named_in_the_map_exists_in_its_journey(self):
        missing = []
        for flow, journey, checks in self.rows:
            if not journey:
                continue
            self.assertTrue(checks, f"no checks named for: {flow}")
            for check in checks:
                if not any(fnmatch.fnmatchcase(check, pattern) for pattern in self.patterns[journey]):
                    missing.append(f"{journey}: {check}")
        self.assertEqual(missing, [])

    def test_every_not_covered_row_gives_a_reason(self):
        uncovered = [(flow, reason) for flow, journey, reason in self.rows if journey is None]
        self.assertGreaterEqual(len(uncovered), 5)
        for flow, reason in uncovered:
            self.assertGreater(len(reason), 30, f"not covered without a reason: {flow}")

    def test_every_flow_is_listed_once(self):
        flows = [flow for flow, _, _ in self.rows]
        self.assertEqual(len(flows), len(set(flows)))

    def test_every_literal_check_name_fits_the_recorders_alphabet(self):
        for journey, patterns in self.patterns.items():
            for pattern in patterns:
                self.assertRegex(pattern.replace("*", "x"), recorder.NAME.pattern + r"\Z", f"{journey}: {pattern}")
                self.assertIsNone(recorder.LONG_HEX.search(pattern), f"{journey}: {pattern}")

    def test_the_summary_shape_is_documented(self):
        text = (ROOT / "docs" / "journeys.md").read_text()
        for needle in ("--summary", "--only", "--allow-remote", "--shuffle", "loopback", "not covered"):
            self.assertIn(needle, text)

    def test_journey_names_follow_the_registry_pattern(self):
        for name in self.journeys:
            self.assertRegex(name, registry.NAME.pattern)


class SourceHygieneTests(unittest.TestCase):
    def test_the_suite_uses_only_the_standard_library(self):
        stdlib = set(sys.stdlib_module_names)
        for path in (ROOT / "scripts" / "journeys").rglob("*.py"):
            for node in ast.walk(ast.parse(path.read_text())):
                modules = []
                if isinstance(node, ast.Import):
                    modules = [a.name.split(".")[0] for a in node.names]
                elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                    modules = [node.module.split(".")[0]]
                for module in modules:
                    self.assertTrue(module in stdlib or module == "journeys", f"{path.name} imports {module}")

    def test_no_journey_prints_a_response_or_credential(self):
        for path in (ROOT / "scripts" / "journeys").rglob("*.py"):
            for node in ast.walk(ast.parse(path.read_text())):
                if isinstance(node, ast.Call) and getattr(node.func, "id", "") in {"print", "pprint"}:
                    self.assertIn(path.name, {"cli.py", "__main__.py"}, f"{path.name} prints")

    def test_journeys_hand_the_recorder_statuses_only(self):
        """Inside the suite a check is always t.expect(name, response, status), t.status, t.ok or t.skip."""
        for path in (ROOT / "scripts" / "journeys" / "suite").glob("*.py"):
            for node in ast.walk(ast.parse(path.read_text())):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr in {"check", "assertion"}:
                    self.fail(f"{path.name} bypasses Ctx.expect/status/ok")


if __name__ == "__main__":
    unittest.main()

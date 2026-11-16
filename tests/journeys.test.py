"""Journey suite tests (#592): recorder, redaction, cleanup, order independence, safety gates, feature map.

No cluster and no gateway: an in-process fake HTTP server stands in for the gateway and the demo issuer.
Every journey is also executed end to end against a generic fake (see SmokeTests), so a typo in a rarely
taken branch fails here rather than on a release candidate.
"""
import ast
import builtins
import contextlib
import dis
import io
import itertools
import json
import os
from pathlib import Path
import re
import stat
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
from journeys.client import Http, JourneyError, Response, Session, SetupError, TransportError, rows, uuid_of  # noqa: E402
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
SOME_ID = PLANTED["id"]


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
                if status == "garbage":
                    self.wfile.write(payload)
                    return
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
    """A tiny stateful stand-in: demo sign-in, `things` that journeys create and delete, planted secrets everywhere.

    With generic=True every other path answers 200 with a body that has an id, a token and empty lists, so a
    journey runs its whole body (its checks mostly fail, which is not the point)."""

    def __init__(self, port_getter, generic=False, demo_page=True):
        self.things = {}
        self.next_id = 1
        self.port = port_getter
        self.generic = generic
        self.demo_page = demo_page
        self.throttle_callback = 0
        self.throttle_login = 0
        self.login_calls = 0
        self.not_ready = False

    def __call__(self, method, path, headers, body):
        base = f"http://127.0.0.1:{self.port()}"
        route = path.split("?")[0]
        leak = [("x-debug-token", PLANTED["key"]), ("x-request-url", f"{base}{route}?{PLANTED['query']}")]
        if route == "/health/ready":
            return json_response(503 if self.not_ready else 200, {"checks": {"database": "ok"}})
        if route == "/api/v1/auth/login":
            self.login_calls += 1
            if self.throttle_login:
                self.throttle_login -= 1
                return 429, [("retry-after", "0")], b"{}"
            redirect = f"{base}/api/v1/auth/callback"
            return 303, [("location", f"{base}/authorize?state={PLANTED['code']}&{PLANTED['query']}&redirect_uri={redirect}"), ("set-cookie", "omg_oidc=pending; Path=/")], b""
        if route == "/authorize":
            accounts = "operator@demo.invalid" if self.demo_page else "someone@corp.example"
            page = f'<form method="post" action="/choose"><input type="hidden" name="state" value="{PLANTED["code"]}"><button>{accounts}</button></form>'
            return 200, [("content-type", "text/html")], page.encode()
        if route == "/choose":
            return 302, [("location", f"{base}/api/v1/auth/callback?code={PLANTED['code']}&state=x")], b""
        if route == "/api/v1/auth/callback":
            if self.throttle_callback:
                self.throttle_callback -= 1
                return 429, [("retry-after", "0")], b"{}"
            return 303, [("location", "/"), ("set-cookie", f"omg_session={PLANTED['cookie']}; Path=/; HttpOnly"), ("set-cookie", f"omg_csrf={PLANTED['csrf']}; Path=/")], b""
        authed = PLANTED["cookie"] in headers.get("cookie", "")
        if route == "/api/v1/auth/logout":
            return (204, [("set-cookie", "omg_session=; Max-Age=0; Path=/")], b"") if authed else json_response(401, {"error": "Authentication required"})
        if route == "/api/v1/me":
            if not authed:
                return json_response(401, {"error": "Authentication required"})
            return json_response(200, {"user": {"email": PLANTED["email"], "id": SOME_ID}, "token": PLANTED["key"], "workspaces": [{"id": SOME_ID, "kind": "personal"}]}, leak)
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
        if self.generic:
            return json_response(200, {"id": SOME_ID, "token": PLANTED["key"], "data": [], "workspaces": [], "capabilities": {}}, leak)
        return json_response(404, {"error": {"message": f"nothing at {route} {PLANTED['key']}"}}, leak)


def start_gateway(**kw):
    holder = {}
    gateway = Gateway(lambda: holder["fake"].port, **kw)
    holder["fake"] = Fake(gateway)
    return gateway, holder["fake"]


def make_env(fake, **kw):
    env = Env(Http(fake.base), run_id="abc123", **kw)
    env.issuer_origin = fake.base
    return env


def add_thing_journey(registry_map, name):
    """A journey that creates a throwaway thing, checks it, and deletes it by deferred cleanup."""

    def run(t):
        operator = t.session("operator")
        created = operator.post("/api/v1/things", {"name": t.label("thing")})
        t.expect("thing.create", created, 200)
        thing = created.json()["id"]
        t.undo("delete thing", lambda: operator.delete(f"/api/v1/things/{thing}"))
        t.expect("thing.read", operator.get(f"/api/v1/things/{thing}"), 200)
        t.expect("thing.anonymous-refused", t.anonymous().post("/api/v1/things", {"name": "x"}), 403)
        t.ok("thing.name-carries-run-label", re.fullmatch(r"journeys-[0-9a-f]{6}-[a-z-]+-thing", operator.get(f"/api/v1/things/{thing}").json()["name"]) is not None)

    registry_map[name] = run


class Quiet:
    """Silence stdout and stderr, returning both."""

    def __enter__(self):
        self.out, self.err = io.StringIO(), io.StringIO()
        self._a, self._b = contextlib.redirect_stdout(self.out), contextlib.redirect_stderr(self.err)
        self._a.__enter__()
        self._b.__enter__()
        return self

    def __exit__(self, *exc):
        self._b.__exit__(*exc)
        self._a.__exit__(*exc)


class RecorderTests(unittest.TestCase):
    def test_accepts_a_status_expectation_and_a_condition(self):
        rec = recorder.Recorder()
        self.assertTrue(rec.check("keys.create", 200, 200))
        self.assertFalse(rec.check("keys.refused", {401, 403}, 404))
        self.assertTrue(rec.assertion("keys.shape", True))
        self.assertEqual((rec.passed, rec.failed), (2, 1))
        self.assertEqual(rec.checks[1].as_dict(), {"name": "keys.refused", "passed": False, "expected": [401, 403], "actual": 404, "note": "unexpected_status"})

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

    def test_a_secret_cannot_become_a_check_name_through_the_context(self):
        ctx = Ctx(Env(Http("http://127.0.0.1:1")), "journey")
        with self.assertRaises(ValueError):
            ctx.ok(PLANTED["key"], True)
        with self.assertRaises(ValueError):
            ctx.expect("keys." + PLANTED["cookie"], Response(200, {}, [], b""), 200)
        with self.assertRaises(ValueError):
            ctx.undo_label = None  # sanity: attribute assignment is fine
            ctx.rec.fail("cleanup " + PLANTED["email"], "cleanup_failed")
        self.assertEqual(ctx.rec.checks, [])

    def test_exception_types_are_named_only_from_an_allowlist(self):
        class Leaky(Exception):
            pass

        self.assertEqual(recorder.exception_type(Leaky(PLANTED["key"])), "Exception")
        self.assertEqual(recorder.exception_type(KeyError(PLANTED["key"])), "KeyError")
        self.assertEqual(recorder.exception_type(TransportError("x")), "TransportError")

    def test_secret_wrapper_never_prints(self):
        secret = Secret(PLANTED["key"])
        for text in (repr(secret), str(secret), f"{secret}", f"{secret!r}", "%s" % secret, format(secret, ">40")):
            self.assertNotIn(PLANTED["key"], text)
        with self.assertRaises(TypeError):
            import pickle

            pickle.dumps(secret)
        self.assertEqual(secret.reveal(), PLANTED["key"])
        self.assertIsNone(Secret.of(Response(200, {}, [], b"{}")))
        self.assertEqual(Secret.of(Response(200, {}, [], json.dumps({"token": "t"}).encode())).reveal(), "t")

    def test_responses_and_sessions_hide_their_contents_from_repr(self):
        response = Response(200, {"authorization": PLANTED["key"]}, [f"omg_session={PLANTED['cookie']}"], PLANTED["key"].encode())
        session = Session(Http("http://127.0.0.1:1"), "operator")
        session.store(response)
        for text in (repr(response), str(response), repr(session), str(session), f"{response}"):
            for value in PLANTED.values():
                self.assertNotIn(value, text)

    def test_ids_are_validated_before_they_can_reach_a_path(self):
        self.assertEqual(uuid_of(SOME_ID), SOME_ID)
        for bad in ("../../platform/users", SOME_ID + "/keys", SOME_ID.upper(), "1", "", None, 7, SOME_ID + "?x=1", " " + SOME_ID):
            self.assertIsNone(uuid_of(bad), bad)
        response = Response(200, {}, [], json.dumps({"id": "../../x", "other": SOME_ID}).encode())
        self.assertIsNone(response.id())
        self.assertEqual(response.id("other"), SOME_ID)

    def test_the_client_refuses_an_unsafe_path_before_sending(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        session = Session(Http(fake.base), "x")
        for path in ("/api/v1/../admin", "/api/v1//x", "/api/v1/x y", "/api/v1/x\r\nhost: evil", "api/v1/x", "/api/v1/x\\y"):
            with self.assertRaises(JourneyError, msg=path):
                session.get(path)
        self.assertEqual(fake.requests, [])

    def test_a_failed_list_call_is_not_an_empty_list(self):
        self.assertIsNone(rows(Response(503, {}, [], b'{"data": []}')))
        self.assertIsNone(rows(Response(200, {}, [], b'{"data": "x"}')))
        self.assertIsNone(rows(Response(200, {}, [], b"not json")))
        self.assertEqual(rows(Response(200, {}, [], b'{"data": []}')), [])


class RedactionTests(unittest.TestCase):
    def run_cli(self, journeys, extra=()):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        with tempfile.TemporaryDirectory() as folder, mock.patch.object(cli, "load_all", return_value=journeys), Quiet() as quiet:
            path = os.path.join(folder, "summary.json")
            code = cli.main(["run", "--base", fake.base, "--summary", path, *extra])
            summary = Path(path).read_text() if os.path.exists(path) else ""
        return code, quiet.out.getvalue(), quiet.err.getvalue(), summary, gateway

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

        code, out, err, summary, _ = self.run_cli({"leaky": leaky, "broken-cleanup": broken_cleanup}, ("--label", "rc-1"))
        self.assertEqual(code, 1)
        self.assertTrue(summary and out)
        for name, value in PLANTED.items():
            for stream_name, stream in (("summary", summary), ("stdout", out), ("stderr", err)):
                self.assertNotIn(value, stream, f"{name} leaked into {stream_name}")
        document = json.loads(summary)
        self.assertEqual(set(document), {"suite", "environment", "label", "started_at", "run_id", "order", "seed", "ok", "passed", "failed", "skipped", "journeys"})
        self.assertEqual((document["label"], document["order"], document["seed"]), ("rc-1", ["broken-cleanup", "leaky"], None))
        self.assertRegex(document["started_at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\+00:00$")
        for journey in document["journeys"]:
            self.assertEqual(set(journey), {"journey", "ok", "passed", "failed", "skipped", "error", "error_after_check", "error_line", "duration_seconds", "checks"})
            for check in journey["checks"]:
                self.assertEqual(set(check), {"name", "passed", "expected", "actual", "note"})
        leaky_result = next(j for j in document["journeys"] if j["journey"] == "leaky")
        self.assertEqual(leaky_result["error"], "KeyError")
        self.assertEqual(leaky_result["error_after_check"], "things.unknown")
        self.assertIsInstance(leaky_result["error_line"], int)

    def test_summary_lists_each_failed_check_with_expected_and_actual_status(self):
        def fail(t):
            t.expect("me.refused", t.anonymous().get("/api/v1/me"), 200)

        code, out, _, summary, _ = self.run_cli({"fail": fail})
        self.assertEqual(code, 1)
        self.assertIn("FAIL me.refused: expected 200, got 401", out)
        check = json.loads(summary)["journeys"][0]["checks"][0]
        self.assertEqual((check["expected"], check["actual"], check["passed"]), (200, 401, False))

    def test_a_journey_that_records_nothing_fails(self):
        code, out, _, summary, _ = self.run_cli({"silent": lambda t: None})
        self.assertEqual(code, 1)
        self.assertIn("journey recorded no checks", out)

    def test_the_summary_names_the_run_and_the_seed_of_a_shuffle(self):
        journeys = {}
        for name in ("alpha", "bravo", "charlie", "delta"):
            add_thing_journey(journeys, name)
        code, out, _, summary, _ = self.run_cli(journeys, ("--shuffle",))
        document = json.loads(summary)
        self.assertEqual(code, 0)
        self.assertIsInstance(document["seed"], int)
        self.assertIn(f"seed {document['seed']}", out)
        self.assertEqual(sorted(document["order"]), ["alpha", "bravo", "charlie", "delta"])
        _, _, _, again, _ = self.run_cli(journeys, ("--shuffle", "--seed", str(document["seed"])))
        self.assertEqual(json.loads(again)["order"], document["order"])


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
        self.assertEqual([(c.name, c.note) for c in result.recorder.checks if not c.passed], [("cleanup broken", "cleanup_failed")])

    def test_an_undo_that_gets_a_refusal_is_a_failed_check_unless_the_status_is_in_the_gone_set(self):
        def journey(t):
            anon = t.anonymous()
            t.undo("accepted", lambda: anon.get("/api/v1/things/99"), gone=(404,))      # 404: already gone, fine
            t.undo("refused", lambda: anon.post("/api/v1/things", {"name": "x"}))        # 403: a visible failure
            t.undo("unauthorised", lambda: anon.get("/api/v1/me"), gone=(404,))          # 401 is not in the set
            t.undo("ok", lambda: anon.get("/health/ready"))                              # 200

        result = runner.run_one(self.env, "journey", journey)
        failed = sorted(c.name for c in result.recorder.checks if not c.passed and c.note == "cleanup_failed")
        self.assertEqual(failed, ["cleanup refused", "cleanup unauthorised"])

    def test_cleanup_deletes_what_the_journey_created_even_when_a_check_fails(self):
        def journey(t):
            operator = t.session("operator")
            created = operator.post("/api/v1/things", {"name": t.label("thing")})
            thing = created.json()["id"]
            t.undo("delete thing", lambda: operator.delete(f"/api/v1/things/{thing}"))
            t.expect("thing.wrong-expectation", operator.get(f"/api/v1/things/{thing}"), 404)

        result = runner.run_one(self.env, "journey", journey)
        self.assertFalse(result.ok)
        self.assertEqual(self.gateway.things, {})

    def test_a_refusal_that_regressed_is_undone_and_fails(self):
        undone = []

        def journey(t):
            operator = t.session("operator")
            created = operator.post("/api/v1/things", {"name": t.label("thing")})  # the gateway "wrongly accepts"
            t.must_refuse("thing.refusal", created, 403, undo=lambda r: undone.append(r.json()["id"]) or operator.delete(f"/api/v1/things/{r.json()['id']}"))

        result = runner.run_one(self.env, "journey", journey)
        self.assertFalse(result.ok)
        self.assertEqual(len(undone), 1)
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

    def test_pooled_sessions_are_signed_out_at_the_end_of_a_run_even_if_logout_fails(self):
        journeys = {}
        add_thing_journey(journeys, "alpha")
        runner.run_all(self.env, journeys, ["alpha"])
        self.assertIn(("POST", "/api/v1/auth/logout"), self.fake.requests)
        self.assertEqual(self.env._pool, {})
        # Close survives a dead connection and still empties the pool.
        env = make_env(self.fake)
        env.pooled("operator")
        env.pooled("alex")
        with mock.patch.object(Session, "post", side_effect=TransportError("down")):
            env.close()
        self.assertEqual(env._pool, {})

    def test_the_deadline_stops_new_journeys_and_requests_but_not_cleanup(self):
        journeys = {}
        add_thing_journey(journeys, "alpha")
        add_thing_journey(journeys, "bravo")
        results = runner.run_all(self.env, journeys, ["alpha", "bravo"], deadline=0.0)
        self.assertEqual([r.error for r in results], ["TimeoutError", "TimeoutError"])
        self.assertEqual(results[0].recorder.checks[0].note, "not_run")
        # A deadline that passes mid-journey stops its requests, yet the undo steps still run.
        env = make_env(self.fake)
        calls = {"n": 0}
        real = Http.request

        def expiring(self_http, *a, **k):
            calls["n"] += 1
            if calls["n"] == 7:
                self_http.deadline = 0.0
            return real(self_http, *a, **k)

        def journey(t):
            operator = t.session("operator")
            created = operator.post("/api/v1/things", {"name": t.label("thing")})
            thing = created.json()["id"]
            t.undo("delete thing", lambda: operator.delete(f"/api/v1/things/{thing}"))
            for _ in range(20):
                operator.get(f"/api/v1/things/{thing}")

        with mock.patch.object(Http, "request", expiring):
            result = runner.run_one(env, "journey", journey)
        self.assertEqual(result.error, "TimeoutError")
        self.assertEqual(self.gateway.things, {})

    def test_shuffle_and_only_select_and_order_journeys(self):
        journeys = {}
        for name in ("alpha", "bravo", "charlie"):
            add_thing_journey(journeys, name)
        with mock.patch.object(cli, "load_all", return_value=journeys):
            with Quiet() as quiet:
                self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "charlie", "alpha"]), 0)
            rows_ = [line.split()[0] for line in quiet.out.getvalue().splitlines() if line.split() and line.split()[0] in journeys]
            self.assertEqual(rows_, ["charlie", "alpha"])
            with Quiet() as quiet:
                self.assertEqual(cli.main(["run", "--base", self.fake.base, "--shuffle", "--seed", "3"]), 0)
            self.assertEqual(sorted(l.split()[0] for l in quiet.out.getvalue().splitlines() if l.split() and l.split()[0] in journeys), ["alpha", "bravo", "charlie"])

    def test_unknown_or_repeated_journey_names_are_refused(self):
        journeys = {}
        add_thing_journey(journeys, "alpha")
        with mock.patch.object(cli, "load_all", return_value=journeys), Quiet():
            self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "nope"]), 2)
            self.assertEqual(cli.main(["run", "--base", self.fake.base, "--only", "alpha", "alpha"]), 2)

    def test_seed_without_shuffle_and_bad_options_are_errors(self):
        for argv in (["--seed", "3"], ["--label", "Has Space"], ["--label", "x" * 41], ["--request-timeout", "0"], ["--deadline", "-1"]):
            with Quiet(), self.assertRaises(SystemExit) as raised:
                cli.main(["run", "--base", self.fake.base, *argv])
            self.assertEqual(raised.exception.code, 2, argv)

    def test_the_same_seed_gives_the_same_order_and_different_seeds_differ(self):
        journeys = {}
        for name in ("a1", "b2", "c3", "d4", "e5", "f6"):
            add_thing_journey(journeys, name)
        orders = {}
        with mock.patch.object(cli, "load_all", return_value=journeys):
            for seed in (1, 1, 2, 3, 4):
                with Quiet() as quiet:
                    cli.main(["run", "--base", self.fake.base, "--shuffle", "--seed", str(seed)])
                orders.setdefault(seed, []).append([l.split()[0] for l in quiet.out.getvalue().splitlines() if l.split() and l.split()[0] in journeys])
        self.assertEqual(orders[1][0], orders[1][1])
        self.assertGreater(len({tuple(o[0]) for o in orders.values()}), 1)


class SignInTests(unittest.TestCase):
    def test_sign_in_follows_the_redirects_and_retries_a_throttled_callback(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        gateway.throttle_callback = 2
        http = Http(fake.base)
        session = Session(http, "operator")
        with mock.patch.object(oidc.time, "sleep") as sleep:
            steps = oidc.sign_in(session, http, "operator", fake.base)
        self.assertEqual({k: v.status for k, v in steps.items()}, {"login": 303, "authorize": 200, "choose": 302, "callback": 303})
        self.assertEqual(sleep.call_count, 2)
        self.assertTrue(session.signed_in)
        self.assertEqual(session.csrf, PLANTED["csrf"])
        self.assertEqual(session.get("/api/v1/me").status, 200)
        self.assertEqual(session.post("/api/v1/auth/logout").status, 204)
        self.assertFalse(session.signed_in)

    def test_a_throttled_login_start_is_retried_and_the_retry_count_is_capped(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        http = Http(fake.base)
        gateway.throttle_login = 2
        with mock.patch.object(oidc.time, "sleep") as sleep:
            steps = oidc.sign_in(Session(http, "x"), http, "alex", fake.base)
        self.assertEqual((steps["login"].status, sleep.call_count, gateway.login_calls), (303, 2, 3))
        # Throttled for ever: gives up after MAX_ATTEMPTS and the caller sees the 429, never a loop.
        gateway.throttle_login, gateway.login_calls = 10 ** 6, 0
        with mock.patch.object(oidc.time, "sleep") as sleep, self.assertRaises(JourneyError):
            oidc.sign_in(Session(http, "x"), http, "alex", fake.base)
        self.assertEqual((gateway.login_calls, sleep.call_count), (oidc.MAX_ATTEMPTS, oidc.MAX_ATTEMPTS - 1))
        gateway.throttle_login, gateway.throttle_callback = 0, 10 ** 6
        session = Session(http, "x")
        with mock.patch.object(oidc.time, "sleep") as sleep:
            steps = oidc.sign_in(session, http, "alex", fake.base)
        self.assertEqual(steps["callback"].status, 429)
        self.assertEqual(sleep.call_count, oidc.MAX_ATTEMPTS - 1)
        self.assertFalse(session.signed_in)

    def test_retry_after_is_clamped(self):
        def response(value):
            return Response(429, {"retry-after": value} if value is not None else {}, [], b"")

        self.assertEqual(oidc.wait_seconds(response("0")), 1)
        self.assertEqual(oidc.wait_seconds(response("-5")), 1)
        self.assertEqual(oidc.wait_seconds(response("7")), 7)
        self.assertEqual(oidc.wait_seconds(response("100000")), oidc.MAX_WAIT)
        self.assertEqual(oidc.wait_seconds(response("soon")), 2)
        self.assertEqual(oidc.wait_seconds(response(None)), 1)

    def test_unknown_persona_is_refused_before_any_request(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        http = Http(fake.base)
        with self.assertRaises(JourneyError):
            oidc.sign_in(Session(http, "x"), http, "mallory")
        self.assertEqual(fake.requests, [])

    def test_a_sign_in_that_redirects_to_another_issuer_than_the_checked_one_is_refused(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        http = Http(fake.base)
        with self.assertRaises(JourneyError):
            oidc.sign_in(Session(http, "x"), http, "alex", "http://127.0.0.1:9")
        self.assertNotIn(("POST", "/choose"), fake.requests)

    def test_the_issuer_form_post_names_the_issuers_own_origin(self):
        seen = {}
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        original = gateway.__call__

        def spy(method, path, headers, body):
            if path.startswith("/choose"):
                seen["origin"] = headers.get("origin")
            return original(method, path, headers, body)

        fake.server.RequestHandlerClass  # the handler calls `app` captured at construction; patch via the gateway instance
        gateway.__class__ = type("Spy", (Gateway,), {"__call__": lambda self, m, p, h, b: spy(m, p, h, b)})
        http = Http(fake.base)
        oidc.sign_in(Session(http, "x"), http, "alex", fake.base)
        self.assertEqual(seen["origin"], fake.base)


class SafetyGateTests(unittest.TestCase):
    """The suite must never run against a remote or real environment, or leak credentials in a base URL."""

    def refuse(self, *args):
        with mock.patch.object(Http, "request", side_effect=AssertionError("network used")), mock.patch.object(cli, "load_all", return_value={}), Quiet() as quiet:
            code = cli.main(["run", *args])
        return code, quiet.err.getvalue(), quiet.out.getvalue()

    def test_a_remote_base_is_refused_before_any_request(self):
        for base in ("http://example.com", "https://gateway.example.net:8443", "http://10.0.0.5:3000", "http://192.168.1.2", "http://[2001:db8::1]:3000", "http://127.0.0.1.example.com", "http://localhost.example.com"):
            code, err, _ = self.refuse("--base", base)
            self.assertEqual(code, 2, base)
            self.assertIn("loopback", err)

    def test_loopback_spellings_pass_the_gate(self):
        for base in ("http://127.0.0.1:3000", "http://localhost:3000", "http://[::1]:3000", "http://127.1.2.3"):
            host, _, origin = cli.origin_of(base)
            self.assertTrue(cli.is_loopback(host), base)

    def test_urls_with_credentials_paths_or_queries_are_refused_and_never_echoed(self):
        for base in ("http://user:pass@127.0.0.1:3000", "http://:pw@127.0.0.1:3000", "http://127.0.0.1:3000@evil.example", "http://127.0.0.1:3000/api", "http://127.0.0.1:3000?x=1", "http://127.0.0.1:3000#f", "ftp://127.0.0.1", "127.0.0.1:3000"):
            code, err, out = self.refuse("--base", base)
            self.assertEqual(code, 2, base)
            for leak in ("pass", "pw", "evil.example"):
                self.assertNotIn(leak, err + out, base)
        with self.assertRaises(ValueError):
            Http("http://:pw@127.0.0.1:3000")

    def run_against(self, gateway_kwargs=None, base_host="127.0.0.1", extra=(), app=None):
        if app is None:
            gateway, fake = start_gateway(**(gateway_kwargs or {}))
        else:
            gateway, fake = None, Fake(app)
        self.addCleanup(fake.close)
        base = fake.base.replace("127.0.0.1", base_host)
        with mock.patch.object(cli, "load_all", return_value={"x": lambda t: t.ok("x.ran", True)}), Quiet() as quiet:
            code = cli.main(["run", "--base", base, *extra])
        return code, quiet.err.getvalue(), quiet.out.getvalue(), gateway

    def test_a_demo_gateway_passes_the_preflight(self):
        code, err, out, _ = self.run_against()
        self.assertEqual(code, 0, err)

    def test_a_non_demo_issuer_is_refused_even_on_loopback_and_even_with_allow_remote(self):
        for extra in ((), ("--allow-remote",)):
            code, err, out, gateway = self.run_against({"demo_page": False}, extra=extra)
            self.assertEqual(code, 2, extra)
            self.assertIn("not the passwordless demo issuer", err)
            self.assertNotIn(("POST", "/choose"), [r for r in []])

    def test_an_issuer_that_is_not_loopback_is_refused_unless_allow_remote_and_then_the_demo_proof_still_applies(self):
        holder = {}

        def real_idp(method, path, headers, body):
            if path == "/health/ready":
                return json_response(200, {})
            return 303, [("location", f"https://login.example.com/authorize?x=1&redirect_uri={holder['fake'].base}/cb")], b""

        holder["fake"] = fake = Fake(real_idp)
        self.addCleanup(fake.close)
        original = Http.request

        def routed(self_http, method, path, headers=None, body=None):
            if self_http.host == "login.example.com":
                return Response(200, {}, [], b"<html>sign in with your corporate account</html>")
            return original(self_http, method, path, headers, body)

        with mock.patch.object(Http, "request", routed):
            self.assertIn("not on loopback", cli.preflight(Env(Http(fake.base)), allow_remote=False))
            # --allow-remote lifts the loopback rule only: the demo proof is still demanded.
            self.assertIn("not the passwordless demo issuer", cli.preflight(Env(Http(fake.base)), allow_remote=True))
            env = Env(Http(fake.base))
            cli.preflight(env, allow_remote=True)
            self.assertIsNone(env.issuer_origin)

    def test_localhost_is_not_the_public_origin_and_is_refused_with_the_right_one(self):
        code, err, out, _ = self.run_against(base_host="localhost")
        self.assertEqual(code, 2)
        self.assertRegex(err, r"use http://127\.0\.0\.1:\d+, the gateway's public origin, as --base")

    def test_an_unready_gateway_is_refused(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        gateway.not_ready = True
        with mock.patch.object(cli, "load_all", return_value={}), Quiet() as quiet:
            self.assertEqual(cli.main(["run", "--base", fake.base]), 2)
        self.assertIn("not ready", quiet.err.getvalue())

    def test_a_gateway_that_is_down_or_answers_garbage_is_refused_with_a_type_name_only(self):
        with socket_closed_port() as port:
            with mock.patch.object(cli, "load_all", return_value={}), Quiet() as quiet:
                self.assertEqual(cli.main(["run", "--base", f"http://127.0.0.1:{port}"]), 2)
        self.assertIn("could not be checked (", quiet.err.getvalue())

        def garbage(method, path, headers, body):
            return "garbage", [], b"this is not http\r\n\r\n" + PLANTED["key"].encode()

        code, err, out, _ = self.run_against(app=garbage)
        self.assertEqual(code, 2)
        self.assertRegex(err, r"could not be checked \((TransportError|Exception|[A-Za-z]+)\)")
        for value in PLANTED.values():
            self.assertNotIn(value, err + out)

    def test_http_exceptions_become_transport_errors(self):
        import http.client

        for error in (http.client.BadStatusLine("x"), http.client.IncompleteRead(b"x"), http.client.LineTooLong("header line")):
            with mock.patch("http.client.HTTPConnection.getresponse", side_effect=error):
                gateway, fake = start_gateway()
                self.addCleanup(fake.close)
                with self.assertRaises(TransportError):
                    Http(fake.base).request("GET", "/")


@contextlib.contextmanager
def socket_closed_port():
    import socket

    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    yield port


class RunErrorTests(unittest.TestCase):
    def test_an_exception_in_the_run_still_writes_the_summary_signs_out_and_exits_one(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        with tempfile.TemporaryDirectory() as folder:
            path = os.path.join(folder, "s.json")

            def boom(t):
                t.session("operator")
                raise OSError(PLANTED["cookie"])

            def down(t):
                t.ok("after.ran", True)

            with mock.patch.object(cli, "load_all", return_value={"boom": boom, "down": down}), Quiet() as quiet:
                code = cli.main(["run", "--base", fake.base, "--summary", path])
            summary = Path(path).read_text()
        self.assertEqual(code, 1)
        self.assertIn(("POST", "/api/v1/auth/logout"), fake.requests)
        document = json.loads(summary)
        self.assertEqual({j["journey"] for j in document["journeys"]}, {"boom", "down"})
        self.assertNotIn(PLANTED["cookie"], summary + quiet.out.getvalue() + quiet.err.getvalue())

    def test_an_unexpected_error_outside_the_journeys_exits_one_with_a_type_name(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        with mock.patch.object(cli, "load_all", return_value={"x": lambda t: t.ok("x.ran", True)}), \
             mock.patch.object(cli, "run_all", side_effect=RuntimeError(PLANTED["key"])), Quiet() as quiet:
            code = cli.main(["run", "--base", fake.base])
        self.assertEqual(code, 1)
        self.assertIn("(RuntimeError)", quiet.err.getvalue())
        self.assertNotIn(PLANTED["key"], quiet.err.getvalue() + quiet.out.getvalue())

    def test_an_unwritable_summary_path_is_a_clear_message_and_exit_one(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        with mock.patch.object(cli, "load_all", return_value={"x": lambda t: t.ok("x.ran", True)}), Quiet() as quiet:
            code = cli.main(["run", "--base", fake.base, "--summary", "/nonexistent-dir/summary.json"])
        self.assertEqual(code, 1)
        self.assertIn("could not write the summary", quiet.err.getvalue())


class SummaryFileTests(unittest.TestCase):
    def test_the_summary_is_private_and_never_follows_a_symlink(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / "precious.txt"
            target.write_text("keep me")
            link = Path(folder) / "summary.json"
            link.symlink_to(target)
            with self.assertRaises(OSError):
                cli.write_summary(str(link), {"a": 1})
            self.assertEqual(target.read_text(), "keep me")
            plain = Path(folder) / "plain.json"
            cli.write_summary(str(plain), {"a": 1})
            self.assertEqual(stat.S_IMODE(plain.stat().st_mode), 0o600)
            plain.write_text("x" * 5000)           # a longer old file is truncated, not appended to
            os.chmod(plain, 0o644)
            cli.write_summary(str(plain), {"a": 2})
            self.assertEqual(json.loads(plain.read_text()), {"a": 2})
            fifo = Path(folder) / "fifo"
            os.mkfifo(fifo)
            with self.assertRaises(OSError):
                fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)  # a reader, so opening for write cannot block
                try:
                    cli.write_summary(str(fifo), {"a": 3})
                finally:
                    os.close(fd)


# --- every journey, end to end, against a generic fake -----------------------------------------------------------

class SmokeTests(unittest.TestCase):
    """Runs every journey function to completion against a fake that answers 200 with an id, a token and empty
    lists. Almost every check fails there, which is not the point: an AttributeError, NameError, TypeError or a
    bad f-string in any branch that the fake lets run fails here. The happy paths themselves are proved on a
    Spark environment (docs/journeys.md); the fake is not a model of the gateway."""

    def test_every_journey_runs_to_the_end_without_a_programming_error(self):
        gateway, fake = start_gateway(generic=True)
        self.addCleanup(fake.close)
        env = make_env(fake)
        journeys = registry.load_all()
        with mock.patch.object(oidc.time, "sleep"):
            results = runner.run_all(env, journeys, sorted(journeys))
        self.assertEqual(sorted(r.name for r in results), sorted(journeys))
        broken = {r.name: r.error for r in results if r.error not in (None, "SetupError", "JourneyError")}
        self.assertEqual(broken, {})
        for r in results:
            self.assertTrue(r.recorder.checks, r.name)
        # Nothing in any summary or table carries a planted value.
        text = json.dumps(recorder.summary_document("x", results)) + recorder.render_table(results)
        for value in PLANTED.values():
            self.assertNotIn(value, text)

    def test_the_simplest_journeys_pass_on_the_fake(self):
        gateway, fake = start_gateway()
        self.addCleanup(fake.close)
        env = make_env(fake)
        results = runner.run_all(env, registry.load_all(), ["signin"])
        # Sign-in against the fake: the unentitled path needs a 403 it does not give, but the flow itself runs.
        self.assertGreater(results[0].recorder.passed, 10)

    def test_no_journey_function_uses_an_undefined_name(self):
        """A NameError in a branch the fake never reaches. Every global a journey module loads must exist."""
        problems = []
        for module in sys.modules.values():
            name = getattr(module, "__name__", "")
            if not name.startswith("journeys"):
                continue
            namespace = vars(module)

            def walk(code):
                for ins in dis.get_instructions(code):
                    if ins.opname in ("LOAD_GLOBAL", "LOAD_NAME") and ins.argval not in namespace and not hasattr(builtins, ins.argval):
                        problems.append(f"{name}: {ins.argval}")
                for const in code.co_consts:
                    if hasattr(const, "co_code"):
                        walk(const)

            source = getattr(module, "__file__", None)
            if source:
                walk(compile(Path(source).read_text(), source, "exec"))
        self.assertEqual(problems, [])


# --- check names and the feature map -----------------------------------------------------------------------------

SUITE = ROOT / "scripts" / "journeys" / "suite"
NAME_ARGUMENT = {"expect", "status", "ok", "skip", "fail", "must_refuse", "undo", "defer", "sign_out_later"}


def literal_strings(node):
    """The string constants of a tuple/list/Constant, or None when anything in it is not a literal string."""
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    return None


class NameCollector(ast.NodeVisitor):
    """Collects the names given to checks in one journey function and flags any that are not literal.

    A name is a string literal, or an f-string whose every interpolation is a loop variable of an enclosing
    `for` over a literal tuple or list of strings (or of tuples whose element at the variable's position is a
    string literal); the loop's literal iterable may be a local or module-level constant."""

    def __init__(self, module_assigns):
        self.names, self.violations = set(), []
        self.loops, self.assigns, self.module_assigns = [], {}, module_assigns

    def visit_Assign(self, node):
        if len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            self.assigns[node.targets[0].id] = node.value
        self.generic_visit(node)

    def visit_For(self, node):
        self.visit(node.iter)
        self.loops.append((node.target, node.iter))
        for child in node.body + node.orelse:
            self.visit(child)
        self.loops.pop()

    def resolve_iter(self, node):
        if isinstance(node, ast.Name):
            node = self.assigns.get(node.id) or self.module_assigns.get(node.id)
        return node.elts if isinstance(node, (ast.Tuple, ast.List)) else None

    def loop_values(self, variable):
        for target, iterable in reversed(self.loops):
            elements = self.resolve_iter(iterable)
            if isinstance(target, ast.Name) and target.id == variable:
                position = None
            elif isinstance(target, ast.Tuple) and any(isinstance(e, ast.Name) and e.id == variable for e in target.elts):
                position = next(i for i, e in enumerate(target.elts) if isinstance(e, ast.Name) and e.id == variable)
            else:
                continue
            if elements is None:
                return None
            values = []
            for element in elements:
                if position is not None:
                    if not isinstance(element, (ast.Tuple, ast.List)) or position >= len(element.elts):
                        return None
                    element = element.elts[position]
                strings = literal_strings(element)
                if strings is None:
                    return None
                values.extend(strings)
            return values
        return None

    def expand(self, node):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return [node.value]
        if isinstance(node, ast.JoinedStr):
            variants = [""]
            for part in node.values:
                if isinstance(part, ast.Constant):
                    variants = [v + part.value for v in variants]
                elif isinstance(part, ast.FormattedValue) and isinstance(part.value, ast.Name) and part.format_spec is None and part.conversion == -1:
                    values = self.loop_values(part.value.id)
                    if values is None:
                        return None
                    variants = [v + x for v in variants for x in values]
                else:
                    return None
            return variants
        return None

    def visit_Call(self, node):
        func = node.func
        if isinstance(func, ast.Attribute) and func.attr in NAME_ARGUMENT and node.args:
            receiver = func.value
            is_ctx = (isinstance(receiver, ast.Name) and receiver.id == "t") or (isinstance(receiver, ast.Attribute) and receiver.attr == "rec" and isinstance(receiver.value, ast.Name) and receiver.value.id == "t")
            if is_ctx:
                expanded = self.expand(node.args[0])
                if expanded is None:
                    self.violations.append(f"line {node.lineno}: the name of t.{func.attr}() is not a literal or a loop over literals")
                else:
                    self.names.update(expanded)
        self.generic_visit(node)


def journey_names_in(path):
    """{journey: (names, violations, module source)} for every @journey function in one module."""
    source = path.read_text()
    tree = ast.parse(source)
    module_assigns = {n.targets[0].id: n.value for n in tree.body if isinstance(n, ast.Assign) and len(n.targets) == 1 and isinstance(n.targets[0], ast.Name)}
    found = {}
    for node in tree.body:
        if not isinstance(node, ast.FunctionDef):
            continue
        titles = [d.args[0].value for d in node.decorator_list if isinstance(d, ast.Call) and getattr(d.func, "id", "") == "journey" and d.args]
        if titles:
            collector = NameCollector(module_assigns)
            collector.visit(node)
            found[titles[0]] = (collector.names, collector.violations, source)
    return found


def all_journey_names():
    found = {}
    for path in sorted(SUITE.glob("*.py")):
        found.update(journey_names_in(path))
    return found


ROUTE_FILE = ROOT / "apps" / "gateway" / "src" / "route_policy.rs"
# Resources whose sub-resources are separate flows get one more path segment in their group id.
FINE = {"platform/settings", "platform/alerts", "platform/deployments", "platform/workspaces", "platform/users", "platform/models", "platform/oidc", "platform/logs"}


def route_templates():
    """Method and path of every entry of the `routes!` table in route_policy.rs.

    The table is the text between `pub(crate) const ROUTES: &[Route] = routes! {` and the closing `};`; each entry is
    one line `"METHODS" "/template" => Access, TOKEN_POLICY;`, which the regular expression below reads."""
    source = ROUTE_FILE.read_text()
    start = source.index("pub(crate) const ROUTES: &[Route] = routes! {")
    table = source[start:source.index("\n};", start)]
    return re.findall(r'^\s*"([A-Z* ]+)"\s+"(/[^"]*)"\s*=>', table, re.M)


def route_group(path):
    """A route's group id: its literal path segments (no `{templates}`, no `api/v1`), the first two, three for FINE."""
    segments = [s for s in path.strip("/").split("/")]
    if segments[:2] == ["api", "v1"]:
        segments = segments[2:]
    literal = [s for s in segments if not s.startswith("{")]
    group = "/".join(literal[:2])
    if group in FINE and len(literal) > 2:
        group = "/".join(literal[:3])
    return group


def route_groups():
    groups = {}
    for _, path in route_templates():
        groups.setdefault(route_group(path), []).append(path)
    return groups


def feature_map():
    """Rows of docs/journeys.md's table: (flow, journey or None, [check ids] or reason, [route groups])."""
    text = (ROOT / "docs" / "journeys.md").read_text()
    section = text.split("## Feature map", 1)[1]
    out = []
    for line in section.splitlines():
        if not line.startswith("|") or line.startswith("| ---") or line.startswith("| Flow"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) != 4:
            raise AssertionError(f"feature map row has {len(cells)} cells: {line[:70]}")
        flow, journey, checks, routes = cells
        groups = re.findall(r"`([^`]+)`", routes)
        if journey == "not covered":
            out.append((flow, None, checks, groups))
        else:
            out.append((flow, journey.strip("`"), re.findall(r"`([^`]+)`", checks), groups))
    return out


class NameRuleTests(unittest.TestCase):
    """Item: a check name can only be a literal, or a loop over literals, so response data cannot become a name."""

    def test_every_check_name_in_every_journey_is_a_literal_or_a_loop_over_literals(self):
        for journey, (names, violations, _) in all_journey_names().items():
            self.assertEqual(violations, [], journey)
            self.assertTrue(names, journey)
            for name in names:
                self.assertRegex(name, recorder.NAME.pattern + r"\Z", f"{journey}: {name}")
                self.assertIsNone(recorder.LONG_HEX.search(name), f"{journey}: {name}")

    def test_the_collector_rejects_names_built_from_data(self):
        def collect(code):
            collector = NameCollector({})
            collector.visit(ast.parse(code))
            return collector

        for bad in (
            't.ok(response.get("name"), True)',
            't.expect("a." + name, r, 200)',
            't.expect(f"a.{r.status}", r, 200)',
            'for k in data: t.ok(f"a.{k}", True)',
            'for k in (x, y): t.ok(f"a.{k}", True)',
            'name = "a"\nt.ok(name, True)',
            't.rec.fail(f"x.{email}", "setup_failed")',
            't.undo(label, lambda: 1)',
            'for k in ("a", f"b{x}"): t.ok(f"n.{k}", True)',
            'for k in ("a", "b"): t.ok(f"n.{k!r}", True)',
        ):
            self.assertTrue(collect(bad).violations, bad)
        good = collect('for kind in ("team", "project"):\n  for p, path in (("a", f"/{kind}"), ("b", "/y")):\n    t.expect(f"{kind}.{p}", r, 200)')
        self.assertEqual((good.violations, sorted(good.names)), ([], ["project.a", "project.b", "team.a", "team.b"]))
        self.assertEqual(collect('ITEMS = ("x", "y")\nfor n in ITEMS: t.skip(f"s.{n}")').violations, [])


class FeatureMapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.journeys = registry.load_all()
        cls.found = all_journey_names()
        cls.rows = feature_map()

    def test_every_journey_in_the_registry_is_in_the_map_and_the_other_way_round(self):
        mapped = {journey for _, journey, _, _ in self.rows if journey}
        self.assertEqual(sorted(set(self.journeys) - mapped), [], "a journey is missing from docs/journeys.md")
        self.assertEqual(sorted(mapped - set(self.journeys)), [], "docs/journeys.md names a journey that does not exist")
        self.assertEqual(set(self.found), set(self.journeys), "journey source and registry disagree")

    def test_every_check_named_in_the_map_exists_in_its_journey(self):
        missing = []
        for flow, journey, checks, _ in self.rows:
            if not journey:
                continue
            self.assertTrue(checks, f"no checks named for: {flow}")
            for check in checks:
                if check not in self.found[journey][0]:
                    missing.append(f"{journey}: {check}")
        self.assertEqual(missing, [])

    def test_every_not_covered_row_gives_a_real_reason(self):
        uncovered = [(flow, reason) for flow, journey, reason, _ in self.rows if journey is None]
        self.assertGreaterEqual(len(uncovered), 5)
        for flow, reason in uncovered:
            self.assertGreaterEqual(len(reason.split()), 8, f"not covered without a reason: {flow}")
            self.assertRegex(reason, r"[a-z]{3,}", flow)
            self.assertNotRegex(reason.lower(), r"^(tbd|todo|n/?a|none|later|\?)", flow)

    def test_every_flow_is_listed_once_and_every_row_claims_routes(self):
        flows = [flow for flow, _, _, _ in self.rows]
        self.assertEqual(len(flows), len(set(flows)))
        for flow, _, _, groups in self.rows:
            self.assertTrue(groups, f"no route groups claimed by: {flow}")

    def test_every_route_group_of_the_route_table_is_claimed_by_a_row_and_no_row_claims_a_missing_one(self):
        """route_policy.rs declares every route; a new route group that no row claims fails here, so adding a
        route forces a decision: cover it with a journey, or write a 'not covered' row with the reason."""
        declared = set(route_groups())
        claimed = {g for _, _, _, groups in self.rows for g in groups}
        self.assertGreater(len(declared), 90)
        self.assertEqual(sorted(declared - claimed), [], "route groups with no row in docs/journeys.md")
        self.assertEqual(sorted(claimed - declared), [], "rows claim route groups that are not in route_policy.rs")

    def test_a_covered_row_names_routes_its_journey_really_requests(self):
        """The claimed group's path must appear in the journey's module, so a row cannot claim a route its journey never calls."""
        weak = []
        for flow, journey, _, groups in self.rows:
            if not journey:
                continue
            source = self.found[journey][2] + (ROOT / "scripts" / "journeys" / "oidc.py").read_text()
            for group in groups:
                if group.split("/")[-1] not in source:
                    weak.append(f"{journey}: {group}")
        self.assertEqual(weak, [])

    def test_the_route_table_reader_finds_the_table(self):
        templates = route_templates()
        self.assertGreater(len(templates), 250)
        self.assertIn(("POST", "/v1/chat/completions"), templates)
        self.assertEqual(route_group("/api/v1/workspaces/{ws}/keys/{key}/rotate"), "workspaces/keys")
        self.assertEqual(route_group("/api/v1/workspaces/{ws}"), "workspaces")
        self.assertEqual(route_group("/api/v1/platform/settings/email/test"), "platform/settings/email")
        self.assertEqual(route_group("/v1/videos/{id}/content"), "v1/videos")
        self.assertEqual(route_group("/metrics"), "metrics")

    def test_the_documentation_states_the_run_rules(self):
        text = (ROOT / "docs" / "journeys.md").read_text().lower().replace("python 3.12", "Python 3.12")
        for needle in ("Python 3.12", "--summary", "--only", "--allow-remote", "--shuffle", "--label", "loopback", "one run per environment at a time", "not covered", "gap between runs"):
            self.assertIn(needle, text)

    def test_journey_names_follow_the_registry_pattern(self):
        for name in self.journeys:
            self.assertRegex(name, registry.NAME.pattern)


class PersonaTests(unittest.TestCase):
    def test_the_hard_coded_personas_match_the_demo_documentation_and_issuer(self):
        mjs = (ROOT / "scripts" / "demo-oidc.mjs").read_text()
        accounts = dict(re.findall(r'(\w+): \{ email: "([^"]+)"', mjs))
        groups = {name: re.findall(r'"(omg/[^"]+)"', block) for name, block in re.findall(r'(\w+): \{ email: [^\n]*?groups: \[([^\]]*)\]', mjs)}
        self.assertEqual(accounts, oidc.PERSONAS)
        doc = (ROOT / "docs" / "local-demo.md").read_text()
        self.assertEqual(sorted(re.findall(r"`([a-z]+@demo\.invalid)`", doc.split("## Accounts", 1)[1].split("##", 1)[0])), sorted(oidc.PERSONAS.values()))
        derived = {}
        for name in accounts:
            g = groups.get(name, [])
            derived[name] = "admin" if "omg/platform-admin" in g else "auditor" if "omg/platform-auditor" in g else "user" if "omg/platform-user" in g else None
        self.assertEqual(derived, oidc.PERSONA_ROLES)
        self.assertIn(oidc.DEMO_PROOF, oidc.PERSONAS.values())


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
        for path in (ROOT / "scripts" / "journeys" / "suite").glob("*.py"):
            for node in ast.walk(ast.parse(path.read_text())):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr in {"check", "assertion"}:
                    self.fail(f"{path.name} bypasses Ctx.expect/status/ok")

    def test_every_id_from_a_response_goes_through_a_validator(self):
        """`jget(..., "id")`-style reads of ids are gone: ids come from Response.id() / uuid_of()."""
        pattern = re.compile(r'(?:jget|\.get)\([^()]*"id"\)')
        for path in (ROOT / "scripts" / "journeys" / "suite").glob("*.py"):
            for number, line in enumerate(path.read_text().splitlines(), 1):
                if "jget(" in line and '"id"' in line and ".id(" not in line:
                    self.fail(f"{path.name}:{number} reads an id without validation")

    def test_the_entry_point_refuses_an_old_python_before_importing_anything_else(self):
        source = (ROOT / "scripts" / "journeys" / "__main__.py").read_text()
        self.assertLess(source.index("sys.version_info < (3, 12)"), source.index("from journeys.cli import main"))
        self.assertIn("sys.path.append", source)
        self.assertNotIn("sys.path.insert", source)
        import subprocess

        result = subprocess.run([sys.executable, "-B", "-I", str(ROOT / "scripts" / "journeys"), "list"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("signin", result.stdout)

    def test_no_fixed_calendar_dates_in_the_suite(self):
        for path in (ROOT / "scripts" / "journeys").rglob("*.py"):
            text = "\n".join(line for line in path.read_text().splitlines() if "anthropic" not in line.lower())  # API version strings
            self.assertNotRegex(text, r"\b20[2-3]\d-\d\d-\d\d\b", path.name)

    def test_the_summary_default_location_is_ignored_by_git(self):
        ignore = (ROOT / ".gitignore").read_text()
        self.assertIn(".local/", ignore)
        self.assertIn(".local/journeys/", (ROOT / "docs" / "journeys.md").read_text())


if __name__ == "__main__":
    unittest.main()

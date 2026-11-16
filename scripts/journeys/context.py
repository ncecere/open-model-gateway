"""Per-journey context: sessions, throwaway objects, deferred cleanup, check helpers."""
import secrets

from .client import JourneyError, Session, SetupError, TransportError, uuid_of  # noqa: F401 - re-exported for journeys
from .oidc import sign_in
from .recorder import Recorder


class Secret:
    """A credential that refuses to be printed, formatted or serialised."""

    __slots__ = ("_value",)

    def __init__(self, value):
        self._value = value

    @classmethod
    def of(cls, response, *path):
        """The string under `path` (default `token`) of a response as a Secret, or None."""
        value = response.get(*(path or ("token",)))
        return cls(value) if isinstance(value, str) and value else None

    def reveal(self):
        return self._value

    def __repr__(self):
        return "<secret>"

    __str__ = __repr__

    def __format__(self, spec):
        return "<secret>"

    def __reduce__(self):
        raise TypeError("a secret cannot be serialised")


class Env:
    """What every journey in one run shares: where to talk to and the unique run label."""

    def __init__(self, http, issuer_origin=None, run_id=None):
        self.http = http
        self.issuer_origin = issuer_origin   # set by the preflight: the demo issuer every sign-in must use
        self.run_id = run_id or secrets.token_hex(3)
        self._pool = {}

    def pooled(self, persona):
        """One signed-in session per persona for the whole run (sign-ins are rate limited per address).

        Journeys that sign out or revoke sessions use private sessions instead. Returns None if sign-in failed."""
        if persona not in self._pool:
            session = Session(self.http, persona)
            steps = sign_in(session, self.http, persona, self.issuer_origin)
            if steps["callback"].status not in (302, 303) or not session.signed_in:
                return None
            self._pool[persona] = session
        return self._pool[persona]

    def forget(self, persona):
        """Drop a pooled session (a journey revoked it); the next use signs in again."""
        session = self._pool.pop(persona, None)
        if session is not None and session.signed_in:
            try:
                session.post("/api/v1/auth/logout")
            except Exception:  # noqa: BLE001 - best effort, the session is probably already dead
                pass

    def close(self):
        """Sign the pooled sessions out. Called once at the end of a run, also after an error."""
        self.http.cleaning = True
        for session in list(self._pool.values()):
            try:
                if session.signed_in:
                    session.post("/api/v1/auth/logout")
            except Exception:  # noqa: BLE001 - one dead connection must not leave the others signed in
                pass
        self._pool.clear()


class Ctx:
    OK = (200, 204)

    def __init__(self, env, journey_name):
        self.env = env
        self.journey = journey_name
        self.rec = Recorder()
        self._cleanups = []

    # --- naming -------------------------------------------------------------------------------
    def label(self, what):
        """A unique, recognisable name for a throwaway object (never put in a check name)."""
        return f"journeys-{self.env.run_id}-{self.journey}-{what}"[:120]

    @property
    def run_prefix(self):
        return f"journeys-{self.env.run_id}-"

    # --- checks -------------------------------------------------------------------------------
    def expect(self, name, response, expected):
        return self.rec.check(name, expected, response.status)

    def status(self, name, expected, actual):
        return self.rec.check(name, expected, actual)

    def skip(self, name):
        self.rec.skip(name)

    def ok(self, name, condition):
        return self.rec.assertion(name, bool(condition))

    def must_refuse(self, name, response, expected, undo=None):
        """A check that a mutation is refused, that cleans up after itself if the refusal has regressed.

        If the answer is a success and `undo(response)` is given, the undo is registered, so a broken
        gateway does not leave the change behind."""
        if 200 <= response.status < 300 and undo is not None:
            self.undo("revert accepted refusal", lambda: undo(response), gone=(404, 409))
        return self.expect(name, response, expected)

    # --- sessions -----------------------------------------------------------------------------
    def anonymous(self):
        return Session(self.env.http, "anonymous")

    def session(self, persona, private=False):
        """A signed-in session for a demo persona. Shared across the run unless `private` (then signed out at cleanup)."""
        if persona == "unentitled":
            raise SetupError("the unentitled persona has no session")
        if private:
            session = Session(self.env.http, persona)
            self.sign_out_later("sign out " + persona, session)
            steps = sign_in(session, self.env.http, persona, self.env.issuer_origin)
            if steps["callback"].status not in (302, 303) or not session.signed_in:
                raise SetupError("sign-in did not complete")
            return session
        session = self.env.pooled(persona)
        if session is None:
            raise SetupError("sign-in did not complete")
        return session

    def user_id(self, persona):
        response = self.session(persona).get("/api/v1/me")
        uid = response.id("user", "id") if response.status == 200 else None
        if not uid:
            raise SetupError("cannot read the persona's user id")
        return uid

    def personal_workspace(self, persona):
        response = self.session(persona).get("/api/v1/me")
        for w in (response.get("workspaces", default=[]) if response.status == 200 else []):
            if w.get("kind") == "personal" and uuid_of(w.get("id")):
                return w["id"]
        raise SetupError("cannot find the persona's personal workspace")

    # --- cleanup ------------------------------------------------------------------------------
    def defer(self, label, fn):
        """Register an undo step that is not an HTTP call. Runs last-in first-out, also after a failure."""
        self._cleanups.append((label, fn))

    def undo(self, label, call, gone=()):
        """Register an undo step that makes one HTTP call (`call()` returns the Response).

        It fails visibly: a status outside 200/204 and the explicit `gone` set (already deleted, already
        signed out) is a failed check for this journey."""
        def run():
            response = call()
            status = getattr(response, "status", None)
            if status not in self.OK and status not in gone:
                raise JourneyError("undo step refused")
        self._cleanups.append((label, run))

    def sign_out_later(self, label, session):
        """Sign `session` out at cleanup (nothing to do if it is no longer signed in or was already ended)."""
        def run():
            if session.signed_in and session.post("/api/v1/auth/logout").status not in (204, 401):
                raise JourneyError("undo step refused")
        self._cleanups.append((label, run))

    def run_cleanups(self):
        self.env.http.cleaning = True
        try:
            while self._cleanups:
                label, fn = self._cleanups.pop()
                try:
                    fn()
                except Exception:  # noqa: BLE001 - recorded by label only
                    self.rec.fail("cleanup " + label, "cleanup_failed")
        finally:
            self.env.http.cleaning = False

    # --- throwaway objects --------------------------------------------------------------------
    def team_workspace(self, owner="alex", kind="team", discoverable=False):
        """A throwaway Team or Project owned by `owner`, created by the Platform Admin and disabled at cleanup."""
        operator = self.session("operator")
        body = {"name": self.label(kind), "kind": kind, "owner_user_id": self.user_id(owner)}
        response = operator.post("/api/v1/platform/workspaces", body)
        ws = response.id() if response.status == 200 else None
        if not ws:
            raise SetupError("cannot create the throwaway workspace")
        self.undo("disable workspace", lambda: operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": True}))
        if discoverable:
            if operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}).status != 200:
                raise SetupError("cannot make the throwaway workspace discoverable")
        return ws

    def add_member(self, ws, persona, role="member"):
        response = self.session("operator").post(f"/api/v1/platform/workspaces/{ws}/members", {"user_id": self.user_id(persona), "role": role})
        if response.status != 200:
            raise SetupError("cannot add the throwaway member")

    def mint_key(self, persona, ws, expires_in_days=1, **extra):
        """A throwaway key for `persona`; revoked at cleanup. Returns (key id, Secret token)."""
        session = self.session(persona)
        response = session.post(f"/api/v1/workspaces/{ws}/keys", {"name": self.label("key"), "expires_in_days": expires_in_days, **extra})
        key_id = response.id() if response.status == 200 else None
        token = Secret.of(response)
        if key_id:
            self.undo("revoke key", lambda: session.delete(f"/api/v1/workspaces/{ws}/keys/{key_id}"), gone=(404,))
        if not key_id or not token:
            raise SetupError("cannot create the throwaway key")
        return key_id, token

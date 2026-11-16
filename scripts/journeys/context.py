"""Per-journey context: sessions, throwaway objects, deferred cleanup, check helpers."""
import secrets

from .client import Http, JourneyError, Session, SetupError
from .oidc import PERSONAS, sign_in
from .recorder import Recorder


class Secret:
    """A credential that refuses to be printed, formatted or serialised."""

    __slots__ = ("_value",)

    def __init__(self, value):
        self._value = value

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

    def __init__(self, http, issuer_http=None, run_id=None):
        self.http = http
        self.issuer_http = issuer_http
        self.run_id = run_id or secrets.token_hex(3)
        self._pool = {}

    def pooled(self, persona):
        """One signed-in session per persona for the whole run (sign-ins are rate limited per address).

        Journeys that sign out or revoke sessions use private sessions instead. Returns None if sign-in failed."""
        if persona not in self._pool:
            session = Session(self.http, persona)
            steps = sign_in(session, self.http, persona, self.issuer_http)
            if steps["callback"].status not in (302, 303) or not session.signed_in:
                return None
            self._pool[persona] = session
        return self._pool[persona]

    def forget(self, persona):
        """Drop a pooled session (a journey revoked it); the next use signs in again."""
        session = self._pool.pop(persona, None)
        if session is not None and session.signed_in:
            session.post("/api/v1/auth/logout")

    def close(self):
        """Sign the pooled sessions out. Called once at the end of a run, also after an error."""
        for session in self._pool.values():
            try:
                if session.signed_in:
                    session.post("/api/v1/auth/logout")
            except OSError:
                pass
        self._pool.clear()


class Ctx:
    def __init__(self, env, journey_name):
        self.env = env
        self.journey = journey_name
        self.rec = Recorder()
        self._cleanups = []
        self._user_ids = {}

    # --- naming -------------------------------------------------------------------------------
    def label(self, what):
        """A unique, recognisable name for a throwaway object (never put in a check name)."""
        return f"journeys-{self.env.run_id}-{self.journey}-{what}"[:120]

    # --- checks -------------------------------------------------------------------------------
    def expect(self, name, response, expected):
        return self.rec.check(name, expected, response.status)

    def status(self, name, expected, actual):
        return self.rec.check(name, expected, actual)

    def skip(self, name):
        self.rec.skip(name)

    def ok(self, name, condition):
        return self.rec.assertion(name, bool(condition))

    # --- sessions -----------------------------------------------------------------------------
    def anonymous(self):
        return Session(self.env.http, "anonymous")

    def session(self, persona, private=False):
        """A signed-in session for a demo persona. Shared across the run unless `private` (then signed out at cleanup)."""
        if persona == "unentitled":
            raise SetupError("the unentitled persona has no session")
        if private:
            session = Session(self.env.http, persona)
            self.defer("sign out " + persona, lambda: self._sign_out(session))
            steps = sign_in(session, self.env.http, persona, self.env.issuer_http)
            if steps["callback"].status not in (302, 303) or not session.signed_in:
                raise SetupError("sign-in did not complete")
            return session
        session = self.env.pooled(persona)
        if session is None:
            raise SetupError("sign-in did not complete")
        return session

    @staticmethod
    def _sign_out(session):
        if session.signed_in:
            session.post("/api/v1/auth/logout")

    def user_id(self, persona):
        if persona not in self._user_ids:
            response = self.session(persona).get("/api/v1/me")
            data = response.json() if response.status == 200 else None
            if not isinstance(data, dict) or not isinstance(data.get("user", {}).get("id"), str):
                raise SetupError("cannot read the persona's user id")
            self._user_ids[persona] = data["user"]["id"]
        return self._user_ids[persona]

    # --- cleanup ------------------------------------------------------------------------------
    def defer(self, label, fn):
        """Register an undo step. Runs last-in first-out, also after a failure."""
        self._cleanups.append((label, fn))

    def run_cleanups(self):
        while self._cleanups:
            label, fn = self._cleanups.pop()
            try:
                fn()
            except Exception:
                self.rec.fail("cleanup " + label, "cleanup_failed")

    # --- throwaway objects --------------------------------------------------------------------
    def team_workspace(self, owner="alex", kind="team", discoverable=False):
        """A throwaway Team or Project owned by `owner`, created by the Platform Admin and disabled at cleanup."""
        operator = self.session("operator")
        body = {"name": self.label(kind), "kind": kind, "owner_user_id": self.user_id(owner)}
        response = operator.post("/api/v1/platform/workspaces", body)
        data = response.json() if response.status == 200 else None
        if not isinstance(data, dict) or not isinstance(data.get("id"), str):
            raise SetupError("cannot create the throwaway workspace")
        ws = data["id"]
        self.defer("disable workspace", lambda: self._expect_ok(operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": True})))
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
        data = response.json() if response.status == 200 else None
        if not isinstance(data, dict) or not isinstance(data.get("id"), str) or not isinstance(data.get("token"), str):
            raise SetupError("cannot create the throwaway key")
        key_id = data["id"]
        self.defer("revoke key", lambda: self._revoke_key(session, ws, key_id))
        return key_id, Secret(data["token"])

    @staticmethod
    def _revoke_key(session, ws, key_id):
        response = session.delete(f"/api/v1/workspaces/{ws}/keys/{key_id}")
        if response.status not in (200, 404, 409):
            raise JourneyError("revoke failed")

    @staticmethod
    def _expect_ok(response):
        if response.status != 200:
            raise JourneyError("undo step refused")

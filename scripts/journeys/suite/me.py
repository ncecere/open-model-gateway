"""The signed-in person: /me, summaries, their keys and models, notifications, sessions, export."""
from ..client import Session
from ..oidc import sign_in
from ..registry import journey
from .common import ENTITLED, jget

OWN_READS = (
    ("me", "/api/v1/me"),
    ("summary", "/api/v1/me/summary"),
    ("keys", "/api/v1/me/keys"),
    ("models", "/api/v1/me/models"),
    ("notifications", "/api/v1/me/notifications"),
    ("notification-summary", "/api/v1/me/notifications/summary"),
    ("sessions", "/api/v1/me/sessions"),
    ("access-reviews-inbox", "/api/v1/me/access-reviews"),
    ("access-requests", "/api/v1/me/access-requests"),
    ("access-request-targets", "/api/v1/access-request-targets"),
)


@journey("me-summary")
def me_summary(t):
    anon = t.anonymous()
    for name, path in OWN_READS + (("export", "/api/v1/me/export"),):
        t.expect(f"anonymous.{name}-refused", anon.get(path), 401)

    for persona in ENTITLED:
        session = t.session(persona)
        for name, path in OWN_READS:
            t.expect(f"{persona}.{name}", session.get(path), 200)
        me = session.get("/api/v1/me")
        kinds = {w.get("kind") for w in jget(me, "workspaces", default=[])}
        t.ok(f"{persona}.personal-workspace-listed", "personal" in kinds)
        t.ok(f"{persona}.installation-block", isinstance(jget(me, "installation", "key_max_lifetime_days"), int))
        capabilities = jget(me, "capabilities", default={})
        t.ok(f"{persona}.capabilities-match-role", capabilities.get("platform_write") is (persona == "operator") and capabilities.get("platform_read") is (persona in ("operator", "auditor")))
        summary = session.get("/api/v1/me/summary")
        t.ok(f"{persona}.summary-money-is-text", all(isinstance(v, str) for v in _money(summary)))

    for persona in ("alex", "blair"):
        kinds = {w.get("kind") for w in jget(t.session(persona).get("/api/v1/me"), "workspaces", default=[])}
        t.ok(f"{persona}.shared-workspaces-listed", {"team", "project"} <= kinds)

    # The personal data export is limited to 3 an hour per person, so one persona asks once per run and the
    # documented 429 is accepted when earlier runs used the allowance.
    export = t.session("blair").get("/api/v1/me/export")
    t.status("export.own-data", {200, 429}, export.status)
    if export.status == 200:
        t.ok("export.has-checksum-header", bool(export.header("x-omg-export-sha256")))
    else:
        t.skip("export.has-checksum-header")

    # Sessions: list, revoke one by id, revoke all others. Uses extra sign-ins of the same persona.
    http = t.env.http
    main = t.session("blair", private=True)
    # Revoking "all other sessions" also ends the run's shared Blair session; sign in again afterwards.
    t.defer("renew shared session", lambda: t.env.forget("blair"))
    extras = []
    for _ in range(2):
        extra = Session(http, "blair-extra")
        t.defer("sign out extra", lambda s=extra: s.post("/api/v1/auth/logout") if s.signed_in else None)
        sign_in(extra, http, "blair", t.env.issuer_http)
        extras.append(extra)
    listing = main.get("/api/v1/me/sessions")
    t.expect("sessions.list", listing, 200)
    sessions = jget(listing, "data", default=[])
    others = [s for s in sessions if not s.get("current")]
    t.ok("sessions.lists-others-and-current", len(others) >= 2 and sum(1 for s in sessions if s.get("current")) == 1)
    first = extras[0].get("/api/v1/me/sessions")
    their_id = next((s.get("id") for s in jget(first, "data", default=[]) if s.get("current")), None)
    if their_id:
        t.expect("sessions.revoke-one-no-csrf-refused", main.delete(f"/api/v1/me/sessions/{their_id}", csrf=False), 403)
        t.status("sessions.revoke-one", {200, 204}, main.delete(f"/api/v1/me/sessions/{their_id}").status)
        t.expect("sessions.revoked-session-dead", extras[0].get("/api/v1/me"), 401)
    else:
        t.rec.fail("sessions.found-second-session", "setup_failed")
    t.expect("sessions.other-still-alive", extras[1].get("/api/v1/me"), 200)
    t.status("sessions.revoke-others", {200, 204}, main.post("/api/v1/me/sessions/revoke-others").status)
    t.expect("sessions.revoke-others-kills-others", extras[1].get("/api/v1/me"), 401)
    t.expect("sessions.revoke-others-keeps-mine", main.get("/api/v1/me"), 200)

    # Notifications: marking read needs CSRF.
    t.expect("notifications.mark-read-no-csrf-refused", main.post("/api/v1/me/notifications/read", {"ids": []}, csrf=False), 403)


def _money(response):
    """Every value under a key ending in _microusd (monetary values are text, never numbers)."""
    found = []

    def walk(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key.endswith("_microusd") and value is not None:
                    found.append(value)
                walk(value)
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(response.json())
    return found

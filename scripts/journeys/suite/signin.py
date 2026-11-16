"""Sign-in and sign-out through the real OIDC redirect flow against the demo issuer."""
from ..client import Session
from ..oidc import PERSONAS, sign_in
from ..registry import journey

ROLES = {"operator": "admin", "auditor": "auditor", "alex": "user", "blair": "user"}


@journey("signin")
def signin(t):
    http = t.env.http
    anon = t.anonymous()
    t.expect("anonymous.me-refused", anon.get("/api/v1/me"), 401)
    t.expect("anonymous.logout-refused", anon.post("/api/v1/auth/logout"), 401)
    t.expect("callback.without-login-refused", anon.get("/api/v1/auth/callback?code=none&state=none", csrf=False, origin=False), 400)
    t.expect("callback.bare-refused", anon.get("/api/v1/auth/callback", csrf=False, origin=False), 400)

    for persona in PERSONAS:
        session = Session(http, persona)
        t.defer("sign out " + persona, lambda s=session: s.post("/api/v1/auth/logout") if s.signed_in else None)
        steps = sign_in(session, http, persona, t.env.issuer_http)
        t.expect(f"signin.{persona}.login-redirects", steps["login"], 303)
        t.expect(f"signin.{persona}.issuer-page", steps["authorize"], 200)
        t.expect(f"signin.{persona}.issuer-choice", steps["choose"], 302)
        if persona == "unentitled":
            t.expect("signin.unentitled.denied", steps["callback"], 403)
            t.ok("signin.unentitled.no-session-cookie", not session.signed_in)
            t.expect("signin.unentitled.me-refused", session.get("/api/v1/me"), 401)
            continue
        t.expect(f"signin.{persona}.callback", steps["callback"], 303)
        t.ok(f"signin.{persona}.session-cookies-set", session.signed_in and bool(session.csrf))
        me = session.get("/api/v1/me")
        t.expect(f"signin.{persona}.me", me, 200)
        data = me.json() if me.status == 200 else {}
        user = data.get("user", {}) if isinstance(data, dict) else {}
        t.ok(f"signin.{persona}.identity", user.get("email") == PERSONAS[persona])
        t.ok(f"signin.{persona}.platform-role", user.get("platform_role") == ROLES[persona])
        t.ok(f"signin.{persona}.has-personal-workspace", any(w.get("kind") == "personal" for w in data.get("workspaces", [])))

        stolen = session.clone("replay")
        t.expect(f"signout.{persona}.no-csrf-refused", session.post("/api/v1/auth/logout", csrf=False), 403)
        t.expect(f"signout.{persona}.no-origin-refused", session.post("/api/v1/auth/logout", origin=False), 403)
        t.expect(f"signout.{persona}.foreign-origin-refused", session.post("/api/v1/auth/logout", origin="https://evil.invalid"), 403)
        t.expect(f"signout.{persona}.still-signed-in", session.get("/api/v1/me"), 200)
        t.expect(f"signout.{persona}.logout", session.post("/api/v1/auth/logout"), 204)
        t.expect(f"signout.{persona}.me-after-logout", session.get("/api/v1/me"), 401)
        t.expect(f"signout.{persona}.old-cookie-refused", stolen.get("/api/v1/me"), 401)

    # The callback cannot be replayed: a second sign-in's code is single use.
    first = Session(http, "replay-first")
    t.defer("sign out replay", lambda: first.post("/api/v1/auth/logout") if first.signed_in else None)
    steps = sign_in(first, http, "blair", t.env.issuer_http)
    callback = steps["choose"].header("location")
    path = callback[callback.index("/api/v1/"):] if "/api/v1/" in callback else ""
    replayed = first.get(path, csrf=False, origin=False)
    t.expect("callback.replayed-code-refused", replayed, 400)

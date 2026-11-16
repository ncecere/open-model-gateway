"""Credential families do not cross: inference keys, session cookies and admin API tokens."""
from ..context import Secret
from ..registry import journey
from .common import bearer, contains_secret, inference_get, jget


@journey("credential-boundaries")
def credential_boundaries(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    _, key = t.mint_key("alex", ws)
    operator, auditor, alex = (t.session(p) for p in ("operator", "auditor", "alex"))

    # An inference key is never a management credential.
    for name, path in (
        ("me", "/api/v1/me"),
        ("workspace", f"/api/v1/workspaces/{ws}"),
        ("workspace-keys", f"/api/v1/workspaces/{ws}/keys"),
        ("platform-users", "/api/v1/platform/users"),
        ("platform-workspaces", "/api/v1/platform/workspaces"),
        ("admin-tokens", "/api/v1/platform/admin-tokens"),
    ):
        t.expect(f"key-as-bearer.{name}-refused", anon.get(path, bearer=bearer(key)), 401)
    t.expect("key-as-bearer.mutation-refused", anon.post(f"/api/v1/workspaces/{ws}/keys", {"name": t.label("k"), "expires_in_days": 1}, bearer=bearer(key)), 401)
    t.expect("key-as-session-cookie-refused", anon.get("/api/v1/me", headers={"cookie": "omg_session=" + bearer(key)}), 401)
    t.expect("key-with-session-cookie-refused", operator.get("/api/v1/me", bearer=bearer(key)), 401)

    # Admin API tokens: browser-minted, scoped, never workspace or inference credentials.
    t.expect("admin-token.anonymous-list-refused", anon.get("/api/v1/platform/admin-tokens"), 401)
    t.expect("admin-token.user-cannot-create", alex.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}), 403)
    t.expect("admin-token.auditor-cannot-create", auditor.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}), 403)
    t.expect("admin-token.zero-lifetime-refused", operator.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 0}), 400)
    t.expect("admin-token.no-csrf-refused", operator.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}, csrf=False), 403)
    created = operator.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1})
    t.expect("admin-token.create", created, 200)
    token_id, token = jget(created, "id"), (Secret(created.json()["token"]) if isinstance(jget(created, "token"), str) else None)
    if not token:
        t.rec.fail("admin-token.created-token-missing", "setup_failed")
        return
    t.defer("revoke admin token", lambda: operator.delete(f"/api/v1/platform/admin-tokens/{token_id}"))
    t.ok("admin-token.list-never-returns-token", not contains_secret(operator.get("/api/v1/platform/admin-tokens"), token))
    t.expect("admin-token.read-scope-reads", anon.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 200)
    t.expect("admin-token.read-scope-cannot-write", anon.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}, bearer=bearer(token)), 403)
    t.expect("admin-token.cannot-create-tokens", anon.post("/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}, bearer=bearer(token)), 403)
    t.expect("admin-token.cannot-list-keys", anon.get(f"/api/v1/workspaces/{ws}/keys", bearer=bearer(token)), 403)
    t.expect("admin-token.cannot-use-self-service", anon.get("/api/v1/me", bearer=bearer(token)), 403)
    t.expect("admin-token.is-not-an-inference-key", inference_get(anon, "/v1/models", token), 401)
    t.expect("admin-token.with-session-cookie-refused", operator.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 401)
    t.expect("admin-token.revoke", operator.delete(f"/api/v1/platform/admin-tokens/{token_id}"), 200)
    t.expect("admin-token.revoked-token-refused", anon.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 401)

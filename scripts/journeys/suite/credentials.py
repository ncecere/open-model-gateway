"""Credential families do not cross: inference keys, session cookies and admin API tokens."""
from ..context import Secret
from ..registry import journey
from .common import bearer, contains_secret, inference_get, rows

POLICY = {"requests_per_minute": 30, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "5000000"}


@journey("credential-boundaries")
def credential_boundaries(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    _, key = t.mint_key("alex", ws)
    operator, auditor, alex = (t.session(p) for p in ("operator", "auditor", "alex"))
    new_token = {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}

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
    def revoke(response):
        return operator.delete(f"/api/v1/platform/admin-tokens/{response.id()}") if response.id() else response

    t.expect("admin-token.anonymous-list-refused", anon.get("/api/v1/platform/admin-tokens"), 401)
    t.must_refuse("admin-token.user-cannot-create", alex.post("/api/v1/platform/admin-tokens", new_token), 403, undo=revoke)
    t.must_refuse("admin-token.auditor-cannot-create", auditor.post("/api/v1/platform/admin-tokens", new_token), 403, undo=revoke)
    t.expect("admin-token.zero-lifetime-refused", operator.post("/api/v1/platform/admin-tokens", {**new_token, "expires_in_days": 0}), 400)
    t.must_refuse("admin-token.no-csrf-refused", operator.post("/api/v1/platform/admin-tokens", new_token, csrf=False), 403, undo=revoke)

    created = operator.post("/api/v1/platform/admin-tokens", new_token)
    t.expect("admin-token.create", created, 200)
    token_id = created.id()
    if token_id:
        # Registered before anything else is read from the answer, so a missing token field cannot leave it active.
        t.undo("revoke admin token", lambda: operator.delete(f"/api/v1/platform/admin-tokens/{token_id}"))
    token = Secret.of(created)
    if not token_id or not token:
        t.rec.fail("admin-token.created-token-incomplete", "setup_failed")
        return
    listing = operator.get("/api/v1/platform/admin-tokens")
    t.ok("admin-token.list-never-returns-token", listing.status == 200 and not contains_secret(listing, token))
    t.ok("admin-token.listed", any(x.get("id") == token_id for x in rows(listing) or []))
    t.expect("admin-token.read-scope-reads", anon.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 200)
    t.expect("admin-token.cannot-create-tokens", anon.post("/api/v1/platform/admin-tokens", new_token, bearer=bearer(token)), 403)
    t.expect("admin-token.cannot-list-keys", anon.get(f"/api/v1/workspaces/{ws}/keys", bearer=bearer(token)), 403)
    t.expect("admin-token.cannot-use-self-service", anon.get("/api/v1/me", bearer=bearer(token)), 403)
    t.expect("admin-token.is-not-an-inference-key", inference_get(anon, "/v1/models", token), 401)
    t.expect("admin-token.with-session-cookie-refused", operator.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 401)

    # Scopes: a route gated by `governance:write` (the workspace policy override) refuses a token that only has
    # `platform:read`, accepts one that has the scope, and the scope does not imply the read.
    policy = f"/api/v1/platform/workspaces/{ws}/policy"
    refused = anon.put(policy, POLICY, bearer=bearer(token))
    t.must_refuse("admin-token.read-scope-cannot-write", refused, 403, undo=lambda r: operator.delete(policy))
    t.ok("admin-token.refused-write-changed-nothing", operator.get(policy).get("mode") == "inherit")
    writer = operator.post("/api/v1/platform/admin-tokens", {**new_token, "name": t.label("w"), "scopes": ["governance:write"]})
    t.expect("admin-token.create-scoped", writer, 200)
    writer_id, writer_token = writer.id(), Secret.of(writer)
    if writer_id:
        t.undo("revoke scoped admin token", lambda: operator.delete(f"/api/v1/platform/admin-tokens/{writer_id}"))
    if writer_id and writer_token:
        t.undo("clear policy override", lambda: operator.delete(policy))
        t.expect("admin-token.write-scope-writes", anon.put(policy, POLICY, bearer=bearer(writer_token)), 200)
        t.ok("admin-token.write-scope-took-effect", operator.get(policy).get("effective", "requests_per_minute") == 30)
        t.expect("admin-token.write-scope-does-not-imply-read", anon.get("/api/v1/platform/users?limit=1", bearer=bearer(writer_token)), 403)
        t.expect("admin-token.scoped-revoke", operator.delete(f"/api/v1/platform/admin-tokens/{writer_id}"), 200)
        t.expect("admin-token.scoped-revoked-refused", anon.put(policy, POLICY, bearer=bearer(writer_token)), 401)
    else:
        t.rec.fail("admin-token.scoped-token-incomplete", "setup_failed")

    t.expect("admin-token.revoke", operator.delete(f"/api/v1/platform/admin-tokens/{token_id}"), 200)
    t.expect("admin-token.revoked-token-refused", anon.get("/api/v1/platform/users?limit=1", bearer=bearer(token)), 401)

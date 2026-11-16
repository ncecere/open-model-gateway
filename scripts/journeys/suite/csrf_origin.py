"""Browser-session mutations need the configured Origin and a matching CSRF token."""
from ..registry import journey
from .common import jget

FOREIGN = "https://evil.invalid"


@journey("csrf-origin")
def csrf_origin(t):
    ws = t.team_workspace(owner="alex")
    alex, operator = t.session("alex"), t.session("operator")
    base = f"/api/v1/workspaces/{ws}"

    def keys_count():
        return len(jget(alex.get(f"{base}/keys"), "data", default=[]))

    variants = (
        ("no-csrf", {"csrf": False}),
        ("wrong-csrf", {"csrf": "0" * 32}),
        ("no-origin", {"origin": False}),
        ("foreign-origin", {"origin": FOREIGN}),
        ("null-origin", {"origin": "null"}),
        ("neither", {"csrf": False, "origin": False}),
    )
    before_keys = keys_count()
    original = jget(alex.get(base), "name")
    for name, kw in variants:
        t.expect(f"create-key.{name}-refused", alex.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 1}, **kw), 403)
        t.expect(f"rename-workspace.{name}-refused", alex.patch(base, {"name": t.label("changed")}, **kw), 403)
        t.expect(f"platform-create-workspace.{name}-refused", operator.post("/api/v1/platform/workspaces", {"name": t.label("x"), "kind": "team", "owner_user_id": t.user_id("alex")}, **kw), 403)
        t.expect(f"platform-discoverable.{name}-refused", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}, **kw), 403)
    t.ok("refused-mutations-created-no-key", keys_count() == before_keys)
    t.ok("refused-mutations-did-not-rename", jget(alex.get(base), "name") == original)
    t.ok("refused-mutations-did-not-publish", jget(operator.get(f"/api/v1/platform/workspaces/{ws}"), "discoverable") is False)

    # Controls: reads need neither header; the same mutations with both headers succeed.
    t.expect("get-needs-no-csrf", alex.get(f"{base}/keys", csrf=False, origin=False), 200)
    t.expect("rename-with-both-succeeds", alex.patch(base, {"name": original}), 200)
    # A CSRF token from another person's session is not valid for this one.
    t.expect("other-persons-csrf-refused", alex.patch(base, {"name": t.label("changed")}, csrf=t.session("blair").csrf), 403)
    t.ok("other-persons-csrf-did-not-rename", jget(alex.get(base), "name") == original)

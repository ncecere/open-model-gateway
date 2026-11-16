"""Browser-session mutations need the configured Origin and a matching CSRF token.

Every request here is harmless if the gateway wrongly accepts it: a rename to the same name, a
discoverability flag set to its current value, a workspace body no create accepts, and a key that is
revoked again if it was created."""
from ..registry import journey
from .common import NO_SUCH_ID, rows

FOREIGN = "https://evil.invalid"


@journey("csrf-origin")
def csrf_origin(t):
    ws = t.team_workspace(owner="alex")
    alex, operator = t.session("alex"), t.session("operator")
    base = f"/api/v1/workspaces/{ws}"
    original = alex.get(base).get("name")
    new_key = {"name": t.label("refused"), "expires_in_days": 1}
    blank = {"name": " ", "kind": "team", "owner_user_id": NO_SUCH_ID}

    def key_count():
        listing = rows(alex.get(f"{base}/keys"))
        return None if listing is None else len(listing)

    variants = (
        ("no-csrf", {"csrf": False}),
        ("wrong-csrf", {"csrf": "0" * 32}),
        ("no-origin", {"origin": False}),
        ("foreign-origin", {"origin": FOREIGN}),
        ("null-origin", {"origin": "null"}),
        ("neither", {"csrf": False, "origin": False}),
    )
    before_keys = key_count()
    for name, kw in variants:
        revoke = lambda r: alex.delete(f"{base}/keys/{r.id()}") if r.id() else r  # noqa: E731
        t.must_refuse(f"create-key.{name}-refused", alex.post(f"{base}/keys", new_key, **kw), 403, undo=revoke)
        t.expect(f"rename-workspace.{name}-refused", alex.patch(base, {"name": original}, **kw), 403)
        t.expect(f"platform-create-workspace.{name}-refused", operator.post("/api/v1/platform/workspaces", blank, **kw), 403)
        t.expect(f"platform-discoverable.{name}-refused", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": False}, **kw), 403)
    after_keys = key_count()
    t.ok("refused-mutations-created-no-key", before_keys is not None and after_keys == before_keys)
    t.ok("refused-mutations-left-the-name", alex.get(base).get("name") == original)
    t.ok("refused-mutations-left-it-unpublished", operator.get(f"/api/v1/platform/workspaces/{ws}").get("discoverable") is False)

    # Controls: reads need neither header; the same mutations with both headers succeed.
    t.expect("get-needs-no-csrf", alex.get(f"{base}/keys", csrf=False, origin=False), 200)
    t.expect("rename-with-both-succeeds", alex.patch(base, {"name": original}), 200)
    # A CSRF token from another person's session is not valid for this one.
    t.expect("other-persons-csrf-refused", alex.patch(base, {"name": original}, csrf=t.session("blair").csrf), 403)

"""Workspace listing, access, rename, disable and the boundaries between workspaces (Teams, Projects, personal)."""
from ..registry import journey
from .common import NO_SUCH_ID, rows

# A body no valid create accepts: a refused attempt that has regressed is a 400, never a new workspace.
BLANK_WORKSPACE = {"name": " ", "kind": "team", "owner_user_id": NO_SUCH_ID}


@journey("workspaces")
def workspaces(t):
    anon = t.anonymous()
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    for kind in ("team", "project"):  # every check below runs for both shared kinds
        ws = t.team_workspace(owner="alex", kind=kind)
        base = f"/api/v1/workspaces/{ws}"

        t.expect(f"{kind}.anonymous-refused", anon.get(base), 401)
        detail = alex.get(base)
        t.expect(f"{kind}.owner-reads", detail, 200)
        t.ok(f"{kind}.owner-kind-matches", detail.get("kind") == kind)
        t.ok(f"{kind}.owner-capabilities", detail.get("capabilities", "manage_members") is True and detail.get("capabilities", "manage_policy") is True)
        t.expect(f"{kind}.owner-reads-access", alex.get(f"{base}/access"), 200)
        t.expect(f"{kind}.owner-lists-catalog", alex.get(f"{base}/catalog"), 200)
        t.expect(f"{kind}.owner-lists-available-models", alex.get(f"{base}/available-models"), 200)
        t.expect(f"{kind}.non-member-refused", blair.get(base), 403)
        t.expect(f"{kind}.non-member-access-refused", blair.get(f"{base}/access"), 403)
        metadata = operator.get(base)
        t.expect(f"{kind}.platform-admin-reads-metadata", metadata, 200)
        t.ok(f"{kind}.platform-admin-has-no-workspace-powers", metadata.get("capabilities", "manage_keys") is False and metadata.get("capabilities", "manage_members") is False)
        t.expect(f"{kind}.auditor-reads-metadata", auditor.get(base), 200)

        mine = rows(alex.get("/api/v1/me"), "workspaces")
        t.ok(f"{kind}.listed-for-owner", mine is not None and any(w.get("id") == ws for w in mine))
        theirs = rows(blair.get("/api/v1/me"), "workspaces")
        t.ok(f"{kind}.not-listed-for-non-member", theirs is not None and not any(w.get("id") == ws for w in theirs))
        listing = operator.get("/api/v1/platform/workspaces?limit=200")
        staff = rows(listing)
        t.expect(f"{kind}.platform-list", listing, 200)
        t.ok(f"{kind}.platform-list-has-it", staff is not None and any(w.get("id") == ws and w.get("kind") == kind for w in staff))
        t.expect(f"{kind}.platform-detail", operator.get(f"/api/v1/platform/workspaces/{ws}"), 200)

        renamed = t.label("renamed")
        t.expect(f"{kind}.rename-by-non-member-refused", blair.patch(base, {"name": t.label("nope")}), 403)
        t.expect(f"{kind}.rename-by-platform-admin-refused", operator.patch(base, {"name": t.label("nope")}), 403)
        t.expect(f"{kind}.rename", alex.patch(base, {"name": renamed}), 200)
        t.ok(f"{kind}.rename-visible", alex.get(base).get("name") == renamed)
        t.expect(f"{kind}.disable-by-owner-refused", alex.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": False}), 403)
        t.expect(f"{kind}.disable", operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": True}), 200)
        t.expect(f"{kind}.disabled-is-gone-for-owner", alex.get(base), 404)
        t.expect(f"{kind}.reenable", operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": False}), 200)
        t.expect(f"{kind}.reenabled-is-back", alex.get(base), 200)

    t.expect("workspace.unknown-id", alex.get(f"/api/v1/workspaces/{NO_SUCH_ID}"), 404)
    t.expect("workspace.malformed-id", alex.get("/api/v1/workspaces/not-a-uuid"), 404)
    t.expect("platform.list-anonymous-refused", anon.get("/api/v1/platform/workspaces"), 401)
    t.expect("platform.list-member-refused", alex.get("/api/v1/platform/workspaces"), 403)
    t.expect("platform.list-auditor", auditor.get("/api/v1/platform/workspaces"), 200)

    # Creation is platform-admin only; a refusal that has regressed must not leave a workspace behind.
    t.expect("platform.create-by-member-refused", alex.post("/api/v1/platform/workspaces", BLANK_WORKSPACE), 403)
    t.expect("platform.create-by-auditor-refused", auditor.post("/api/v1/platform/workspaces", BLANK_WORKSPACE), 403)
    t.expect("platform.create-without-csrf-refused", operator.post("/api/v1/platform/workspaces", BLANK_WORKSPACE, csrf=False), 403)
    invalid = operator.post("/api/v1/platform/workspaces", {"name": t.label("personal"), "kind": "personal", "owner_user_id": t.user_id("alex")})
    t.must_refuse("platform.create-invalid-kind", invalid, 400, undo=lambda r: operator.patch(f"/api/v1/platform/workspaces/{r.id()}", {"disabled": True}) if r.id() else r)

    # Personal workspaces are owner-private: no other person, staff included, reads their contents.
    mine = t.personal_workspace("alex")
    t.expect("personal.owner-reads", alex.get(f"/api/v1/workspaces/{mine}"), 200)
    t.expect("personal.owner-reads-keys", alex.get(f"/api/v1/workspaces/{mine}/keys"), 200)
    for persona, session in (("blair", blair), ("operator", operator), ("auditor", auditor)):
        t.expect(f"personal.{persona}-cannot-read", session.get(f"/api/v1/workspaces/{mine}"), 404)
        t.expect(f"personal.{persona}-cannot-list-keys", session.get(f"/api/v1/workspaces/{mine}/keys"), 404)
        t.expect(f"personal.{persona}-cannot-read-requests", session.get(f"/api/v1/workspaces/{mine}/requests"), 404)
    t.expect("personal.admin-platform-detail-is-missing", operator.get(f"/api/v1/platform/workspaces/{mine}"), 404)
    # Personal workspaces cannot be shared. If this regressed, the invitation it creates is revoked again.
    invitation = alex.post(f"/api/v1/workspaces/{mine}/invitations", {"email": "blair@demo.invalid", "role": "member"})
    t.must_refuse("personal.cannot-be-shared", invitation, 400, undo=lambda r: alex.delete(f"/api/v1/workspaces/{mine}/invitations/{r.id()}") if r.id() else r)

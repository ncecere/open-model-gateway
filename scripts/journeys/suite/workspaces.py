"""Workspace listing, access, rename, disable and the boundaries between workspaces."""
from ..registry import journey
from .common import jget


def _personal(t, persona):
    return next(w["id"] for w in jget(t.session(persona).get("/api/v1/me"), "workspaces", default=[]) if w.get("kind") == "personal")


@journey("workspaces")
def workspaces(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))

    t.expect("workspace.anonymous-refused", anon.get(f"/api/v1/workspaces/{ws}"), 401)
    detail = alex.get(f"/api/v1/workspaces/{ws}")
    t.expect("workspace.owner-reads", detail, 200)
    t.ok("workspace.owner-capabilities", jget(detail, "capabilities", "manage_members") is True and jget(detail, "capabilities", "manage_policy") is True)
    t.expect("workspace.owner-reads-access", alex.get(f"/api/v1/workspaces/{ws}/access"), 200)
    t.expect("workspace.owner-lists-catalog", alex.get(f"/api/v1/workspaces/{ws}/catalog"), 200)
    t.expect("workspace.owner-lists-available-models", alex.get(f"/api/v1/workspaces/{ws}/available-models"), 200)
    t.expect("workspace.non-member-refused", blair.get(f"/api/v1/workspaces/{ws}"), 403)
    t.expect("workspace.non-member-access-refused", blair.get(f"/api/v1/workspaces/{ws}/access"), 403)
    metadata = operator.get(f"/api/v1/workspaces/{ws}")
    t.expect("workspace.platform-admin-reads-metadata", metadata, 200)
    t.ok("workspace.platform-admin-has-no-workspace-powers", jget(metadata, "capabilities", "manage_keys") is False and jget(metadata, "capabilities", "manage_members") is False)
    t.expect("workspace.auditor-reads-metadata", auditor.get(f"/api/v1/workspaces/{ws}"), 200)
    t.expect("workspace.unknown-id", alex.get("/api/v1/workspaces/00000000-0000-4000-8000-000000000000"), 404)
    t.expect("workspace.malformed-id", alex.get("/api/v1/workspaces/not-a-uuid"), 404)

    # Listing: a person sees the workspaces they belong to; the platform list is staff only.
    t.ok("workspace.listed-for-owner", any(w.get("id") == ws for w in jget(alex.get("/api/v1/me"), "workspaces", default=[])))
    t.ok("workspace.not-listed-for-non-member", not any(w.get("id") == ws for w in jget(blair.get("/api/v1/me"), "workspaces", default=[])))
    listing = operator.get("/api/v1/platform/workspaces?limit=200")
    t.expect("platform.workspace-list", listing, 200)
    t.ok("platform.workspace-list-has-throwaway", any(w.get("id") == ws for w in jget(listing, "data", default=[])))
    t.expect("platform.workspace-list-auditor", auditor.get("/api/v1/platform/workspaces"), 200)
    t.expect("platform.workspace-list-member-refused", alex.get("/api/v1/platform/workspaces"), 403)
    t.expect("platform.workspace-list-anonymous-refused", anon.get("/api/v1/platform/workspaces"), 401)
    t.expect("platform.workspace-detail", operator.get(f"/api/v1/platform/workspaces/{ws}"), 200)

    # Rename is a workspace-admin action; a non-member and a platform admin cannot.
    renamed = t.label("renamed")
    t.expect("workspace.rename-by-non-member-refused", blair.patch(f"/api/v1/workspaces/{ws}", {"name": "nope"}), 403)
    t.expect("workspace.rename-by-platform-admin-refused", operator.patch(f"/api/v1/workspaces/{ws}", {"name": "nope"}), 403)
    t.expect("workspace.rename", alex.patch(f"/api/v1/workspaces/{ws}", {"name": renamed}), 200)
    t.ok("workspace.rename-visible", jget(alex.get(f"/api/v1/workspaces/{ws}"), "name") == renamed)

    # Creation and disabling are platform-admin only.
    t.expect("platform.create-by-member-refused", alex.post("/api/v1/platform/workspaces", {"name": t.label("x"), "kind": "team", "owner_user_id": t.user_id("alex")}), 403)
    t.expect("platform.create-by-auditor-refused", auditor.post("/api/v1/platform/workspaces", {"name": t.label("x"), "kind": "team", "owner_user_id": t.user_id("alex")}), 403)
    t.expect("platform.disable-by-owner-refused", alex.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": True}), 403)
    t.expect("platform.create-invalid-kind", operator.post("/api/v1/platform/workspaces", {"name": t.label("x"), "kind": "personal", "owner_user_id": t.user_id("alex")}), 400)
    t.expect("platform.disable", operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": True}), 200)
    t.expect("workspace.disabled-is-gone-for-owner", alex.get(f"/api/v1/workspaces/{ws}"), 404)
    t.expect("platform.reenable", operator.patch(f"/api/v1/platform/workspaces/{ws}", {"disabled": False}), 200)
    t.expect("workspace.reenabled-is-back", alex.get(f"/api/v1/workspaces/{ws}"), 200)

    # Personal workspaces are owner-private: no other person, staff included, reads their contents.
    mine = _personal(t, "alex")
    t.expect("personal.owner-reads", alex.get(f"/api/v1/workspaces/{mine}"), 200)
    t.expect("personal.owner-reads-keys", alex.get(f"/api/v1/workspaces/{mine}/keys"), 200)
    for persona, session in (("blair", blair), ("operator", operator), ("auditor", auditor)):
        t.expect(f"personal.{persona}-cannot-read", session.get(f"/api/v1/workspaces/{mine}"), 404)
        t.expect(f"personal.{persona}-cannot-list-keys", session.get(f"/api/v1/workspaces/{mine}/keys"), 404)
        t.expect(f"personal.{persona}-cannot-read-requests", session.get(f"/api/v1/workspaces/{mine}/requests"), 404)
    t.expect("personal.cannot-be-shared", alex.post(f"/api/v1/workspaces/{mine}/invitations", {"email": "blair@demo.invalid", "role": "member"}), 400)

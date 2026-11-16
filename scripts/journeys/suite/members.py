"""Members and invitations on a throwaway Team."""
from ..context import Secret
from ..registry import journey
from .common import rows


def member_ids(response):
    listing = rows(response)
    return None if listing is None else {m.get("user_id") for m in listing}


@journey("members-invitations")
def members_invitations(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    base = f"/api/v1/workspaces/{ws}"
    blair_id, alex_id = t.user_id("blair"), t.user_id("alex")

    t.expect("members.anonymous-refused", anon.get(f"{base}/members"), 401)
    t.expect("members.owner-lists", alex.get(f"{base}/members"), 200)
    t.expect("members.non-member-refused", blair.get(f"{base}/members"), 403)
    t.expect("members.invitations-non-member-refused", blair.get(f"{base}/invitations"), 403)

    # The picker finds people who are not members yet.
    found = alex.get(f"{base}/member-candidates?q=blair")
    t.expect("candidates.search", found, 200)
    t.ok("candidates.finds-non-member", any(c.get("user_id") == blair_id for c in rows(found) or []))
    t.expect("candidates.non-member-refused", blair.get(f"{base}/member-candidates?q=blair"), 403)
    t.expect("candidates.control-character-refused", alex.get(f"{base}/member-candidates?q=%00"), 400)

    # Manual membership: add, appears, cannot manage, remove.
    t.expect("members.bad-role-refused", alex.post(f"{base}/members", {"user_id": blair_id, "role": "superuser"}), 400)
    t.expect("members.add-by-non-member-refused", blair.post(f"{base}/members", {"user_id": blair_id, "role": "member"}), 403)
    t.expect("members.add", alex.post(f"{base}/members", {"user_id": blair_id, "role": "member"}), 200)
    t.undo("remove blair", lambda: alex.delete(f"{base}/members/{blair_id}"), gone=(404, 409))
    t.ok("members.added-person-listed", blair_id in (member_ids(alex.get(f"{base}/members")) or set()))
    candidates = rows(alex.get(f"{base}/member-candidates?q=blair"))
    t.ok("candidates.excludes-members", candidates is not None and not any(c.get("user_id") == blair_id for c in candidates))
    t.expect("members.member-sees-workspace", blair.get(base), 200)
    t.expect("members.member-cannot-list-members", blair.get(f"{base}/members"), 403)
    t.expect("members.member-cannot-add", blair.post(f"{base}/members", {"user_id": t.user_id("operator"), "role": "member"}), 403)
    after_refusal = member_ids(alex.get(f"{base}/members"))
    t.ok("members.refused-add-left-no-trace", after_refusal is not None and t.user_id("operator") not in after_refusal)
    t.expect("members.member-cannot-remove-owner", blair.delete(f"{base}/members/{alex_id}"), 403)
    t.expect("members.last-owner-protected", alex.delete(f"{base}/members/{alex_id}"), 409)
    t.ok("members.owner-still-listed", alex_id in (member_ids(alex.get(f"{base}/members")) or set()))
    t.expect("members.remove", alex.delete(f"{base}/members/{blair_id}"), 200)
    t.expect("members.removed-person-loses-access", blair.get(base), 403)

    # Platform views of the same members.
    t.expect("platform-members.admin-lists", operator.get(f"/api/v1/platform/workspaces/{ws}/members"), 200)
    t.expect("platform-members.auditor-lists", auditor.get(f"/api/v1/platform/workspaces/{ws}/members"), 200)
    t.expect("platform-members.user-refused", blair.get(f"/api/v1/platform/workspaces/{ws}/members"), 403)
    t.expect("platform-members.candidates", operator.get(f"/api/v1/platform/workspaces/{ws}/member-candidates?q=blair"), 200)
    t.expect("platform-members.auditor-cannot-add", auditor.post(f"/api/v1/platform/workspaces/{ws}/members", {"user_id": blair_id, "role": "member"}), 403)
    t.ok("platform-members.refused-add-left-no-trace", blair_id not in (member_ids(operator.get(f"/api/v1/platform/workspaces/{ws}/members")) or {blair_id}))
    t.expect("platform-members.admin-adds", operator.post(f"/api/v1/platform/workspaces/{ws}/members", {"user_id": blair_id, "role": "member"}), 200)
    t.expect("platform-members.admin-removes", operator.delete(f"/api/v1/platform/workspaces/{ws}/members/{blair_id}"), 200)

    # Invitations: create, list, accept by the invitee only, single use, revoke.
    invite = {"email": "blair@demo.invalid", "role": "member"}
    t.expect("invitations.anonymous-refused", anon.get(f"{base}/invitations"), 401)
    t.expect("invitations.member-cannot-create", blair.post(f"{base}/invitations", invite), 403)
    t.expect("invitations.bad-email-refused", alex.post(f"{base}/invitations", {"email": "not-an-address", "role": "member"}), 400)
    t.expect("invitations.owner-role-refused", alex.post(f"{base}/invitations", {**invite, "role": "owner"}), 400)
    t.expect("invitations.no-csrf-refused", alex.post(f"{base}/invitations", {"email": "not-an-address", "role": "member"}, csrf=False), 403)
    invited = alex.post(f"{base}/invitations", invite)
    t.expect("invitations.create", invited, 200)
    inv_id, code = invited.id(), Secret.of(invited)
    if inv_id:
        t.undo("revoke invitation", lambda: alex.delete(f"{base}/invitations/{inv_id}"), gone=(404, 409))
    if not inv_id or not code:
        t.rec.fail("invitations.created-invitation-missing", "setup_failed")
        return
    t.ok("invitations.delivery-state-reported", invited.get("email_delivery") in ("not_configured", "sent", "failed"))
    listed = alex.get(f"{base}/invitations")
    t.expect("invitations.list", listed, 200)
    t.ok("invitations.list-has-pending", any(i.get("id") == inv_id and i.get("accepted_at") is None for i in rows(listed) or []))
    t.ok("invitations.list-never-returns-code", listed.status == 200 and code.reveal().encode() not in listed.body)
    t.expect("invitations.anonymous-accept-refused", anon.post("/api/v1/invitations/accept", {"token": code.reveal()}), 401)
    t.expect("invitations.wrong-person-refused", operator.post("/api/v1/invitations/accept", {"token": code.reveal()}), 404)
    t.expect("invitations.inviter-cannot-accept-for-invitee", alex.post("/api/v1/invitations/accept", {"token": code.reveal()}), 404)
    t.expect("invitations.garbage-code-refused", blair.post("/api/v1/invitations/accept", {"token": "x" * 64}), 400)
    t.expect("invitations.accept", blair.post("/api/v1/invitations/accept", {"token": code.reveal()}), 200)
    t.undo("remove invited member", lambda: alex.delete(f"{base}/members/{blair_id}"), gone=(404, 409))
    t.expect("invitations.accepted-person-has-access", blair.get(base), 200)
    t.expect("invitations.single-use", blair.post("/api/v1/invitations/accept", {"token": code.reveal()}), 404)
    t.expect("invitations.cleanup-member", alex.delete(f"{base}/members/{blair_id}"), 200)

    again = alex.post(f"{base}/invitations", invite)
    t.expect("invitations.create-second", again, 200)
    second, second_code = again.id(), Secret.of(again)
    if second:
        t.undo("revoke second invitation", lambda: alex.delete(f"{base}/invitations/{second}"), gone=(404, 409))
    if second and second_code:
        t.expect("invitations.revoke", alex.delete(f"{base}/invitations/{second}"), 200)
        t.expect("invitations.revoked-code-refused", blair.post("/api/v1/invitations/accept", {"token": second_code.reveal()}), 404)
        t.expect("invitations.revoked-gives-no-access", blair.get(base), 403)

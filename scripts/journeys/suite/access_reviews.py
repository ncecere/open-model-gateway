"""Access reviews: start a small campaign on a throwaway Team, decide, export, close."""
from ..registry import journey
from .common import jget

PREFIX = "journeys-"


@journey("access-reviews")
def access_reviews(t):
    anon = t.anonymous()
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    base = "/api/v1/platform/access-reviews"

    # Campaigns left open by an earlier run that was killed would count towards the limit of 20.
    for campaign in jget(operator.get(f"{base}?limit=100"), "data", default=[]):
        if str(campaign.get("name", "")).startswith(PREFIX) and campaign.get("closed_at") is None:
            operator.post(f"{base}/{campaign['id']}/close")

    t.expect("review.anonymous-list-refused", anon.get(base), 401)
    t.expect("review.member-list-refused", alex.get(base), 403)
    t.expect("review.member-cannot-start", alex.post(base, {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws]}), 403)
    t.expect("review.auditor-cannot-start", auditor.post(base, {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws]}), 403)
    t.expect("review.no-csrf-refused", operator.post(base, {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws]}, csrf=False), 403)
    t.expect("review.blank-name-refused", operator.post(base, {"name": "  ", "subjects": ["workspace_memberships"], "workspace_ids": [ws]}), 400)
    t.expect("review.no-subjects-refused", operator.post(base, {"name": t.label("review"), "subjects": [], "workspace_ids": [ws]}), 400)
    t.expect("review.unknown-subject-refused", operator.post(base, {"name": t.label("review"), "subjects": ["everything"], "workspace_ids": [ws]}), 400)

    started = operator.post(base, {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws], "due_in_days": 1})
    t.expect("review.start", started, 200)
    cid = jget(started, "id")
    if not cid:
        t.rec.fail("review.started-campaign-missing", "setup_failed")
        return
    closed = {"done": False}

    def close():
        if not closed["done"]:
            operator.post(f"{base}/{cid}/close")

    t.defer("close campaign", close)
    t.ok("review.counts-two-memberships", jget(started, "counts", "by_kind", "workspace_membership") == 2)
    t.expect("review.list-admin", operator.get(base), 200)
    t.ok("review.listed", any(c.get("id") == cid for c in jget(operator.get(f"{base}?limit=100"), "data", default=[])))
    t.expect("review.list-auditor", auditor.get(base), 200)
    t.expect("review.detail-auditor", auditor.get(f"{base}/{cid}"), 200)

    items = jget(operator.get(f"{base}/{cid}/items?limit=100"), "data", default=[])
    t.expect("review.items-admin", operator.get(f"{base}/{cid}/items"), 200)
    t.expect("review.items-auditor", auditor.get(f"{base}/{cid}/items"), 200)
    t.expect("review.items-member-refused", alex.get(f"{base}/{cid}/items"), 403)
    blair_item = next((i["id"] for i in items if i.get("user_id") == t.user_id("blair")), None)
    alex_item = next((i["id"] for i in items if i.get("user_id") == t.user_id("alex")), None)
    if not blair_item or not alex_item:
        t.rec.fail("review.snapshot-items-missing", "setup_failed")
        return

    # Reviewers: the workspace owner decides the member's item; nobody decides their own.
    inbox = alex.get("/api/v1/me/access-reviews?limit=200")
    t.expect("review.reviewer-inbox", inbox, 200)
    t.ok("review.owner-sees-members-item", any(i.get("id") == blair_item for i in jget(inbox, "data", default=[])))
    t.ok("review.owner-does-not-see-own-item", not any(i.get("id") == alex_item for i in jget(inbox, "data", default=[])))
    t.expect("review.member-cannot-decide-own-item", blair.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}), 404)
    t.expect("review.owner-cannot-decide-own-item", alex.post(f"/api/v1/me/access-reviews/{alex_item}/decision", {"decision": "keep"}), 404)
    t.expect("review.decision-no-csrf-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}, csrf=False), 403)
    t.expect("review.unknown-decision-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "shrug"}), 400)
    flagged = alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "flag_idp"})
    t.expect("review.flag-requires-group-access", flagged, 409)
    t.ok("review.flag-reason", flagged.error_reason == "access_review_flag_requires_group")
    decided = alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"})
    t.expect("review.decide-keep", decided, 200)
    t.ok("review.decision-recorded", jget(decided, "decision") == "keep")
    t.ok("review.counts-reflect-decision", jget(operator.get(f"{base}/{cid}"), "counts", "by_decision", "keep") == 1)
    t.expect("review.clear-reviewer", operator.post(f"{base}/{cid}/items/{blair_item}/reviewer", {"user_id": None}), 200)
    t.expect("review.assign-by-member-refused", alex.post(f"{base}/{cid}/items/{blair_item}/reviewer", {"user_id": None}), 403)

    # Export.
    export = operator.get(f"{base}/{cid}/export")
    t.expect("review.export-admin", export, 200)
    t.ok("review.export-is-csv", export.header("content-type").startswith("text/csv") and export.body.startswith(b"item_id,"))
    t.ok("review.export-not-cached", "no-store" in export.header("cache-control"))
    t.expect("review.export-auditor", auditor.get(f"{base}/{cid}/export"), 200)
    t.expect("review.export-member-refused", alex.get(f"{base}/{cid}/export"), 403)
    t.expect("review.export-anonymous-refused", anon.get(f"{base}/{cid}/export"), 401)

    # Close.
    t.expect("review.close-by-auditor-refused", auditor.post(f"{base}/{cid}/close"), 403)
    t.expect("review.close-by-member-refused", alex.post(f"{base}/{cid}/close"), 403)
    t.expect("review.close", operator.post(f"{base}/{cid}/close"), 200)
    closed["done"] = True
    again = operator.post(f"{base}/{cid}/close")
    t.expect("review.close-twice-refused", again, 409)
    t.ok("review.close-twice-reason", again.error_reason == "access_review_closed")
    t.expect("review.decision-after-close-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}), 404)
    t.ok("review.closed-campaign-leaves-inbox", not any(i.get("id") == blair_item for i in jget(alex.get("/api/v1/me/access-reviews?limit=200"), "data", default=[])))
    t.ok("review.detail-shows-closed", jget(auditor.get(f"{base}/{cid}"), "closed_at") is not None)

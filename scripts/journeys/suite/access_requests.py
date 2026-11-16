"""Self-service access requests: request, withdraw, deny, approve by the workspace admin or a Platform Admin."""
import uuid

from ..registry import journey
from .common import jget


def _request(session, ws, reason="journey"):
    return session.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": reason, "idempotency_key": uuid.uuid4().hex})


def _mark_read(session):
    """Decisions notify the requester; leave their feed as it was found (all read)."""
    unread = [n["id"] for n in jget(session.get("/api/v1/me/notifications"), "data", default=[]) if not n.get("read")]
    if unread:
        session.post("/api/v1/me/notifications/read", {"ids": unread[:50]})


@journey("access-requests")
def access_requests(t):
    """Four flows, each with a different requester: submissions are limited per person (5 a minute, 50 a day),
    so one persona must not carry the whole journey if it is to be re-run."""
    anon = t.anonymous()
    hidden = t.team_workspace(owner="alex")
    ws = t.team_workspace(owner="alex", discoverable=True)
    other = t.team_workspace(owner="blair", discoverable=True)
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    for requester in (blair, auditor, operator, alex):
        t.defer("mark notifications read", lambda s=requester: _mark_read(s))

    t.expect("targets.anonymous-refused", anon.get("/api/v1/access-request-targets"), 401)
    targets = blair.get("/api/v1/access-request-targets")
    t.expect("targets.list", targets, 200)
    t.ok("targets.discoverable-workspace-listed", any(x.get("id") == ws for x in jget(targets, "data", default=[])))
    t.ok("targets.hidden-workspace-not-listed", not any(x.get("id") == hidden for x in jget(targets, "data", default=[])))
    t.expect("request.anonymous-refused", _request(anon, ws), 401)
    t.expect("request.hidden-workspace-refused", _request(blair, hidden), 404)
    t.expect("request.no-csrf-refused", blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "idempotency_key": uuid.uuid4().hex}, csrf=False), 403)
    t.expect("request.bad-idempotency-key-refused", blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "idempotency_key": ""}), 400)
    t.expect("request.unknown-workspace-refused", _request(blair, "00000000-0000-4000-8000-000000000000"), 404)

    # Flow A (Blair): request, read, list for each audience, refusals, withdraw.
    blair_id = t.user_id("blair")
    key = uuid.uuid4().hex
    first = blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": "journey", "idempotency_key": key})
    t.expect("request.create", first, 200)
    rid = jget(first, "id")
    if not rid:
        t.rec.fail("request.created-request-missing", "setup_failed")
        return
    t.expect("request.replay-is-idempotent", blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": "journey", "idempotency_key": key}), 200)
    t.expect("request.read-own", blair.get(f"/api/v1/me/access-requests/{rid}"), 200)
    t.ok("request.listed-for-requester", any(r.get("id") == rid for r in jget(blair.get("/api/v1/me/access-requests"), "data", default=[])))
    t.expect("request.others-cannot-read", alex.get(f"/api/v1/me/access-requests/{rid}"), 404)
    t.ok("request.listed-for-workspace-admin", any(r.get("id") == rid for r in jget(alex.get(f"/api/v1/workspaces/{ws}/access-requests"), "data", default=[])))
    t.expect("request.workspace-list-by-requester-refused", blair.get(f"/api/v1/workspaces/{ws}/access-requests"), 403)
    t.ok("request.listed-for-platform", any(r.get("id") == rid for r in jget(operator.get("/api/v1/platform/access-requests"), "data", default=[])))
    t.expect("request.platform-list-by-requester-refused", blair.get("/api/v1/platform/access-requests"), 403)
    t.expect("request.requester-cannot-approve", blair.post(f"/api/v1/workspaces/{ws}/access-requests/{rid}/approve", {}), 403)
    t.expect("request.auditor-cannot-approve", auditor.post(f"/api/v1/platform/access-requests/{rid}/approve", {}), 403)
    t.expect("request.others-cannot-withdraw", alex.post(f"/api/v1/me/access-requests/{rid}/withdraw", {}), 404)
    t.expect("request.withdraw", blair.post(f"/api/v1/me/access-requests/{rid}/withdraw", {}), 200)
    t.expect("request.withdrawn-cannot-be-approved", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{rid}/approve", {}), 409)
    t.expect("request.no-membership-after-withdraw", blair.get(f"/api/v1/workspaces/{ws}/requests"), 403)

    # Flow B (Auditor): denied by the workspace admin.
    second = _request(auditor, ws)
    t.expect("request.create-for-denial", second, 200)
    sid = jget(second, "id")
    if sid:
        t.expect("request.deny-by-requester-refused", auditor.post(f"/api/v1/workspaces/{ws}/access-requests/{sid}/deny", {}), 403)
        t.expect("request.deny", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{sid}/deny", {}), 200)
        t.expect("request.denied-cannot-be-withdrawn", auditor.post(f"/api/v1/me/access-requests/{sid}/withdraw", {}), 409)
        t.expect("request.no-membership-after-deny", auditor.get(f"/api/v1/workspaces/{ws}/requests"), 403)

    # Flow C (Platform Admin as requester): approved by the workspace admin, which grants membership.
    third = _request(operator, ws)
    t.expect("request.create-for-approval", third, 200)
    tid = jget(third, "id")
    if tid:
        t.defer("remove approved member", lambda: operator.delete(f"/api/v1/platform/workspaces/{ws}/members/{t.user_id('operator')}"))
        t.expect("request.approve-by-workspace-admin", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{tid}/approve", {}), 200)
        t.expect("request.membership-after-approve", operator.get(f"/api/v1/workspaces/{ws}"), 200)
        t.expect("request.membership-grants-activity-access", operator.get(f"/api/v1/workspaces/{ws}/requests"), 200)
        t.expect("request.approved-cannot-be-withdrawn", operator.post(f"/api/v1/me/access-requests/{tid}/withdraw", {}), 409)
        t.ok("request.requester-notified", any(n.get("id") == tid for n in jget(operator.get("/api/v1/me/notifications"), "data", default=[])))

    # Flow D (Alex into a workspace Blair owns): approved on the platform path by a Platform Admin.
    fourth = _request(alex, other)
    t.expect("request.create-for-platform-decision", fourth, 200)
    fid = jget(fourth, "id")
    if fid:
        t.defer("remove platform-approved member", lambda: operator.delete(f"/api/v1/platform/workspaces/{other}/members/{t.user_id('alex')}"))
        t.expect("request.platform-approve-by-auditor-refused", auditor.post(f"/api/v1/platform/access-requests/{fid}/approve", {}), 403)
        t.expect("request.approve-by-platform-admin", operator.post(f"/api/v1/platform/access-requests/{fid}/approve", {}), 200)
        t.expect("request.membership-after-platform-approve", alex.get(f"/api/v1/workspaces/{other}/requests"), 200)

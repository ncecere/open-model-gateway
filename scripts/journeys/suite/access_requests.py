"""Self-service access requests: request, withdraw, deny, approve by the workspace admin or a Platform Admin."""
import uuid

from ..registry import journey
from .common import NO_SUCH_ID, rows


def unread_ids(session):
    feed = rows(session.get("/api/v1/me/notifications"))
    return None if feed is None else {n["id"] for n in feed if not n.get("read") and isinstance(n.get("id"), str)}


def mark_new_read(session, before):
    """Mark read only the notifications this journey caused (unread now, and not unread before it began)."""
    now = unread_ids(session)
    if now is None or before is None:
        raise RuntimeError("notification feed unreadable")
    fresh = sorted(now - before)
    if fresh:
        response = session.post("/api/v1/me/notifications/read", {"ids": fresh[:50]})
        if response.status not in (200, 204):
            raise RuntimeError("marking notifications read was refused")


def submit(session, ws, reason="journey"):
    # Submissions are limited per person (a few a minute and a day: docs/management-api.md, access requests),
    # which is why each flow below uses a different requester.
    return session.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": reason, "idempotency_key": uuid.uuid4().hex})


@journey("access-requests")
def access_requests(t):
    """Four flows, each with a different requester, so the journey can be re-run."""
    anon = t.anonymous()
    hidden = t.team_workspace(owner="alex")
    ws = t.team_workspace(owner="alex", discoverable=True)
    other = t.team_workspace(owner="blair", discoverable=True)
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    for requester in (blair, auditor, operator, alex):
        before = unread_ids(requester)
        t.defer("mark own notifications read", lambda s=requester, b=before: mark_new_read(s, b))

    t.expect("targets.anonymous-refused", anon.get("/api/v1/access-request-targets"), 401)
    targets = blair.get("/api/v1/access-request-targets")
    t.expect("targets.list", targets, 200)
    listed = rows(targets)
    t.ok("targets.discoverable-workspace-listed", listed is not None and any(x.get("id") == ws for x in listed))
    t.ok("targets.hidden-workspace-not-listed", listed is not None and not any(x.get("id") == hidden for x in listed))
    t.expect("request.anonymous-refused", submit(anon, ws), 401)
    t.expect("request.hidden-workspace-refused", submit(blair, hidden), 404)
    t.expect("request.no-csrf-refused", blair.post("/api/v1/me/access-requests", {"workspace_id": NO_SUCH_ID, "idempotency_key": uuid.uuid4().hex}, csrf=False), 403)
    t.expect("request.bad-idempotency-key-refused", blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "idempotency_key": ""}), 400)
    t.expect("request.unknown-workspace-refused", submit(blair, NO_SUCH_ID), 404)

    # Flow A (Blair): request, read, list for each audience, refusals, withdraw.
    key = uuid.uuid4().hex
    first = blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": "journey", "idempotency_key": key})
    t.expect("request.create", first, 200)
    rid = first.id()
    if not rid:
        t.rec.fail("request.created-request-missing", "setup_failed")
        return
    t.expect("request.replay-is-idempotent", blair.post("/api/v1/me/access-requests", {"workspace_id": ws, "reason": "journey", "idempotency_key": key}), 200)
    t.expect("request.read-own", blair.get(f"/api/v1/me/access-requests/{rid}"), 200)
    t.ok("request.listed-for-requester", any(r.get("id") == rid for r in rows(blair.get("/api/v1/me/access-requests")) or []))
    t.expect("request.others-cannot-read", alex.get(f"/api/v1/me/access-requests/{rid}"), 404)
    t.ok("request.listed-for-workspace-admin", any(r.get("id") == rid for r in rows(alex.get(f"/api/v1/workspaces/{ws}/access-requests")) or []))
    t.expect("request.workspace-list-by-requester-refused", blair.get(f"/api/v1/workspaces/{ws}/access-requests"), 403)
    t.ok("request.listed-for-platform", any(r.get("id") == rid for r in rows(operator.get("/api/v1/platform/access-requests")) or []))
    t.expect("request.platform-list-by-requester-refused", blair.get("/api/v1/platform/access-requests"), 403)
    t.expect("request.requester-cannot-approve", blair.post(f"/api/v1/workspaces/{ws}/access-requests/{rid}/approve", {}), 403)
    t.expect("request.auditor-cannot-approve", auditor.post(f"/api/v1/platform/access-requests/{rid}/approve", {}), 403)
    t.expect("request.refused-approvals-granted-nothing", blair.get(f"/api/v1/workspaces/{ws}/requests"), 403)
    t.expect("request.others-cannot-withdraw", alex.post(f"/api/v1/me/access-requests/{rid}/withdraw", {}), 404)
    t.expect("request.withdraw", blair.post(f"/api/v1/me/access-requests/{rid}/withdraw", {}), 200)
    t.expect("request.withdrawn-cannot-be-approved", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{rid}/approve", {}), 409)
    t.expect("request.no-membership-after-withdraw", blair.get(f"/api/v1/workspaces/{ws}/requests"), 403)

    # Flow B (Auditor): denied by the workspace admin.
    second = submit(auditor, ws)
    t.expect("request.create-for-denial", second, 200)
    sid = second.id()
    if sid:
        t.expect("request.deny-by-requester-refused", auditor.post(f"/api/v1/workspaces/{ws}/access-requests/{sid}/deny", {}), 403)
        t.expect("request.deny", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{sid}/deny", {}), 200)
        t.expect("request.denied-cannot-be-withdrawn", auditor.post(f"/api/v1/me/access-requests/{sid}/withdraw", {}), 409)
        t.expect("request.no-membership-after-deny", auditor.get(f"/api/v1/workspaces/{ws}/requests"), 403)

    # Flow C (Platform Admin as requester): approved by the workspace admin, which grants membership.
    third = submit(operator, ws)
    t.expect("request.create-for-approval", third, 200)
    tid = third.id()
    if tid:
        operator_id = t.user_id("operator")
        t.undo("remove approved member", lambda: operator.delete(f"/api/v1/platform/workspaces/{ws}/members/{operator_id}"), gone=(404, 409))
        t.expect("request.approve-by-workspace-admin", alex.post(f"/api/v1/workspaces/{ws}/access-requests/{tid}/approve", {}), 200)
        t.expect("request.membership-after-approve", operator.get(f"/api/v1/workspaces/{ws}"), 200)
        t.expect("request.membership-grants-activity-access", operator.get(f"/api/v1/workspaces/{ws}/requests"), 200)
        t.expect("request.approved-cannot-be-withdrawn", operator.post(f"/api/v1/me/access-requests/{tid}/withdraw", {}), 409)
        t.ok("request.requester-notified", any(n.get("id") == tid for n in rows(operator.get("/api/v1/me/notifications")) or []))

    # Flow D (Alex into a workspace Blair owns): approved on the platform path by a Platform Admin.
    fourth = submit(alex, other)
    t.expect("request.create-for-platform-decision", fourth, 200)
    fid = fourth.id()
    if fid:
        alex_id = t.user_id("alex")
        t.undo("remove platform-approved member", lambda: operator.delete(f"/api/v1/platform/workspaces/{other}/members/{alex_id}"), gone=(404, 409))
        t.expect("request.platform-approve-by-auditor-refused", auditor.post(f"/api/v1/platform/access-requests/{fid}/approve", {}), 403)
        t.expect("request.approve-by-platform-admin", operator.post(f"/api/v1/platform/access-requests/{fid}/approve", {}), 200)
        t.expect("request.membership-after-platform-approve", alex.get(f"/api/v1/workspaces/{other}/requests"), 200)

"""Report schedules, workspace alert rules and legal holds, on throwaway data."""
import uuid

from ..registry import journey
from .common import NO_SUCH_ID, rows


@journey("schedules-alerts-holds")
def schedules_alerts_holds(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    t.add_member(ws, "blair", "member")
    base = f"/api/v1/workspaces/{ws}"
    alex_id = t.user_id("alex")

    # Report schedules (workspace admins; recipients are members of the workspace).
    schedule = {"cadence": "weekly", "timezone": "UTC", "delivery_time": "08:00", "recipient_user_ids": [alex_id]}
    t.expect("schedule.anonymous-refused", anon.get(f"{base}/report-schedules"), 401)
    t.expect("schedule.member-list-refused", blair.get(f"{base}/report-schedules"), 403)
    t.expect("schedule.member-create-refused", blair.post(f"{base}/report-schedules", {}), 403)
    t.expect("schedule.invalid-refused", alex.post(f"{base}/report-schedules", {**schedule, "delivery_time": "25:99"}), 400)
    created = alex.post(f"{base}/report-schedules", schedule)
    t.expect("schedule.create", created, 200)
    sid = created.id()
    if sid:
        t.undo("delete report schedule", lambda: alex.delete(f"{base}/report-schedules/{sid}"), gone=(404,))
        t.ok("schedule.created-active", created.get("cadence") == "weekly" and created.get("delivery_time") == "08:00")
        t.expect("schedule.read", alex.get(f"{base}/report-schedules/{sid}"), 200)
        t.ok("schedule.listed", any(s.get("id") == sid for s in rows(alex.get(f"{base}/report-schedules")) or []))
        t.expect("schedule.pause", alex.patch(f"{base}/report-schedules/{sid}", {"state": "paused"}), 200)
        t.ok("schedule.paused", alex.get(f"{base}/report-schedules/{sid}").get("state") == "paused")
        t.expect("schedule.runs", alex.get(f"{base}/report-schedules/{sid}/runs"), 200)
        t.expect("schedule.platform-sees-no-workspace-schedule", operator.get(f"/api/v1/platform/report-schedules/{sid}"), 404)
        t.expect("schedule.member-delete-refused", blair.delete(f"{base}/report-schedules/{sid}"), 403)
        t.expect("schedule.delete", alex.delete(f"{base}/report-schedules/{sid}"), 200)
        t.expect("schedule.deleted-is-gone", alex.get(f"{base}/report-schedules/{sid}"), 404)
    t.expect("schedule.platform-list", operator.get("/api/v1/platform/report-schedules"), 200)
    t.expect("schedule.platform-create-by-member-refused", alex.post("/api/v1/platform/report-schedules", {}), 403)

    # Workspace alert rules.
    rule = {"name": t.label("rule"), "kind": "budget_threshold", "enabled": False, "budget_layers": ["local"], "thresholds": [80], "notify_workspace_admins": True, "notify_platform_admins": False, "notify_emails": []}
    t.expect("rule.anonymous-refused", anon.get(f"{base}/alerts/rules"), 401)
    t.expect("rule.member-list-refused", blair.get(f"{base}/alerts/rules"), 403)
    t.expect("rule.member-create-refused", blair.post(f"{base}/alerts/rules", rule), 403)
    t.expect("rule.invalid-refused", alex.post(f"{base}/alerts/rules", {**rule, "thresholds": [0]}), 400)
    created = alex.post(f"{base}/alerts/rules", rule)
    t.expect("rule.create", created, 200)
    rid = created.id()
    if rid:
        t.undo("delete alert rule", lambda: alex.delete(f"{base}/alerts/rules/{rid}"), gone=(404,))
        t.expect("rule.read", alex.get(f"{base}/alerts/rules/{rid}"), 200)
        t.expect("rule.update", alex.put(f"{base}/alerts/rules/{rid}", {**rule, "thresholds": [90]}), 200)
        t.ok("rule.updated", alex.get(f"{base}/alerts/rules/{rid}").get("thresholds") == [90])
        t.expect("rule.auditor-reads", auditor.get(f"{base}/alerts/rules/{rid}"), 200)
        t.expect("rule.auditor-cannot-delete", auditor.delete(f"{base}/alerts/rules/{rid}"), 403)
        t.expect("rule.events", alex.get(f"{base}/alerts/events"), 200)
        t.expect("rule.delete", alex.delete(f"{base}/alerts/rules/{rid}"), 200)
        t.expect("rule.deleted-is-gone", alex.get(f"{base}/alerts/rules/{rid}"), 404)

    # Legal holds: place on the throwaway workspace and release. A released hold stays as a record.
    holds = "/api/v1/platform/legal-holds"
    reference = "journeys-" + uuid.uuid4().hex[:8]
    t.expect("hold.anonymous-refused", anon.get(holds), 401)
    t.expect("hold.auditor-create-refused", auditor.post(holds, {"reference": " "}), 403)
    t.expect("hold.member-create-refused", alex.post(holds, {"reference": " "}), 403)
    t.expect("hold.invalid-reference-refused", operator.post(holds, {"reference": " ", "workspace_id": ws}), 400)
    placed = operator.post(holds, {"reference": reference, "workspace_id": ws})
    t.expect("hold.create", placed, 201)
    hid = placed.id()
    if hid:
        t.undo("release legal hold", lambda: operator.post(f"{holds}/{hid}/release", {}), gone=(409,))
        listed = rows(operator.get(holds))
        t.ok("hold.listed-with-its-target", any(h.get("id") == hid and isinstance(h.get("target"), dict) and h["target"].get("id") == ws for h in listed or []))
        t.expect("hold.auditor-reads", auditor.get(holds), 200)
        t.expect("hold.auditor-release-refused", auditor.post(f"{holds}/{hid}/release", {}), 403)
        t.expect("hold.release", operator.post(f"{holds}/{hid}/release", {}), 200)
        again = operator.post(f"{holds}/{hid}/release", {})
        t.expect("hold.release-twice-refused", again, 409)
    t.expect("hold.release-unknown", operator.post(f"{holds}/{NO_SUCH_ID}/release", {}), 404)

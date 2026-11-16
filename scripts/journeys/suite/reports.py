"""Reports, usage and logs: workspace and platform scope, their documented shape, and their refusals."""
from ..registry import journey
from .common import NO_SUCH_ID, money_is_text, reversed_window, rows, window

PLATFORM_REPORTS = (
    ("usage-overview", f"/api/v1/platform/usage/overview?{window()}"),
    ("usage-explore", f"/api/v1/platform/usage/explore?{window()}&metric=spend&group_by=model"),
    ("usage-storage", f"/api/v1/platform/usage/storage?{window()}"),
    ("cost-report", f"/api/v1/platform/cost-report?{window()}"),
    ("request-logs", "/api/v1/platform/logs/requests?limit=5"),
    ("generation-logs", "/api/v1/platform/logs/generations"),
    ("session-logs", "/api/v1/platform/logs/sessions"),
    ("log-metrics", "/api/v1/platform/logs/metrics"),
    ("report-schedules", "/api/v1/platform/report-schedules"),
    ("batches", "/api/v1/platform/batches"),
)


@journey("reports-usage")
def reports_usage(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    other = t.team_workspace(owner="alex")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    q = window()
    base = f"/api/v1/workspaces/{ws}"

    for name, path in PLATFORM_REPORTS:
        t.expect(f"platform.{name}-admin", operator.get(path), 200)
        t.expect(f"platform.{name}-auditor", auditor.get(path), 200)
        t.expect(f"platform.{name}-member-refused", alex.get(path), 403)
        t.expect(f"platform.{name}-anonymous-refused", anon.get(path), 401)
    t.expect("platform.usage-missing-dates-refused", operator.get("/api/v1/platform/usage/overview"), 400)
    t.expect("platform.usage-bad-date-refused", operator.get("/api/v1/platform/usage/overview?start_date=nope&end_date=nope"), 400)
    t.expect("platform.usage-reversed-dates-refused", operator.get(f"/api/v1/platform/usage/overview?{reversed_window()}"), 400)
    t.expect("platform.request-detail-unknown", operator.get(f"/api/v1/platform/logs/requests/{NO_SUCH_ID}"), 404)
    t.expect("platform.session-detail-unknown", operator.get(f"/api/v1/platform/logs/sessions/{ws}/none"), 404)

    # Documented shape: periods, currency, money as integer strings, and empty (never invented) data.
    cost = operator.get(f"/api/v1/platform/cost-report?{q}")
    t.ok("platform.cost-report-shape", cost.get("basis") is not None and cost.get("currency") == "USD" and isinstance(cost.get("daily"), list) and isinstance(cost.get("breakdowns", "workspaces"), list) and cost.get("coverage", "total_attempts") is not None)
    t.ok("platform.cost-report-money-is-text", money_is_text(cost))
    t.ok("platform.cost-report-empty-not-invented", cost.get("coverage", "total_attempts") == "0" and cost.get("breakdowns", "models") == [])
    usage = operator.get(f"/api/v1/platform/usage/overview?{q}")
    t.ok("platform.usage-overview-shape", usage.get("scope") == "platform" and usage.get("currency") == "USD" and isinstance(usage.get("tiles"), dict) and usage.get("period", "timezone") == "UTC")
    explore = operator.get(f"/api/v1/platform/usage/explore?{q}&metric=spend&group_by=workspace")
    t.ok("platform.usage-explore-shape", explore.get("metric") == "spend" and explore.get("group_by") == "workspace" and explore.get("rows") == [] and isinstance(explore.get("series"), list))
    storage = operator.get(f"/api/v1/platform/usage/storage?{q}")
    t.ok("platform.usage-storage-shape", storage.get("scope") == "platform" and storage.get("unit") == "gb_day" and isinstance(storage.get("gb_bytes"), str))

    # Personal workspaces: an admin's platform views list shared workspaces only, and no admin or auditor route
    # opens a personal workspace's usage or costs.
    personal = t.personal_workspace("alex")
    listed = rows(operator.get("/api/v1/platform/workspaces?limit=200"))
    t.ok("platform.workspace-list-has-no-personal-workspace", listed is not None and not any(w.get("kind") == "personal" or w.get("id") == personal for w in listed))
    for persona, session in (("operator", operator), ("auditor", auditor)):
        t.expect(f"personal.{persona}-usage-overview-is-missing", session.get(f"/api/v1/workspaces/{personal}/usage/overview?{q}"), 404)
        t.expect(f"personal.{persona}-cost-report-is-missing", session.get(f"/api/v1/workspaces/{personal}/cost-report?{q}"), 404)
        t.expect(f"personal.{persona}-export-is-missing", session.get(f"/api/v1/workspaces/{personal}/usage-export?{q}"), 404)

    reads = (
        ("usage-overview", f"{base}/usage/overview?{q}"),
        ("usage-explore", f"{base}/usage/explore?{q}&metric=requests&group_by=day"),
        ("cost-report", f"{base}/cost-report?{q}"),
        ("cost-summary", f"{base}/cost-summary"),
        ("costs", f"{base}/costs?{q}"),
        ("usage-export", f"{base}/usage-export?{q}"),
        ("usage", f"{base}/usage"),
        ("requests", f"{base}/requests"),
        ("generations", f"{base}/generations"),
        ("sessions", f"{base}/sessions"),
        ("audit", f"{base}/audit"),
        ("log-metrics", f"{base}/logs/metrics"),
        ("executions", f"{base}/executions"),
        ("holds", f"{base}/holds"),
        ("files", f"{base}/files"),
        ("batches", f"{base}/batches"),
    )
    for name, path in reads:
        t.expect(f"workspace.{name}-admin", alex.get(path), 200)
        t.expect(f"workspace.{name}-member", blair.get(path), 200)
        t.expect(f"workspace.{name}-anonymous-refused", anon.get(path), 401)
    t.expect("workspace.storage-usage-admin", alex.get(f"{base}/usage/storage?{q}"), 200)
    t.expect("workspace.storage-usage-member-refused", blair.get(f"{base}/usage/storage?{q}"), 403)
    t.expect("workspace.request-detail-unknown", alex.get(f"{base}/requests/{NO_SUCH_ID}"), 404)
    t.expect("workspace.session-detail-unknown", alex.get(f"{base}/sessions/none"), 404)
    t.expect("workspace.batch-detail-unknown", alex.get(f"{base}/batches/none"), 404)
    export = alex.get(f"{base}/usage-export?{q}")
    t.ok("workspace.usage-export-is-csv", export.header("content-type").startswith("text/csv"))

    summary = alex.get(f"{base}/cost-summary")
    t.ok("workspace.cost-summary-shape", summary.get("currency") == "USD" and summary.get("requests") == "0" and summary.get("known_cost_microusd") == "0")
    t.ok("workspace.cost-summary-money-is-text", money_is_text(summary))
    t.ok("workspace.request-log-starts-empty", alex.get(f"{base}/requests").get("data") == [])

    # Platform staff see totals and metadata of a shared workspace, but not its members' activity.
    for name in ("usage-overview", "cost-report", "cost-summary", "usage-export"):
        path = next(p for n, p in reads if n == name)
        t.expect(f"workspace.{name}-auditor-totals", auditor.get(path), 200)
    for name in ("requests", "sessions", "audit", "executions", "log-metrics"):
        path = next(p for n, p in reads if n == name)
        t.expect(f"workspace.{name}-auditor-refused", auditor.get(path), 403)

    # A person who is not in the workspace sees none of it.
    for name in ("usage-overview", "requests", "cost-report"):
        path = next(p for n, p in reads if n == name).replace(ws, other)
        t.expect(f"workspace.{name}-non-member-refused", blair.get(path), 403)

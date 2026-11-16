"""Reports and usage reads, workspace and platform scope, with their refusals."""
from ..registry import journey
from .common import jget, window

PLATFORM_REPORTS = (
    ("usage-overview", f"/api/v1/platform/usage/overview?{window()}"),
    ("usage-explore", f"/api/v1/platform/usage/explore?{window()}&metric=spend&group_by=model"),
    ("usage-storage", f"/api/v1/platform/usage/storage?{window()}"),
    ("cost-report", f"/api/v1/platform/cost-report?{window()}"),
    ("request-logs", "/api/v1/platform/logs/requests?limit=5"),
    ("log-metrics", "/api/v1/platform/logs/metrics"),
    ("report-schedules", "/api/v1/platform/report-schedules"),
)


@journey("reports-usage")
def reports_usage(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
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
    t.expect("platform.usage-reversed-dates-refused", operator.get("/api/v1/platform/usage/overview?start_date=2026-12-31&end_date=2026-01-01"), 400)

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
        ("audit", f"{base}/audit"),
        ("log-metrics", f"{base}/logs/metrics"),
        ("executions", f"{base}/executions"),
        ("holds", f"{base}/holds"),
    )
    for name, path in reads:
        t.expect(f"workspace.{name}-admin", alex.get(path), 200)
        t.expect(f"workspace.{name}-member", blair.get(path), 200)
        t.expect(f"workspace.{name}-anonymous-refused", anon.get(path), 401)
    t.expect("workspace.storage-usage-admin", alex.get(f"{base}/usage/storage?{q}"), 200)
    t.expect("workspace.storage-usage-member-refused", blair.get(f"{base}/usage/storage?{q}"), 403)
    t.expect("workspace.report-schedules-admin", alex.get(f"{base}/report-schedules"), 200)
    t.expect("workspace.report-schedules-member-refused", blair.get(f"{base}/report-schedules"), 403)
    export = alex.get(f"{base}/usage-export?{q}")
    t.ok("workspace.usage-export-is-csv", export.header("content-type").startswith("text/csv"))

    # Platform staff see totals and metadata of a shared workspace, but not its members' activity.
    for name in ("usage-overview", "cost-report", "cost-summary", "usage-export"):
        path = next(p for n, p in reads if n == name)
        t.expect(f"workspace.{name}-auditor-totals", auditor.get(path), 200)
    for name in ("requests", "audit", "executions", "log-metrics"):
        path = next(p for n, p in reads if n == name)
        t.expect(f"workspace.{name}-auditor-refused", auditor.get(path), 403)

    # A person who is not in the workspace sees none of it.
    outsider = t.session("blair")
    other = t.team_workspace(owner="alex")
    for name in ("usage-overview", "requests", "cost-report"):
        path = next(p for n, p in reads if n == name).replace(ws, other)
        t.expect(f"workspace.{name}-non-member-refused", outsider.get(path), 403)

    # Fresh data is empty, never invented.
    summary = alex.get(f"{base}/cost-summary")
    t.ok("workspace.cost-summary-money-is-text", all(isinstance(v, str) for v in _amounts(summary)))
    t.ok("workspace.request-log-starts-empty", jget(alex.get(f"{base}/requests"), "data") == [])


def _amounts(response):
    found = []

    def walk(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key.endswith("_microusd") and value is not None:
                    found.append(value)
                walk(value)
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(response.json())
    return found

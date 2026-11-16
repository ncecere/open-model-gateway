"""The Platform Auditor reads configuration, usage and audit but mutates nothing."""
from ..registry import journey
from .common import jget, window

AUDITOR_READS = (
    ("users", "/api/v1/platform/users?limit=5"),
    ("overview", "/api/v1/platform/overview"),
    ("catalogs", "/api/v1/platform/catalogs"),
    ("models", "/api/v1/platform/models?limit=5"),
    ("deployments", "/api/v1/platform/deployments"),
    ("settings-general", "/api/v1/platform/settings/general"),
    ("audit", "/api/v1/platform/audit?limit=5"),
    ("workspaces", "/api/v1/platform/workspaces?limit=5"),
    ("access-reviews", "/api/v1/platform/access-reviews"),
    ("access-requests", "/api/v1/platform/access-requests"),
    ("group-mappings", "/api/v1/platform/oidc/group-mappings"),
    ("cost-report", f"/api/v1/platform/cost-report?{window()}"),
)


@journey("auditor-readonly")
def auditor_readonly(t):
    ws = t.team_workspace(owner="alex", discoverable=False)
    auditor, operator, alex = (t.session(p) for p in ("auditor", "operator", "alex"))
    for name, path in AUDITOR_READS:
        t.expect(f"auditor.reads-{name}", auditor.get(path), 200)
    t.expect("auditor.cannot-read-admin-tokens", auditor.get("/api/v1/platform/admin-tokens"), 403)
    me = auditor.get("/api/v1/me")
    t.ok("auditor.capabilities-read-only", jget(me, "capabilities", "platform_read") is True and jget(me, "capabilities", "platform_write") is False)

    before = operator.get(f"/api/v1/platform/workspaces/{ws}").json()
    mutations = (
        ("create-workspace", "POST", "/api/v1/platform/workspaces", {"name": t.label("x"), "kind": "team", "owner_user_id": t.user_id("alex")}),
        ("rename-workspace", "PATCH", f"/api/v1/platform/workspaces/{ws}", {"name": t.label("x")}),
        ("disable-workspace", "PATCH", f"/api/v1/platform/workspaces/{ws}", {"disabled": True}),
        ("discoverable", "PUT", f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}),
        ("workspace-policy", "PUT", f"/api/v1/platform/workspaces/{ws}/policy", {"requests_per_minute": 1, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "1"}),
        ("add-member", "POST", f"/api/v1/platform/workspaces/{ws}/members", {"user_id": t.user_id("blair"), "role": "member"}),
        ("settings-general", "PUT", "/api/v1/platform/settings/general", {"display_name": "nope"}),
        ("create-user", "POST", "/api/v1/platform/users", {"email": "nobody@demo.invalid"}),
        ("change-role", "POST", f"/api/v1/platform/users/{t.user_id('blair')}/roles", {"role": "auditor"}),
        ("create-cost-center", "POST", "/api/v1/platform/cost-centers", {"code": "NOPE", "name": "nope"}),
        ("create-admin-token", "POST", "/api/v1/platform/admin-tokens", {"name": t.label("t"), "scopes": ["platform:read"], "expires_in_days": 1}),
        ("start-access-review", "POST", "/api/v1/platform/access-reviews", {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws]}),
        ("create-catalog", "POST", "/api/v1/platform/catalogs", {"name": t.label("catalog")}),
        ("create-group-mapping", "POST", "/api/v1/platform/oidc/group-mappings", {"issuer": "https://idp.example.test", "group_value": "g", "target_kind": "platform", "platform_role": "user", "enabled": False}),
    )
    for name, method, path, body in mutations:
        t.expect(f"auditor.cannot-{name}", auditor.call(method, path, body), 403)
    after = operator.get(f"/api/v1/platform/workspaces/{ws}").json()
    t.ok("auditor.refusals-changed-nothing", before == after)
    t.ok("auditor.refusals-started-no-review", not [r for r in jget(operator.get("/api/v1/platform/access-reviews?limit=100"), "data", default=[]) if str(r.get("name", "")).startswith(t.label(""))])

    # The auditor reads shared-workspace metadata and totals, never a member's activity or someone's personal workspace.
    t.expect("auditor.reads-workspace-metadata", auditor.get(f"/api/v1/workspaces/{ws}"), 200)
    t.expect("auditor.reads-workspace-usage-overview", auditor.get(f"/api/v1/workspaces/{ws}/usage/overview?{window()}"), 200)
    t.expect("auditor.cannot-read-workspace-requests", auditor.get(f"/api/v1/workspaces/{ws}/requests"), 403)
    t.expect("auditor.reads-workspace-members", auditor.get(f"/api/v1/workspaces/{ws}/members"), 200)
    t.expect("auditor.cannot-list-workspace-keys", auditor.get(f"/api/v1/workspaces/{ws}/keys"), 403)
    t.expect("auditor.cannot-mutate-workspace-policy", auditor.put(f"/api/v1/workspaces/{ws}/policy", {"requests_per_minute": 1, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "1"}), 403)
    personal = next(w["id"] for w in jget(alex.get("/api/v1/me"), "workspaces", default=[]) if w.get("kind") == "personal")
    t.expect("auditor.cannot-read-personal-workspace", auditor.get(f"/api/v1/workspaces/{personal}"), 404)

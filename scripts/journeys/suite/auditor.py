"""The Platform Auditor reads configuration, usage and audit but mutates nothing.

Each mutation is sent in a form that changes nothing even if the gateway wrongly accepted it: a body
no create accepts, the current value written back, or a throwaway object that is cleaned up."""
from ..registry import journey
from .common import NO_SUCH_ID, rows, window
from .platform_admin import general_body

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
    ws = t.team_workspace(owner="alex")
    auditor, operator, alex = (t.session(p) for p in ("auditor", "operator", "alex"))
    for name, path in AUDITOR_READS:
        t.expect(f"auditor.reads-{name}", auditor.get(path), 200)
    t.expect("auditor.cannot-read-admin-tokens", auditor.get("/api/v1/platform/admin-tokens"), 403)
    me = auditor.get("/api/v1/me")
    t.ok("auditor.capabilities-read-only", me.get("capabilities", "platform_read") is True and me.get("capabilities", "platform_write") is False)

    detail = f"/api/v1/platform/workspaces/{ws}"
    before = operator.get(detail)
    settings = general_body(operator.get("/api/v1/platform/settings/general"))
    blair_id = t.user_id("blair")
    policy = {"requests_per_minute": 1, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "1"}
    mutations = (
        ("create-workspace", "POST", "/api/v1/platform/workspaces", {"name": " ", "kind": "team", "owner_user_id": NO_SUCH_ID}),
        ("rename-workspace", "PATCH", detail, {"name": before.get("name")}),
        ("disable-workspace", "PATCH", detail, {"disabled": False}),
        ("discoverable", "PUT", f"{detail}/discoverable", {"discoverable": False}),
        ("workspace-policy", "PUT", f"{detail}/policy", policy),
        ("add-member", "POST", f"{detail}/members", {"user_id": blair_id, "role": "member"}),
        ("settings-general", "PUT", "/api/v1/platform/settings/general", settings or {}),
        ("create-user", "POST", "/api/v1/platform/users", {}),
        ("change-role", "POST", f"/api/v1/platform/users/{blair_id}/roles", {"role": "not-a-role"}),
        ("create-cost-center", "POST", "/api/v1/platform/cost-centers", {}),
        ("create-admin-token", "POST", "/api/v1/platform/admin-tokens", {"name": " ", "scopes": [], "expires_in_days": 0}),
        ("start-access-review", "POST", "/api/v1/platform/access-reviews", {"name": " ", "subjects": []}),
        ("create-catalog", "POST", "/api/v1/platform/catalogs", {}),
        ("create-group-mapping", "POST", "/api/v1/platform/oidc/group-mappings", {}),
        ("model-setup", "POST", "/api/v1/platform/model-setup", {}),
        ("legal-hold", "POST", "/api/v1/platform/legal-holds", {"reference": " "}),
    )
    for name, method, path, body in mutations:
        t.expect(f"auditor.cannot-{name}", auditor.call(method, path, body), 403)
    after = operator.get(detail)
    t.ok("auditor.refusals-changed-nothing", before.status == 200 and after.status == 200 and before.json() == after.json())
    t.ok("auditor.refused-settings-unchanged", settings is not None and general_body(operator.get("/api/v1/platform/settings/general")) == settings)
    reviews = rows(operator.get("/api/v1/platform/access-reviews?limit=100"))
    t.ok("auditor.refusals-started-no-review", reviews is not None and not [r for r in reviews if str(r.get("name", "")).startswith(t.label(""))])

    # The auditor reads shared-workspace metadata and totals, never a member's activity or someone's personal workspace.
    t.expect("auditor.reads-workspace-metadata", auditor.get(f"/api/v1/workspaces/{ws}"), 200)
    t.expect("auditor.reads-workspace-usage-overview", auditor.get(f"/api/v1/workspaces/{ws}/usage/overview?{window()}"), 200)
    t.expect("auditor.cannot-read-workspace-requests", auditor.get(f"/api/v1/workspaces/{ws}/requests"), 403)
    t.expect("auditor.reads-workspace-members", auditor.get(f"/api/v1/workspaces/{ws}/members"), 200)
    t.expect("auditor.cannot-list-workspace-keys", auditor.get(f"/api/v1/workspaces/{ws}/keys"), 403)
    t.expect("auditor.cannot-mutate-workspace-policy", auditor.put(f"/api/v1/workspaces/{ws}/policy", policy), 403)
    t.ok("auditor.refused-policy-left-the-default", operator.get(f"{detail}/policy").get("mode") == "inherit")
    personal = t.personal_workspace("alex")
    t.expect("auditor.cannot-read-personal-workspace", auditor.get(f"/api/v1/workspaces/{personal}"), 404)

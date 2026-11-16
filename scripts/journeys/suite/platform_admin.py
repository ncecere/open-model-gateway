"""Platform Admin flows: directory, catalogue and configuration reads, policy and limits, and the refusals."""
from ..registry import journey
from .common import jget

READS = (
    ("users", "/api/v1/platform/users?limit=5"),
    ("overview", "/api/v1/platform/overview"),
    ("catalogs", "/api/v1/platform/catalogs"),
    ("catalog-defaults", "/api/v1/platform/catalog-defaults"),
    ("models", "/api/v1/platform/models?limit=5"),
    ("providers", "/api/v1/platform/providers"),
    ("deployments", "/api/v1/platform/deployments"),
    ("settings-general", "/api/v1/platform/settings/general"),
    ("settings-privacy", "/api/v1/platform/settings/privacy"),
    ("settings-sign-in", "/api/v1/platform/settings/sign-in"),
    ("server-policy", "/api/v1/platform/server-policy"),
    ("team-type-policy", "/api/v1/platform/workspace-types/team/policy"),
    ("project-type-policy", "/api/v1/platform/workspace-types/project/policy"),
    ("audit", "/api/v1/platform/audit?limit=5"),
    ("group-mappings", "/api/v1/platform/oidc/group-mappings"),
    ("cost-centers", "/api/v1/platform/cost-centers"),
    ("key-safety", "/api/v1/platform/key-safety"),
    ("alert-events", "/api/v1/platform/alerts/events"),
    ("holds", "/api/v1/platform/holds"),
    ("legal-holds", "/api/v1/platform/legal-holds"),
    ("workspace-templates", "/api/v1/platform/workspace-templates"),
    ("admin-tokens", "/api/v1/platform/admin-tokens"),
)


@journey("platform-admin")
def platform_admin(t):
    anon = t.anonymous()
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))

    for name, path in READS:
        t.expect(f"admin.{name}", operator.get(path), 200)
        t.expect(f"member.{name}-refused", blair.get(path), 403)
        t.expect(f"anonymous.{name}-refused", anon.get(path), 401)
    t.expect("installation-policy.removed-for-admin", operator.get("/api/v1/platform/installation/policy"), 410)
    t.expect("installation-policy.member-refused", alex.get("/api/v1/platform/installation/policy"), 403)
    t.expect("route.undeclared-platform-route-denied", operator.get("/api/v1/platform/not-a-route"), 404)

    # Directory.
    users = operator.get("/api/v1/platform/users?limit=100")
    t.ok("users.lists-demo-personas", {"operator@demo.invalid", "alex@demo.invalid"} <= {u.get("email") for u in jget(users, "data", default=[])})
    t.expect("users.detail", operator.get(f"/api/v1/platform/users/{t.user_id('alex')}"), 200)
    t.expect("users.detail-by-member-refused", alex.get(f"/api/v1/platform/users/{t.user_id('blair')}"), 403)
    t.expect("users.search", operator.get("/api/v1/platform/users?q=alex"), 200)
    t.expect("users.control-character-search-refused", operator.get("/api/v1/platform/users?q=%00"), 400)
    t.expect("users.integer-valued-float-limit-refused", operator.get("/api/v1/platform/users?limit=1.0"), 400)
    t.expect("users.role-change-by-member-refused", alex.post(f"/api/v1/platform/users/{t.user_id('blair')}/roles", {"role": "admin"}), 403)

    # Catalogue.
    models = operator.get("/api/v1/platform/models?limit=50")
    t.ok("catalog.models-have-readiness", all("readiness" in m for m in jget(models, "data", default=[])))
    first = next(iter(jget(models, "data", default=[])), None)
    if first:
        t.expect("catalog.model-detail", operator.get(f"/api/v1/platform/models/{first['id']}"), 200)
        t.expect("catalog.model-routing", operator.get(f"/api/v1/platform/models/{first['id']}/routing"), 200)
    else:
        t.skip("catalog.model-detail")
    deployments = jget(operator.get("/api/v1/platform/deployments"), "data", default=[])
    if deployments:
        t.expect("catalog.deployment-detail", operator.get(f"/api/v1/platform/deployments/{deployments[0]['id']}"), 200)
        t.expect("catalog.deployment-prices", operator.get(f"/api/v1/platform/deployments/{deployments[0]['id']}/prices"), 200)
    else:
        t.skip("catalog.deployment-detail")

    # Policy and limits on a throwaway Team: set, read back, compose with a tighter local limit, clear.
    ws = t.team_workspace(owner="alex")
    policy = f"/api/v1/platform/workspaces/{ws}/policy"
    local = f"/api/v1/workspaces/{ws}/policy"
    t.expect("policy.platform-read", operator.get(policy), 200)
    inherited = jget(operator.get(policy), "effective", "requests_per_minute")
    t.ok("policy.workspace-inherits-type-default", jget(operator.get(policy), "mode") == "inherit")
    body = {"requests_per_minute": 30, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "5000000"}
    t.expect("policy.override-by-member-refused", alex.put(policy, body), 403)
    t.expect("policy.override-without-csrf-refused", operator.put(policy, body, csrf=False), 403)
    t.expect("policy.invalid-refused", operator.put(policy, {"requests_per_minute": 0, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "5000000"}), 400)
    t.expect("policy.override", operator.put(policy, body), 200)
    t.defer("clear platform override", lambda: operator.delete(policy))
    t.ok("policy.override-applies", jget(operator.get(policy), "effective", "requests_per_minute") == 30)
    t.ok("policy.money-is-text", jget(operator.get(policy), "effective", "monthly_budget_microusd") == "5000000")
    tighter = {"requests_per_minute": 10, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "1000000"}
    t.expect("policy.local-tighter-accepted", alex.put(local, tighter), 200)
    t.ok("policy.limits-compose", jget(alex.get(local), "effective", "requests_per_minute") == 10)
    loosened = alex.put(local, {**tighter, "requests_per_minute": 100})
    t.expect("policy.local-cannot-loosen", loosened, 400)
    t.ok("policy.loosen-reason", loosened.error_reason == "exceeds_parent_rate")
    t.expect("policy.local-by-non-member-refused", blair.put(local, tighter), 403)
    t.expect("policy.clear-override", operator.delete(policy), 200)
    t.ok("policy.local-limit-survives-override-removal", jget(operator.get(policy), "effective", "requests_per_minute") == 10)
    t.ok("policy.inherited-value-known", inherited is None or isinstance(inherited, int))

    # Discoverability toggle (reversible).
    t.expect("discoverable.on", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}), 200)
    t.expect("discoverable.off", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": False}), 200)
    t.expect("discoverable.personal-refused", operator.put(f"/api/v1/platform/workspaces/{_personal(t)}/discoverable", {"discoverable": True}), 404)

    # Cost centres: create, rename, remove.
    created = operator.post("/api/v1/platform/cost-centers", {"code": ("J" + t.env.run_id).upper(), "name": t.label("cost-centre")})
    t.expect("cost-centers.create", created, 200)
    cc = jget(created, "id")
    if cc:
        t.defer("delete cost centre", lambda: operator.delete(f"/api/v1/platform/cost-centers/{cc}"))
        t.expect("cost-centers.read", operator.get(f"/api/v1/platform/cost-centers/{cc}"), 200)
        t.expect("cost-centers.create-by-member-refused", alex.post("/api/v1/platform/cost-centers", {"code": "NOPE", "name": "nope"}), 403)
        t.expect("cost-centers.delete", operator.delete(f"/api/v1/platform/cost-centers/{cc}"), 200)
        t.ok("cost-centers.delete-archives", jget(operator.get(f"/api/v1/platform/cost-centers/{cc}"), "archived_at") is not None)


def _personal(t):
    return next(w["id"] for w in jget(t.session("alex").get("/api/v1/me"), "workspaces", default=[]) if w.get("kind") == "personal")

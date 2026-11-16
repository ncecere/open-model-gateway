"""Platform Admin flows: directory, catalogue and configuration reads, policy and limits, and the refusals.

Refusal checks send bodies that are harmless if the gateway wrongly accepts them (a body no create
accepts, the current value written back, or a throwaway object)."""
from ..registry import journey
from .common import NO_SUCH_ID, rows

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
    ("alert-rules", "/api/v1/platform/alerts/rules"),
    ("alert-webhooks", "/api/v1/platform/alerts/webhooks"),
    ("holds", "/api/v1/platform/holds"),
    ("legal-holds", "/api/v1/platform/legal-holds"),
    ("workspace-templates", "/api/v1/platform/workspace-templates"),
    ("admin-tokens", "/api/v1/platform/admin-tokens"),
    ("storage-settings", "/api/v1/platform/settings/storage"),
    ("audit-export-settings", "/api/v1/platform/settings/audit-export"),
    ("email-settings", "/api/v1/platform/settings/email"),
)
# The general settings an unchanged PUT writes back (what the dashboard form submits).
GENERAL_FIELDS = ("display_name", "support_url", "human_key_max_lifetime_days")


def general_body(response):
    data = response.json()
    if response.status != 200 or not isinstance(data, dict):
        return None
    return {k: data[k] for k in GENERAL_FIELDS if k in data}


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
    t.expect("route.unknown-platform-route-as-admin", operator.get("/api/v1/platform/not-a-route"), 404)

    # Directory.
    users = operator.get("/api/v1/platform/users?limit=100")
    t.ok("users.lists-demo-personas", {"operator@demo.invalid", "alex@demo.invalid"} <= {u.get("email") for u in rows(users) or []})
    t.expect("users.detail", operator.get(f"/api/v1/platform/users/{t.user_id('alex')}"), 200)
    t.expect("users.detail-by-member-refused", alex.get(f"/api/v1/platform/users/{t.user_id('blair')}"), 403)
    t.expect("users.search", operator.get("/api/v1/platform/users?q=alex"), 200)
    t.expect("users.control-character-search-refused", operator.get("/api/v1/platform/users?q=%00"), 400)
    t.expect("users.integer-valued-float-limit-refused", operator.get("/api/v1/platform/users?limit=1.0"), 400)
    # A role change by a non-admin with a body no valid change uses: harmless if it were accepted.
    t.expect("users.role-change-by-member-refused", alex.post(f"/api/v1/platform/users/{t.user_id('blair')}/roles", {"role": "not-a-role"}), 403)
    t.expect("users.create-by-member-refused", alex.post("/api/v1/platform/users", {}), 403)

    # Catalogue.
    models = operator.get("/api/v1/platform/models?limit=50")
    listing = rows(models)
    t.ok("catalog.models-have-readiness", listing is not None and bool(listing) and all("readiness" in m for m in listing))
    first = next(iter(listing or []), None)
    if first and first.get("id"):
        t.expect("catalog.model-detail", operator.get(f"/api/v1/platform/models/{first['id']}"), 200)
        t.expect("catalog.model-routing", operator.get(f"/api/v1/platform/models/{first['id']}/routing"), 200)
    else:
        t.skip("catalog.model-detail")
    deployments = rows(operator.get("/api/v1/platform/deployments")) or []
    if deployments and deployments[0].get("id"):
        t.expect("catalog.deployment-detail", operator.get(f"/api/v1/platform/deployments/{deployments[0]['id']}"), 200)
        t.expect("catalog.deployment-prices", operator.get(f"/api/v1/platform/deployments/{deployments[0]['id']}/prices"), 200)
        t.expect("catalog.deployment-configuration-history", operator.get(f"/api/v1/platform/deployments/{deployments[0]['id']}/configuration/history"), 200)
    else:
        t.skip("catalog.deployment-detail")

    # Settings: the current values written back change nothing; a non-admin, or a body without the headers, is refused.
    current = operator.get("/api/v1/platform/settings/general")
    body = general_body(current)
    if body is not None:
        t.expect("settings.general-unchanged-write-by-auditor-refused", auditor.put("/api/v1/platform/settings/general", body), 403)
        t.expect("settings.general-unchanged-write-by-member-refused", alex.put("/api/v1/platform/settings/general", body), 403)
        t.expect("settings.general-unchanged-write-without-csrf-refused", operator.put("/api/v1/platform/settings/general", body, csrf=False), 403)
        t.expect("settings.general-unchanged-write", operator.put("/api/v1/platform/settings/general", body), 200)
        t.ok("settings.general-unchanged-afterwards", general_body(operator.get("/api/v1/platform/settings/general")) == body)
    else:
        t.rec.fail("settings.general-readable", "setup_failed")

    # Policy and limits on a throwaway Team: set, read back, compose with a tighter local limit, clear.
    ws = t.team_workspace(owner="alex")
    policy = f"/api/v1/platform/workspaces/{ws}/policy"
    local = f"/api/v1/workspaces/{ws}/policy"
    t.expect("policy.platform-read", operator.get(policy), 200)
    inherited = operator.get(policy).get("effective", "requests_per_minute")
    t.ok("policy.workspace-inherits-type-default", operator.get(policy).get("mode") == "inherit")
    t.ok("policy.inherited-limit-is-a-number", isinstance(inherited, int) and inherited > 0)
    override = {"requests_per_minute": 30, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "5000000"}
    t.expect("policy.override-by-member-refused", alex.put(policy, override), 403)
    t.expect("policy.override-without-csrf-refused", operator.put(policy, override, csrf=False), 403)
    t.expect("policy.invalid-refused", operator.put(policy, {**override, "requests_per_minute": 0}), 400)
    t.expect("policy.override", operator.put(policy, override), 200)
    t.undo("clear platform override", lambda: operator.delete(policy))
    t.ok("policy.override-applies", operator.get(policy).get("effective", "requests_per_minute") == 30)
    t.ok("policy.money-is-text", operator.get(policy).get("effective", "monthly_budget_microusd") == "5000000")
    tighter = {"requests_per_minute": 10, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "1000000"}
    t.expect("policy.local-tighter-accepted", alex.put(local, tighter), 200)
    t.ok("policy.limits-compose", alex.get(local).get("effective", "requests_per_minute") == 10)
    loosened = alex.put(local, {**tighter, "requests_per_minute": 100})
    t.expect("policy.local-cannot-loosen", loosened, 400)
    t.ok("policy.loosen-reason", loosened.error_reason == "exceeds_parent_rate")
    t.expect("policy.local-by-non-member-refused", blair.put(local, tighter), 403)
    t.expect("policy.clear-override", operator.delete(policy), 200)
    t.ok("policy.local-limit-survives-override-removal", operator.get(policy).get("effective", "requests_per_minute") == 10)

    # Discoverability toggle (reversible). The personal workspace refusal writes the current value, false.
    t.expect("discoverable.on", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": True}), 200)
    t.expect("discoverable.off", operator.put(f"/api/v1/platform/workspaces/{ws}/discoverable", {"discoverable": False}), 200)
    personal = t.personal_workspace("alex")
    t.expect("discoverable.personal-refused", operator.put(f"/api/v1/platform/workspaces/{personal}/discoverable", {"discoverable": False}), 404)

    # Cost centres: create, rename, remove (DELETE archives).
    t.expect("cost-centers.create-by-member-refused", alex.post("/api/v1/platform/cost-centers", {}), 403)
    t.expect("cost-centers.create-by-auditor-refused", auditor.post("/api/v1/platform/cost-centers", {}), 403)
    created = operator.post("/api/v1/platform/cost-centers", {"code": ("J" + t.env.run_id).upper(), "name": t.label("cost-centre")})
    t.expect("cost-centers.create", created, 200)
    cc = created.id()
    if cc:
        t.undo("archive cost centre", lambda: operator.delete(f"/api/v1/platform/cost-centers/{cc}"))
        t.expect("cost-centers.read", operator.get(f"/api/v1/platform/cost-centers/{cc}"), 200)
        t.expect("cost-centers.delete", operator.delete(f"/api/v1/platform/cost-centers/{cc}"), 200)
        t.ok("cost-centers.delete-archives", operator.get(f"/api/v1/platform/cost-centers/{cc}").get("archived_at") is not None)
    t.expect("cost-centers.unknown", operator.get(f"/api/v1/platform/cost-centers/{NO_SUCH_ID}"), 404)

    # Group mappings: only a body no create accepts is sent to the refusal checks.
    t.expect("group-mappings.create-by-auditor-refused", auditor.post("/api/v1/platform/oidc/group-mappings", {}), 403)
    t.expect("group-mappings.create-by-member-refused", alex.post("/api/v1/platform/oidc/group-mappings", {}), 403)

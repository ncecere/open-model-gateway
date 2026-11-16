"""Inference keys: create, list, detail, rotate, disable, revoke and service-account keys."""
from ..context import Secret
from ..registry import journey
from .common import contains_secret, inference_get, jget


@journey("keys")
def keys(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    alex, blair, operator, auditor = (t.session(p) for p in ("alex", "blair", "operator", "auditor"))
    base = f"/api/v1/workspaces/{ws}"

    t.expect("keys.anonymous-list-refused", anon.get(f"{base}/keys"), 401)
    t.expect("keys.anonymous-create-refused", anon.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 1}), 401)
    t.expect("keys.platform-admin-cannot-mint", operator.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 1}), 403)
    t.expect("keys.auditor-cannot-mint", auditor.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 1}), 403)
    before = len(jget(alex.get(f"{base}/keys"), "data", default=[]))

    # Input validation: nothing is created.
    max_days = jget(alex.get("/api/v1/me"), "installation", "key_max_lifetime_days", default=365)
    t.expect("keys.lifetime-zero-refused", alex.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 0}), 400)
    t.expect("keys.lifetime-over-365-refused", alex.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 366}), 400)
    t.expect("keys.blank-name-refused", alex.post(f"{base}/keys", {"name": "", "expires_in_days": 1}), 400)
    t.expect("keys.missing-field-refused", alex.post(f"{base}/keys", {"name": t.label("k")}), 422)
    if max_days < 365:
        refused = alex.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": max_days + 1})
        t.expect("keys.over-installation-maximum-refused", refused, 400)
        t.ok("keys.over-installation-maximum-reason", refused.error_reason == "key_lifetime_exceeds_maximum")
    else:
        t.skip("keys.over-installation-maximum-refused")
    t.ok("keys.refusals-create-nothing", len(jget(alex.get(f"{base}/keys"), "data", default=[])) == before)
    t.expect("keys.issuance-budget", alex.get(f"{base}/key-issuance"), 200)

    # Create, list, detail.
    key_id, token = t.mint_key("alex", ws)
    listing = alex.get(f"{base}/keys")
    t.expect("keys.list", listing, 200)
    row = next((k for k in jget(listing, "data", default=[]) if k.get("id") == key_id), {})
    t.ok("keys.list-has-new-key", row.get("status") == "active")
    t.ok("keys.list-never-returns-token", not contains_secret(listing, token))
    detail = alex.get(f"{base}/keys/{key_id}")
    t.expect("keys.detail", detail, 200)
    t.ok("keys.detail-never-returns-token", not contains_secret(detail, token))
    t.expect("keys.stats", alex.get(f"{base}/keys/{key_id}/stats"), 200)
    t.expect("keys.key-access", alex.get(f"{base}/keys/{key_id}/access"), 200)
    t.expect("keys.key-policy-read", alex.get(f"{base}/keys/{key_id}/policy"), 200)
    t.expect("keys.authenticates-inference", inference_get(anon, "/v1/models", token), 200)

    # Visibility between people in one workspace.
    t.expect("keys.member-cannot-see-others-key", blair.get(f"{base}/keys/{key_id}"), 404)
    t.expect("keys.member-cannot-rotate-others-key", blair.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1}), 404)
    t.expect("keys.member-cannot-revoke-others-key", blair.delete(f"{base}/keys/{key_id}"), 404)
    blair_key, blair_token = t.mint_key("blair", ws)
    t.expect("keys.member-mints-own", blair.get(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.admin-reads-members-key", alex.get(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.platform-admin-cannot-read-key", operator.get(f"{base}/keys/{key_id}"), 403)
    t.expect("keys.mutation-without-csrf-refused", alex.post(f"{base}/keys", {"name": t.label("k"), "expires_in_days": 1}, csrf=False), 403)

    # Disable and re-enable (reversible), then rotate.
    t.expect("keys.disable", alex.patch(f"{base}/keys/{key_id}", {"disabled": True}), 200)
    t.expect("keys.disabled-key-refused", inference_get(anon, "/v1/models", token), 401)
    t.expect("keys.disabled-key-cannot-rotate", alex.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1}), 409)
    t.expect("keys.enable", alex.patch(f"{base}/keys/{key_id}", {"disabled": False}), 200)
    t.expect("keys.enabled-key-works", inference_get(anon, "/v1/models", token), 200)
    rotated = alex.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1})
    t.expect("keys.rotate", rotated, 200)
    new_id = jget(rotated, "id")
    new_token = None
    if isinstance(jget(rotated, "token"), str):
        new_token = Secret(rotated.json()["token"])
        t.defer("revoke rotated key", lambda: alex.delete(f"{base}/keys/{new_id}"))
    t.expect("keys.rotated-old-key-refused", inference_get(anon, "/v1/models", token), 401)
    if new_token:
        t.expect("keys.rotated-new-key-works", inference_get(anon, "/v1/models", new_token), 200)
        # Revoke the new credential: permanent, and it can never be re-enabled.
        t.expect("keys.revoke", alex.delete(f"{base}/keys/{new_id}"), 200)
        t.expect("keys.revoked-key-refused", inference_get(anon, "/v1/models", new_token), 401)
        t.expect("keys.revoked-key-cannot-reenable", alex.patch(f"{base}/keys/{new_id}", {"disabled": False}), 409)
        t.ok("keys.revoked-key-reason", alex.patch(f"{base}/keys/{new_id}", {"disabled": False}).error_reason == "key_revoked")
        t.ok("keys.revoked-key-listed-as-revoked", jget(alex.get(f"{base}/keys/{new_id}"), "status") == "revoked")
        t.expect("keys.revoke-is-idempotent", alex.delete(f"{base}/keys/{new_id}"), 200)
    t.expect("keys.blair-key-revoked-by-admin", alex.delete(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.blair-revoked-key-refused", inference_get(anon, "/v1/models", blair_token), 401)

    # Service-account keys belong to the workspace, not a person.
    t.expect("service-accounts.member-cannot-create", blair.post(f"{base}/service-accounts", {"name": t.label("sa")}), 403)
    created = alex.post(f"{base}/service-accounts", {"name": t.label("sa")})
    t.expect("service-accounts.create", created, 200)
    sa = jget(created, "id")
    if sa:
        t.expect("service-accounts.list", alex.get(f"{base}/service-accounts"), 200)
        minted = alex.post(f"{base}/keys", {"name": t.label("sk"), "expires_in_days": 1, "service_account_id": sa})
        t.expect("service-accounts.mint-key", minted, 200)
        if isinstance(jget(minted, "token"), str):
            sa_token = Secret(minted.json()["token"])
            sa_key = jget(minted, "id")
            t.defer("revoke service key", lambda: alex.delete(f"{base}/keys/{sa_key}"))
            t.expect("service-accounts.key-works", inference_get(anon, "/v1/models", sa_token), 200)
            t.expect("service-accounts.disable", alex.patch(f"{base}/service-accounts/{sa}", {"disabled": True}), 200)
            t.expect("service-accounts.disabled-key-refused", inference_get(anon, "/v1/models", sa_token), 401)

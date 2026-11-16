"""Inference keys: create, list, detail, rotate, disable, revoke, key policy and service-account keys."""
from ..context import Secret
from ..registry import journey
from .common import contains_secret, inference_get, rows


@journey("keys")
def keys(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    alex, blair, operator, auditor = (t.session(p) for p in ("alex", "blair", "operator", "auditor"))
    base = f"/api/v1/workspaces/{ws}"
    new_key = {"name": t.label("refused"), "expires_in_days": 1}

    def revoke(session):
        """Undo for a key that a regressed refusal created."""
        return lambda r: session.delete(f"{base}/keys/{r.id()}") if r.id() else r

    t.expect("keys.anonymous-list-refused", anon.get(f"{base}/keys"), 401)
    t.expect("keys.anonymous-create-refused", anon.post(f"{base}/keys", new_key), 401)
    t.must_refuse("keys.platform-admin-cannot-mint", operator.post(f"{base}/keys", new_key), 403, undo=revoke(operator))
    t.must_refuse("keys.auditor-cannot-mint", auditor.post(f"{base}/keys", new_key), 403, undo=revoke(auditor))
    listing = rows(alex.get(f"{base}/keys"))
    before = len(listing) if listing is not None else None

    # Input validation: nothing is created.
    max_days = alex.get("/api/v1/me").get("installation", "key_max_lifetime_days", default=365)
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
    after = rows(alex.get(f"{base}/keys"))
    t.ok("keys.refusals-create-nothing", before is not None and after is not None and len(after) == before)
    t.expect("keys.issuance-budget", alex.get(f"{base}/key-issuance"), 200)
    t.expect("keys.key-safety", alex.get(f"{base}/key-safety"), 200)
    t.expect("keys.key-safety-anonymous-refused", anon.get(f"{base}/key-safety"), 401)

    # Create, list, detail.
    key_id, token = t.mint_key("alex", ws)
    listing = alex.get(f"{base}/keys")
    t.expect("keys.list", listing, 200)
    row = next((k for k in (rows(listing) or []) if k.get("id") == key_id), {})
    t.ok("keys.list-has-new-key", row.get("status") == "active")
    t.ok("keys.list-never-returns-token", not contains_secret(listing, token))
    detail = alex.get(f"{base}/keys/{key_id}")
    t.expect("keys.detail", detail, 200)
    t.ok("keys.detail-never-returns-token", not contains_secret(detail, token))
    t.expect("keys.stats", alex.get(f"{base}/keys/{key_id}/stats"), 200)
    t.expect("keys.key-access", alex.get(f"{base}/keys/{key_id}/access"), 200)
    t.expect("keys.key-policy-read", alex.get(f"{base}/keys/{key_id}/policy"), 200)
    t.expect("keys.authenticates-inference", inference_get(anon, "/v1/models", token), 200)

    # Key policy: a key can only tighten what the workspace and the platform allow.
    policy = f"{base}/keys/{key_id}/policy"
    tight = {"requests_per_minute": 5, "tokens_per_minute": None, "concurrent_requests": None, "monthly_budget_microusd": "100000"}
    t.expect("keys.policy-by-member-refused", blair.put(policy, tight), 404)
    t.expect("keys.policy-tighter-accepted", alex.put(policy, tight), 200)
    t.ok("keys.policy-applies", alex.get(policy).get("effective", "requests_per_minute") == 5)
    loosened = alex.put(policy, {**tight, "requests_per_minute": 1000000})
    t.expect("keys.policy-cannot-loosen", loosened, 400)
    t.ok("keys.policy-loosen-reason", loosened.error_reason == "exceeds_parent_rate")
    t.ok("keys.policy-unchanged-after-refusal", alex.get(policy).get("effective", "requests_per_minute") == 5)

    # Visibility between people in one workspace.
    t.expect("keys.member-cannot-see-others-key", blair.get(f"{base}/keys/{key_id}"), 404)
    t.expect("keys.member-cannot-rotate-others-key", blair.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1}), 404)
    t.expect("keys.member-cannot-revoke-others-key", blair.delete(f"{base}/keys/{key_id}"), 404)
    t.expect("keys.still-valid-after-refusals", inference_get(anon, "/v1/models", token), 200)
    blair_key, blair_token = t.mint_key("blair", ws)
    t.expect("keys.member-mints-own", blair.get(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.admin-reads-members-key", alex.get(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.platform-admin-cannot-read-key", operator.get(f"{base}/keys/{key_id}"), 403)
    t.must_refuse("keys.mutation-without-csrf-refused", alex.post(f"{base}/keys", new_key, csrf=False), 403, undo=revoke(alex))

    # Disable and re-enable (reversible), then rotate.
    t.expect("keys.disable", alex.patch(f"{base}/keys/{key_id}", {"disabled": True}), 200)
    t.expect("keys.disabled-key-refused", inference_get(anon, "/v1/models", token), 401)
    t.expect("keys.disabled-key-cannot-rotate", alex.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1}), 409)
    t.expect("keys.enable", alex.patch(f"{base}/keys/{key_id}", {"disabled": False}), 200)
    t.expect("keys.enabled-key-works", inference_get(anon, "/v1/models", token), 200)
    rotated = alex.post(f"{base}/keys/{key_id}/rotate", {"expires_in_days": 1})
    t.expect("keys.rotate", rotated, 200)
    new_id, new_token = rotated.id(), Secret.of(rotated)
    if new_id:
        t.undo("revoke rotated key", lambda: alex.delete(f"{base}/keys/{new_id}"), gone=(404,))
    t.expect("keys.rotated-old-key-refused", inference_get(anon, "/v1/models", token), 401)
    if new_id and new_token:
        t.expect("keys.rotated-new-key-works", inference_get(anon, "/v1/models", new_token), 200)
        # Revoke the new credential: permanent, and it can never be re-enabled.
        t.expect("keys.revoke", alex.delete(f"{base}/keys/{new_id}"), 200)
        t.expect("keys.revoked-key-refused", inference_get(anon, "/v1/models", new_token), 401)
        again = alex.patch(f"{base}/keys/{new_id}", {"disabled": False})
        t.expect("keys.revoked-key-cannot-reenable", again, 409)
        t.ok("keys.revoked-key-reason", again.error_reason == "key_revoked")
        t.ok("keys.revoked-key-listed-as-revoked", alex.get(f"{base}/keys/{new_id}").get("status") == "revoked")
        t.expect("keys.revoke-is-idempotent", alex.delete(f"{base}/keys/{new_id}"), 200)
    else:
        t.rec.fail("keys.rotation-returned-a-key", "setup_failed")
    t.expect("keys.blair-key-revoked-by-admin", alex.delete(f"{base}/keys/{blair_key}"), 200)
    t.expect("keys.blair-revoked-key-refused", inference_get(anon, "/v1/models", blair_token), 401)

    # Service-account keys belong to the workspace, not a person.
    t.expect("service-accounts.member-cannot-create", blair.post(f"{base}/service-accounts", {"name": t.label("sa")}), 403)
    created = alex.post(f"{base}/service-accounts", {"name": t.label("sa")})
    t.expect("service-accounts.create", created, 200)
    sa = created.id()
    if sa:
        t.undo("disable service account", lambda: alex.patch(f"{base}/service-accounts/{sa}", {"disabled": True}))
        t.expect("service-accounts.list", alex.get(f"{base}/service-accounts"), 200)
        minted = alex.post(f"{base}/keys", {"name": t.label("sk"), "expires_in_days": 1, "service_account_id": sa})
        t.expect("service-accounts.mint-key", minted, 200)
        sa_key, sa_token = minted.id(), Secret.of(minted)
        if sa_key and sa_token:
            t.undo("revoke service key", lambda: alex.delete(f"{base}/keys/{sa_key}"), gone=(404,))
            t.expect("service-accounts.key-works", inference_get(anon, "/v1/models", sa_token), 200)
            t.expect("service-accounts.disable", alex.patch(f"{base}/service-accounts/{sa}", {"disabled": True}), 200)
            t.expect("service-accounts.disabled-key-refused", inference_get(anon, "/v1/models", sa_token), 401)

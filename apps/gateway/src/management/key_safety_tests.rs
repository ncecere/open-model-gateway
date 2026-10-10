//! Key safety audit and model compare: findings against live configuration,
//! workspace/platform visibility and the personal-key privacy invariant.
use super::*;

async fn get(f: &Fixture, u: &BrowserPrincipal, path: &str) -> (StatusCode, Value) {
    call(&f.s, u, "GET", path, json!({})).await
}
/// An enabled route (enabled connection) for model `m`.
async fn route(pool: &PgPool, m: Uuid) -> Uuid {
    let (connection, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Mock','openai','env:TEST',true)").bind(connection).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'mock-upstream',true)").bind(deployment).bind(m).bind(connection).execute(pool).await.unwrap();
    deployment
}
/// One root request (single attempt) `at` (SQL expression) by key `k`.
#[allow(clippy::too_many_arguments)]
async fn run(
    pool: &PgPool,
    ws: Uuid,
    k: Uuid,
    deployment: Uuid,
    model: &str,
    state: &str,
    at: &str,
    ttft: Option<i64>,
) {
    sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,completed_at,elapsed_ms,root_request_id,input_tokens,output_tokens,workload_kind,generation_ms,time_to_first_token_ms) VALUES($1,$2,$3,$4,$5,'openai',$7 IS NOT NULL,$6,{at},{at}+interval '400 milliseconds',400,$1,10,20,'generation',300,$7)")).bind(Uuid::new_v4()).bind(ws).bind(k).bind(deployment).bind(model).bind(state).bind(ttft).execute(pool).await.unwrap();
}
fn row(v: &Value, key: Uuid) -> Option<&Value> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"]["id"] == key.to_string())
}
fn codes(v: &Value, key: Uuid) -> Vec<String> {
    row(v, key).map_or_else(Vec::new, |r| {
        r["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["code"].as_str().unwrap().to_owned())
            .collect()
    })
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_safety_findings_follow_effective_limits_and_list_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/workspaces/{}/key-safety", f.team);
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (ok, mk) = (id(&owner_key), id(&member_key));
    // No budget or cap anywhere: high.
    let (status, v) = get(&f, &f.owner, &path).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(codes(&v, ok), ["no_limits"]);
    assert_eq!(v["summary"]["high"], 2);
    assert_eq!(v["thresholds"]["unused_days"], 30);
    // A rate cap on the type default: budget still missing (medium).
    sqlx::query("INSERT INTO workspace_type_policies(kind,requests_per_minute) VALUES('team',10) ON CONFLICT(kind) DO UPDATE SET requests_per_minute=EXCLUDED.requests_per_minute")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(codes(&get(&f, &f.owner, &path).await.1, ok), ["no_budget"]);
    // A platform override replaces the type default, so its cap no longer counts.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(codes(&get(&f, &f.owner, &path).await.1, ok), ["no_limits"]);
    // A platform override budget applies to every key of the workspace.
    crate::governance::set_test_budget(
        &pool,
        "override",
        None,
        Some(f.team),
        None,
        "month",
        Some(9),
    )
    .await;
    let (_, v) = get(&f, &f.owner, &path).await;
    assert!(codes(&v, ok).is_empty() && codes(&v, mk).is_empty(), "{v}");
    assert_eq!(v["summary"]["flagged"], 0);
    // Expiry beyond the installation maximum (human keys), no expiry, old secret, never used.
    sqlx::query("UPDATE api_keys SET expires_at=now()+interval '300 days' WHERE id=$1")
        .bind(ok)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE installation_settings SET human_key_max_lifetime_days=90")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE api_keys SET expires_at=NULL,created_at=now()-interval '200 days' WHERE id=$1",
    )
    .bind(mk)
    .execute(&pool)
    .await
    .unwrap();
    let (_, v) = get(&f, &f.owner, &path).await;
    assert_eq!(codes(&v, ok), ["expiry_beyond_max"]);
    assert_eq!(codes(&v, mk), ["no_expiry", "not_rotated", "never_used"]);
    assert_eq!(row(&v, mk).unwrap()["severity"], "high");
    assert_eq!(row(&v, mk).unwrap()["key"]["holder"], "member");
    // Unused for 45 days; custom threshold makes it current again.
    let m = model(&pool, "safety-model").await;
    let d = route(&pool, m).await;
    run(
        &pool,
        f.team,
        ok,
        d,
        "safety-model",
        "succeeded",
        "now()-interval '45 days'",
        None,
    )
    .await;
    let (_, v) = get(&f, &f.owner, &path).await;
    assert_eq!(codes(&v, ok), ["expiry_beyond_max", "unused"]);
    assert_eq!(row(&v, ok).unwrap()["findings"][1]["days"], 45);
    assert_eq!(
        codes(
            &get(&f, &f.owner, &format!("{path}?unused_days=60")).await.1,
            ok
        ),
        ["expiry_beyond_max"]
    );
    // Broad access: five usable models and no key restriction; a restricted key is fine.
    let mut models = vec![m];
    direct(&pool, f.team, m).await;
    for n in 0..4 {
        let extra = model(&pool, &format!("broad-{n}")).await;
        direct(&pool, f.team, extra).await;
        models.push(extra);
    }
    let (_, restricted) = key(&f, &f.owner, f.team, json!([models[0]])).await;
    let (_, v) = get(&f, &f.owner, &path).await;
    assert!(codes(&v, ok).contains(&"broad_model_access".to_owned()));
    assert!(codes(&v, id(&restricted)).is_empty(), "{v}");
    // Holder lost access (membership revoked without revoking the key): shared keys only.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = get(&f, &f.owner, &path).await;
    assert_eq!(codes(&v, mk)[0], "no_expiry");
    assert!(codes(&v, mk).contains(&"owner_lost_access".to_owned()));
    // Revoked, disabled and expired keys are not audited.
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(mk)
        .execute(&pool)
        .await
        .unwrap();
    assert!(row(&get(&f, &f.owner, &path).await.1, mk).is_none());
    // Single key filter (the key page).
    let (_, one) = get(&f, &f.owner, &format!("{path}?key_id={ok}")).await;
    assert_eq!(one["summary"]["keys"], 1);
    for bad in [
        "unused_days=0",
        "rotation_days=5000",
        "workspace_id=x",
        "nope=1",
    ] {
        assert_eq!(
            get(&f, &f.owner, &format!("{path}?{bad}")).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_safety_members_see_own_keys_and_personal_keys_stay_private(pool: PgPool) {
    let f = fixture(&pool).await;
    let team = format!("/api/v1/workspaces/{}/key-safety", f.team);
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, project_key) = key(&f, &f.owner, f.project, Value::Null).await;
    let (status, personal_key) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.personal),
        json!({"name":"Secret personal name","expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pk = id(&personal_key);
    // Members: only their own key.
    let (status, v) = get(&f, &f.member, &team).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["summary"]["keys"], 1);
    assert!(row(&v, id(&member_key)).is_some());
    assert!(row(&v, id(&owner_key)).is_none());
    assert_eq!(row(&v, id(&member_key)).unwrap()["key"]["holder"], "you");
    // The member can't single out another key either.
    let (_, other) = get(&f, &f.member, &format!("{team}?key_id={}", id(&owner_key))).await;
    assert_eq!(other["summary"]["keys"], 0);
    // Shared administrators: every key of the workspace.
    assert_eq!(get(&f, &f.owner, &team).await.1["summary"]["keys"], 2);
    // Personal: owner only; platform roles and outsiders never.
    let personal = format!("/api/v1/workspaces/{}/key-safety", f.personal);
    assert!(row(&get(&f, &f.owner, &personal).await.1, pk).is_some());
    for u in [&f.admin, &f.auditor, &f.outsider] {
        assert_eq!(get(&f, u, &personal).await.0, StatusCode::FORBIDDEN);
        // Non-member platform staff have no workspace key list either.
        assert_eq!(get(&f, u, &team).await.0, StatusCode::FORBIDDEN);
    }
    // Platform scope: Team/Project rows without names or holders; personal keys only as counts.
    for u in [&f.admin, &f.auditor] {
        let (status, v) = get(&f, u, "/api/v1/platform/key-safety").await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let text = v.to_string();
        assert!(!text.contains(&pk.to_string()), "{v}");
        assert!(!text.contains("Secret personal name"));
        assert!(!text.contains(&f.owner.user_id.to_string()));
        assert!(!text.contains("\"name\":\"Key\""));
        assert_eq!(v["summary"]["keys"], 3);
        assert_eq!(v["personal"]["keys"], 1);
        assert_eq!(v["personal"]["high"], 1);
        assert!(row(&v, id(&project_key)).is_some());
        let r = row(&v, id(&owner_key)).unwrap();
        assert_eq!(r["key"]["workspace"]["name"], "Team");
        assert!(r["key"].get("name").is_none() && r["key"].get("issued_to_user_id").is_none());
    }
    let (_, filtered) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/key-safety?workspace_id={}", f.project),
    )
    .await;
    assert_eq!(filtered["summary"]["keys"], 1);
    assert!(filtered["personal"].is_null());
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/platform/key-safety?workspace_id={}", f.personal)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/platform/key-safety?key_id={pk}")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&f, &f.owner, "/api/v1/platform/key-safety").await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_compare_prices_metrics_and_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = model(&pool, "cmp-a").await;
    let b = model(&pool, "cmp-b").await;
    let hidden = model(&pool, "cmp-hidden").await;
    let (da, db) = (route(&pool, a).await, route(&pool, b).await);
    route(&pool, hidden).await;
    for m in [a, b] {
        direct(&pool, f.team, m).await;
        direct(&pool, f.personal, m).await;
    }
    // A: v3 price lines (exact integers); B: no price (unknown, not zero).
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,200000,8000,3,$3,'{}')").bind(Uuid::new_v4()).bind(da).bind(json!([
        {"meter":"input_tokens","microusd_per_batch":"123457","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
        {"meter":"output_tokens","microusd_per_batch":"9007199254740993","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"}
    ])).execute(&pool).await.unwrap();
    let (_, ok) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, mk) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, pk) = key(&f, &f.owner, f.personal, Value::Null).await;
    let now = "now()-interval '1 hour'";
    run(
        &pool,
        f.team,
        id(&ok),
        da,
        "cmp-a",
        "succeeded",
        now,
        Some(100),
    )
    .await;
    run(&pool, f.team, id(&ok), da, "cmp-a", "failed", now, None).await;
    run(
        &pool,
        f.team,
        id(&mk),
        da,
        "cmp-a",
        "succeeded",
        now,
        Some(50),
    )
    .await;
    run(
        &pool,
        f.personal,
        id(&pk),
        da,
        "cmp-a",
        "succeeded",
        now,
        None,
    )
    .await;
    run(
        &pool,
        f.personal,
        id(&pk),
        da,
        "cmp-a",
        "succeeded",
        now,
        None,
    )
    .await;
    // Outside the 30-day window.
    run(
        &pool,
        f.team,
        id(&ok),
        db,
        "cmp-b",
        "succeeded",
        "now()-interval '40 days'",
        None,
    )
    .await;
    let ws = |w: Uuid, ids: String| format!("/api/v1/workspaces/{w}/models/compare?ids={ids}");
    let pair = format!("{a},{b}");
    // Team admin: workspace-wide activity, ordered as asked.
    let (status, v) = get(&f, &f.owner, &ws(f.team, format!("{b},{a}"))).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["activity"], "workspace");
    assert_eq!(v["data"][0]["public_name"], "cmp-b");
    let ma = &v["data"][1];
    assert_eq!(ma["metrics"]["requests"], "3");
    assert_eq!(ma["metrics"]["failed"], "1");
    assert_eq!(ma["metrics"]["error_rate"], "0.3333");
    assert_eq!(ma["metrics"]["latency_p50_ms"], 400);
    assert_eq!(ma["metrics"]["ttft_p50_ms"], 50);
    assert_eq!(
        ma["price"]["lines"][1]["microusd_per_batch"],
        "9007199254740993"
    );
    assert_eq!(ma["price"]["input_token_limit"], 200000);
    assert_eq!(ma["workload"], "generation");
    assert_eq!(ma["enabled_routes"], 1);
    assert!(v["data"][0]["price"].is_null());
    assert_eq!(v["data"][0]["metrics"]["requests"], "0");
    assert!(v["data"][0]["metrics"]["error_rate"].is_null());
    // Member: own activity only.
    let (_, mv) = get(&f, &f.member, &ws(f.team, pair.clone())).await;
    assert_eq!(mv["activity"], "own");
    assert_eq!(mv["data"][0]["metrics"]["requests"], "1");
    // Personal owner: personal activity.
    let (_, pv) = get(&f, &f.owner, &ws(f.personal, pair.clone())).await;
    assert_eq!(pv["data"][0]["metrics"]["requests"], "2");
    // Platform: Team/Project aggregate only (personal requests excluded).
    for u in [&f.admin, &f.auditor] {
        let (status, v) = get(
            &f,
            u,
            &format!("/api/v1/platform/models/compare?ids={a},{hidden}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["activity"], "shared_workspaces");
        assert_eq!(v["data"][0]["metrics"]["requests"], "3");
    }
    // Not in this workspace, wrong counts, non-members.
    assert_eq!(
        get(&f, &f.owner, &ws(f.team, format!("{a},{hidden}")))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for bad in [a.to_string(), format!("{a},{a}"), "x,y".into()] {
        assert_eq!(
            get(&f, &f.owner, &ws(f.team, bad)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    for u in [&f.admin, &f.outsider] {
        assert_eq!(
            get(&f, u, &ws(f.personal, pair.clone())).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        get(
            &f,
            &f.owner,
            &format!("/api/v1/platform/models/compare?ids={pair}")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

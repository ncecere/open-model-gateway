//! Wave-2 gaps: key creation with an initial policy, machine-readable policy
//! rejections, single-key reads, usage/records filters, string series,
//! installation budget usage, platform member privacy and model `created_at`.
use super::*;

async fn key_count(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE workspace_id=$1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}
fn new_key(extra: Value) -> Value {
    let mut v = json!({"name":"Capped","expires_in_days":1});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}
fn rates(budgets: Value, rpm: Value) -> Value {
    json!({"requests_per_minute":rpm,"tokens_per_minute":null,"concurrent_requests":null,"budgets":budgets})
}
/// Asserts a structured policy rejection that never echoes an amount.
fn rejected(status: StatusCode, v: &Value, code: StatusCode, reason: &str, detail: (&str, &str)) {
    assert_eq!(status, code, "{v}");
    assert_eq!(v["error"]["reason"], reason, "{v}");
    assert_eq!(v["error"][detail.0], detail.1, "{v}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        !message.bytes().any(|b| b.is_ascii_digit()),
        "no amounts in {message}"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_create_applies_policy_atomically_and_rejections_have_reasons(pool: PgPool) {
    let f = fixture(&pool).await;
    let keys = format!("/api/v1/workspaces/{}/keys", f.team);
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        rates(json!([{"period":"day","amount_microusd":"100"}]), json!(20)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // Backward compatible: no policy fields, no key layer.
    let (status, plain) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert!(plain["policy"].is_null());
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM key_policies WHERE workspace_id=$1 AND governance_key_id=$2",
    )
    .bind(f.team)
    .bind(id(&plain))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, 0);
    // Limits and stacked budgets are stored with the key.
    let (status, capped) = call(
        &f.s,
        &f.member,
        "POST",
        &keys,
        new_key(json!({"requests_per_minute":10,"budgets":[
            {"period":"month","amount_microusd":"1000"},{"period":"day","amount_microusd":"50"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{capped}");
    assert_eq!(capped["policy"]["requests_per_minute"], 10);
    let kpolicy = format!("{keys}/{}/policy", id(&capped));
    let (_, p) = call(&f.s, &f.member, "GET", &kpolicy, json!({})).await;
    assert_eq!(
        p["policy"]["budgets"],
        json!([{"period":"day","amount_microusd":"50"},{"period":"month","amount_microusd":"1000"}])
    );
    assert_eq!(p["policy"]["requests_per_minute"], 10);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='policy.key_updated' AND resource_id=$1",
    )
    .bind(id(&capped))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // Rejections create nothing (same transaction) and name the rule, not amounts.
    let before = key_count(&pool, f.team).await;
    let (status, v) = call(
        &f.s,
        &f.member,
        "POST",
        &keys,
        new_key(json!({"budgets":[{"period":"day","amount_microusd":"150"}]})),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::BAD_REQUEST,
        "exceeds_parent_budget",
        ("period", "day"),
    );
    let (status, v) = call(
        &f.s,
        &f.member,
        "POST",
        &keys,
        new_key(json!({"requests_per_minute":30})),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::BAD_REQUEST,
        "exceeds_parent_rate",
        ("limit", "requests_per_minute"),
    );
    for bad in [
        json!({"budgets":[{"period":"day","amount_microusd":"1"},{"period":"day","amount_microusd":"2"}]}),
        json!({"budgets":[{"period":"year","amount_microusd":"1"}]}),
        json!({"budgets":[{"period":"day","amount_microusd":"0"}]}),
        json!({"tokens_per_minute":0}),
    ] {
        let (status, v) = call(&f.s, &f.member, "POST", &keys, new_key(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} {v}");
    }
    assert_eq!(key_count(&pool, f.team).await, before);
    // Key policy PUT: stored caps cannot be loosened; each case has its reason.
    let day_month = |day: &str| json!([{"period":"day","amount_microusd":day},{"period":"month","amount_microusd":"1000"}]);
    let (status, v) = call(
        &f.s,
        &f.member,
        "PUT",
        &kpolicy,
        rates(day_month("60"), json!(10)),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::FORBIDDEN,
        "stored_budget_raise_not_allowed",
        ("period", "day"),
    );
    let (status, v) = call(
        &f.s,
        &f.member,
        "PUT",
        &kpolicy,
        rates(
            json!([{"period":"week","amount_microusd":"50"},{"period":"month","amount_microusd":"1000"}]),
            json!(10),
        ),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::FORBIDDEN,
        "period_change_not_allowed",
        ("period", "day"),
    );
    let (status, v) = call(
        &f.s,
        &f.member,
        "PUT",
        &kpolicy,
        rates(day_month("50"), Value::Null),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::FORBIDDEN,
        "stored_rate_loosen_not_allowed",
        ("limit", "requests_per_minute"),
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "PUT",
            &kpolicy,
            rates(day_month("40"), json!(10))
        )
        .await
        .0,
        StatusCode::OK
    );
    // Local PUT above a platform (type default) budget of the same period.
    crate::governance::set_test_budget(&pool, "type", Some("team"), None, None, "week", Some(10))
        .await;
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        rates(
            json!([{"period":"day","amount_microusd":"100"},{"period":"week","amount_microusd":"20"}]),
            json!(20),
        ),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::BAD_REQUEST,
        "exceeds_parent_budget",
        ("period", "week"),
    );
    // Personal keys may carry their own caps; personal workspace limits stay platform-set.
    let (status, v) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.personal),
        new_key(json!({"budgets":[{"period":"lifetime","amount_microusd":"5"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.personal),
        rates(json!([]), Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v["error"]["reason"], "personal_limits_platform_controlled");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn single_key_read_follows_list_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, mine) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, owners) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, personal) = key(&f, &f.owner, f.personal, Value::Null).await;
    let path = |ws: Uuid, k: &Value| format!("/api/v1/workspaces/{ws}/keys/{}", id(k));
    let (status, v) = get(&f, &f.member, &path(f.team, &mine)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["id"], mine["id"]);
    assert_eq!(v["status"], "active");
    assert_eq!(v["usage"]["period"], "month");
    assert!(v["lineage_id"].is_string());
    assert!(!v.to_string().contains("omg_") && v.get("token").is_none());
    // Same row as the list.
    let (_, list) = get(&f, &f.owner, &format!("/api/v1/workspaces/{}/keys", f.team)).await;
    let listed = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == mine["id"])
        .unwrap()
        .clone();
    let (status, by_admin) = get(&f, &f.owner, &path(f.team, &mine)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_admin, listed);
    // Members cannot read other people's keys; non-members and staff are denied.
    assert_eq!(
        get(&f, &f.member, &path(f.team, &owners)).await.0,
        StatusCode::NOT_FOUND
    );
    for u in [&f.admin, &f.auditor, &f.outsider] {
        assert_eq!(
            get(&f, u, &path(f.team, &mine)).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&f, u, &path(f.personal, &personal)).await.0,
            StatusCode::FORBIDDEN
        );
    }
    // A key id from another workspace is not found here.
    assert_eq!(
        get(&f, &f.owner, &path(f.team, &personal)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, &f.owner, &path(f.personal, &personal)).await.0,
        StatusCode::OK
    );
}

async fn raw_get(f: &Fixture, u: &BrowserPrincipal, path: &str) -> (StatusCode, String, String) {
    let response = routes()
        .layer(Extension(u.clone()))
        .with_state(f.s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let rows = response
        .headers()
        .get("x-export-rows")
        .map(|h| h.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, rows, String::from_utf8(bytes.to_vec()).unwrap())
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn usage_and_records_filters_are_strict_scoped_and_series_are_strings(pool: PgPool) {
    let f = fixture(&pool).await;
    let ra = route(&pool, "flt-alpha", "openai").await;
    let rb = route(&pool, "flt-beta", "openai").await;
    let alpha = model_id(&pool, "flt-alpha").await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, personal_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let (_, account) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"Bot"}),
    )
    .await;
    let (status, service_key) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Bot key","expires_in_days":1,"service_account_id":account["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{service_key}");
    let today = Utc::now().date_naive();
    let range = format!(
        "start_date={}&end_date={}",
        today - chrono::TimeDelta::days(6),
        today + chrono::TimeDelta::days(1)
    );
    attempt(
        &pool,
        &ra,
        simple(f.team, id(&owner_key), "flt-alpha", "now()", 300),
    )
    .await;
    attempt(
        &pool,
        &rb,
        simple(f.team, id(&member_key), "flt-beta", "now()", 100),
    )
    .await;
    let mut failed = simple(f.team, id(&member_key), "flt-beta", "now()", 7);
    failed.state = "failed";
    let failed_id = attempt(&pool, &rb, failed).await;
    attempt(
        &pool,
        &rb,
        simple(f.team, id(&service_key), "flt-beta", "now()", 40),
    )
    .await;
    attempt(
        &pool,
        &ra,
        simple(f.personal, id(&personal_key), "flt-alpha", "now()", 5),
    )
    .await;
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Research','R1')")
        .bind(cc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE inference_executions SET cost_center_id=$1,cost_center_name='Research',cost_center_code='R1' WHERE id=$2").bind(cc).bind(failed_id).execute(&pool).await.unwrap();
    let ov = |q: &str| format!("/api/v1/workspaces/{}/usage/overview?{range}{q}", f.team);
    let spend = |v: &Value| v["tiles"]["spend"]["value"].as_str().unwrap().to_owned();
    let (status, v) = get(&f, &f.owner, &ov("")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(spend(&v), "447");
    // Installation budgets were removed (0026): no field at all.
    assert!(v.get("installation_budgets").is_none());
    // Top models: blended rate per model (300 over 15 tokens) and the model id.
    let top_alpha = v["top"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "flt-alpha")
        .unwrap()
        .clone();
    assert_eq!(top_alpha["blended_microusd_per_million"], "20000000");
    assert_eq!(top_alpha["model_id"], alpha.to_string());
    let member = f.member.user_id;
    for (q, expected) in [
        (format!("&model_id={alpha}"), "300"),
        (
            format!("&key_id={}", member_key["id"].as_str().unwrap()),
            "107",
        ),
        (format!("&member_user_id={member}"), "107"),
        ("&status=failed".to_owned(), "7"),
        ("&status=succeeded,failed".to_owned(), "447"),
        ("&status=in_progress".to_owned(), "0"),
        (format!("&cost_center_id={cc}"), "7"),
        ("&cost_center_id=unallocated".to_owned(), "440"),
        (
            format!("&service_account_id={}", account["id"].as_str().unwrap()),
            "40",
        ),
        (
            format!(
                "&model_id={alpha}&key_id={}",
                member_key["id"].as_str().unwrap()
            ),
            "0",
        ),
    ] {
        let (status, v) = get(&f, &f.owner, &ov(&q)).await;
        assert_eq!(status, StatusCode::OK, "{q} {v}");
        assert_eq!(spend(&v), expected, "{q}");
    }
    // Members: filters narrow their own rows; a member filter needs workspace-wide visibility.
    let (_, m) = get(&f, &f.member, &ov("&status=failed")).await;
    assert_eq!(spend(&m), "7");
    let (_, m) = get(
        &f,
        &f.member,
        &ov(&format!("&key_id={}", owner_key["id"].as_str().unwrap())),
    )
    .await;
    assert_eq!(spend(&m), "0");
    let (status, m) = get(&f, &f.member, &ov(&format!("&member_user_id={member}"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(m["error"]["reason"], "workspace_wide_visibility_required");
    for bad in [
        "&status=",
        "&status=done",
        "&status=failed,,succeeded",
        "&cost_center_id=",
        "&cost_center_id=abc",
        "&model_id=x",
        "&key_id=1",
        "&status=failed&status=succeeded",
        "&service_account_id=",
    ] {
        assert_eq!(
            get(&f, &f.owner, &ov(bad)).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Explore: same filters; every series value is a decimal string.
    let ex = |q: &str| format!("/api/v1/workspaces/{}/usage/explore?{range}&{q}", f.team);
    let (status, e) = get(
        &f,
        &f.owner,
        &ex(&format!(
            "metric=spend&group_by=model&key_id={}",
            member_key["id"].as_str().unwrap()
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{e}");
    assert_eq!(e["total"]["value"], "107");
    assert_eq!(e["rows"].as_array().unwrap().len(), 1);
    assert_eq!(e["rows"][0]["group"]["name"], "flt-beta");
    for metric in ["spend", "requests", "tokens", "cache_hit_rate"] {
        let (status, e) = get(
            &f,
            &f.owner,
            &ex(&format!("metric={metric}&group_by=model&status=succeeded")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{e}");
        let series = e["series"].as_array().unwrap();
        assert_eq!(series.len(), 7);
        for day in series {
            for value in day["values"].as_array().unwrap() {
                assert!(
                    value["value"].is_string()
                        || (metric == "cache_hit_rate" && value["value"].is_null()),
                    "{metric}: {value}"
                );
            }
        }
    }
    let (_, e) = get(&f, &f.owner, &ex("metric=requests&group_by=model")).await;
    let last = e["series"][6]["values"].as_array().unwrap();
    assert!(last.iter().any(|v| v["value"] == "3"), "{e}");
    let (status, e) = get(
        &f,
        &f.member,
        &ex(&format!(
            "metric=spend&group_by=model&member_user_id={member}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{e}");
    assert_eq!(
        get(&f, &f.owner, &ex("metric=spend&group_by=model&status=bad"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // Records: key and multi-status filters in details and CSV.
    let costs = |q: &str| format!("/api/v1/workspaces/{}/costs?{range}{q}", f.team);
    let (status, c) = get(
        &f,
        &f.owner,
        &costs(&format!(
            "&key_id={}&status=failed,succeeded",
            member_key["id"].as_str().unwrap()
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["data"].as_array().unwrap().len(), 2);
    let (_, c) = get(&f, &f.owner, &costs("&status=failed")).await;
    assert_eq!(c["data"].as_array().unwrap().len(), 1);
    assert_eq!(c["data"][0]["state"], "failed");
    for bad in ["&status=nope", "&key_id=zz", "&status="] {
        assert_eq!(
            get(&f, &f.owner, &costs(bad)).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    let (status, rows, csv) = raw_get(
        &f,
        &f.owner,
        &format!(
            "/api/v1/workspaces/{}/usage-export?{range}&key_id={}&status=succeeded",
            f.team,
            member_key["id"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{csv}");
    assert_eq!(rows, "1");
    assert!(csv.contains("\"succeeded\"") && !csv.contains("\"failed\""));
    let (_, c) = get(
        &f,
        &f.owner,
        &format!(
            "/api/v1/workspaces/{}/cost-report?{range}&status=failed",
            f.team
        ),
    )
    .await;
    assert_eq!(c["totals"]["known_cost_microusd"], "7");
    // Request logs accept a status list too.
    let (status, r) = get(
        &f,
        &f.owner,
        &format!(
            "/api/v1/workspaces/{}/requests?status=failed,succeeded",
            f.team
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["data"].as_array().unwrap().len(), 4);
    // Platform overviews carry no installation budgets (removed in 0026).
    let (status, p) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/usage/overview?{range}&model_id={alpha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert!(p.get("installation_budgets").is_none());
    let (_, o) = get(&f, &f.admin, "/api/v1/platform/overview").await;
    assert!(o.get("installation_budgets").is_none());
    assert!(o["setup"].is_object());
    assert_eq!(
        get(&f, &f.owner, "/api/v1/platform/overview").await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_member_rows_show_totals_never_personal_key_identity(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "priv-model", "openai").await;
    let (_, team_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (status, personal_key) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.personal),
        json!({"name":"Hidden personal credential","expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let personal_id = personal_key["id"].as_str().unwrap().to_owned();
    let root = Uuid::new_v4();
    attempt(
        &pool,
        &r,
        simple(f.team, id(&team_key), "priv-model", "now()", 30),
    )
    .await;
    attempt(
        &pool,
        &r,
        Attempt {
            root,
            ..simple(f.personal, id(&personal_key), "priv-model", "now()", 12)
        },
    )
    .await;
    let today = Utc::now().date_naive();
    let range = format!(
        "start_date={}&end_date={}",
        today - chrono::TimeDelta::days(1),
        today + chrono::TimeDelta::days(1)
    );
    let private = |v: &Value| {
        let s = v.to_string();
        assert!(!s.contains(&personal_id), "personal key id leaked: {s}");
        assert!(
            !s.contains("Hidden personal credential"),
            "personal key name leaked: {s}"
        );
        assert!(!s.contains(&root.to_string()), "request id leaked: {s}");
    };
    for u in [&f.admin, &f.auditor] {
        let (status, v) = get(&f, u, &format!("/api/v1/platform/usage/overview?{range}")).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        private(&v);
        // The owner's per-user total includes personal activity (totals allowed).
        let members = v["top"]["members"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["id"], f.owner.user_id.to_string());
        assert_eq!(members[0]["name"], "owner@example.test");
        assert_eq!(members[0]["spend_microusd"], "42");
        let mut fields: Vec<&String> = members[0].as_object().unwrap().keys().collect();
        fields.sort();
        assert_eq!(
            fields,
            [
                "blended_microusd_per_million",
                "id",
                "name",
                "requests",
                "share",
                "spend_microusd",
                "tokens"
            ]
        );
        assert!(
            v["top"]["keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["name"] == "Personal workspace keys" && k["id"].is_null())
        );
        for q in [
            "metric=spend&group_by=member&then_by=key",
            "metric=requests&group_by=key&then_by=member",
            "metric=tokens&group_by=member&then_by=day",
        ] {
            let (status, e) = get(
                &f,
                u,
                &format!("/api/v1/platform/usage/explore?{range}&{q}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{q} {e}");
            private(&e);
        }
        // Knowing a personal key id never yields per-key figures at platform scope.
        let (_, e) = get(
            &f,
            u,
            &format!("/api/v1/platform/usage/overview?{range}&key_id={personal_id}"),
        )
        .await;
        assert_eq!(e["tiles"]["spend"]["value"], "0");
        assert!(e["top"]["members"].as_array().unwrap().is_empty());
        let (_, c) = get(
            &f,
            u,
            &format!("/api/v1/platform/cost-report?{range}&key_id={personal_id}"),
        )
        .await;
        assert_eq!(c["totals"]["known_cost_microusd"], "0");
        // Request details and keys of a personal workspace stay owner-only.
        for path in [
            format!("/api/v1/workspaces/{}/requests", f.personal),
            format!("/api/v1/workspaces/{}/requests/{root}", f.personal),
            format!("/api/v1/workspaces/{}/keys/{personal_id}", f.personal),
        ] {
            assert_eq!(get(&f, u, &path).await.0, StatusCode::FORBIDDEN, "{path}");
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_reads_include_created_at_and_catalog_sorts(pool: PgPool) {
    let f = fixture(&pool).await;
    let old = route(&pool, "srt-old", "openai").await;
    let new = route(&pool, "srt-new", "openai").await;
    for r in [&old, &new] {
        enable(&pool, r).await;
    }
    let (old_id, new_id) = (
        model_id(&pool, "srt-old").await,
        model_id(&pool, "srt-new").await,
    );
    sqlx::query("UPDATE models SET created_at=now()-interval '3 days' WHERE id=$1")
        .bind(old_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000,2000,100,10,1)").bind(Uuid::new_v4()).bind(old.deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,9000,9000,100,10,1)").bind(Uuid::new_v4()).bind(new.deployment).execute(&pool).await.unwrap();
    team_catalog(&pool, &[old_id, new_id]).await;
    let (status, m) = get(&f, &f.auditor, &format!("/api/v1/platform/models/{old_id}")).await;
    assert_eq!(status, StatusCode::OK, "{m}");
    let created: String =
        sqlx::query_scalar("SELECT to_jsonb(created_at)#>>'{}' FROM models WHERE id=$1")
            .bind(old_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(m["created_at"], created);
    let catalog = |q: &str| format!("/api/v1/workspaces/{}/catalog{q}", f.team);
    let names = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["public_name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (status, v) = get(&f, &f.member, &catalog("")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(names(&v), ["srt-new", "srt-old"]);
    assert!(v["data"][0]["created_at"].is_string());
    let (_, v) = get(&f, &f.member, &catalog("?sort=newest")).await;
    assert_eq!(names(&v), ["srt-new", "srt-old"]);
    let (_, v) = get(&f, &f.member, &catalog("?sort=price")).await;
    assert_eq!(names(&v), ["srt-old", "srt-new"]);
    let (_, v) = get(&f, &f.member, &catalog("?sort=name")).await;
    assert_eq!(names(&v), ["srt-new", "srt-old"]);
    sqlx::query("UPDATE models SET created_at=now()+interval '1 day' WHERE id=$1")
        .bind(old_id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = get(&f, &f.member, &catalog("?sort=newest")).await;
    assert_eq!(names(&v), ["srt-old", "srt-new"]);
    assert_eq!(
        get(&f, &f.member, &catalog("?sort=oldest")).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn jobs_at_once_stacks_like_other_limits(pool: PgPool) {
    let f = fixture(&pool).await;
    // Type defaults start at 2; the effective workspace limit inherits it.
    let (status, t) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/workspace-types/team/policy",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    assert_eq!(t["policy"]["concurrent_jobs"], 2);
    let ws_policy = format!("/api/v1/workspaces/{}/policy", f.team);
    let (_, w) = call(&f.s, &f.owner, "GET", &ws_policy, json!({})).await;
    assert_eq!(
        (
            w["effective"]["concurrent_jobs"].clone(),
            w["policy"]["concurrent_jobs"].clone()
        ),
        (json!(2), Value::Null)
    );
    // Workspace layer: tighten-only, with a structured reason.
    let mut body = rates(json!([]), Value::Null);
    body["concurrent_jobs"] = json!(3);
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body.clone()).await;
    rejected(
        status,
        &v,
        StatusCode::BAD_REQUEST,
        "exceeds_parent_rate",
        ("limit", "concurrent_jobs"),
    );
    body["concurrent_jobs"] = json!(1);
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // A body without the field keeps the stored cap; null cannot remove it.
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &ws_policy,
        rates(json!([]), json!(5)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let mut body = rates(json!([]), json!(5));
    body["concurrent_jobs"] = Value::Null;
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body).await;
    rejected(
        status,
        &v,
        StatusCode::FORBIDDEN,
        "stored_rate_loosen_not_allowed",
        ("limit", "concurrent_jobs"),
    );
    let (_, w) = call(&f.s, &f.owner, "GET", &ws_policy, json!({})).await;
    assert_eq!(w["policy"]["concurrent_jobs"], 1);
    assert_eq!(w["effective"]["concurrent_jobs"], 1);
    assert_eq!(w["provenance"]["platform"]["concurrent_jobs"], 2);
    // A key created with a jobs cap above the workspace's is refused.
    let keys = format!("/api/v1/workspaces/{}/keys", f.team);
    let (status, v) = call(
        &f.s,
        &f.member,
        "POST",
        &keys,
        new_key(json!({"concurrent_jobs":2})),
    )
    .await;
    rejected(
        status,
        &v,
        StatusCode::BAD_REQUEST,
        "exceeds_parent_rate",
        ("limit", "concurrent_jobs"),
    );
    let (status, k) = call(
        &f.s,
        &f.member,
        "POST",
        &keys,
        new_key(json!({"concurrent_jobs":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{k}");
    assert_eq!(k["policy"]["concurrent_jobs"], 1);
    // Platform layers are free (replacement override, installation).
    let mut over = rates(json!([]), Value::Null);
    over["concurrent_jobs"] = json!(8);
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PUT",
        &format!("/api/v1/platform/workspaces/{}/policy", f.team),
        over,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, p) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/workspaces/{}/policy", f.team),
        json!({}),
    )
    .await;
    assert_eq!(
        (
            p["provenance"]["platform"]["concurrent_jobs"].clone(),
            p["provenance"]["type_default"]["concurrent_jobs"].clone(),
            p["effective"]["concurrent_jobs"].clone()
        ),
        (json!(8), json!(2), json!(1))
    );
    // Effective access shows the limit per layer.
    let (status, a) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/access", f.team),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    let workspace = a["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["layer"] == "workspace")
        .unwrap();
    assert_eq!(workspace["limits"]["concurrent_jobs"], 1);
}

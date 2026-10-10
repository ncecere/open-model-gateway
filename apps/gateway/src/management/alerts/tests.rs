//! Alerts API: authority per scope, strict validation, auditing, and the
//! privacy of history and in-app notifications.
use super::*;

const RULES: &str = "/api/v1/platform/alerts/rules";

fn budget_rule() -> Value {
    json!({"name":"Budgets","kind":"budget_threshold","budget_layers":["type","local"],"thresholds":[50,80,100],"notify_platform_admins":true})
}
fn spend_rule() -> Value {
    json!({"name":"Installation spend","kind":"spend_threshold","spend_period":"month","spend_amount_microusd":"9007199254740993","thresholds":[80,100],"notify_platform_admins":true})
}
fn ws_rules(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/alerts/rules")
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn installation_rules_are_admin_write_auditor_read(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, created) = call(&f.s, &f.admin, "POST", RULES, budget_rule()).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["scope"], "installation");
    assert_eq!(created["thresholds"], json!([50, 80, 100]));
    assert_eq!(created["firing"], 0);
    let rule = id(&created);
    let one = format!("{RULES}/{rule}");
    for user in [&f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, user, "GET", RULES, json!({})).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f.s, user, "GET", &one, json!({})).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f.s,
                user,
                "GET",
                "/api/v1/platform/alerts/events",
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for user in [&f.owner, &f.member] {
        assert_eq!(
            call(&f.s, user, "GET", RULES, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    for (method, path) in [
        ("POST", RULES),
        ("PUT", one.as_str()),
        ("DELETE", one.as_str()),
    ] {
        assert_eq!(
            call(&f.s, &f.auditor, method, path, budget_rule()).await.0,
            StatusCode::FORBIDDEN,
            "{method}"
        );
    }
    // Strict validation.
    for bad in [
        json!({"name":"x","kind":"budget_threshold","budget_layers":["local"],"thresholds":[0]}),
        json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1","notify_workspace_admins":true}),
        json!({"name":"x","kind":"provider_failing","window_minutes":15,"consecutive_failures":3,"provider_connection_id":Uuid::new_v4()}),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "POST", RULES, bad.clone()).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // There is no installation budget layer (0026): a stable reason says so.
    let (status, err) = call(&f.s, &f.admin, "POST", RULES, json!({"name":"x","kind":"budget_threshold","budget_layers":["installation"],"thresholds":[80]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["reason"], "installation_limits_removed");
    // Installation spend rules round-trip exact micro-USD strings.
    let (status, spend) = call(&f.s, &f.admin, "POST", RULES, spend_rule()).await;
    assert_eq!(status, StatusCode::OK, "{spend}");
    assert_eq!(
        (
            &spend["kind"],
            &spend["spend_period"],
            &spend["spend_amount_microusd"],
            &spend["thresholds"],
            &spend["budget_layers"]
        ),
        (
            &json!("spend_threshold"),
            &json!("month"),
            &json!("9007199254740993"),
            &json!([80, 100]),
            &Value::Null
        )
    );
    let mut extra = budget_rule();
    extra["extra"] = json!(true);
    assert_eq!(
        call(&f.s, &f.admin, "POST", RULES, extra).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // The kind is fixed; everything else is replaced.
    let (status, err) = call(&f.s, &f.admin, "PUT", &one, json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["reason"], "alert_rule_kind_fixed");
    let (status, updated) = call(&f.s, &f.admin, "PUT", &one, json!({"name":"Monthly","kind":"budget_threshold","enabled":false,"budget_layers":["type"],"thresholds":[90],"notify_emails":["Finance@Example.test"]})).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["notify_emails"], json!(["finance@example.test"]));
    assert_eq!(updated["notify_platform_admins"], false);
    assert_eq!(
        call(&f.s, &f.admin, "DELETE", &one, json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "GET", &one, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    // Soft-deleted: history keeps the row; audit records each change.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE resource_type='alert_rule' ORDER BY created_at,action").fetch_all(&pool).await.unwrap();
    assert_eq!(
        actions,
        [
            "alert_rule.created",
            "alert_rule.created",
            "alert_rule.updated",
            "alert_rule.deleted"
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_rules WHERE deleted_at IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_rules_belong_to_shared_admins_and_personal_is_built_in(pool: PgPool) {
    let f = fixture(&pool).await;
    let spike = json!({"name":"Spike","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1000000","notify_workspace_admins":true});
    let path = ws_rules(f.team);
    let (status, created) = call(&f.s, &f.owner, "POST", &path, spike.clone()).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["workspace_id"], f.team.to_string());
    // Members cannot read or write; platform readers read; nobody else.
    assert_eq!(
        call(&f.s, &f.member, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.s, &f.member, "POST", &path, spike.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.s, &f.outsider, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, list) = call(&f.s, &f.auditor, "GET", &path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["writable"], false);
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path, spike.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    // Workspace rules never watch installation spend or connections.
    for bad in [
        spend_rule(),
        json!({"name":"x","kind":"provider_failing","window_minutes":15,"consecutive_failures":3}),
    ] {
        assert_eq!(
            call(&f.s, &f.owner, "POST", &path, bad).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // A workspace's rule is not reachable through another scope.
    let rule = id(&created);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "GET",
            &format!("{}/{rule}", ws_rules(f.project)),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.s, &f.admin, "GET", &format!("{RULES}/{rule}"), json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Personal: built-in only, owner only.
    let (status, personal) = call(&f.s, &f.owner, "GET", &ws_rules(f.personal), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(personal["builtin"]["thresholds"], json!([80, 100]));
    let (status, err) = call(&f.s, &f.owner, "POST", &ws_rules(f.personal), spike).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(err["error"]["reason"], "personal_alerts_built_in");
    for user in [&f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, user, "GET", &ws_rules(f.personal), json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &f.s,
                user,
                "GET",
                &format!("/api/v1/workspaces/{}/alerts/events", f.personal),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}

/// Spend in `ws` through a fresh key of `user`, settled now.
async fn spend(f: &Fixture, pool: &PgPool, ws: Uuid, user: &BrowserPrincipal, amount: i64) {
    let (status, k) = key(f, user, ws, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{k}");
    let (m, p, d, e) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,$2)")
        .bind(m)
        .bind(format!("m-{m}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Mock','openai','env:TEST')").bind(p).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'x')").bind(d).bind(m).bind(p).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES($1,$2,$3,$4,'m','openai',false,'succeeded',$1)").bind(e).bind(ws).bind(id(&k)).bind(d).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,input_tokens,output_tokens) VALUES($1,$2,$3,$4,now(),now(),now(),now(),'settled',$5,1,1)").bind(e).bind(ws).bind(id(&k)).bind(d).bind(amount).execute(pool).await.unwrap();
}
async fn local_budget(pool: &PgPool, ws: Uuid, amount: i64) {
    sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',$1,'month',$2)").bind(ws).bind(amount).execute(pool).await.unwrap();
}
async fn feed(f: &Fixture, user: &BrowserPrincipal) -> Vec<Value> {
    let (status, v) = call(&f.s, user, "GET", "/api/v1/me/notifications", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v["data"].as_array().unwrap().clone()
}
async fn unread(f: &Fixture, user: &BrowserPrincipal) -> i64 {
    call(
        &f.s,
        user,
        "GET",
        "/api/v1/me/notifications/summary",
        json!({}),
    )
    .await
    .1["unread"]
        .as_i64()
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn notifications_follow_live_authority_and_never_leak_private_details(pool: PgPool) {
    let f = fixture(&pool).await;
    local_budget(&pool, f.team, 1_000_000).await;
    local_budget(&pool, f.personal, 1_000_000).await;
    spend(&f, &pool, f.team, &f.owner, 2_000_000).await;
    spend(&f, &pool, f.personal, &f.owner, 2_000_000).await;
    // Installation rule (platform readers), team rule (team admins only).
    assert_eq!(call(&f.s, &f.admin, "POST", RULES, json!({"name":"Shared budgets","kind":"budget_threshold","budget_layers":["local"],"thresholds":[100]})).await.0, StatusCode::OK);
    let (status, team_rule) = call(&f.s, &f.owner, "POST", &ws_rules(f.team), json!({"name":"Team budget","kind":"budget_threshold","budget_layers":["local"],"thresholds":[80]})).await;
    assert_eq!(status, StatusCode::OK);
    let report = crate::alerts::evaluate_once(&f.s).await.unwrap().unwrap();
    assert_eq!(report.fired, 3, "{report:?}");
    // Owner: own team rule incident + own personal built-in (not the installation rule).
    let mine = feed(&f, &f.owner).await;
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert!(
        mine.iter()
            .any(|n| n["builtin"] == true && n["workspace"]["id"] == f.personal.to_string())
    );
    assert!(mine.iter().any(|n| n["rule"]["name"] == "Team budget"
        && n["state"] == "firing"
        && n["read"] == false));
    // Platform readers: the installation incident only; never the personal built-in or team-only rule.
    for user in [&f.admin, &f.auditor] {
        let theirs = feed(&f, user).await;
        assert_eq!(theirs.len(), 1, "{theirs:?}");
        assert_eq!(theirs[0]["rule"]["name"], "Shared budgets");
        assert_eq!(theirs[0]["workspace"]["kind"], "team");
    }
    // Members of the team see nothing; outsiders neither.
    assert!(feed(&f, &f.member).await.is_empty());
    assert!(feed(&f, &f.outsider).await.is_empty());
    // No key names or owner identities anywhere in payloads.
    let all = serde_json::to_string(&[feed(&f, &f.owner).await, feed(&f, &f.admin).await]).unwrap();
    assert!(
        !all.contains("owner@example.test") && !all.contains("\"Key\""),
        "{all}"
    );
    // Platform history: installation rules only. Workspace history: that scope only.
    let (_, history) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/alerts/events?status=firing",
        json!({}),
    )
    .await;
    assert_eq!(history["data"].as_array().unwrap().len(), 1);
    let (_, team_history) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/alerts/events", f.team),
        json!({}),
    )
    .await;
    assert_eq!(team_history["data"].as_array().unwrap().len(), 1);
    assert_eq!(team_history["data"][0]["rule"]["id"], team_rule["id"]);
    let (_, personal_history) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/alerts/events", f.personal),
        json!({}),
    )
    .await;
    assert_eq!(personal_history["data"][0]["builtin"], true);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            "/api/v1/platform/alerts/events?status=open",
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // Rules notifying Platform Admins share their incidents with platform readers.
    sqlx::query("UPDATE alert_rules SET notify_platform_admins=true WHERE id=$1")
        .bind(id(&team_rule))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(feed(&f, &f.admin).await.len(), 2);
    // Read state is per user; invisible ids are ignored, not disclosed.
    assert_eq!(unread(&f, &f.owner).await, 2);
    let personal_event = mine.iter().find(|n| n["builtin"] == true).unwrap()["id"].clone();
    let (_, marked) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/me/notifications/read",
        json!({"ids":[personal_event]}),
    )
    .await;
    assert_eq!(marked["marked"], 0);
    let (_, marked) = call(
        &f.s,
        &f.owner,
        "POST",
        "/api/v1/me/notifications/read",
        json!({"ids":[personal_event]}),
    )
    .await;
    assert_eq!(marked["marked"], 1);
    assert_eq!(unread(&f, &f.owner).await, 1);
    assert_eq!(unread(&f, &f.admin).await, 2);
    let (_, unread_only) = call(
        &f.s,
        &f.owner,
        "GET",
        "/api/v1/me/notifications?status=unread",
        json!({}),
    )
    .await;
    assert_eq!(unread_only["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            "/api/v1/me/notifications/read",
            json!({"all":true})
        )
        .await
        .1["marked"],
        1
    );
    assert_eq!(unread(&f, &f.owner).await, 0);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            "/api/v1/me/notifications/read",
            json!({"all":true,"ids":[]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // Losing workspace admin rights removes the team's incidents from the feed.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(f.owner.user_id).execute(&pool).await.unwrap();
    let after = feed(&f, &f.owner).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["builtin"], true);
    // Disabling a rule closes its incidents at once.
    let (_, list) = call(&f.s, &f.admin, "GET", RULES, json!({})).await;
    let platform_rule = id(&list["data"][0]);
    assert_eq!(list["data"][0]["firing"], 1);
    let (status, _) = call(&f.s, &f.admin, "PUT", &format!("{RULES}/{platform_rule}"), json!({"name":"Shared budgets","kind":"budget_threshold","enabled":false,"budget_layers":["local"],"thresholds":[100]})).await;
    assert_eq!(status, StatusCode::OK);
    let (_, closed) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/alerts/events?rule_id={platform_rule}"),
        json!({}),
    )
    .await;
    assert_eq!(closed["data"][0]["state"], "resolved");
    assert_eq!(closed["data"][0]["resolution"], "rule_disabled");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scim_last_admin_incident_is_for_platform_readers(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    assert!(
        crate::alerts::fire_scim_last_admin(&mut tx, "user")
            .await
            .unwrap()
    );
    // At most one open incident: a repeat is absorbed.
    assert!(
        !crate::alerts::fire_scim_last_admin(&mut tx, "group")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    // Email goes to Platform Admins (the relay is not configured here).
    assert_eq!(crate::alerts::deliver_pending(&f.s, 10).await, 1);
    let status: String = sqlx::query_scalar("SELECT d.status FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.builtin='scim_last_admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "not_configured");
    for user in [&f.admin, &f.auditor] {
        let (status, v) = call(
            &f.s,
            user,
            "GET",
            "/api/v1/platform/alerts/events?kind=scim_last_admin",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let event = &v["data"][0];
        assert_eq!(
            (
                event["summary"].clone(),
                event["builtin"].clone(),
                event["rule"].clone(),
                event["workspace"].clone()
            ),
            (
                json!(crate::alerts::SCIM_LAST_ADMIN_SUMMARY),
                json!(true),
                Value::Null,
                Value::Null
            )
        );
        assert_eq!(event["details"], json!({"resource":"user"}));
        let (_, n) = call(&f.s, user, "GET", "/api/v1/me/notifications", json!({})).await;
        assert!(
            n["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "scim_last_admin"),
            "{n}"
        );
    }
    for user in [&f.owner, &f.member] {
        let (_, n) = call(&f.s, user, "GET", "/api/v1/me/notifications", json!({})).await;
        assert!(
            !n["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "scim_last_admin"),
            "{n}"
        );
    }
}

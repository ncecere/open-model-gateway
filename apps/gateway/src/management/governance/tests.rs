use super::*;
#[test]
fn strict_exact_money_and_required_nullable_fields() {
    for s in ["", "-1", "+1", "1.0", "1e6", "9223372036854775808"] {
        assert!(money(s, false).is_err());
    }
    assert_eq!(
        money("9007199254740993", false).unwrap(),
        9_007_199_254_740_993
    );
    assert!(serde_json::from_value::<policies::Policy>(json!({"requests_per_minute":1})).is_err());
    assert!(
        serde_json::from_value::<prices::Price>(json!({"input_microusd_per_million":1})).is_err()
    );
}
#[cfg(feature = "integration-tests")]
mod db {
    use super::*;
    use crate::{
        auth::NewApiKey,
        governance::tests::db::{Fixture, billing, done, fixture, rates, request as chat},
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use chrono::{Datelike, Utc};
    use sqlx::PgPool;
    use tower::ServiceExt;
    fn user(id: Uuid, admin: bool) -> BrowserPrincipal {
        BrowserPrincipal {
            user_id: id,
            email: "synthetic@test.invalid".into(),
            platform_admin: admin,
            platform_auditor: false,
        }
    }
    fn policy() -> Value {
        json!({"requests_per_minute":10,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null})
    }
    fn price() -> Value {
        json!({"input_microusd_per_million":"1000000","output_microusd_per_million":"1000000","input_token_limit":100,"output_token_limit":50,"pricing_version":2,"cache_pricing":rates()})
    }
    fn range() -> String {
        let date = Utc::now().date_naive();
        format!(
            "start_date={}&end_date={}",
            date.with_day(1).unwrap(),
            date.checked_add_signed(chrono::TimeDelta::days(1)).unwrap()
        )
    }
    async fn response(
        f: &Fixture,
        u: &BrowserPrincipal,
        method: &str,
        path: &str,
        body: Value,
    ) -> Response {
        routes()
            .merge(platform_routes())
            .merge(resources::platform_routes())
            .layer(Extension(u.clone()))
            .with_state(f.store.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    async fn call(
        f: &Fixture,
        u: &BrowserPrincipal,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let r = response(f, u, method, path, body).await;
        let status = r.status();
        let bytes = to_bytes(r.into_body(), 8 * 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn member_key(f: &Fixture) -> Uuid {
        let k = NewApiKey::generate();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'member',$4)").bind(k.id).bind(f.team.workspace_id).bind(f.other).bind(k.digest.as_slice()).execute(&f.store.pool).await.unwrap();
        k.id
    }
    async fn execution(
        f: &Fixture,
        key: Uuid,
        workspace: Uuid,
        model: &str,
        state: &str,
        actual: Option<i64>,
        held: Option<i64>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let price = if actual.is_some() {
            Some(f.price(1_000_000).await)
        } else {
            None
        };
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,'openai',false,'succeeded',$1,1,$6,CASE WHEN $6::bigint IS NOT NULL THEN 0 END)").bind(id).bind(workspace).bind(key).bind(f.deployment).bind(model).bind(actual).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,price_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,now(),date_trunc('minute',now(),'UTC'),date_trunc('month',now(),'UTC'),now()-interval '1 day',$5,$6,$7,$8,$9,$6,CASE WHEN $6::bigint IS NOT NULL THEN 0 END)").bind(id).bind(workspace).bind(key).bind(f.deployment).bind(state).bind(actual).bind(held).bind(held.is_none()&&actual.is_none()).bind(price).execute(&f.store.pool).await.unwrap();
        id
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn policy_roles_privacy_scope_and_transactional_audit(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let member = user(f.other, false);
        let ws = f.team.workspace_id;
        let p = format!("/api/v1/workspaces/{ws}/policy");
        assert_eq!(
            call(&f, &member, "PUT", &p, policy()).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f, &admin, "PUT", &p, policy()).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f, &member, "GET", &p, Value::Null).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f,
                &member,
                "GET",
                &format!("/api/v1/workspaces/{}/policy", f.principal.workspace_id),
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action='policy.local_updated'"
            )
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            1
        );
        sqlx::query("UPDATE platform_role_grants SET role='user' WHERE user_id=$1")
            .bind(f.owner)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(
                &f,
                &admin,
                "PUT",
                "/api/v1/platform/installation/policy",
                policy()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn platform_override_audit_excludes_all_personal_updates_and_resets(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let mut auditor = user(f.other, false);
        auditor.platform_auditor = true;
        let foreign_personal = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Foreign personal','personal',$2)")
            .bind(foreign_personal).bind(f.other).execute(&f.store.pool).await.unwrap();
        for ws in [
            f.principal.workspace_id,
            foreign_personal,
            f.team.workspace_id,
        ] {
            let path = format!("/api/v1/platform/workspaces/{ws}/policy");
            assert_eq!(
                call(&f, &admin, "PUT", &path, policy()).await.0,
                StatusCode::OK
            );
            assert_eq!(
                call(&f, &admin, "DELETE", &path, Value::Null).await.0,
                StatusCode::OK
            );
            let stored: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT workspace_id,resource_id FROM audit_events WHERE resource_id=$1 AND action IN('policy.override_updated','policy.override_reset')")
                .bind(ws).fetch_all(&f.store.pool).await.unwrap();
            assert_eq!(stored, vec![(ws, ws); 2]);
        }
        let (status, audit) =
            call(&f, &auditor, "GET", "/api/v1/platform/audit", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let events = audit["data"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        for action in ["policy.override_updated", "policy.override_reset"] {
            assert!(events.iter().any(|event| event["action"] == action
                && event["workspace_id"] == f.team.workspace_id.to_string()
                && event["resource_id"] == f.team.workspace_id.to_string()));
        }
        assert!(
            !audit
                .to_string()
                .contains(&f.principal.workspace_id.to_string())
        );
        assert!(!audit.to_string().contains(&foreign_personal.to_string()));
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v1_inclusive_settlement_reports_only_aggregate_legacy_cost(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let mut ids = Vec::new();
        for raw in [
            json!({"input_tokens":6,"output_tokens":2}),
            json!({"input_tokens":6,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":10}),
        ] {
            let start = f.start();
            crate::governance::admit(&f.store, &start, &chat(), 30)
                .await
                .unwrap();
            let mut record = done(start.id, None, None);
            record.usage = crate::providers::metering::anthropic(&raw).unwrap();
            crate::governance::finish(&f.store, &record).await.unwrap();
            ids.push(start.id);
        }
        let admin = user(f.owner, true);
        let path = format!(
            "/api/v1/workspaces/{}/cost-report?{}",
            f.principal.workspace_id,
            range()
        );
        let (status, report) = call(&f, &admin, "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(report["totals"]["known_cost_microusd"], "22");
        assert_eq!(report["totals"]["held_microusd"], "110");
        assert_eq!(report["totals"]["unresolved_attempts"], "1");
        assert_eq!(report["cost_components"]["legacy_microusd"], "22");
        for (category, value) in report["cost_components"].as_object().unwrap() {
            if category != "legacy_microusd" {
                assert_eq!(value, "0", "{category}");
            }
        }
        let path = format!(
            "/api/v1/workspaces/{}/costs?{}",
            f.principal.workspace_id,
            range()
        );
        let (_, details) = call(&f, &admin, "GET", &path, Value::Null).await;
        let rows = details["data"].as_array().unwrap();
        for (id, expected) in ids.into_iter().zip([Value::Null, json!("22")]) {
            let row = rows.iter().find(|row| row["id"] == id.to_string()).unwrap();
            assert_eq!(row["cost_microusd"], expected);
            assert!(row["cost_components"].is_null());
            assert_eq!(row["input_tokens"], "6");
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn key_and_cross_workspace_resources_never_leak(pool: PgPool) {
        let f = fixture(pool).await;
        let member = user(f.other, false);
        let own = member_key(&f).await;
        let ws = f.team.workspace_id;
        assert_eq!(
            call(
                &f,
                &member,
                "GET",
                &format!("/api/v1/workspaces/{ws}/keys/{}/policy", f.team.key_id),
                Value::Null
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &f,
                &member,
                "GET",
                &format!("/api/v1/workspaces/{ws}/keys/{own}/policy"),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f,
                &member,
                "GET",
                &format!("/api/v1/workspaces/{ws}/keys/{}/policy", f.principal.key_id),
                Value::Null
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let admin = user(f.owner, true);
        let private = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Other personal','personal',$2)").bind(private).bind(f.other).execute(&f.store.pool).await.unwrap();
        for suffix in ["costs", "policy", "cost-report", "usage-export"] {
            assert_eq!(
                call(
                    &f,
                    &admin,
                    "GET",
                    &format!("/api/v1/workspaces/{private}/{suffix}?{}", range()),
                    Value::Null
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn pricing_append_only_and_authority_rechecked(pool: PgPool) {
        let f = fixture(pool).await;
        let p = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
        let admin = user(f.owner, true);
        assert_eq!(
            call(&f, &user(f.other, false), "POST", &p, price()).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, value) = call(&f, &admin, "POST", &p, price()).await;
        assert_eq!(status, StatusCode::OK);
        let id = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
        assert!(
            sqlx::query("UPDATE deployment_prices SET input_token_limit=200 WHERE id=$1")
                .bind(id)
                .execute(&f.store.pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(&f, &user(f.other, false), "GET", &p, Value::Null)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f, &user(f.other, false), "POST", &p, price()).await.0,
            StatusCode::FORBIDDEN
        );
        let legacy = json!({"input_microusd_per_million":"1","output_microusd_per_million":"1","input_token_limit":100,"output_token_limit":0});
        assert_eq!(call(&f, &admin, "POST", &p, legacy).await.0, StatusCode::OK);
        let (_, prices) = call(&f, &admin, "GET", &p, Value::Null).await;
        assert_eq!(prices["data"][0]["pricing_version"], 1);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn malformed_bodies_money_and_pagination_bounded(pool: PgPool) {
        let f = fixture(pool).await;
        let u = user(f.owner, true);
        let p = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
        for mut b in [price(), price(), price(), price()] {
            b["input_microusd_per_million"] = json!("-1");
            assert_eq!(call(&f, &u, "POST", &p, b).await.0, StatusCode::BAD_REQUEST);
        }
        let mut b = price();
        b["pricing_version"] = json!(2);
        b["cache_pricing"] = Value::Null;
        assert_eq!(call(&f, &u, "POST", &p, b).await.0, StatusCode::BAD_REQUEST);
        for query in ["limit=201", "offset=-1", "limit=0", "offset=100001"] {
            assert_eq!(
                call(&f, &u, "GET", &format!("{p}?{query}"), Value::Null)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let ws = f.team.workspace_id;
        for q in [
            format!("{}&limit=201", range()),
            format!("{}&provider=a&provider=b", range()),
            "start_date=2025-01-01&end_date=2025-04-05".into(),
        ] {
            assert_eq!(
                call(
                    &f,
                    &u,
                    "GET",
                    &format!("/api/v1/workspaces/{ws}/costs?{q}"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn routing_bounds_unknown_health_and_audit(pool: PgPool) {
        let f = fixture(pool).await;
        let u = user(f.owner, true);
        let p = format!("/api/v1/platform/models/{}/routing", f.model);
        let policy = json!({"strategy":"weighted","max_attempts":3,"allow_ambiguous_failover":false,"required_residency":"us"});
        assert_eq!(
            call(&f, &u, "PUT", &p, policy.clone()).await.0,
            StatusCode::OK
        );
        let mut bad = policy;
        bad["max_attempts"] = json!(4);
        assert_eq!(
            call(&f, &u, "PUT", &p, bad).await.0,
            StatusCode::BAD_REQUEST
        );
        let p = format!("/api/v1/platform/deployments/{}/routing", f.deployment);
        let (_, v) = call(&f, &u, "GET", &p, Value::Null).await;
        assert!(v["health"]["last_observed_at"].is_null());
        assert_eq!(call(&f,&u,"PUT",&p,json!({"priority":-2,"weight":20,"residency":"us","failure_threshold":2,"cooldown_seconds":60})).await.0,StatusCode::OK);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action LIKE 'routing.%'"
            )
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            2
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn monthly_member_scope_unknown_is_not_zero_and_csv_formula_safe(pool: PgPool) {
        let f = fixture(pool).await;
        let member = user(f.other, false);
        let admin = user(f.owner, true);
        let key = member_key(&f).await;
        execution(
            &f,
            key,
            f.team.workspace_id,
            "=formula",
            "unknown",
            None,
            Some(7),
        )
        .await;
        execution(
            &f,
            f.team.key_id,
            f.team.workspace_id,
            "secret",
            "settled",
            Some(9007199254740993),
            Some(10),
        )
        .await;
        let path = format!(
            "/api/v1/workspaces/{}/cost-report?{}",
            f.team.workspace_id,
            range()
        );
        let (status, v) = call(&f, &member, "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["totals"]["attempts"], "1");
        assert_eq!(v["totals"]["known_cost_microusd"], "0");
        assert_eq!(v["totals"]["held_microusd"], "7");
        assert_eq!(v["breakdowns"]["models"][0]["name"], "=formula");
        let (_, v) = call(&f, &admin, "GET", &path, Value::Null).await;
        assert_eq!(v["totals"]["known_cost_microusd"], "9007199254740993");
        let response = response(
            &f,
            &member,
            "GET",
            &format!(
                "/api/v1/workspaces/{}/usage-export?{}",
                f.team.workspace_id,
                range()
            ),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let csv = String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(csv.contains("\"'=formula\""));
        assert!(!csv.contains("secret"));
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn reconciliation_operator_private_scoped_atomic(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        crate::governance::admit(&f.store, &start, &chat(), 30)
            .await
            .unwrap();
        crate::governance::finish(&f.store, &done(start.id, None, None))
            .await
            .unwrap();
        let p = format!(
            "/api/v1/workspaces/{}/costs/{}/reconcile",
            f.principal.workspace_id, start.id
        );
        let b = json!({"input_tokens":"3","output_tokens":"2","billing_usage":null,"evidence":"receipt:1"});
        assert_eq!(
            call(&f, &user(f.other, false), "POST", &p, b.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f, &user(f.owner, true), "POST", &p, b.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f, &user(f.owner, true), "POST", &p, b).await.0,
            StatusCode::OK
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT actual_microusd FROM governance_reservations WHERE execution_id=$1"
            )
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            5
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn replacement_type_defaults_compose_local_and_installation_not_exposed(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let member = user(f.other, false);
        let ws = f.team.workspace_id;
        let type_path = "/api/v1/platform/workspace-types/team/policy";
        let mut p = policy();
        p["monthly_budget_microusd"] = json!("1000");
        assert_eq!(
            call(&f, &admin, "PUT", type_path, p).await.0,
            StatusCode::OK
        );
        let mut install = policy();
        install["monthly_budget_microusd"] = json!("5000");
        call(
            &f,
            &admin,
            "PUT",
            "/api/v1/platform/installation/policy",
            install,
        )
        .await;
        let path = format!("/api/v1/workspaces/{ws}/policy");
        let (_, v) = call(&f, &member, "GET", &path, Value::Null).await;
        assert_eq!(v["effective"]["monthly_budget_microusd"], "1000");
        assert!(!v.to_string().contains("5000"));
        let platform = format!("/api/v1/platform/workspaces/{ws}/policy");
        let nulls = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
        assert_eq!(
            call(&f, &admin, "PUT", &platform, nulls).await.0,
            StatusCode::OK
        );
        let (_, v) = call(&f, &member, "GET", &path, Value::Null).await;
        assert_eq!(v["mode"], "replace");
        assert!(v["effective"]["monthly_budget_microusd"].is_null());
        assert_eq!(
            call(&f, &admin, "DELETE", &platform, Value::Null).await.0,
            StatusCode::OK
        );
        let (_, v) = call(&f, &member, "GET", &path, Value::Null).await;
        assert_eq!(v["mode"], "inherit");
        assert_eq!(v["effective"]["monthly_budget_microusd"], "1000");
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn delegated_admin_tighten_requires_live_membership_and_masked_caps_persist(
        pool: PgPool,
    ) {
        let f = fixture(pool).await;
        sqlx::query("UPDATE workspace_membership_grants SET role='admin' WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let member = user(f.other, false);
        let admin = user(f.owner, true);
        let ws = f.team.workspace_id;
        let p = format!("/api/v1/workspaces/{ws}/policy");
        assert_eq!(
            call(&f, &member, "PUT", &p, policy()).await.0,
            StatusCode::OK
        );
        let mut cap = policy();
        cap["requests_per_minute"] = json!(5);
        call(
            &f,
            &admin,
            "PUT",
            "/api/v1/platform/workspace-types/team/policy",
            cap,
        )
        .await;
        let nulls = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
        assert_eq!(
            call(&f, &member, "PUT", &p, nulls).await.0,
            StatusCode::FORBIDDEN
        );
        let mut relax = policy();
        relax["requests_per_minute"] = json!(11);
        assert_eq!(
            call(&f, &member, "PUT", &p, relax).await.0,
            StatusCode::BAD_REQUEST
        );
        sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(&f, &member, "PUT", &p, policy()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn configuration_authority_rechecked_after_installation_lock_wait(pool: PgPool) {
        let f = fixture(pool).await;
        let u = user(f.owner, true);
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM installation FOR NO KEY UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE platform_role_grants SET role='user' WHERE user_id=$1")
            .bind(f.owner)
            .execute(&mut *tx)
            .await
            .unwrap();
        let op = call(
            &f,
            &u,
            "PUT",
            "/api/v1/platform/installation/policy",
            policy(),
        );
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(op.await.0, StatusCode::FORBIDDEN);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn platform_personal_totals_never_details_or_key_breakdowns(pool: PgPool) {
        let f = fixture(pool).await;
        execution(
            &f,
            f.principal.key_id,
            f.principal.workspace_id,
            "personal-model",
            "settled",
            Some(13),
            Some(20),
        )
        .await;
        let u = user(f.owner, true);
        let p = format!("/api/v1/platform/cost-report?{}", range());
        let (status, v) = call(&f, &u, "GET", &p, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["totals"]["known_cost_microusd"], "13");
        assert_eq!(v["breakdowns"]["service_accounts"], json!([]));
        // Personal workspaces are told apart by owner (platform totals only), never all "Personal".
        let owner_email: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
            .bind(f.owner)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            v["breakdowns"]["workspaces"][0]["name"],
            format!("Personal · {owner_email}")
        );
        sqlx::query("UPDATE users SET display_name='Pat Owner' WHERE id=$1")
            .bind(f.owner)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let (_, named) = call(&f, &u, "GET", &p, Value::Null).await;
        assert_eq!(
            named["breakdowns"]["workspaces"][0]["name"],
            "Personal · Pat Owner"
        );
        assert!(!v.to_string().contains(&f.principal.key_id.to_string()));
        assert_eq!(
            call(&f, &user(f.other, false), "GET", &p, Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(&f, &user(f.other, false), "GET", &p, Value::Null)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f,
                &user(f.other, false),
                "GET",
                &format!(
                    "/api/v1/workspaces/{}/costs?{}",
                    f.principal.workspace_id,
                    range()
                ),
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn health_indicators_overlap_cache_components_exact_and_unknown_ledger_not_additive(
        pool: PgPool,
    ) {
        let f = fixture(pool).await;
        f.v2(rates()).await;
        let start = f.start();
        crate::governance::admit(&f.store, &start, &chat(), 30)
            .await
            .unwrap();
        let mut record = done(start.id, Some(10), Some(2));
        record.usage.billing = Some(billing());
        crate::governance::finish(&f.store, &record).await.unwrap();
        let unknown = execution(
            &f,
            f.principal.key_id,
            f.principal.workspace_id,
            "unknown",
            "unknown",
            None,
            None,
        )
        .await;
        sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd) VALUES($1,$2,'unknown',999999)").bind(Uuid::new_v4()).bind(unknown).execute(&f.store.pool).await.unwrap();
        let p = format!(
            "/api/v1/workspaces/{}/cost-report?{}",
            f.principal.workspace_id,
            range()
        );
        let (status, v) = call(&f, &user(f.owner, true), "GET", &p, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["totals"]["known_cost_microusd"], "38");
        assert_eq!(v["cost_components"]["legacy_microusd"], "0");
        assert_eq!(v["coverage"]["complete_billing_attempts"], "1");
        assert_eq!(v["coverage"]["incomplete_billing_attempts"], "1");
        let rowpath = format!(
            "/api/v1/workspaces/{}/costs?{}",
            f.principal.workspace_id,
            range()
        );
        let (_, details) = call(&f, &user(f.owner, true), "GET", &rowpath, Value::Null).await;
        let settled = details["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == start.id.to_string())
            .unwrap();
        assert_eq!(settled["active_held_microusd"], "0");
        assert_ne!(settled["reserved_microusd"], "0");
        execution(
            &f,
            f.principal.key_id,
            f.principal.workspace_id,
            "legacy",
            "settled",
            Some(13),
            Some(20),
        )
        .await;
        let (_, all) = call(&f, &user(f.owner, true), "GET", &p, Value::Null).await;
        assert_eq!(all["totals"]["known_cost_microusd"], "51");
        assert_eq!(all["cost_components"]["legacy_microusd"], "13");
        assert_eq!(all["coverage"]["legacy_pricing_attempts"], "1");
        let sum = all["cost_components"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap().parse::<i128>().unwrap())
            .sum::<i128>();
        assert_eq!(sum, 51);
        assert_eq!(v["health"]["unknown_attempts"], "1");
        assert_eq!(v["health"]["unpriced_attempts"], "1");
        assert_eq!(v["health"]["unbounded_attempts"], "1");
        assert_eq!(v["cost_components"]["cache_write_1h_microusd"], "16");
        assert_eq!(v["billing_usage"]["cache_write_input_tokens"], "15");
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn report_filter_comparison_dates_and_output_bounds_consistent(pool: PgPool) {
        let f = fixture(pool).await;
        let mut ids = Vec::new();
        for i in 0..105 {
            ids.push(
                execution(
                    &f,
                    f.team.key_id,
                    f.team.workspace_id,
                    &format!("m{i:03}"),
                    "settled",
                    Some(1),
                    Some(2),
                )
                .await,
            );
        }
        let old = execution(
            &f,
            f.team.key_id,
            f.team.workspace_id,
            "m000",
            "settled",
            Some(7),
            Some(2),
        )
        .await;
        sqlx::query(
            "UPDATE inference_executions SET started_at=now()-interval '1 day' WHERE id=$1",
        )
        .bind(old)
        .execute(&f.store.pool)
        .await
        .unwrap();
        let date = Utc::now().date_naive();
        let query = format!(
            "start_date={date}&end_date={}&compare=previous_period&model=m000",
            date.checked_add_signed(chrono::TimeDelta::days(1)).unwrap()
        );
        let path = format!(
            "/api/v1/workspaces/{}/cost-report?{query}",
            f.team.workspace_id
        );
        let (status, v) = call(&f, &user(f.owner, true), "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["totals"]["known_cost_microusd"], "1");
        assert_eq!(v["comparison"]["totals"]["known_cost_microusd"], "7");
        assert_eq!(v["breakdowns"]["models"].as_array().unwrap().len(), 1);
        let p = format!(
            "/api/v1/workspaces/{}/cost-report?{}",
            f.team.workspace_id,
            range()
        );
        let (_, v) = call(&f, &user(f.owner, true), "GET", &p, Value::Null).await;
        assert_eq!(v["breakdowns"]["models"].as_array().unwrap().len(), 100);
        assert_eq!(v["breakdowns_truncated"], true);
        let p = format!(
            "/api/v1/workspaces/{}/costs?{}&limit=100",
            f.team.workspace_id,
            range()
        );
        let (_, v) = call(&f, &user(f.owner, true), "GET", &p, Value::Null).await;
        assert_eq!(v["data"].as_array().unwrap().len(), 100);
        assert_eq!(v["has_more"], true);
        let current = format!(
            "/api/v1/workspaces/{}/cost-report?start_date={date}&end_date={}",
            f.team.workspace_id,
            date.checked_add_signed(chrono::TimeDelta::days(1)).unwrap()
        );
        let (_, v) = call(&f, &user(f.owner, true), "GET", &current, Value::Null).await;
        assert_eq!(v["totals"]["attempts"], "105");
        assert_eq!(v["totals"]["known_cost_microusd"], "105");
        assert_eq!(v["daily"].as_array().unwrap().len(), 1);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn report_admission_installation_lock_contention_is_measured(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let first = f.start();
        let req = chat();
        crate::governance::admit(&f.store, &first, &req, 30)
            .await
            .unwrap();
        let mut blocker = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM installation FOR NO KEY UPDATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let u = user(f.owner, true);
        let path = format!(
            "/api/v1/workspaces/{}/cost-report?{}",
            f.principal.workspace_id,
            range()
        );
        let started = std::time::Instant::now();
        let report = call(&f, &u, "GET", &path, Value::Null);
        tokio::pin!(report);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut report)
                .await
                .is_err()
        );
        let second = f.start();
        let admission = crate::governance::admit(&f.store, &second, &req, 30);
        tokio::pin!(admission);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut admission)
                .await
                .is_err()
        );
        blocker.commit().await.unwrap();
        let (status, value) = tokio::time::timeout(std::time::Duration::from_secs(2), &mut report)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["totals"]["attempts"], "1");
        assert!(started.elapsed() >= std::time::Duration::from_millis(200));
        eprintln!(
            "financial report/admission controlled installation contention: {}ms",
            started.elapsed().as_millis()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut admission)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            2
        );
    }

    fn v3_body() -> Value {
        let l = |meter: &str, amount: &str, batch: u64, unit: &str| json!({"meter":meter,"microusd_per_batch":amount,"batch":batch,"unit_label":unit,"sku_label":"Line"});
        let na = |meter: &str| json!({"meter":meter,"not_applicable":true});
        let mut tier = l("input_tokens", "2000000", 1_000_000, "/M tokens");
        tier["min_prompt_tokens"] = json!(272000);
        json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":50,"max_units":{"requests":"1"},"price_lines":[
            l("input_tokens","100000",1_000_000,"/M tokens"), tier,
            l("output_tokens","500000",1_000_000,"/M tokens"),
            na("cache_read_tokens"), na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"),
            na("output_images"), na("input_characters"), na("input_audio_seconds_ms"), na("output_audio_seconds_ms"), na("search_units"),
            l("requests","1000",1,"/request")
        ]})
    }
    /// Every meter the route's workload can use must be stated (priced,
    /// `not_applicable` or `unknown`); omitted meters are refused with
    /// `price_meters_incomplete` instead of silently making budgeted keys fail.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v3_prices_must_state_every_meter_the_route_can_use(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let p = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
        let rows = |f: &Fixture| {
            let pool = f.store.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM deployment_prices")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let before = rows(&f).await;
        let mut omitted = v3_body();
        omitted["price_lines"]
            .as_array_mut()
            .unwrap()
            .retain(|l| l["meter"] != "search_units" && l["meter"] != "requests");
        omitted.as_object_mut().unwrap().remove("max_units");
        let (status, v) = call(&f, &admin, "POST", &p, omitted.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], "400", "{v}");
        // Deprecated duplicate, kept for one release.
        assert_eq!(v["error"]["reason"], "price_meters_incomplete", "{v}");
        assert_eq!(
            v["error"]["missing_meters"],
            json!(["search_units", "requests"])
        );
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.contains("search_units, requests"), "{message}");
        assert!(message.contains("Text generation"), "{message}");
        assert_eq!(rows(&f).await, before);
        // Stating them explicitly unknown publishes; the stored form of an
        // unknown is no line (exactly what an omitted meter values as).
        let mut explicit = omitted.clone();
        for meter in ["search_units", "requests"] {
            explicit["price_lines"]
                .as_array_mut()
                .unwrap()
                .push(json!({"meter":meter,"unknown":true}));
        }
        let (status, v) = call(&f, &admin, "POST", &p, explicit).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let (_, prices) = call(&f, &admin, "GET", &p, Value::Null).await;
        let stored = prices["data"][0]["price_lines"].as_array().unwrap();
        assert!(stored.iter().all(|l| l.get("unknown").is_none()));
        assert!(!stored.iter().any(|l| l["meter"] == "search_units"));
        assert_eq!(
            prices["data"][0]["display_lines"].as_array().unwrap().len(),
            stored.len()
        );
        // An unknown must be exactly `{meter, unknown:true}` and the meter's only line.
        for bad in [
            json!({"meter":"requests","unknown":true,"sku_label":"Request"}),
            json!({"meter":"input_tokens","unknown":true}),
        ] {
            let mut body = omitted.clone();
            body["price_lines"]
                .as_array_mut()
                .unwrap()
                .push(bad.clone());
            body["price_lines"]
                .as_array_mut()
                .unwrap()
                .push(json!({"meter":"search_units","unknown":true}));
            let status = call(&f, &admin, "POST", &p, body).await.0;
            assert!(
                matches!(
                    status,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "{bad}: {status}"
            );
        }
        // A batch list states the same meters, unknowns included.
        let mut batch = v3_body();
        batch["batch_price_lines"] = omitted["price_lines"].clone();
        let (status, v) = call(&f, &admin, "POST", &p, batch).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        // v1/v2 are unchanged.
        assert_eq!(
            call(&f, &admin, "POST", &p, price()).await.0,
            StatusCode::OK
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v3_prices_publish_display_and_report_meter_totals(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let p = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
        let mut with_rates = v3_body();
        with_rates["input_microusd_per_million"] = json!("1");
        let mut with_cache = v3_body();
        with_cache["cache_pricing"] = json!(rates());
        let mut no_lines = v3_body();
        no_lines.as_object_mut().unwrap().remove("price_lines");
        let mut v2_lines = price();
        v2_lines["price_lines"] = v3_body()["price_lines"].clone();
        let mut bad_line = v3_body();
        bad_line["price_lines"][0]["batch"] = json!(1000);
        let mut token_ceiling = v3_body();
        token_ceiling["max_units"] = json!({"input_tokens":"5"});
        for bad in [
            with_rates,
            with_cache,
            no_lines,
            v2_lines,
            bad_line,
            token_ceiling,
        ] {
            let status = call(&f, &admin, "POST", &p, bad.clone()).await.0;
            assert!(
                matches!(
                    status,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "{bad}"
            );
        }
        assert_eq!(
            call(&f, &admin, "POST", &p, v3_body()).await.0,
            StatusCode::OK
        );
        let (_, prices) = call(&f, &admin, "GET", &p, Value::Null).await;
        let v = &prices["data"][0];
        assert_eq!(v["pricing_version"], 3);
        assert_eq!(v["input_microusd_per_million"], Value::Null);
        assert_eq!(v["max_units"], json!({"requests":"1"}));
        assert_eq!(v["display_lines"][0], "$0.10/M input tokens");
        assert_eq!(
            v["display_lines"][1],
            "$2/M input tokens (prompt > 272,000 tokens)"
        );
        assert_eq!(v["display_lines"][3], "Cache read tokens: not applicable");
        assert_eq!(v["display_lines"][12], "$0.001/request");
        assert!(
            v["display_summary"]
                .as_str()
                .unwrap()
                .starts_with("$0.10/M input tokens · $2/M input tokens")
        );
        // v1/v2 rows remain unchanged and carry no line display.
        assert_eq!(
            call(&f, &admin, "POST", &p, price()).await.0,
            StatusCode::OK
        );
        let (_, prices) = call(&f, &admin, "GET", &p, Value::Null).await;
        assert_eq!(prices["data"][0]["pricing_version"], 2);
        assert_eq!(prices["data"][0]["display_lines"], Value::Null);
        assert_eq!(prices["data"][1]["pricing_version"], 3);
        // Pin v3 for a metered attempt and report its meter totals.
        assert_eq!(
            call(&f, &admin, "POST", &p, v3_body()).await.0,
            StatusCode::OK
        );
        let start = f.start();
        crate::governance::admit(&f.store, &start, &chat(), 30)
            .await
            .unwrap();
        let mut record = done(start.id, Some(20), Some(4));
        record.usage.billing = Some(crate::billing::BillingUsage {
            total_input_tokens: Some(20),
            uncached_input_tokens: Some(20),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        });
        record.usage.meters = Some(crate::billing::MeterUsage {
            requests: Some(1),
            ..Default::default()
        });
        record.usage.provider_cost_microusd = Some(5);
        crate::governance::finish(&f.store, &record).await.unwrap();
        let ws = f.principal.workspace_id;
        let (status, report) = call(
            &f,
            &admin,
            "GET",
            &format!("/api/v1/workspaces/{ws}/cost-report?{}", range()),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // ceil(20 × 0.1) + ceil(4 × 0.5) + 1000
        assert_eq!(report["totals"]["known_cost_microusd"], "1004");
        assert_eq!(report["meter_usage"]["requests"], "1");
        assert_eq!(report["meter_usage"]["output_images"], Value::Null);
        assert_eq!(report["provider_reported_cost_microusd"], "5");
        assert_eq!(report["cost_components"]["requests_microusd"], "1000");
        assert_eq!(report["cost_components"]["uncached_input_microusd"], "2");
        // Generation attempts cannot produce unit meters: none is relevant.
        for key in crate::billing::MeterUsage::KEYS {
            assert_eq!(report["meter_relevant_attempts"][key], "0", "{key}");
            assert_eq!(report["meter_unknown_attempts"][key], "0", "{key}");
        }
        // A speech-workload attempt without an observed request count is
        // unknown for `requests`; meters its price marks not applicable are
        // irrelevant rather than zero or unknown.
        let speech = f.start();
        crate::governance::admit(&f.store, &speech, &chat(), 30)
            .await
            .unwrap();
        let mut failed = done(speech.id, None, None);
        failed.outcome = crate::inference::repository::Outcome::Failed;
        failed.error = Some(crate::inference::error::InferenceError::UpstreamUnavailable);
        crate::governance::finish(&f.store, &failed).await.unwrap();
        sqlx::query("UPDATE inference_executions SET workload_kind='audio_speech' WHERE id=$1")
            .bind(speech.id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let (_, report) = call(
            &f,
            &admin,
            "GET",
            &format!("/api/v1/workspaces/{ws}/cost-report?{}", range()),
            Value::Null,
        )
        .await;
        assert_eq!(report["meter_relevant_attempts"]["requests"], "1");
        assert_eq!(report["meter_unknown_attempts"]["requests"], "1");
        assert_eq!(report["meter_usage"]["requests"], "1", "known lower bound");
        assert_eq!(report["meter_relevant_attempts"]["input_characters"], "0");
        assert_eq!(
            report["meter_relevant_attempts"]["output_audio_seconds_ms"],
            "0"
        );
        assert_eq!(report["meter_unknown_attempts"]["output_images"], "0");
        let (status, platform) = call(
            &f,
            &admin,
            "GET",
            &format!("/api/v1/platform/cost-report?{}", range()),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(platform["meter_usage"]["requests"], "1");
        let response = response(
            &f,
            &admin,
            "GET",
            &format!("/api/v1/workspaces/{ws}/usage-export?{}", range()),
            Value::Null,
        )
        .await;
        let csv = String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let header = csv.lines().next().unwrap();
        assert!(header.starts_with("id,root_request_id,attempt_number,public_model,provider,state,workload_kind,started_at,input_tokens,output_tokens,billing_usage,cost_components,price_id,pricing_version,cost_microusd,reserved_microusd,active_held_microusd,unresolved_reason,cost_status,unbounded_cost,cost_center_id,cost_center_name,cost_center_code,"));
        assert!(header.ends_with(",meter_usage,output_image_variant,provider_cost_microusd"));
        assert!(csv.lines().skip(1).any(|l| l.ends_with(",\"\",\"5\"")));
        let (_, rows) = call(
            &f,
            &admin,
            "GET",
            &format!("/api/v1/workspaces/{ws}/costs?{}", range()),
            Value::Null,
        )
        .await;
        let row = rows["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == json!(start.id))
            .unwrap();
        assert_eq!(row["meter_usage"]["requests"], "1");
        assert_eq!(row["provider_cost_microusd"], "5");
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn openrouter_price_suggestion_uses_mock_catalog_without_key(pool: PgPool) {
        use super::super::suggestion::{OpenRouterCatalog, tests::mock};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let catalog = json!({"data":[{"id":"anthropic/claude-haiku-5.5","canonical_slug":"anthropic/claude-haiku-5.5-20261007","context_length":1000000,"top_provider":{"max_completion_tokens":128000},"pricing":{"prompt":"0.0000001","completion":"0.0000005","input_cache_read":"0.00000001","input_cache_write":"0.000000125","input_cache_write_1h":"0.0000002","overrides":[{"min_prompt_tokens":100000,"prompt":"0.0000005"}]}}]});
        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let h = h.clone();
                let body = catalog.clone();
                async move {
                    assert!(headers.get("authorization").is_none(), "no key is sent");
                    h.fetch_add(1, Ordering::SeqCst);
                    axum::Json(body)
                }
            }),
        );
        let (url, task) = mock(app).await;
        let catalog = Arc::new(OpenRouterCatalog::for_test(
            url,
            std::time::Duration::from_secs(2),
        ));
        let connection = Uuid::new_v4();
        let model = Uuid::new_v4();
        let deployment = Uuid::new_v4();
        let missing_deployment = Uuid::new_v4();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'OpenRouter','openrouter','env:OPENROUTER_API_KEY','https://openrouter.ai/api/v1',true)").bind(connection).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'router/haiku',ARRAY['chat_completions','messages'])").bind(model).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$3,$4,'anthropic/claude-haiku-5.5'),($2,$3,$4,'vendor/not-listed')").bind(deployment).bind(missing_deployment).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
        let get = |u: BrowserPrincipal, id: Uuid| {
            let store = f.store.clone();
            let catalog = catalog.clone();
            async move {
                let r = platform_routes()
                    .layer(Extension(u))
                    .layer(Extension(catalog))
                    .with_state(store)
                    .oneshot(
                        Request::builder()
                            .uri(format!(
                                "/api/v1/platform/deployments/{id}/price-suggestion"
                            ))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = r.status();
                (
                    status,
                    serde_json::from_slice::<Value>(
                        &to_bytes(r.into_body(), 1 << 20).await.unwrap(),
                    )
                    .unwrap_or(Value::Null),
                )
            }
        };
        sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1")
            .bind(f.other)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            get(user(f.other, false), deployment).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(admin.clone(), f.deployment).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            get(admin.clone(), Uuid::new_v4()).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, v) = get(admin.clone(), deployment).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["draft"]["pricing_version"], 3);
        assert_eq!(v["draft"]["input_token_limit"], 8192);
        assert_eq!(v["draft"]["output_token_limit"], 1024);
        // The mock has no endpoint list: the top-provider catalog price is used.
        assert_eq!(v["endpoint"]["selection"], "catalog");
        assert_eq!(v["display_lines"][0], "$0.10/M input tokens");
        assert!(
            v["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["min_prompt_tokens"] == 100000 && l["microusd_per_batch"] == "500000")
        );
        // The draft is publishable only through the normal immutable price POST.
        let mut body = v["draft"].clone();
        body["input_token_limit"] = json!(100);
        body["output_token_limit"] = json!(50);
        let p = format!("/api/v1/platform/deployments/{deployment}/prices");
        assert_eq!(call(&f, &admin, "POST", &p, body).await.0, StatusCode::OK);
        assert_eq!(
            get(admin.clone(), missing_deployment).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "catalog responses are cached"
        );
        task.abort();
        // An unreachable catalog is a sanitized 502, never a fabricated draft.
        let dead = Arc::new(OpenRouterCatalog::for_test(
            "http://127.0.0.1:9".into(),
            std::time::Duration::from_millis(500),
        ));
        let r = platform_routes()
            .layer(Extension(admin.clone()))
            .layer(Extension(dead))
            .with_state(f.store.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/platform/deployments/{deployment}/price-suggestion"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn budget_periods_round_trip_tighten_audit_and_scoped_usage(pool: PgPool) {
        let f = fixture(pool).await;
        let admin = user(f.owner, true);
        let member = user(f.other, false);
        let ws = f.team.workspace_id;
        let install = "/api/v1/platform/installation/policy";
        let with = |budget: &str, period: Option<&str>| {
            let mut p = policy();
            p["requests_per_minute"] = Value::Null;
            p["monthly_budget_microusd"] = json!(budget);
            if let Some(period) = period {
                p["budget_period"] = json!(period);
            }
            p
        };
        // Round trip; absent keeps the stored period; strict values.
        assert_eq!(
            call(&f, &admin, "PUT", install, with("5000", Some("week")))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f, &admin, "GET", install, Value::Null).await.1["policy"]["budget_period"],
            "week"
        );
        assert_eq!(
            call(&f, &admin, "PUT", install, with("6000", None)).await.0,
            StatusCode::OK
        );
        let (_, v) = call(&f, &admin, "GET", install, Value::Null).await;
        assert_eq!(
            (
                &v["policy"]["budget_period"],
                &v["policy"]["monthly_budget_microusd"]
            ),
            (&json!("week"), &json!("6000"))
        );
        for bad in [json!("year"), json!("Week"), json!("")] {
            let mut b = with("1", None);
            b["budget_period"] = bad;
            assert_eq!(
                call(&f, &admin, "PUT", install, b).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        // Wrongly typed or explicit null periods are rejected by deserialization.
        for bad in [Value::Null, json!(1)] {
            let mut b = with("1", None);
            b["budget_period"] = bad;
            assert!(
                call(&f, &admin, "PUT", install, b)
                    .await
                    .0
                    .is_client_error()
            );
        }
        let audit: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='policy.installation_updated' ORDER BY created_at,id")
            .fetch_all(&f.store.pool).await.unwrap();
        assert_eq!(
            audit[0],
            json!({"budget_period":"week","previous_budget_period":"month","count":1})
        );
        // Team default 1000/month; local tighten-only considers amount and period.
        call(
            &f,
            &admin,
            "PUT",
            "/api/v1/platform/workspace-types/team/policy",
            with("1000", Some("month")),
        )
        .await;
        let local = format!("/api/v1/workspaces/{ws}/policy");
        let stacked = |list: Value| {
            let mut p = policy();
            p["requests_per_minute"] = Value::Null;
            p.as_object_mut().unwrap().remove("monthly_budget_microusd");
            p["budgets"] = list;
            p
        };
        // Tighten-only per period: only the same period is compared with the parent.
        for (body, status) in [
            (with("2000", Some("month")), StatusCode::BAD_REQUEST),
            (with("5000", Some("week")), StatusCode::OK),
            (with("400", Some("day")), StatusCode::FORBIDDEN),
            (with("400", Some("week")), StatusCode::OK),
            (with("400", Some("month")), StatusCode::FORBIDDEN),
            (
                stacked(
                    json!([{"period":"week","amount_microusd":"400"},{"period":"day","amount_microusd":"100"}]),
                ),
                StatusCode::OK,
            ),
            (
                stacked(json!([{"period":"day","amount_microusd":"100"}])),
                StatusCode::FORBIDDEN,
            ),
            (
                stacked(
                    json!([{"period":"week","amount_microusd":"400"},{"period":"day","amount_microusd":"100"},{"period":"month","amount_microusd":"1001"}]),
                ),
                StatusCode::BAD_REQUEST,
            ),
            (with("50", Some("day")), StatusCode::CONFLICT),
        ] {
            assert_eq!(call(&f, &admin, "PUT", &local, body).await.0, status);
        }
        execution(
            &f,
            f.team.key_id,
            ws,
            "company/smart",
            "settled",
            Some(70),
            None,
        )
        .await;
        let (_, v) = call(&f, &admin, "GET", &local, Value::Null).await;
        // Deprecated mirror = smallest budget; `budgets` lists every period.
        assert_eq!(v["policy"]["budget_period"], "day");
        assert_eq!(v["policy"]["monthly_budget_microusd"], "100");
        assert_eq!(
            v["policy"]["budgets"],
            json!([{"period":"day","amount_microusd":"100"},{"period":"week","amount_microusd":"400"}])
        );
        assert_eq!(
            v["effective"]["budgets"],
            json!([{"period":"day","amount_microusd":"100"},{"period":"week","amount_microusd":"400"},{"period":"month","amount_microusd":"1000"}])
        );
        let budgets = v["budgets"].as_array().unwrap();
        assert_eq!(budgets.len(), 3);
        assert_eq!(budgets[0]["layer"], "platform");
        assert_eq!(budgets[0]["budget_period"], "month");
        assert_eq!(budgets[1]["layer"], "local");
        assert_eq!(budgets[1]["period"], "day");
        assert_eq!(budgets[1]["used_microusd"], "70");
        assert_eq!(budgets[1]["exhausted"], false);
        assert_eq!(budgets[1]["unresolved_usage"], false);
        assert_eq!(budgets[2]["period"], "week");
        // Ordinary members see the windows but not workspace-wide consumption.
        let (_, v) = call(&f, &member, "GET", &local, Value::Null).await;
        assert_eq!(v["budgets"][1]["usage_visible"], false);
        assert!(v["budgets"][1]["used_microusd"].is_null());
        assert!(v["budgets"][1]["exhausted"].is_null());
        assert!(!v.to_string().contains("6000"));
        // Platform readers see the type default behind an override.
        let platform = format!("/api/v1/platform/workspaces/{ws}/policy");
        assert_eq!(
            call(&f, &admin, "PUT", &platform, with("300", Some("day")))
                .await
                .0,
            StatusCode::OK
        );
        let (_, v) = call(&f, &admin, "GET", &platform, Value::Null).await;
        assert_eq!(v["mode"], "replace");
        assert_eq!(
            v["provenance"]["type_default"]["monthly_budget_microusd"],
            "1000"
        );
        assert_eq!(v["budgets"][0]["budget_period"], "day");
        assert_eq!(v["budgets"][0]["used_microusd"], "70");
        // A period change rewrites no consumption.
        let held: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(&f, &admin, "PUT", &platform, with("300", Some("week")))
                .await
                .0,
            StatusCode::OK
        );
        let (_, v) = call(&f, &admin, "GET", &platform, Value::Null).await;
        assert_eq!(v["budgets"][0]["used_microusd"], "70");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            held
        );
    }
}

use super::*;
use crate::{auth::NewApiKey, bootstrap, config::Environment};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;

struct Fixture {
    store: Store,
    keys: bootstrap::DevelopmentKeys,
    owner: BrowserPrincipal,
    member: BrowserPrincipal,
    deployment: Uuid,
    model: Uuid,
    member_key: Uuid,
}
async fn fixture(pool: &PgPool) -> Fixture {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let owner_id = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(keys.personal_key.id)
        .fetch_one(pool)
        .await
        .unwrap();
    let owner = BrowserPrincipal {
        user_id: owner_id,
        email: "developer@local.invalid".into(),
        platform_admin: false,
    };
    let member = BrowserPrincipal {
        user_id: Uuid::new_v4(),
        email: "member@example.invalid".into(),
        platform_admin: false,
    };
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(member.user_id)
        .bind(&member.email)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')",
    )
    .bind(keys.organization_id)
    .bind(member.user_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')").bind(keys.organization_id).bind(keys.team_workspace_id).bind(member.user_id).execute(pool).await.unwrap();
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,'member key',$5)").bind(key.id).bind(keys.organization_id).bind(keys.team_workspace_id).bind(member.user_id).bind(key.digest.as_slice()).execute(pool).await.unwrap();
    let (deployment, model): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,model_id FROM deployments WHERE organization_id=$1")
            .bind(keys.organization_id)
            .fetch_one(pool)
            .await
            .unwrap();
    Fixture {
        store,
        keys,
        owner,
        member,
        deployment,
        model,
        member_key: key.id,
    }
}
async fn request(
    f: &Fixture,
    user: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> Response {
    super::super::routes()
        .layer(Extension(user.clone()))
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
    user: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = request(f, user, method, path, body).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[sqlx::test]
async fn rotating_a_key_preserves_policy_and_consumed_allowance(pool: PgPool) {
    use crate::inference::{
        repository::{ExecutionFinish, ExecutionStart, Outcome},
        types::ChatRequest,
    };
    let f = fixture(&pool).await;
    // Admission now rechecks the live global target; the demo is disabled by default.
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
        .bind(f.deployment)
        .execute(&pool)
        .await
        .unwrap();
    let ws = f.keys.team_workspace_id;
    let original = f.keys.team_key.id;
    let path = format!("/api/v1/workspaces/{ws}/keys/{original}/policy");
    assert_eq!(call(&f,&f.owner,"PUT",&path,json!({"requests_per_minute":1,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null})).await.0,StatusCode::OK);
    let principal = f
        .store
        .authenticate(&f.keys.team_key.token)
        .await
        .unwrap()
        .unwrap();
    let id = Uuid::new_v4();
    let make = |id, principal| ExecutionStart {
        id,
        root_request_id: id,
        attempt_number: 1,
        principal,
        deployment_id: f.deployment,
        provider: "openai".into(),
        model: "company/smart".into(),
        streamed: false,
    };
    let input = ChatRequest {
        model: "company/smart".into(),
        messages: vec![],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        stream: false,
    };
    crate::governance::admit(&f.store, &make(id, principal), &input, 30)
        .await
        .unwrap();
    crate::governance::finish(
        &f.store,
        &ExecutionFinish {
            id,
            outcome: Outcome::Cancelled,
            error: None,
            usage: Usage::default(),
            elapsed_ms: 1,
        },
    )
    .await
    .unwrap();
    let (status, rotated) = call(
        &f,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys/{original}/rotate"),
        json!({"expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_id = rotated["id"].as_str().unwrap();
    let new_principal = f
        .store
        .authenticate(rotated["token"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    let read = call(
        &f,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{ws}/keys/{new_id}/policy"),
        json!({}),
    )
    .await;
    assert_eq!(read.1["policy"]["requests_per_minute"], 1);
    assert_eq!(
        crate::governance::admit(&f.store, &make(Uuid::new_v4(), new_principal), &input, 30).await,
        Err(InferenceError::Busy)
    );
}
fn policy() -> Value {
    json!({"requests_per_minute":100,"tokens_per_minute":2000,"concurrent_requests":2,"monthly_budget_microusd":"9007199254740993"})
}
async fn operator(f: &Fixture) -> BrowserPrincipal {
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    BrowserPrincipal {
        platform_admin: true,
        ..f.owner.clone()
    }
}
fn price() -> Value {
    json!({"input_microusd_per_million":"1000000","output_microusd_per_million":"2000000","input_token_limit":100,"output_token_limit":100})
}
async fn execution(f: &Fixture, key: Uuid, model: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,root_request_id,organization_id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,completed_at,input_tokens,output_tokens) VALUES($1,$1,$2,$3,$4,$5,$6,'openai',false,'failed',now(),3,2)")
        .bind(id).bind(f.keys.organization_id).bind(f.keys.team_workspace_id).bind(key).bind(f.deployment).bind(model).execute(&f.store.pool).await.unwrap();
    id
}
async fn reservation(f: &Fixture, id: Uuid, key: Uuid, price: Option<Uuid>, state: &str) {
    sqlx::query("INSERT INTO governance_reservations(execution_id,organization_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,$6,now(),date_trunc('minute',now(),'UTC'),date_trunc('month',now(),'UTC'),now(),$7,CASE WHEN $6::uuid IS NULL THEN NULL ELSE 500 END,CASE WHEN $7='settled' THEN 7 ELSE NULL END,3,2)")
        .bind(id).bind(f.keys.organization_id).bind(f.keys.team_workspace_id).bind(key).bind(f.deployment).bind(price).bind(state).execute(&f.store.pool).await.unwrap();
}

#[sqlx::test]
async fn policy_roles_privacy_scope_and_transactional_audit(pool: PgPool) {
    let f = fixture(&pool).await;
    let org = format!("/api/v1/orgs/{}/policy", f.keys.organization_id);
    let team = format!("/api/v1/workspaces/{}/policy", f.keys.team_workspace_id);
    let personal = format!("/api/v1/workspaces/{}/policy", f.keys.personal_workspace_id);
    assert_eq!(
        call(&f, &f.member, "GET", &org, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.member, "GET", &team, json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &team, policy()).await.0,
        StatusCode::FORBIDDEN
    );
    // A shared workspace owner may govern that workspace, but not its organization.
    sqlx::query("UPDATE workspace_memberships SET role='owner' WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &f.member, "PUT", &team, policy()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &org, policy()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, policy()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &team, policy()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &personal, policy()).await.0,
        StatusCode::OK
    );
    let got = call(&f, &f.owner, "GET", &org, json!({})).await.1;
    assert_eq!(got["policy"]["monthly_budget_microusd"], "9007199254740993");
    let mut outsider_operator = f.member.clone();
    outsider_operator.platform_admin = true;
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &outsider_operator, "PUT", &personal, policy())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='governance.policy_updated'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 4);
    sqlx::query("UPDATE workspace_memberships SET role='member' WHERE user_id=$1")
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organization_memberships SET role='member' WHERE user_id=$1")
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &f.owner, "PUT", &team, policy()).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn keys_and_cross_tenant_resources_never_leak(pool: PgPool) {
    let f = fixture(&pool).await;
    let key_path = |ws, key| format!("/api/v1/workspaces/{ws}/keys/{key}/policy");
    assert_eq!(
        call(
            &f,
            &f.member,
            "GET",
            &key_path(f.keys.team_workspace_id, f.member_key),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f,
            &f.member,
            "GET",
            &key_path(f.keys.team_workspace_id, f.keys.team_key.id),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f,
            &f.owner,
            "PUT",
            &key_path(f.keys.team_workspace_id, f.member_key),
            policy()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f,
            &f.owner,
            "PUT",
            &key_path(f.keys.team_workspace_id, f.keys.personal_key.id),
            policy()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let foreign = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,name,slug) VALUES($1,'Foreign','foreign')")
        .bind(foreign)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f,
            &f.owner,
            "GET",
            &format!("/api/v1/orgs/{foreign}/policy"),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'owner')",
    )
    .bind(foreign)
    .bind(f.owner.user_id)
    .execute(&pool)
    .await
    .unwrap();
    for suffix in ["prices", "routing"] {
        assert_eq!(
            call(
                &f,
                &f.owner,
                "GET",
                &format!(
                    "/api/v1/orgs/{foreign}/deployments/{}/{suffix}",
                    f.deployment
                ),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &f,
            &f.owner,
            "GET",
            &format!("/api/v1/orgs/{foreign}/models/{}/routing", f.model),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn pricing_append_only_operator_authority_rechecked(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
    assert_eq!(
        call(&f, &f.owner, "POST", &path, price()).await.0,
        StatusCode::FORBIDDEN
    );
    let op = operator(&f).await;
    let (status, first) = call(&f, &op, "POST", &path, price()).await;
    assert_eq!(status, StatusCode::OK);
    let mut changed = price();
    changed["input_microusd_per_million"] = json!("0");
    assert_eq!(
        call(&f, &op, "POST", &path, changed).await.0,
        StatusCode::OK
    );
    let listed = call(&f, &f.owner, "GET", &path, json!({})).await.1;
    assert_eq!(listed["data"].as_array().unwrap().len(), 2);
    assert_eq!(listed["data"][0]["input_microusd_per_million"], "0");
    let id: Uuid = first["id"].as_str().unwrap().parse().unwrap();
    assert!(
        sqlx::query("UPDATE deployment_prices SET input_token_limit=1 WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM deployment_prices WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(op.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &op, "POST", &path, price()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.member, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='governance.price_created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn malformed_bodies_money_and_pagination_are_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/orgs/{}/policy", f.keys.organization_id);
    for bad in [
        json!("-1"),
        json!("0"),
        json!("9223372036854775808"),
        json!("1.5"),
        json!("1e2"),
        json!("+1"),
        json!(1),
    ] {
        let mut p = policy();
        p["monthly_budget_microusd"] = bad;
        assert!(
            call(&f, &f.owner, "PUT", &path, p)
                .await
                .0
                .is_client_error()
        );
    }
    for bad in [0, -1, 2_147_483_648_i64] {
        let mut p = policy();
        p["requests_per_minute"] = json!(bad);
        assert_eq!(
            call(&f, &f.owner, "PUT", &path, p).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(
        call(&f, &f.owner, "PUT", &path, json!({}))
            .await
            .0
            .is_client_error()
    );
    let mut p = policy();
    p["unsupported"] = json!(true);
    assert!(
        call(&f, &f.owner, "PUT", &path, p)
            .await
            .0
            .is_client_error()
    );
    let mut p = policy();
    p["monthly_budget_microusd"] = json!(i64::MAX.to_string());
    assert_eq!(call(&f, &f.owner, "PUT", &path, p).await.0, StatusCode::OK);
    let reset = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    assert_eq!(
        call(&f, &f.owner, "PUT", &path, reset.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &path, json!({})).await.1["policy"],
        reset
    );
    for suffix in [
        "costs?limit=201",
        "costs?offset=-1",
        "usage-export?limit=1001",
        "usage-export?limit=0",
        "usage-export?offset=100001",
    ] {
        assert_eq!(
            call(
                &f,
                &f.owner,
                "GET",
                &format!("/api/v1/workspaces/{}/{suffix}", f.keys.team_workspace_id),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let op = operator(&f).await;
    let path = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
    for field in ["input_microusd_per_million", "output_microusd_per_million"] {
        let mut p = price();
        p[field] = json!("-1");
        assert_eq!(
            call(&f, &op, "POST", &path, p).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut p = price();
    p["input_token_limit"] = json!(2_147_483_648_i64);
    assert_eq!(
        call(&f, &op, "POST", &path, p).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test]
async fn routing_bounds_operator_assertions_unknown_health_and_audit(pool: PgPool) {
    let f = fixture(&pool).await;
    let model = format!("/api/v1/platform/models/{}/routing", f.model);
    let deployment = format!("/api/v1/platform/deployments/{}/routing", f.deployment);
    assert_eq!(
        call(&f, &f.owner, "GET", &deployment, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let mut route = json!({"priority":2,"weight":10,"residency":"us-east"});
    assert_eq!(
        call(&f, &f.owner, "PUT", &deployment, route.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let op = operator(&f).await;
    let got = call(&f, &op, "GET", &deployment, json!({})).await;
    assert_eq!(got.0, StatusCode::OK);
    assert!(got.1["health"]["last_observed_at"].is_null());
    assert_eq!(
        call(&f, &op, "PUT", &deployment, route.clone()).await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(op.user_id)
        .execute(&pool)
        .await
        .unwrap();
    route["weight"] = json!(25);
    assert_eq!(
        call(&f, &f.owner, "PUT", &deployment, route.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    route["residency"] = json!("eu-west");
    assert_eq!(
        call(&f, &op, "PUT", &deployment, route.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    let _op = operator(&f).await;
    // The database role, not a stale cached false flag, now grants access.
    for residency in ["US", "-us", "us east", "us\n", ""] {
        route["residency"] = json!(residency);
        assert_eq!(
            call(&f, &f.owner, "PUT", &deployment, route.clone())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let p = json!({"strategy":"weighted","max_attempts":3,"allow_ambiguous_failover":false,"failure_threshold":3,"cooldown_seconds":30,"required_residency":"us-east"});
    assert_eq!(
        call(&f, &f.owner, "PUT", &model, p.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &model, json!({})).await.1["policy"],
        p
    );
    for (field, bad) in [
        ("strategy", json!("random")),
        ("max_attempts", json!(4)),
        ("failure_threshold", json!(0)),
        ("cooldown_seconds", json!(3601)),
        ("required_residency", json!("unspecified")),
    ] {
        let mut bad_p = p.clone();
        bad_p[field] = bad;
        assert_eq!(
            call(&f, &f.owner, "PUT", &model, bad_p).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action IN ('governance.model_routing_updated','governance.deployment_routing_updated')").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn costs_monthly_member_scope_unknown_is_not_zero_and_export_safe(pool: PgPool) {
    let f = fixture(&pool).await;
    let op = operator(&f).await;
    let prices = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
    let price: Uuid = call(&f, &op, "POST", &prices, price()).await.1["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let owner_id = execution(&f, f.keys.team_key.id, "owner-only").await;
    reservation(&f, owner_id, f.keys.team_key.id, Some(price), "settled").await;
    let held = execution(
        &f,
        f.member_key,
        " =HYPERLINK(\"https://invalid\"),\r\nline",
    )
    .await;
    reservation(&f, held, f.member_key, Some(price), "unknown").await;
    let unpriced = execution(&f, f.member_key, "unpriced").await;
    reservation(&f, unpriced, f.member_key, None, "unknown").await;
    let old = execution(&f, f.member_key, "previous-month").await;
    sqlx::query("UPDATE inference_executions SET started_at=date_trunc('month',now(),'UTC')-interval '1 second' WHERE id=$1").bind(old).execute(&pool).await.unwrap();
    let base = format!("/api/v1/workspaces/{}", f.keys.team_workspace_id);
    let summary = call(
        &f,
        &f.member,
        "GET",
        &format!("{base}/cost-summary"),
        json!({}),
    )
    .await
    .1;
    assert_eq!(
        summary,
        json!({"currency":"USD","known_cost_microusd":"0","held_microusd":"500","unknown_cost_requests":2,"requests":2})
    );
    let owner_summary = call(
        &f,
        &f.owner,
        "GET",
        &format!("{base}/cost-summary"),
        json!({}),
    )
    .await
    .1;
    assert_eq!(owner_summary["known_cost_microusd"], "7");
    assert_eq!(owner_summary["requests"], 3);
    let rows = call(&f, &f.member, "GET", &format!("{base}/costs"), json!({}))
        .await
        .1;
    assert_eq!(rows["data"].as_array().unwrap().len(), 3);
    assert!(!rows.to_string().contains("owner-only"));
    let unpriced_row = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == unpriced.to_string())
        .unwrap();
    assert!(unpriced_row["cost_microusd"].is_null());
    assert!(unpriced_row["reserved_microusd"].is_null());
    assert_eq!(unpriced_row["cost_status"], "unknown");
    let response = request(
        &f,
        &f.member,
        "GET",
        &format!("{base}/usage-export?limit=1000&offset=0"),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"usage.csv\""
    );
    assert_eq!(response.headers()["x-export-rows"], "3");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let csv = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(csv.contains("\"' =HYPERLINK(\"\"https://invalid\"\"),\r\nline\""));
    assert!(!csv.contains("owner-only"));
    for secret in ["secret_hash", "credential_ref", "prompt", "response_body"] {
        assert!(!csv.contains(secret));
    }
    let response = request(
        &f,
        &f.member,
        "GET",
        &format!("{base}/usage-export?limit=1&offset=1"),
        json!({}),
    )
    .await;
    assert_eq!(response.headers()["x-export-rows"], "1");
    assert_eq!(response.headers()["x-export-offset"], "1");
}

#[sqlx::test]
async fn reconciliation_is_operator_private_scoped_and_atomic(pool: PgPool) {
    let f = fixture(&pool).await;
    let op = operator(&f).await;
    let prices = format!("/api/v1/platform/deployments/{}/prices", f.deployment);
    let price: Uuid = call(&f, &op, "POST", &prices, price()).await.1["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let id = execution(&f, f.member_key, "model").await;
    reservation(&f, id, f.member_key, Some(price), "unknown").await;
    let path = format!(
        "/api/v1/workspaces/{}/costs/{id}/reconcile",
        f.keys.team_workspace_id
    );
    let p = json!({"input_tokens":3,"output_tokens":2,"evidence":"provider-reference:123"});
    assert_eq!(
        call(&f, &f.member, "POST", &path, p.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    let personal = format!(
        "/api/v1/workspaces/{}/costs/{id}/reconcile",
        f.keys.personal_workspace_id
    );
    let mut other = f.member.clone();
    other.platform_admin = true;
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(other.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &other, "POST", &personal, p.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &op, "POST", &personal, p.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    for evidence in ["".to_owned(), "x".repeat(201), "bad\nreference".to_owned()] {
        let mut bad = p.clone();
        bad["evidence"] = json!(evidence);
        assert_eq!(
            call(&f, &op, "POST", &path, bad).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(call(&f, &op, "POST", &path, p).await.0, StatusCode::OK);
    let (state, cost): (String, i64) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), cost), ("settled", 7));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE target_id=$1 AND actor_user_id=$2 AND action='usage.reconciled'").bind(id).bind(op.user_id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn platform_parent_ceilings_compose_null_inherits_and_shared_caps_are_not_reserved(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let op = operator(&f).await;
    let platform = format!("/api/v1/platform/orgs/{}/policy", f.keys.organization_id);
    let org = format!("/api/v1/orgs/{}/policy", f.keys.organization_id);
    let ws = format!("/api/v1/workspaces/{}/policy", f.keys.team_workspace_id);
    let personal = format!("/api/v1/workspaces/{}/policy", f.keys.personal_workspace_id);
    let key = format!(
        "/api/v1/workspaces/{}/keys/{}/policy",
        f.keys.team_workspace_id, f.member_key
    );
    let p = |rpm, budget| json!({"requests_per_minute":rpm,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":budget});
    assert_eq!(
        call(&f, &op, "PUT", &platform, p(Some(100), Some("1000")))
            .await
            .0,
        StatusCode::OK
    );
    // Demote the operator to prove local administrators cannot edit hard ceilings.
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(op.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &op, "PUT", &platform, p(Some(200), None)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(Some(101), None)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(Some(90), Some("1001")))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(Some(90), Some("900")))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &org, json!({})).await.1["ceiling"],
        p(Some(100), Some("1000"))
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &ws, p(Some(91), None)).await.0,
        StatusCode::BAD_REQUEST
    );
    for path in [&ws, &personal] {
        // Two 800 caps share the 900 org allowance; there is no reserved-sum rejection.
        assert_eq!(
            call(&f, &f.owner, "PUT", path, p(Some(80), Some("800")))
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(&f, &f.owner, "PUT", &key, p(Some(81), None)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &key, p(Some(70), Some("700")))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &key, json!({})).await.1["ceiling"],
        p(Some(80), Some("800"))
    );
    assert_eq!(
        call(&f, &f.owner, "PUT", &key, p(None, None)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &key, json!({})).await.1["ceiling"],
        p(Some(80), Some("800"))
    );
    let op = operator(&f).await;
    assert_eq!(
        call(&f, &op, "PUT", &platform, p(Some(10), Some("100")))
            .await
            .0,
        StatusCode::OK
    );
    let read = call(&f, &f.owner, "GET", &ws, json!({})).await.1;
    assert_eq!(
        read["policy"],
        p(Some(80), Some("800")),
        "lowering a parent must not rewrite child configuration"
    );
    assert_eq!(read["ceiling"], p(Some(10), Some("100")));
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(None, None)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.owner, "GET", &org, json!({})).await.1["ceiling"],
        p(Some(10), Some("100"))
    );
}

#[sqlx::test]
async fn delegated_workspace_policy_admin_can_only_tighten_and_live_membership_is_required(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let ws = format!("/api/v1/workspaces/{}/policy", f.keys.team_workspace_id);
    let key = format!(
        "/api/v1/workspaces/{}/keys/{}/policy",
        f.keys.team_workspace_id, f.member_key
    );
    let org = format!("/api/v1/orgs/{}/policy", f.keys.organization_id);
    let p = |rpm| json!({"requests_per_minute":rpm,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    assert_eq!(
        call(&f, &f.owner, "PUT", &ws, p(Some(100))).await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE workspace_memberships SET role='admin' WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, &f.member, "PUT", &ws, p(Some(90))).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &ws, p(Some(95))).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &ws, p(None)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &key, p(Some(80))).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.member, "PUT", &org, p(Some(50))).await.0,
        StatusCode::FORBIDDEN
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(f.keys.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = f.store.clone();
    let user = f.member.clone();
    let org_id = f.keys.organization_id;
    let ws_id = f.keys.team_workspace_id;
    let pending = tokio::spawn(async move {
        write_policy(&store,&user,org_id,Some(ws_id),None,serde_json::from_value(json!({"requests_per_minute":50,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null})).unwrap()).await
    });
    sqlx::query("UPDATE workspace_memberships SET role='member' WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap_err().0, StatusCode::FORBIDDEN);
    assert_eq!(
        call(&f, &f.member, "PUT", &key, p(Some(50))).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn delegated_admin_cannot_erase_caps_masked_by_tighter_parent(pool: PgPool) {
    let f = fixture(&pool).await;
    let ws = format!("/api/v1/workspaces/{}/policy", f.keys.team_workspace_id);
    let key = format!(
        "/api/v1/workspaces/{}/keys/{}/policy",
        f.keys.team_workspace_id, f.member_key
    );
    let org = format!("/api/v1/orgs/{}/policy", f.keys.organization_id);
    let p = |n: Option<i64>| json!({"requests_per_minute":n,"tokens_per_minute":n,"concurrent_requests":n,"monthly_budget_microusd":n.map(|n|n.to_string())});
    sqlx::query("UPDATE workspace_memberships SET role='admin' WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for path in [&ws, &key] {
        assert_eq!(
            call(&f, &f.owner, "PUT", path, p(Some(100))).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(Some(50))).await.0,
        StatusCode::OK
    );
    for path in [&ws, &key] {
        assert_eq!(
            call(&f, &f.member, "PUT", path, p(None)).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f, &f.member, "GET", path, json!({})).await.1["policy"],
            p(Some(100))
        );
    }
    assert_eq!(
        call(&f, &f.owner, "PUT", &org, p(Some(200))).await.0,
        StatusCode::OK
    );
    for path in [&ws, &key] {
        assert_eq!(
            call(&f, &f.member, "GET", path, json!({})).await.1["policy"],
            p(Some(100))
        );
        assert_eq!(
            call(&f, &f.member, "PUT", path, p(Some(40))).await.0,
            StatusCode::OK
        );
    }
}

#[sqlx::test]
async fn authority_rechecked_after_waiting_for_configuration_lock(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(f.keys.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = f.store.clone();
    let owner = f.owner.clone();
    let org = f.keys.organization_id;
    let pending = tokio::spawn(async move {
        let p: Policy = serde_json::from_value(policy()).unwrap();
        write_policy(&store, &owner, org, None, None, p).await
    });
    sqlx::query(
        "UPDATE organization_memberships SET role='member' WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(org)
    .bind(f.owner.user_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap_err().0, StatusCode::FORBIDDEN);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_policies")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

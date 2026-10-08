#![cfg(feature = "integration-tests")]

mod common;

use axum::http::StatusCode;
use common::{BrowserSession, management_app};
use open_model_gateway::{auth::NewApiKey, bootstrap, config::Environment, store::Store};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

fn period() -> String {
    let today = chrono::Utc::now().date_naive();
    let tomorrow = today.succ_opt().unwrap();
    format!("start_date={today}&end_date={tomorrow}")
}

async fn key(pool: &PgPool, ws: Uuid, owner: Uuid, name: &str) -> NewApiKey {
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,$5)")
        .bind(key.id).bind(ws).bind(owner).bind(name).bind(key.digest.as_slice()).execute(pool).await.unwrap();
    key
}

async fn execution(pool: &PgPool, ws: Uuid, key: Uuid, alias: &str) -> Uuid {
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    // Deliberately missing reservation: reports must surface out-of-band accounting,
    // not invent a free settlement for seeded/older requests.
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,input_tokens,output_tokens,root_request_id,completed_at) VALUES($1,$2,$3,$4,$5,'fixture',false,'succeeded',7,2,$1,now())")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(alias).execute(pool).await.unwrap();
    id
}

fn exact_report_strings(value: &Value) {
    for object in ["totals", "health", "coverage", "cost_components"] {
        for (field, value) in value[object].as_object().unwrap() {
            assert!(
                value
                    .as_str()
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
                "{object}.{field}: {value}"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_admin_and_auditor_get_personal_totals_without_private_keys_or_details(
    pool: PgPool,
) {
    let store = Store::new(pool.clone());
    let seed = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let admin_user = store
        .authenticate(&seed.personal_key.token)
        .await
        .unwrap()
        .unwrap()
        .user_id
        .unwrap();
    let admin = BrowserSession::new(&pool, admin_user).await;
    let owner = common::user(&pool, "user").await;
    let auditor_user = common::user(&pool, "auditor").await;
    let auditor = BrowserSession::new(&pool, auditor_user).await;
    let personal = common::personal_workspace(&pool, owner).await;
    let owner_session = BrowserSession::new(&pool, owner).await;
    let private_key = key(
        &pool,
        personal,
        owner,
        "Never disclose this private key label",
    )
    .await;
    let private_execution = execution(&pool, personal, private_key.id, "company/smart").await;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,resource_id) VALUES($1,$2,$3,'private.fixture','key',$4)")
        .bind(Uuid::new_v4()).bind(owner).bind(personal).bind(private_key.id).execute(&pool).await.unwrap();
    let app = management_app(store).await;
    for viewer in [&admin, &auditor] {
        for resource in [
            "keys",
            "executions",
            "audit",
            "usage",
            "models",
            "available-models",
            "costs",
            "cost-report",
            "usage-export",
        ] {
            let path = format!("/api/v1/workspaces/{personal}/{resource}?{}", period());
            // Nonfinancial list endpoints do not accept date filters.
            let path = if matches!(resource, "costs" | "cost-report" | "usage-export") {
                path
            } else {
                format!("/api/v1/workspaces/{personal}/{resource}")
            };
            let status = viewer.get(&app, &path).await.0;
            assert!(
                matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
                "{resource}: {status}"
            );
        }
        let (status, report) = viewer
            .get(
                &app,
                &format!(
                    "/api/v1/platform/cost-report?{}&workspace_id={personal}",
                    period()
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        exact_report_strings(&report);
        assert_eq!(report["totals"]["attempts"], "1");
        assert_eq!(report["totals"]["known_cost_microusd"], "0");
        assert_eq!(report["totals"]["unresolved_attempts"], "1");
        assert_eq!(report["health"]["missing_reservation_attempts"], "1");
        assert_eq!(
            report["breakdowns"]["service_accounts"],
            serde_json::json!([])
        );
        assert!(
            report["billing_usage"]
                .as_object()
                .unwrap()
                .values()
                .all(Value::is_null)
        );
        let serialized = report.to_string();
        assert!(!serialized.contains(&private_key.id.to_string()));
        assert!(!serialized.contains(&private_execution.to_string()));
        assert!(!serialized.contains("Never disclose"));
        assert!(!serialized.contains(&private_key.token));
        let (_, me) = viewer.get(&app, "/api/v1/me").await;
        assert!(
            !me["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == personal.to_string())
        );
        let (status, audit) = viewer.get(&app, "/api/v1/platform/audit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!audit.to_string().contains("private.fixture"));
    }
    let (status, own_keys) = owner_session
        .get(&app, &format!("/api/v1/workspaces/{personal}/keys"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own_keys["data"][0]["id"], private_key.id.to_string());
    assert!(own_keys["data"][0].get("token").is_none());
    assert_eq!(
        owner_session
            .get(&app, &format!("/api/v1/platform/cost-report?{}", period()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_members_activity_predicate_precedes_totals_and_dimension_discovery(pool: PgPool) {
    let store = Store::new(pool.clone());
    let seed = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let admin_user = store
        .authenticate(&seed.team_key.token)
        .await
        .unwrap()
        .unwrap()
        .user_id
        .unwrap();
    let admin = BrowserSession::new(&pool, admin_user).await;
    let member_user = common::user(&pool, "user").await;
    let member = BrowserSession::new(&pool, member_user).await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(seed.team_workspace_id).bind(member_user).execute(&pool).await.unwrap();
    let member_key = key(
        &pool,
        seed.team_workspace_id,
        member_user,
        "Member credential",
    )
    .await;
    let own = execution(&pool, seed.team_workspace_id, member_key.id, "own-model").await;
    let peer = execution(
        &pool,
        seed.team_workspace_id,
        seed.team_key.id,
        "hidden-peer-model",
    )
    .await;
    let account = Uuid::new_v4();
    let service_key = NewApiKey::generate();
    sqlx::query(
        "INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Hidden service')",
    )
    .bind(account)
    .bind(seed.team_workspace_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'Service',$4)").bind(service_key.id).bind(seed.team_workspace_id).bind(account).bind(service_key.digest.as_slice()).execute(&pool).await.unwrap();
    execution(
        &pool,
        seed.team_workspace_id,
        service_key.id,
        "hidden-service-model",
    )
    .await;
    let app = management_app(store).await;
    let path = format!(
        "/api/v1/workspaces/{}/cost-report?{}",
        seed.team_workspace_id,
        period()
    );
    let (status, report) = member.get(&app, &path).await;
    assert_eq!(status, StatusCode::OK);
    exact_report_strings(&report);
    assert_eq!(report["totals"]["attempts"], "1");
    assert_eq!(report["breakdowns"]["models"].as_array().unwrap().len(), 1);
    assert_eq!(report["breakdowns"]["models"][0]["name"], "own-model");
    assert!(!report.to_string().contains("hidden-"));
    let accounts = report["breakdowns"]["service_accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], Value::Null);
    assert_eq!(accounts[0]["name"], "Human keys");
    assert!(!report.to_string().contains(&account.to_string()));
    let (_, admin_report) = admin.get(&app, &path).await;
    assert_eq!(admin_report["totals"]["attempts"], "3");
    assert_eq!(
        admin_report["breakdowns"]["models"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let (_, details) = member
        .get(
            &app,
            &format!(
                "/api/v1/workspaces/{}/costs?{}",
                seed.team_workspace_id,
                period()
            ),
        )
        .await;
    assert_eq!(details["data"].as_array().unwrap().len(), 1);
    assert_eq!(details["data"][0]["id"], own.to_string());
    assert_eq!(details["data"][0]["cost_microusd"], Value::Null);
    assert!(!details.to_string().contains(&peer.to_string()));
    let (_, filtered) = member
        .get(&app, &format!("{path}&actor_user_id={admin_user}"))
        .await;
    assert_eq!(filtered["totals"]["attempts"], "0");
    assert_eq!(filtered["breakdowns"]["models"], serde_json::json!([]));
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(seed.team_workspace_id).bind(member_user).execute(&pool).await.unwrap();
    assert_eq!(member.get(&app, &path).await.0, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn financial_date_contract_is_exclusive_bounded_and_rejects_ambiguous_filters(pool: PgPool) {
    let store = Store::new(pool.clone());
    let seed = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let admin_user = store
        .authenticate(&seed.personal_key.token)
        .await
        .unwrap()
        .unwrap()
        .user_id
        .unwrap();
    let admin = BrowserSession::new(&pool, admin_user).await;
    let app = management_app(store).await;
    let (status,report) = admin.get(&app,"/api/v1/platform/cost-report?start_date=2020-01-01&end_date=2020-04-03&compare=previous_period").await;
    assert_eq!(status, StatusCode::OK); // Exactly 93 days in the leap year.
    assert_eq!(report["daily"].as_array().unwrap().len(), 93);
    assert_eq!(report["daily"][92]["date"], "2020-04-02");
    assert_eq!(report["period"]["end_date"], "2020-04-03");
    exact_report_strings(&report);
    assert!(
        report["billing_usage"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    for query in [
        "start_date=2020-01-01&end_date=2020-04-04",
        "start_date=2020-01-01&end_date=2020-01-01",
        "start_date=2020-01-02&end_date=2020-01-01",
        "start_date=2020-02-30&end_date=2020-03-01",
        "start_date=2020-01-01&end_date=2020-01-02&provider=a&provider=b",
        "start_date=2020-01-01&end_date=2020-01-02&unknown=a",
        "start_date=2020-01-01&end_date=2020-01-02&accounting_status=anything",
        "start_date=2020-01-01&end_date=2020-01-02&workspace_id=bad",
        "start_date=2020-01-01&end_date=2020-01-02&limit=1",
        "start_date=0001-01-01&end_date=0001-01-02&compare=previous_period",
    ] {
        assert_eq!(
            admin
                .get(&app, &format!("/api/v1/platform/cost-report?{query}"))
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    sqlx::query(
        "UPDATE browser_sessions SET expires_at=now()-interval '1 second' WHERE user_id=$1",
    )
    .bind(admin_user)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        admin.get(&app, "/api/v1/me").await.0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE browser_sessions SET expires_at=now()+interval '1 hour',revoked_at=now() WHERE user_id=$1").bind(admin_user).execute(&pool).await.unwrap();
    assert_eq!(
        admin.get(&app, "/api/v1/me").await.0,
        StatusCode::UNAUTHORIZED
    );
}

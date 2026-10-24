//! Real database coverage of the read-only aged unknown-hold queue.
use super::*;
use crate::governance::tests::db::{Fixture, fixture};
use axum::{body::{Body, to_bytes}, http::Request};
use sqlx::PgPool;
use tower::ServiceExt;

fn user(id: Uuid) -> BrowserPrincipal {
    BrowserPrincipal { user_id: id, email: "queue-sentinel-owner@test.invalid".into(), platform_admin: false, platform_auditor: false }
}
async fn call(f: &Fixture, who: Uuid, path: &str) -> (StatusCode, Value) {
    let r = routes().merge(platform_routes())
        .layer(Extension(ManagementPrincipal::Browser(user(who))))
        .with_state(f.store.clone()).oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = to_bytes(r.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
async fn hold(f: &Fixture, ws: Uuid, key: Uuid, state: &str, floor: Option<i64>, days: i32, ledger_days: Option<i32>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'succeeded',$1,1,clock_timestamp()-make_interval(days=>$5))")
        .bind(id).bind(ws).bind(key).bind(f.deployment).bind(days).execute(&f.store.pool).await.unwrap();
    if state != "missing" {
        let price = f.price(1_000_000).await;
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,actual_microusd,price_id,input_tokens,output_tokens) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,$2,$3,$3::bigint IS NULL,CASE WHEN $2='settled' THEN 0 END,$4,CASE WHEN $2='settled' THEN 0 END,CASE WHEN $2='settled' THEN 0 END FROM inference_executions WHERE id=$1")
            .bind(id).bind(state).bind(floor).bind(price).execute(&f.store.pool).await.unwrap();
        if let Some(days) = ledger_days {
            sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,created_at) SELECT $1,execution_id,admitted_at,'unknown',clock_timestamp()-make_interval(days=>$3) FROM governance_reservations WHERE execution_id=$2")
                .bind(Uuid::new_v4()).bind(id).bind(days).execute(&f.store.pool).await.unwrap();
        }
    }
    id
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_visibility_privacy_pagination_and_strict_parameters(pool: PgPool) {
    let f = fixture(pool).await;
    let ws = f.team.workspace_id;
    let personal = f.principal.workspace_id;
    let own = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'queue-sentinel-key',$4)").bind(own.id).bind(ws).bind(f.other).bind(own.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    let service = crate::auth::NewApiKey::generate().unwrap();
    let account = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'service')").bind(account).bind(ws).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'queue-sentinel-service-key',$4)").bind(service.id).bind(ws).bind(account).bind(service.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    let mine = hold(&f, ws, own.id, "unknown", Some(10), 3, None).await;
    hold(&f, ws, f.team.key_id, "unknown", Some(20), 3, None).await;
    hold(&f, ws, service.id, "unknown", None, 3, None).await;
    let private = hold(&f, personal, f.principal.key_id, "unknown", Some(9_007_199_254_740_993), 45, None).await;
    sqlx::query("UPDATE inference_executions SET error_code='queue-sentinel-prompt-text' WHERE id=$1").bind(private).execute(&f.store.pool).await.unwrap();
    sqlx::query("UPDATE users SET email='queue-sentinel-owner@test.invalid' WHERE id=$1").bind(f.owner).execute(&f.store.pool).await.unwrap();
    let path = format!("/api/v1/workspaces/{ws}/holds");
    let (status, v) = call(&f, f.other, &path).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["execution_id"], mine.to_string());
    assert_eq!(v["data"][0]["can_reconcile"], false);
    assert!(!v.to_string().contains("queue-sentinel"));
    // Workspace admin authority alone permits rows, never reconciliation.
    sqlx::query("UPDATE workspace_membership_grants SET role='admin' WHERE workspace_id=$1 AND user_id=$2").bind(ws).bind(f.other).execute(&f.store.pool).await.unwrap();
    let (_, workspace_admin) = call(&f, f.other, &path).await;
    assert_eq!(workspace_admin["data"].as_array().unwrap().len(), 3);
    assert!(workspace_admin["data"].as_array().unwrap().iter().all(|r| r["can_reconcile"] == false));
    sqlx::query("UPDATE workspace_membership_grants SET role='member' WHERE workspace_id=$1 AND user_id=$2").bind(ws).bind(f.other).execute(&f.store.pool).await.unwrap();
    let (_, admin) = call(&f, f.owner, &path).await;
    assert_eq!(admin["data"].as_array().unwrap().len(), 3);
    assert!(admin["data"].as_array().unwrap().iter().all(|r| r["can_reconcile"] == true));
    let (_, first) = call(&f, f.owner, &format!("{path}?limit=1")).await;
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = call(&f, f.owner, &format!("{path}?limit=1&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(first["data"][0]["execution_id"], second["data"][0]["execution_id"]);
    for rejected in [format!("{path}?cursor={cursor}x"), format!("/api/v1/workspaces/{personal}/holds?cursor={cursor}"), format!("/api/v1/platform/holds?cursor={cursor}")] {
        assert_eq!(call(&f, f.owner, &rejected).await.0, StatusCode::BAD_REQUEST, "{rejected}");
    }
    assert_eq!(call(&f, f.other, &format!("{path}?cursor={cursor}")).await.0, StatusCode::BAD_REQUEST);
    for query in ["offset=1", "limit=0", "limit=201", "limit=1&limit=2", "limit=1&%6cimit=2", "workspace_id=bad", "execution_id=bad", "cursor=", "limit=+1", "limit=%zz", "limit=1&", "x=1"] {
        assert_eq!(call(&f, f.owner, &format!("{path}?{query}")).await.0, StatusCode::BAD_REQUEST, "{query}");
    }
    assert_eq!(call(&f, f.other, "/api/v1/platform/holds").await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&f, f.other, &format!("/api/v1/workspaces/{personal}/holds")).await.0, StatusCode::FORBIDDEN);
    for role in ["auditor", "admin"] {
        sqlx::query("UPDATE platform_role_grants SET role=$2 WHERE user_id=$1").bind(f.other).bind(role).execute(&f.store.pool).await.unwrap();
        let (status, p) = call(&f, f.other, "/api/v1/platform/holds").await;
        assert_eq!(status, StatusCode::OK, "{p}");
        assert_eq!(p["data"].as_array().unwrap().len(), 3);
        assert_eq!(p["personal"]["count"], "1");
        assert_eq!(p["personal"]["known_floor_microusd"], "9007199254740993");
        assert!(!p.to_string().contains(&private.to_string()));
        assert!(!p.to_string().contains(&personal.to_string()));
        assert!(!p.to_string().contains("queue-sentinel"));
        assert_eq!(call(&f, f.other, &format!("/api/v1/workspaces/{personal}/holds")).await.0, StatusCode::FORBIDDEN);
    }
    // Live revocation: metadata authority alone never expands workspace activity.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1").bind(f.other).execute(&f.store.pool).await.unwrap();
    let (_, p) = call(&f, f.other, &path).await;
    assert_eq!(p["data"].as_array().unwrap().len(), 1);
    sqlx::query("UPDATE platform_role_grants SET role='user' WHERE user_id=$1").bind(f.other).execute(&f.store.pool).await.unwrap();
    assert_eq!(call(&f, f.other, &path).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1").bind(ws).execute(&f.store.pool).await.unwrap();
    assert_eq!(call(&f, f.owner, &path).await.0, StatusCode::NOT_FOUND);
    let (_, p) = call(&f, f.owner, "/api/v1/platform/holds").await;
    assert!(p["data"].as_array().unwrap().is_empty());
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1").bind(f.owner).execute(&f.store.pool).await.unwrap();
    assert_eq!(call(&f, f.owner, "/api/v1/platform/holds").await.0, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_selection_money_unknown_clock_and_no_writes(pool: PgPool) {
    let f = fixture(pool).await;
    let ws = f.principal.workspace_id;
    let key = f.principal.key_id;
    let old = hold(&f, ws, key, "unknown", Some(9_007_199_254_740_993), 3, Some(2)).await;
    let fallback = hold(&f, ws, key, "unknown", None, 2, None).await;
    for state in ["pending", "settled", "missing"] { hold(&f, ws, key, state, Some(1), 3, None).await; }
    // An old admission which only just became unknown is not aged.
    hold(&f, ws, key, "unknown", Some(99), 3, Some(0)).await;
    let before = crate::governance::totals::verify(&f.store).await.unwrap();
    assert_eq!(before.mismatch_count, 0);
    let snapshot: Value = sqlx::query_scalar("SELECT jsonb_build_object('reservations',(SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r),'ledger',(SELECT jsonb_agg(to_jsonb(l) ORDER BY id) FROM monetary_ledger l),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t))").fetch_one(&f.store.pool).await.unwrap();
    let (status, v) = call(&f, f.owner, &format!("/api/v1/workspaces/{ws}/holds")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{v}");
    let row = rows.iter().find(|r| r["execution_id"] == old.to_string()).unwrap();
    assert_eq!(row["held_microusd"], "9007199254740993");
    assert!(row["age_hours"].as_i64().unwrap() >= 48);
    assert!(row["age_hours"].as_i64().unwrap() < 49);
    let row = rows.iter().find(|r| r["execution_id"] == fallback.to_string()).unwrap();
    assert!(row["held_microusd"].is_null());
    assert_eq!(row["unbounded_cost"], true);
    assert_eq!(v["totals"]["known_floor_microusd"], "9007199254740993");
    assert_eq!(v["totals"]["count"], "2");
    assert_eq!(v["totals"]["unbounded_count"], "1");
    let (status, platform) = call(&f, f.owner, "/api/v1/platform/holds").await;
    assert_eq!(status, StatusCode::OK);
    assert!(platform["data"].as_array().unwrap().is_empty());
    assert_eq!(platform["personal"]["count"], "2");
    assert_eq!(platform["personal"]["known_floor_microusd"], "9007199254740993");
    assert_eq!(platform["personal"]["unbounded_count"], "1");
    assert!(!platform.to_string().contains(&old.to_string()));
    assert!(!platform.to_string().contains(&fallback.to_string()));
    let after = crate::governance::totals::verify(&f.store).await.unwrap();
    assert_eq!(serde_json::to_value(before).unwrap(), serde_json::to_value(after).unwrap());
    let unchanged: Value = sqlx::query_scalar("SELECT jsonb_build_object('reservations',(SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r),'ledger',(SELECT jsonb_agg(to_jsonb(l) ORDER BY id) FROM monetary_ledger l),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t))").fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(snapshot, unchanged);
}

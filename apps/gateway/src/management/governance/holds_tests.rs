//! Real database coverage of the read-only aged unknown-hold queue.
use super::*;
use crate::governance::tests::db::{Fixture, fixture};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;

fn user(id: Uuid) -> BrowserPrincipal {
    BrowserPrincipal {
        user_id: id,
        email: "queue-sentinel-owner@test.invalid".into(),
        platform_admin: false,
        platform_auditor: false,
    }
}
async fn call(f: &Fixture, who: Uuid, path: &str) -> (StatusCode, Value) {
    call_store(&f.store, who, path).await
}
async fn call_store(store: &Store, who: Uuid, path: &str) -> (StatusCode, Value) {
    let r = routes()
        .merge(platform_routes())
        .layer(Extension(ManagementPrincipal::Browser(user(who))))
        .with_state(store.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let bytes = to_bytes(r.into_body(), 1_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn hold(
    f: &Fixture,
    ws: Uuid,
    key: Uuid,
    state: &str,
    floor: Option<i64>,
    days: i32,
    ledger_days: Option<i32>,
) -> Uuid {
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
// This helper runs only in a separate test-runner process, invoked by the
// parent below. A fresh Store alone cannot expose the old process-global KEY.
#[tokio::test]
#[ignore = "invoked by queue_cursor_survives_independent_replica with its isolated database"]
async fn queue_cursor_replica_worker() {
    let pool = PgPool::connect(&std::env::var("OMG_HOLDS_REPLICA_DATABASE").unwrap())
        .await
        .unwrap();
    let who: Uuid = std::env::var("OMG_HOLDS_REPLICA_ACTOR")
        .unwrap()
        .parse()
        .unwrap();
    let expected: Uuid = std::env::var("OMG_HOLDS_REPLICA_EXPECTED")
        .unwrap()
        .parse()
        .unwrap();
    let path = std::env::var("OMG_HOLDS_REPLICA_PATH").unwrap();
    let store = Store::new(pool);
    let (status, next) = call_store(&store, who, &path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "second replica must accept the first replica's position"
    );
    assert_eq!(next["data"].as_array().unwrap().len(), 1);
    assert_eq!(next["data"][0]["execution_id"], expected.to_string());
    assert_eq!(next["totals"]["count"], "2");
    assert_eq!(next["has_more"], false);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_cursor_survives_independent_replica(pool: PgPool) {
    use sqlx::ConnectOptions;
    let f = fixture(pool).await;
    let ws = f.team.workspace_id;
    hold(&f, ws, f.team.key_id, "unknown", Some(10), 4, None).await;
    let second = hold(&f, ws, f.team.key_id, "unknown", Some(20), 3, None).await;
    let path = format!("/api/v1/workspaces/{ws}/holds?limit=1");
    let (status, first) = call(&f, f.owner, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second_path = format!("{path}&cursor={cursor}");
    let second_pool = sqlx::postgres::PgPoolOptions::new()
        .connect_with(f.store.pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let second_store = Store::new(second_pool);
    let (status, next) = call_store(&second_store, f.owner, &second_path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["data"][0]["execution_id"], second.to_string());
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "management::governance::holds_tests::queue_cursor_replica_worker",
            "--ignored",
            "--nocapture",
        ])
        .env(
            "OMG_HOLDS_REPLICA_DATABASE",
            f.store.pool.connect_options().to_url_lossy().as_str(),
        )
        .env("OMG_HOLDS_REPLICA_ACTOR", f.owner.to_string())
        .env("OMG_HOLDS_REPLICA_EXPECTED", second.to_string())
        .env("OMG_HOLDS_REPLICA_PATH", second_path)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "independent replica failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_foreign_workspace_cursor_never_expands_visibility(pool: PgPool) {
    let f = fixture(pool).await;
    let personal = f.principal.workspace_id;
    let private_first = hold(
        &f,
        personal,
        f.principal.key_id,
        "unknown",
        Some(101),
        6,
        None,
    )
    .await;
    let private_second = hold(
        &f,
        personal,
        f.principal.key_id,
        "unknown",
        Some(102),
        5,
        None,
    )
    .await;
    let ws = f.team.workspace_id;
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'foreign-cursor-member',$4)").bind(key.id).bind(ws).bind(f.other).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    hold(&f, ws, f.team.key_id, "unknown", Some(20), 4, None).await;
    let mine = hold(&f, ws, key.id, "unknown", Some(10), 3, None).await;
    let (status, private_page) = call(
        &f,
        f.owner,
        &format!("/api/v1/workspaces/{personal}/holds?limit=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        private_page["data"][0]["execution_id"],
        private_first.to_string()
    );
    let cursor = private_page["next_cursor"].as_str().unwrap();
    let (status, member) = call(
        &f,
        f.other,
        &format!("/api/v1/workspaces/{ws}/holds?cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member["data"].as_array().unwrap().len(), 1);
    assert_eq!(member["data"][0]["execution_id"], mine.to_string());
    assert_eq!(member["totals"]["count"], "1");
    assert_eq!(member["totals"]["known_floor_microusd"], "10");
    let (status, platform) = call(
        &f,
        f.owner,
        &format!("/api/v1/platform/holds?cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(platform["data"].as_array().unwrap().len(), 2);
    assert!(
        platform["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["workspace"]["id"] == ws.to_string())
    );
    assert_eq!(platform["personal"]["count"], "2");
    assert_eq!(platform["personal"]["known_floor_microusd"], "203");
    for id in [personal, private_first, private_second] {
        assert!(!platform.to_string().contains(&id.to_string()));
    }
    // Visibility is live even when an old position survives a role change.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.other)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f,
            f.other,
            &format!("/api/v1/workspaces/{ws}/holds?cursor={cursor}")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_age_boundary_uses_database_clock(pool: PgPool) {
    let query = format!(
        "WITH clock AS MATERIALIZED (SELECT clock_timestamp() observed_at), points AS (SELECT observed_at,observed_at-interval '24 hours' unknown_since FROM clock UNION ALL SELECT observed_at,observed_at-interval '24 hours'+interval '1 millisecond' FROM clock) SELECT array_agg(({}) ORDER BY unknown_since) FROM points",
        holds::AGED
    );
    let aged: Vec<bool> = sqlx::query_scalar(&query).fetch_one(&pool).await.unwrap();
    assert_eq!(aged, vec![true, false]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_project_authority_and_resolving_between_pages(pool: PgPool) {
    let f = fixture(pool).await;
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Project','project')")
        .bind(ws)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'project',$4)").bind(key.id).bind(ws).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    for (who, role) in [(f.owner, "admin"), (f.other, "member")] {
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')").bind(ws).bind(who).bind(role).execute(&f.store.pool).await.unwrap();
    }
    let first_id = hold(&f, ws, key.id, "unknown", Some(10), 4, None).await;
    let second_id = hold(&f, ws, key.id, "unknown", Some(20), 3, None).await;
    let path = format!("/api/v1/workspaces/{ws}/holds");
    let (_, member) = call(&f, f.other, &path).await;
    assert!(member["data"].as_array().unwrap().is_empty());
    assert_eq!(member["totals"]["count"], "0");
    assert!(member["totals"]["known_floor_microusd"].is_null());
    let (status, first) = call(&f, f.owner, &format!("{path}?limit=1")).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"][0]["workspace"]["kind"], "project");
    assert_eq!(first["data"][0]["execution_id"], first_id.to_string());
    // The existing reconciliation action is scope-bound too: a queue id is
    // not authority over an execution from a sibling workspace.
    let usage = Usage {
        input_tokens: Some(1),
        output_tokens: Some(0),
        ..Default::default()
    };
    assert!(
        crate::governance::resolve_usage(
            &f.store,
            f.team.workspace_id,
            first_id,
            usage,
            "queue-id-cross-workspace",
            f.owner
        )
        .await
        .is_err()
    );
    crate::governance::resolve_usage(
        &f.store,
        ws,
        first_id,
        usage,
        "queue-page-reconciliation",
        f.owner,
    )
    .await
    .unwrap();
    let c = first["next_cursor"].as_str().unwrap();
    let (status, next) = call(&f, f.owner, &format!("{path}?limit=1&cursor={c}")).await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["data"][0]["execution_id"], second_id.to_string());
    assert_eq!(next["has_more"], false);
    assert!(next["next_cursor"].is_null());
    let (_, platform) = call(&f, f.owner, "/api/v1/platform/holds").await;
    assert_eq!(platform["data"][0]["execution_id"], second_id.to_string());
    assert_eq!(
        call(&f, f.owner, &format!("{path}/{second_id}")).await.0,
        StatusCode::NOT_FOUND
    );
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
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'service')")
        .bind(account)
        .bind(ws)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'queue-sentinel-service-key',$4)").bind(service.id).bind(ws).bind(account).bind(service.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    let mine = hold(&f, ws, own.id, "unknown", Some(10), 3, None).await;
    hold(&f, ws, f.team.key_id, "unknown", Some(20), 4, None).await;
    hold(&f, ws, service.id, "unknown", None, 3, None).await;
    let private = hold(
        &f,
        personal,
        f.principal.key_id,
        "unknown",
        Some(9_007_199_254_740_993),
        45,
        None,
    )
    .await;
    sqlx::query(
        "UPDATE inference_executions SET error_code='queue-sentinel-prompt-text' WHERE id=$1",
    )
    .bind(private)
    .execute(&f.store.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET email='queue-sentinel-owner@test.invalid' WHERE id=$1")
        .bind(f.owner)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let path = format!("/api/v1/workspaces/{ws}/holds");
    let (status, v) = call(&f, f.other, &path).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["execution_id"], mine.to_string());
    assert_eq!(v["data"][0]["can_reconcile"], false);
    assert!(!v.to_string().contains("queue-sentinel"));
    // Workspace admin authority alone permits rows, never reconciliation.
    sqlx::query(
        "UPDATE workspace_membership_grants SET role='admin' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(ws)
    .bind(f.other)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let (_, workspace_admin) = call(&f, f.other, &path).await;
    assert_eq!(workspace_admin["data"].as_array().unwrap().len(), 3);
    assert!(
        workspace_admin["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["can_reconcile"] == false)
    );
    sqlx::query(
        "UPDATE workspace_membership_grants SET role='member' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(ws)
    .bind(f.other)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let (_, admin) = call(&f, f.owner, &path).await;
    assert_eq!(admin["data"].as_array().unwrap().len(), 3);
    assert!(
        admin["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["can_reconcile"] == true)
    );
    let (_, first) = call(&f, f.owner, &format!("{path}?limit=1")).await;
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = call(&f, f.owner, &format!("{path}?limit=1&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        first["data"][0]["execution_id"],
        second["data"][0]["execution_id"]
    );
    assert_eq!(
        call(&f, f.owner, &format!("{path}?cursor={cursor}x"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // A foreign position is not authority. The current caller's SQL scope
    // filters both detail rows AND totals, independently of cursor origin.
    let (status, member_page) = call(&f, f.other, &format!("{path}?cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(member_page["data"][0]["execution_id"], mine.to_string());
    assert_eq!(member_page["totals"]["count"], "1");
    assert_eq!(member_page["totals"]["known_floor_microusd"], "10");
    let (status, personal_page) = call(
        &f,
        f.owner,
        &format!("/api/v1/workspaces/{personal}/holds?cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(personal_page["data"].as_array().unwrap().is_empty());
    assert_eq!(personal_page["totals"]["count"], "1");
    assert_eq!(
        personal_page["totals"]["known_floor_microusd"],
        "9007199254740993"
    );
    let (status, platform_page) = call(
        &f,
        f.owner,
        &format!("/api/v1/platform/holds?cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(platform_page["data"].as_array().unwrap().len(), 2);
    assert!(
        platform_page["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["workspace"]["id"] == ws.to_string())
    );
    assert_eq!(platform_page["totals"]["count"], "3");
    assert_eq!(platform_page["personal"]["count"], "1");
    assert!(!platform_page.to_string().contains(&private.to_string()));
    assert!(!platform_page.to_string().contains(&personal.to_string()));
    assert!(!platform_page.to_string().contains("queue-sentinel"));
    // Changing a valid position to before every row still cannot reveal an
    // admin's/service key or a different workspace's row to a member.
    let edited = format!("0_{}", Uuid::nil());
    let (status, edited_page) = call(&f, f.other, &format!("{path}?cursor={edited}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited_page["data"].as_array().unwrap().len(), 1);
    assert_eq!(edited_page["data"][0]["execution_id"], mine.to_string());
    assert_eq!(edited_page["totals"]["count"], "1");
    assert_eq!(
        call(
            &f,
            f.other,
            &format!("/api/v1/workspaces/{personal}/holds?cursor={cursor}")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for malformed in [
        "bad",
        "0",
        "0_bad",
        "0_00000000000000000000000000000000",
        "00_00000000-0000-0000-0000-000000000000",
        "9223372036854775808_00000000-0000-0000-0000-000000000000",
        "0_00000000-0000-0000-0000-000000000000_extra",
    ] {
        assert_eq!(
            call(&f, f.owner, &format!("{path}?cursor={malformed}"))
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{malformed}"
        );
    }
    for query in [
        "offset=1",
        "limit=0",
        "limit=201",
        "limit=1&limit=2",
        "limit=1&%6cimit=2",
        "workspace_id=bad",
        "execution_id=bad",
        "cursor=",
        "limit=+1",
        "limit=%zz",
        "limit=1&",
        "x=1",
    ] {
        assert_eq!(
            call(&f, f.owner, &format!("{path}?{query}")).await.0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    assert_eq!(
        call(&f, f.other, "/api/v1/platform/holds").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f, f.other, &format!("/api/v1/workspaces/{personal}/holds"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for role in ["auditor", "admin"] {
        sqlx::query("UPDATE platform_role_grants SET role=$2 WHERE user_id=$1")
            .bind(f.other)
            .bind(role)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let (status, p) = call(&f, f.other, "/api/v1/platform/holds").await;
        assert_eq!(status, StatusCode::OK, "{p}");
        assert_eq!(p["data"].as_array().unwrap().len(), 3);
        assert_eq!(p["personal"]["count"], "1");
        assert_eq!(p["personal"]["known_floor_microusd"], "9007199254740993");
        assert!(!p.to_string().contains(&private.to_string()));
        assert!(!p.to_string().contains(&personal.to_string()));
        assert!(!p.to_string().contains("queue-sentinel"));
        assert_eq!(
            call(&f, f.other, &format!("/api/v1/workspaces/{personal}/holds"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    // Live revocation: metadata authority alone never expands workspace activity.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.other)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (_, p) = call(&f, f.other, &path).await;
    assert_eq!(p["data"].as_array().unwrap().len(), 1);
    sqlx::query("UPDATE platform_role_grants SET role='user' WHERE user_id=$1")
        .bind(f.other)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(call(&f, f.other, &path).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(ws)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(call(&f, f.owner, &path).await.0, StatusCode::NOT_FOUND);
    let (_, p) = call(&f, f.owner, "/api/v1/platform/holds").await;
    // Disabling a workspace never hides or releases its retained money.
    // Platform Logs visibility includes it, but reconciliation needs re-enable.
    assert_eq!(p["data"].as_array().unwrap().len(), 3);
    assert!(
        p["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["can_reconcile"] == false)
    );
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, f.owner, "/api/v1/platform/holds").await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn queue_selection_money_unknown_clock_and_no_writes(pool: PgPool) {
    let f = fixture(pool).await;
    let ws = f.principal.workspace_id;
    let key = f.principal.key_id;
    let old = hold(
        &f,
        ws,
        key,
        "unknown",
        Some(9_007_199_254_740_993),
        3,
        Some(2),
    )
    .await;
    let fallback = hold(&f, ws, key, "unknown", None, 2, None).await;
    for state in ["pending", "settled", "missing"] {
        hold(&f, ws, key, state, Some(1), 3, None).await;
    }
    // An old admission which only just became unknown is not aged.
    hold(&f, ws, key, "unknown", Some(99), 3, Some(0)).await;
    let before = crate::governance::totals::verify(&f.store).await.unwrap();
    assert_eq!(before.mismatch_count, 0);
    let snapshot: Value = sqlx::query_scalar("SELECT jsonb_build_object('reservations',(SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r),'ledger',(SELECT jsonb_agg(to_jsonb(l) ORDER BY id) FROM monetary_ledger l),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t))").fetch_one(&f.store.pool).await.unwrap();
    let (status, v) = call(&f, f.owner, &format!("/api/v1/workspaces/{ws}/holds")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{v}");
    let row = rows
        .iter()
        .find(|r| r["execution_id"] == old.to_string())
        .unwrap();
    assert_eq!(row["held_microusd"], "9007199254740993");
    assert!(row["age_hours"].as_i64().unwrap() >= 48);
    assert!(row["age_hours"].as_i64().unwrap() < 49);
    let row = rows
        .iter()
        .find(|r| r["execution_id"] == fallback.to_string())
        .unwrap();
    assert!(row["held_microusd"].is_null());
    assert_eq!(row["unbounded_cost"], true);
    assert_eq!(v["totals"]["known_floor_microusd"], "9007199254740993");
    assert_eq!(v["totals"]["count"], "2");
    assert_eq!(v["totals"]["unbounded_count"], "1");
    let (status, platform) = call(&f, f.owner, "/api/v1/platform/holds").await;
    assert_eq!(status, StatusCode::OK);
    assert!(platform["data"].as_array().unwrap().is_empty());
    assert_eq!(platform["personal"]["count"], "2");
    assert_eq!(
        platform["personal"]["known_floor_microusd"],
        "9007199254740993"
    );
    assert_eq!(platform["personal"]["unbounded_count"], "1");
    assert!(!platform.to_string().contains(&old.to_string()));
    assert!(!platform.to_string().contains(&fallback.to_string()));
    let after = crate::governance::totals::verify(&f.store).await.unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    let unchanged: Value = sqlx::query_scalar("SELECT jsonb_build_object('reservations',(SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r),'ledger',(SELECT jsonb_agg(to_jsonb(l) ORDER BY id) FROM monetary_ledger l),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t))").fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(snapshot, unchanged);
}

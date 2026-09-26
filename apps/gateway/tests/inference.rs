#![cfg(feature = "integration-tests")]

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures_util::stream;
use open_model_gateway::{
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    inference::{
        Engine, EngineLimits, error::InferenceError, repository::InferenceRepository, types::*,
    },
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;

struct FixtureAdapter {
    calls: AtomicUsize,
    truncate: bool,
}
#[async_trait]
impl ProviderAdapter for FixtureAdapter {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    async fn execute(
        &self,
        target: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        let usage = Usage {
            input_tokens: Some(7),
            output_tokens: Some(2),
        };
        if request.stream {
            let mut events = vec![
                Ok(ChatEvent::Delta {
                    text: Some("Hello".into()),
                    tool_calls: vec![],
                }),
                Ok(ChatEvent::Finish(FinishReason::Stop)),
                Ok(ChatEvent::Usage(usage)),
            ];
            if !self.truncate {
                events.push(Ok(ChatEvent::Done));
            }
            Ok(ProviderOutput::Stream(Box::pin(stream::iter(events))))
        } else {
            Ok(ProviderOutput::Complete(ChatResponse {
                content: Some("Hello".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            }))
        }
    }
}
async fn fixture(
    pool: &PgPool,
    truncate: bool,
) -> (axum::Router, DevelopmentKeys, Arc<FixtureAdapter>, Store) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='test_vendor', enabled=true")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true, upstream_model='private-upstream-id'")
        .execute(pool)
        .await
        .unwrap();
    let adapter = Arc::new(FixtureAdapter {
        calls: AtomicUsize::new(0),
        truncate,
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    (
        http::router_with_engine(store.clone(), None, engine),
        keys,
        adapter,
        store,
    )
}
fn request(key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn chat(stream: bool) -> Value {
    json!({"model":"company/smart","messages":[{"role":"user","content":"Do not log this prompt"}],"stream":stream})
}

#[sqlx::test]
async fn database_budget_admission_and_versioned_costs_reach_real_http(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,organization_id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,$3,1000000,2000000,100,10)").bind(price).bind(keys.organization_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,monthly_budget_microusd) VALUES($1,$2,'organization',125)").bind(Uuid::new_v4()).bind(keys.organization_id).execute(&pool).await.unwrap();
    let missing_bound = app
        .clone()
        .oneshot(request(&keys.personal_key.token, chat(false)))
        .await
        .unwrap();
    assert!(!missing_bound.status().is_success());
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    let mut body = chat(false);
    body["max_completion_tokens"] = 10.into();
    assert_eq!(
        app.clone()
            .oneshot(request(&keys.personal_key.token, body.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let row: (Uuid, i64, String) =
        sqlx::query_as("SELECT price_id,actual_microusd,state FROM governance_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (price, 11, "settled".into()));
    assert_eq!(
        app.clone()
            .oneshot(request(&keys.personal_key.token, body.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE governance_policies SET monthly_budget_microusd=1000")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,organization_id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,$3,2000000,3000000,100,10)").bind(Uuid::new_v4()).bind(keys.organization_id).bind(deployment).execute(&pool).await.unwrap();
    assert_eq!(
        app.oneshot(request(&keys.personal_key.token, body))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let costs: Vec<i64> = sqlx::query_scalar(
        "SELECT actual_microusd FROM governance_reservations ORDER BY admitted_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(costs, vec![11, 20]);
}
#[sqlx::test]
async fn independent_engines_share_database_request_limits(pool: PgPool) {
    let (app, keys, adapter, store) = fixture(&pool, false).await;
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,requests_per_minute) VALUES($1,$2,'organization',1)").bind(Uuid::new_v4()).bind(keys.organization_id).execute(&pool).await.unwrap();
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let second = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let other = http::router_with_engine(store, None, second);
    let (a, b) = tokio::join!(
        app.oneshot(request(&keys.personal_key.token, chat(false))),
        other.oneshot(request(&keys.team_key.token, chat(false)))
    );
    let statuses = [a.unwrap().status(), b.unwrap().status()];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::TOO_MANY_REQUESTS));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}
#[sqlx::test]
async fn chat_response_preserves_public_alias_and_records_workspace_usage(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let response = app
        .oneshot(request(&keys.personal_key.token, chat(false)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let id = Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["model"], "company/smart");
    assert_eq!(value["choices"][0]["message"]["content"], "Hello");
    assert_eq!(value["usage"]["total_tokens"], 9);
    assert!(!String::from_utf8_lossy(&body).contains("private-upstream-id"));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    let row: (Uuid, Uuid, String, Option<i64>, Option<i64>) = sqlx::query_as("SELECT organization_id, workspace_id, state, input_tokens, output_tokens FROM inference_executions WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        (
            keys.organization_id,
            keys.personal_workspace_id,
            "succeeded".into(),
            Some(7),
            Some(2)
        )
    );
}

#[sqlx::test]
async fn streaming_emits_usage_only_when_requested_and_done_only_on_success(pool: PgPool) {
    let (app, keys, _, _) = fixture(&pool, false).await;
    for include_usage in [true, false] {
        let mut body = chat(true);
        body["stream_options"] = json!({"include_usage":include_usage});
        let response = app
            .clone()
            .oneshot(request(&keys.team_key.token, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let text = String::from_utf8(
            to_bytes(response.into_body(), 16384)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("Hello"));
        assert!(text.contains("company/smart"));
        assert!(text.contains("[DONE]"));
        assert_eq!(text.contains("\"total_tokens\":9"), include_usage);
        assert!(!text.contains("private-upstream-id"));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inference_executions WHERE state='succeeded' AND workspace_id=$1",
    )
    .bind(keys.team_workspace_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn malformed_stream_yields_safe_error_without_success_marker(pool: PgPool) {
    let (app, keys, _, _) = fixture(&pool, true).await;
    let response = app
        .oneshot(request(&keys.personal_key.token, chat(true)))
        .await
        .unwrap();
    let text = String::from_utf8(
        to_bytes(response.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("invalid_upstream_response"));
    assert!(!text.contains("[DONE]"));
    let state: String = sqlx::query_scalar("SELECT state FROM inference_executions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed");
}

#[sqlx::test]
async fn denied_disabled_unknown_and_unsupported_requests_never_execute(pool: PgPool) {
    let (app, keys, adapter, store) = fixture(&pool, false).await;
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request(&keys.team_key.token, chat(false)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let mut unknown = chat(false);
    unknown["model"] = "missing".into();
    assert_eq!(
        app.clone()
            .oneshot(request(&keys.personal_key.token, unknown))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut unsupported = chat(false);
    unsupported["response_format"] = json!({"type":"json_object"});
    assert_eq!(
        app.clone()
            .oneshot(request(&keys.personal_key.token, unsupported))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    for table in ["models", "deployments", "provider_connections"] {
        sqlx::query(&format!("UPDATE {table} SET enabled=false"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            store
                .deployments(&principal, "company/smart")
                .await
                .unwrap()
                .is_empty()
        );
        let response = app
            .clone()
            .oneshot(request(&keys.personal_key.token, chat(false)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        sqlx::query(&format!("UPDATE {table} SET enabled=true"))
            .execute(&pool)
            .await
            .unwrap();
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn deployment_resolution_cannot_cross_organizations(pool: PgPool) {
    let (_app, keys, _, store) = fixture(&pool, false).await;
    let mut principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    principal.organization_id = Uuid::new_v4();
    assert!(
        store
            .deployments(&principal, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn platform_ceiling_is_shared_by_team_project_and_personal_requests(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let project = Uuid::new_v4();
    let project_key = open_model_gateway::auth::NewApiKey::generate();
    let user: Uuid = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(keys.team_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'Research','project')",
    )
    .bind(project)
    .bind(keys.organization_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')")
        .bind(keys.organization_id).bind(project).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,'Project test',$5)")
        .bind(project_key.id).bind(keys.organization_id).bind(project).bind(user).bind(project_key.digest.as_slice()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) SELECT $1,$2,model_id FROM organization_model_grants WHERE organization_id=$1")
        .bind(keys.organization_id).bind(project).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO platform_organization_policies(organization_id,requests_per_minute) VALUES($1,2)")
        .bind(keys.organization_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,requests_per_minute) VALUES($1,$2,'workspace',$3,1)")
        .bind(Uuid::new_v4()).bind(keys.organization_id).bind(keys.team_workspace_id).execute(&pool).await.unwrap();
    for (token, expected) in [
        (&keys.team_key.token, StatusCode::OK),
        (&keys.team_key.token, StatusCode::TOO_MANY_REQUESTS),
        (&project_key.token, StatusCode::OK),
        (&project_key.token, StatusCode::TOO_MANY_REQUESTS),
        (&keys.personal_key.token, StatusCode::TOO_MANY_REQUESTS),
    ] {
        let response = app
            .clone()
            .oneshot(request(token, chat(false)))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    // Removing a local restriction does not remove the separately-owned parent ceiling.
    sqlx::query("DELETE FROM governance_policies WHERE organization_id=$1")
        .bind(keys.organization_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        app.oneshot(request(&keys.team_key.token, chat(false)))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
}

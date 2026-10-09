#![cfg(feature = "integration-tests")]

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures_util::{StreamExt, stream};
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
    stream_drops: Arc<AtomicUsize>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct DropSignal(Arc<AtomicUsize>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
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
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::ChatCompletions
                | ApiProtocol::Embeddings
                | ApiProtocol::Rerank
                | ApiProtocol::Systemone
        )
    }
    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        Ok(RerankResponse {
            results: vec![
                RerankResult {
                    index: request.documents.len() - 1,
                    relevance_score: 0.75,
                },
                RerankResult {
                    index: 0,
                    relevance_score: 0.25,
                },
            ],
            usage: Usage {
                input_tokens: Some(25),
                output_tokens: Some(0),
                billing: Some(open_model_gateway::billing::BillingUsage {
                    total_input_tokens: Some(25),
                    uncached_input_tokens: Some(25),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                meters: Some(open_model_gateway::billing::MeterUsage {
                    output_images: Some(0),
                    input_characters: Some(0),
                    input_audio_seconds_ms: Some(0),
                    output_audio_seconds_ms: Some(0),
                    search_units: Some(1),
                    requests: Some(1),
                }),
                provider_cost_microusd: Some(7),
                ..Default::default()
            },
        })
    }
    async fn execute_systemone(
        &self,
        target: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        Ok(SystemoneResponse {
            answers: request
                .questions
                .keys()
                .map(|k| (k.clone(), Answer::Noul { noul: 0.9566 }))
                .collect(),
            usage: Usage {
                input_tokens: Some(145),
                output_tokens: Some(0),
                ..Default::default()
            },
        })
    }
    fn supports_embedding_request(&self, request: &EmbeddingRequest) -> bool {
        request.dimensions != Some(3)
    }
    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if target.upstream_model == "embedding-busy-fixture" {
            return Err(InferenceError::Busy);
        }
        assert_eq!(target.upstream_model, "private-upstream-id");
        let dimensions = if request.input[0] == "wrong-dimension" {
            1
        } else {
            request.dimensions.unwrap_or(2) as usize
        };
        let mut embeddings = vec![vec![0.25; dimensions]; request.input.len()];
        if request.input[0] == "nonfinite" {
            embeddings[0][0] = f32::NAN;
        }
        if request.input[0] == "missing-vector" {
            embeddings.pop();
        }
        Ok(EmbeddingResponse {
            embeddings,
            usage: Usage {
                input_tokens: Some(7),
                // Embeddings have no output-token workload; this zero is semantic.
                output_tokens: Some(0),
                billing: Some(open_model_gateway::billing::BillingUsage {
                    total_input_tokens: Some(7),
                    uncached_input_tokens: Some(5),
                    cache_read_input_tokens: Some(2),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                ..Default::default()
            },
        })
    }
    async fn execute(
        &self,
        target: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        if request
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("wait-for-price"))
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        let usage = Usage {
            input_tokens: Some(7),
            output_tokens: Some(2),
            billing: None,
            // Provider-reported telemetry (Logs only, never sent to clients).
            reasoning_tokens: Some(1),
            reported_model: ReportedModel::parse("private-upstream-id-2026-01-01"),
            ..Default::default()
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
            let signal = DropSignal(self.stream_drops.clone());
            let mut upstream = stream::iter(events);
            Ok(ProviderOutput::Stream(Box::pin(async_stream::stream! {
                let _signal = signal;
                while let Some(event) = upstream.next().await { yield event; }
            })))
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
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(pool)
        .await
        .unwrap();
    let adapter = Arc::new(FixtureAdapter {
        calls: AtomicUsize::new(0),
        truncate,
        stream_drops: Arc::new(AtomicUsize::new(0)),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
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
/// Embeddings are a separate workload from generation protocols.
async fn embeddings_only(pool: &PgPool) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['embeddings']")
        .execute(pool)
        .await
        .unwrap();
}
fn request(key: &str, body: Value) -> Request<Body> {
    protocol_request(key, "/v1/chat/completions", body)
}
fn protocol_request(key: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn chat(stream: bool) -> Value {
    json!({"model":"company/smart","messages":[{"role":"user","content":"Do not log this prompt"}],"stream":stream})
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn database_budget_admission_and_versioned_costs_reach_real_http(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let price = Uuid::new_v4();
    // Explicit v1 exercises legacy two-rate accounting; absent billing remains unknown.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,2000000,100,10,1)").bind(price).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',125)",
    )
    .execute(&pool)
    .await
    .unwrap();
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
    sqlx::query("UPDATE policy_budgets SET amount_microusd=1000 WHERE layer='installation'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,2000000,3000000,100,10,1)").bind(Uuid::new_v4()).bind(deployment).execute(&pool).await.unwrap();
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
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn independent_engines_share_database_request_limits(pool: PgPool) {
    let (app, keys, adapter, store) = fixture(&pool, false).await;
    sqlx::query("INSERT INTO installation_policy(singleton,requests_per_minute) VALUES(true,1)")
        .execute(&pool)
        .await
        .unwrap();
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
#[sqlx::test(migrations = "./enterprise_migrations")]
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
    let row: (Uuid, String, Option<i64>, Option<i64>) = sqlx::query_as("SELECT workspace_id, state, input_tokens, output_tokens FROM inference_executions WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        (
            keys.personal_workspace_id,
            "succeeded".into(),
            Some(7),
            Some(2)
        )
    );
}

type TelemetryRow = (
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
);
async fn telemetry(pool: &PgPool, id: Uuid) -> TelemetryRow {
    sqlx::query_as("SELECT finish_reason,time_to_first_token_ms,generation_ms,upstream_model,client_session_id,client_app FROM inference_executions WHERE id=$1")
        .bind(id).fetch_one(pool).await.unwrap()
}
/// Provider-reported served model (0013) and reasoning tokens of an attempt;
/// the configured `upstream_model` snapshot is kept alongside.
async fn reported(pool: &PgPool, id: Uuid) -> (Option<String>, Option<i64>, Option<String>) {
    sqlx::query_as("SELECT reported_upstream_model,reasoning_tokens,upstream_model FROM inference_executions WHERE id=$1")
        .bind(id).fetch_one(pool).await.unwrap()
}
fn served() -> (Option<String>, Option<i64>, Option<String>) {
    (
        Some("private-upstream-id-2026-01-01".into()),
        Some(1),
        Some("private-upstream-id".into()),
    )
}
async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Option<Uuid>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| Uuid::parse_str(v.to_str().ok()?).ok());
    to_bytes(response.into_body(), 65536).await.unwrap();
    (status, id)
}
/// Logs telemetry: finish reason, timings, configured upstream snapshot and
/// optional client labels are recorded per attempt; bodies never are.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn request_telemetry_and_client_labels_are_recorded(pool: PgPool) {
    let (app, keys, _, _) = fixture(&pool, false).await;
    let key = &keys.team_key.token;
    // Header labels win over body candidates.
    let mut r = request(key, chat(false));
    r.headers_mut()
        .insert("x-session-id", "sess-A".parse().unwrap());
    r.headers_mut()
        .insert("x-title", "Logs test".parse().unwrap());
    let (status, id) = send(&app, r).await;
    assert_eq!(status, StatusCode::OK);
    let (finish, ttft, generation, upstream, session, app_name) =
        telemetry(&pool, id.unwrap()).await;
    assert_eq!(finish.as_deref(), Some("stop"));
    assert_eq!(ttft, None, "time to first token is for streams only");
    assert!(generation.is_some());
    assert_eq!(upstream.as_deref(), Some("private-upstream-id"));
    assert_eq!(reported(&pool, id.unwrap()).await, served());
    assert_eq!(session.as_deref(), Some("sess-A"));
    assert_eq!(app_name.as_deref(), Some("Logs test"));
    // Streams: metadata.session_id before user; first delta sets TTFT.
    let mut body = chat(true);
    body["user"] = "end-user-7".into();
    body["metadata"] = json!({"session_id":"conv 42","topic":"x"});
    let (status, id) = send(&app, request(key, body)).await;
    assert_eq!(status, StatusCode::OK);
    let (finish, ttft, generation, _, session, app_name) = telemetry(&pool, id.unwrap()).await;
    assert_eq!(finish.as_deref(), Some("stop"));
    assert!(ttft.is_some() && generation.is_some() && ttft <= generation);
    assert_eq!(reported(&pool, id.unwrap()).await, served());
    assert_eq!(session.as_deref(), Some("conv 42"));
    assert_eq!(app_name, None);
    let mut body = chat(false);
    body["user"] = "end-user-7".into();
    let (_, id) = send(&app, request(key, body)).await;
    assert_eq!(
        telemetry(&pool, id.unwrap()).await.4.as_deref(),
        Some("end-user-7")
    );
    // Invalid labels are not recorded and never fail the request.
    let mut r = request(key, chat(false));
    r.headers_mut()
        .insert("x-session-id", " padded".parse().unwrap());
    r.headers_mut()
        .insert("x-title", "x".repeat(201).parse().unwrap());
    let (status, id) = send(&app, r).await;
    assert_eq!(status, StatusCode::OK);
    let row = telemetry(&pool, id.unwrap()).await;
    assert_eq!((row.4, row.5), (None, None));
    // OpenAI metadata keeps its documented bounds.
    let mut body = chat(false);
    body["metadata"] = (0..17)
        .map(|i| (format!("k{i}"), json!("v")))
        .collect::<serde_json::Map<_, _>>()
        .into();
    assert_eq!(
        send(&app, request(key, body)).await.0,
        StatusCode::BAD_REQUEST
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE client_session_id IS NOT NULL AND client_session_id LIKE '%prompt%'").fetch_one(&pool).await.unwrap();
    assert_eq!(stored, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_resolution_cannot_cross_workspaces(pool: PgPool) {
    let (_app, keys, _, store) = fixture(&pool, false).await;
    let mut principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    principal.workspace_id = keys.team_workspace_id;
    // A forged principal retains the original key, which belongs to the personal workspace.
    assert!(
        store
            .deployments(&principal, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_ceiling_is_shared_by_team_project_and_personal_requests(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let project = Uuid::new_v4();
    let project_key = open_model_gateway::auth::NewApiKey::generate();
    let user: Uuid = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(keys.team_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Research','project')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(project).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'Project test',$4)")
        .bind(project_key.id).bind(project).bind(user).bind(project_key.digest.as_slice()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT $1,id,'direct' FROM models")
        .bind(project).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO installation_policy(singleton,requests_per_minute) VALUES(true,2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_local_policies(workspace_id,requests_per_minute) VALUES($1,1)",
    )
    .bind(keys.team_workspace_id)
    .execute(&pool)
    .await
    .unwrap();
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
    sqlx::query("DELETE FROM workspace_local_policies")
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_budget_holds_are_atomic_and_price_is_pinned_before_dispatch(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let first_price = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,2000000,100,10,1)")
        .bind(first_price).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',125)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut body = chat(false);
    body["messages"][0]["content"] = "wait-for-price".into();
    body["max_completion_tokens"] = 10.into();
    let first_request = request(&keys.personal_key.token, body);
    let first_app = app.clone();
    let task = tokio::spawn(async move { first_app.oneshot(first_request).await.unwrap() });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        adapter.started.notified(),
    )
    .await
    .unwrap();
    let mut second = chat(false);
    second["max_completion_tokens"] = 10.into();
    assert_eq!(
        app.oneshot(request(&keys.team_key.token, second))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let reservation: (Uuid, i64, String) =
        sqlx::query_as("SELECT price_id,held_microusd,state FROM governance_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reservation, (first_price, 120, "pending".into()));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,9000000,9000000,100,10,1)")
        .bind(Uuid::new_v4()).bind(deployment).execute(&pool).await.unwrap();
    adapter.release.notify_one();
    assert_eq!(task.await.unwrap().status(), StatusCode::OK);
    let settled: (Uuid, i64, String) =
        sqlx::query_as("SELECT price_id,actual_microusd,state FROM governance_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settled, (first_price, 11, "settled".into()));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn enabling_budget_after_unpriced_activity_does_not_invent_zero_cost(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    assert_eq!(
        app.clone()
            .oneshot(request(&keys.personal_key.token, chat(false)))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let unknown: (Option<i64>, Option<i64>, String) =
        sqlx::query_as("SELECT actual_microusd,held_microusd,state FROM governance_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unknown, (None, None, "unknown".into()));
    // New requests now have a real finite price; the previous unpriced request
    // must still block admission rather than becoming a fake free settlement.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT $1,id,1000000,2000000,100,10,1 FROM deployments")
        .bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut body = chat(false);
    body["max_completion_tokens"] = 10.into();
    assert_eq!(
        app.oneshot(request(&keys.personal_key.token, body))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dropping_http_stream_cancels_transport_and_retains_unknown_hold(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    let response = app
        .oneshot(request(&keys.personal_key.token, chat(true)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    assert_eq!(adapter.stream_drops.load(Ordering::SeqCst), 1);
    // Cancellation finalization is deliberately asynchronous and bounded.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state: String = sqlx::query_scalar("SELECT state FROM inference_executions")
                .fetch_one(&pool)
                .await
                .unwrap();
            if state == "cancelled" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let row: (String, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd,input_tokens,output_tokens FROM governance_reservations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("unknown".into(), None, None, None));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn embeddings_batch_dimensions_and_input_only_v2_budget_reach_http(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    embeddings_only(&pool).await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let price = Uuid::new_v4();
    let cache = json!({
        "read":{"status":"priced","microusd_per_million":"500000"},
        "write":{"status":"priced","microusd_per_million":"1000000"},
        "write_5m":{"status":"priced","microusd_per_million":"1000000"},
        "write_1h":{"status":"priced","microusd_per_million":"1000000"}
    });
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,1000000,99000000,100,0,2,$3)")
        .bind(price).bind(deployment).bind(cache).execute(&pool).await.unwrap();
    sqlx::query(
        // v2 reserves conservative independent input category ceilings: 450.
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',455)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let body = json!({"model":"company/smart","input":["first","second"],"dimensions":2,"encoding_format":"float"});
    let response = app
        .clone()
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/embeddings",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let id = Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["model"], "company/smart");
    assert_eq!(value["object"], "list");
    assert_eq!(value["data"].as_array().unwrap().len(), 2);
    assert_eq!(value["data"][1]["index"], 1);
    assert_eq!(value["data"][0]["embedding"], json!([0.25, 0.25]));
    assert_eq!(value["usage"], json!({"prompt_tokens":7,"total_tokens":7}));
    assert!(!String::from_utf8_lossy(&bytes).contains("private-upstream-id"));
    let metadata: (Uuid, Uuid, i32, String, i64, i64) = sqlx::query_as("SELECT workspace_id,root_request_id,attempt_number,workload_kind,input_tokens,output_tokens FROM inference_executions WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        metadata,
        (keys.personal_workspace_id, id, 1, "embeddings".into(), 7, 0)
    );
    let row: (Uuid, i64, i64, Value) = sqlx::query_as("SELECT price_id,held_microusd,actual_microusd,cost_components FROM governance_reservations WHERE execution_id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, price);
    assert_eq!(row.1, 450);
    assert_eq!(row.2, 6);
    assert_eq!(row.3["uncached_input_microusd"], "5");
    assert_eq!(row.3["cache_read_microusd"], "1");
    assert_eq!(row.3["output_microusd"], "0");
    assert_eq!(
        app.oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/embeddings",
            body
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn embedding_invalid_inputs_and_profile_model_intersection_never_dispatch(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    embeddings_only(&pool).await;
    for body in [
        json!({"model":"company/smart","input":[]}),
        json!({"model":"company/smart","input":[1,2]}),
        json!({"model":"company/smart","input":"a","dimensions":0}),
        json!({"model":"company/smart","input":"a","encoding_format":"base64"}),
        json!({"model":"company/smart","input":"a","stream":true}),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(protocol_request(
                    &keys.personal_key.token,
                    "/v1/embeddings",
                    body
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut body = json!({"model":"company/smart","input":"a","dimensions":3});
    assert_eq!(
        app.clone()
            .oneshot(protocol_request(
                &keys.personal_key.token,
                "/v1/embeddings",
                body.clone()
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    body["dimensions"] = 2.into();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        app.oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/embeddings",
            body
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn malformed_embedding_vectors_preserve_valid_usage_without_settlement(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    embeddings_only(&pool).await;
    for input in ["wrong-dimension", "nonfinite", "missing-vector"] {
        let response = app
            .clone()
            .oneshot(protocol_request(
                &keys.personal_key.token,
                "/v1/embeddings",
                json!({"model":"company/smart","input":[input,"second"],"dimensions":2}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{input}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "invalid_upstream_response");
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 3);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.state='failed' AND e.workload_kind='embeddings' AND e.input_tokens=7 AND e.billing_usage->>'cache_read_input_tokens'='2' AND e.output_tokens=0 AND r.actual_microusd IS NULL AND r.state='unknown'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(count, 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn embedding_failover_records_distinct_attempts_reservations_and_pinned_prices(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    embeddings_only(&pool).await;
    let (first, model): (Uuid, Uuid) = sqlx::query_as("SELECT id,model_id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let second = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,upstream_model,true FROM deployments WHERE id=$2")
        .bind(second).bind(first).execute(&pool).await.unwrap();
    sqlx::query("UPDATE deployments SET upstream_model='embedding-busy-fixture' WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routing_policies(model_id,strategy,max_attempts) VALUES($1,'priority',2)",
    )
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,0,'fixture-region'),($2,1,'fixture-region')")
        .bind(first)
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
    let first_price = Uuid::new_v4();
    let second_price = Uuid::new_v4();
    for (deployment, price, rate) in [
        (first, first_price, 1_000_000_i64),
        (second, second_price, 2_000_000_i64),
    ] {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,$3,99000000,100,0,1)")
            .bind(price).bind(deployment).bind(rate).execute(&pool).await.unwrap();
    }
    let response = app
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/embeddings",
            json!({"model":"company/smart","input":"text"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let root = Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    type AttemptRow = (
        Uuid,
        Uuid,
        i32,
        Uuid,
        String,
        String,
        Option<i64>,
        Option<i64>,
    );
    let attempts: Vec<AttemptRow> = sqlx::query_as("SELECT e.id,e.root_request_id,e.attempt_number,r.price_id,e.state,e.workload_kind,r.actual_microusd,e.output_tokens FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.attempt_number")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0],
        (
            root,
            root,
            1,
            first_price,
            "failed".into(),
            "embeddings".into(),
            None,
            Some(0)
        )
    );
    assert_ne!(attempts[1].0, root);
    assert_eq!(attempts[1].1, root);
    assert_eq!(attempts[1].2, 2);
    assert_eq!(attempts[1].3, second_price);
    assert_eq!(attempts[1].4, "succeeded");
    assert_eq!(attempts[1].5, "embeddings");
    assert_eq!(attempts[1].6, Some(14));
    assert_eq!(attempts[1].7, Some(0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM monetary_ledger WHERE kind='hold'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
}

fn rerank_body() -> Value {
    json!({"model":"company/smart","query":"cat","documents":["kitten","airplane","dog"],"top_n":2})
}
fn systemone_body() -> Value {
    json!({"model":"company/smart","state":"hello there","questions":{"is_q":{"type":"noul","instructions":"Is the text a greeting?"}}})
}
async fn workload_model(pool: &PgPool, protocol: &str) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY[$1]")
        .bind(protocol)
        .execute(pool)
        .await
        .unwrap();
}
async fn json_body(response: axum::response::Response) -> (StatusCode, Value, String) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    )
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rerank_reaches_http_with_v3_meter_settlement(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    workload_model(&pool, "rerank").await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let na = |m: &str| json!({"meter":m,"not_applicable":true});
    let lines = json!([
        {"meter":"input_tokens","microusd_per_batch":"1000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
        na("output_tokens"), na("cache_read_tokens"), na("cache_write_tokens"), na("cache_write_5m_tokens"),
        na("cache_write_1h_tokens"), na("output_images"), na("input_characters"), na("input_audio_seconds_ms"),
        na("output_audio_seconds_ms"),
        {"meter":"search_units","microusd_per_batch":"2000","batch":1,"unit_label":"/search","sku_label":"Search"},
        {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
    ]);
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,0,3,$3,'{\"search_units\":\"1\"}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(lines).execute(&pool).await.unwrap();
    // Hold: 1000 input tokens + one search unit.
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',3000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = app
        .clone()
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/rerank",
            rerank_body(),
        ))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let (status, body, text) = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        body,
        json!({"object":"list","id":id,"model":"company/smart",
            "results":[{"index":2,"relevance_score":0.75},{"index":0,"relevance_score":0.25}],
            "usage":{"total_tokens":25,"search_units":1}})
    );
    for leaked in ["private-upstream-id", "kitten", "document"] {
        assert!(!text.contains(leaked), "{leaked}");
    }
    let row: (String, String, i64, i64, i64, Option<i64>) = sqlx::query_as("SELECT e.workload_kind,r.state,r.held_microusd,r.actual_microusd,e.output_tokens,e.provider_cost_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1::uuid")
        .bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        ("rerank".into(), "settled".into(), 3000, 2025, 0, Some(7))
    );
    // 975 µUSD remain; the next 3000 hold is a non-retryable budget denial.
    let response = app
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/rerank",
            rerank_body(),
        ))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-should-retry"], "false");
    let (status, body, _) = json_body(response).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "budget_exceeded");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn systemone_speaks_the_typesafe_contract_over_http(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    workload_model(&pool, "systemone").await;
    let response = app
        .clone()
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/systemone",
            systemone_body(),
        ))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let (status, body, text) = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        body,
        json!({"id":id,"model":"company/smart","answers":{"is_q":{"type":"noul","noul":0.9566}},
            "usage":{"input_tokens":145,"output_tokens":0}})
    );
    let kind: String =
        sqlx::query_scalar("SELECT workload_kind FROM inference_executions WHERE id=$1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "systemone");
    // TypeSafe-compatible 422 validation; client routing fields are rejected.
    for bad in [
        json!({"model":"company/smart","state":"x","questions":{}}),
        json!({"model":"company/smart","state":"x","questions":{"a":{"type":"maybe","instructions":"?"}}}),
        json!({"model":"company/smart","state":"x","questions":{"a":{"type":"noul","instructions":"?"}},"provider":{"only":["x"]}}),
        json!("not an object"),
    ] {
        let (status, body, _) = json_body(
            app.clone()
                .oneshot(protocol_request(
                    &keys.personal_key.token,
                    "/v1/systemone",
                    bad,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "invalid_request_error");
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workload_routes_enforce_auth_catalog_key_and_protocol(pool: PgPool) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    workload_model(&pool, "rerank").await;
    for (path, body) in [
        ("/v1/rerank", rerank_body()),
        ("/v1/systemone", systemone_body()),
    ] {
        let unauthenticated = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(protocol_request(
                    "omg_live_not_a_real_key",
                    path,
                    body.clone()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    // A rerank-only model is not a System One model: explicit 400, no dispatch.
    let (status, body, _) = json_body(
        app.clone()
            .oneshot(protocol_request(
                &keys.personal_key.token,
                "/v1/systemone",
                systemone_body(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unsupported_capability");
    // Chat models are never selected for rerank either.
    workload_model(&pool, "chat_completions").await;
    let (status, body, _) = json_body(
        app.clone()
            .oneshot(protocol_request(
                &keys.personal_key.token,
                "/v1/rerank",
                rerank_body(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_capability"))
    );
    workload_model(&pool, "rerank").await;
    // Invalid rerank bodies are 400 before any routing.
    for bad in [
        json!({"model":"company/smart","query":"cat","documents":[]}),
        json!({"model":"company/smart","query":"cat","documents":["a"],"return_documents":true}),
        json!({"model":"company/smart","query":"cat","documents":["a"],"top_n":0}),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(protocol_request(
                    &keys.personal_key.token,
                    "/v1/rerank",
                    bad
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    // Workspace catalog denial.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body, _) = json_body(
        app.clone()
            .oneshot(protocol_request(
                &keys.team_key.token,
                "/v1/rerank",
                rerank_body(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("model_not_found"))
    );
    // Key model restriction without a selection.
    let governance: Uuid = sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
        .bind(keys.personal_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)")
        .bind(keys.personal_workspace_id)
        .bind(governance)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(protocol_request(
                &keys.personal_key.token,
                "/v1/rerank",
                rerank_body()
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workload_body_caps_are_configured_per_route(pool: PgPool) {
    let (_, keys, _, store) = fixture(&pool, false).await;
    workload_model(&pool, "rerank").await;
    let adapter = Arc::new(FixtureAdapter {
        calls: AtomicUsize::new(0),
        truncate: false,
        stream_drops: Arc::new(AtomicUsize::new(0)),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let mut limits = EngineLimits::default();
    limits.workloads.rerank_body_bytes = 4096;
    let engine = Engine::new(Arc::new(store.clone()), registry, limits).unwrap();
    let app = http::router_with_engine(store, None, engine);
    let mut big = rerank_body();
    big["documents"] = json!(["x".repeat(5000)]);
    let response = app
        .clone()
        .oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/rerank",
            big.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // A small rerank under the configured cap still succeeds.
    assert_eq!(
        app.oneshot(protocol_request(
            &keys.personal_key.token,
            "/v1/rerank",
            rerank_body()
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

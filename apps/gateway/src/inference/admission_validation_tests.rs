//! #228: a deterministic adapter refusal (Anthropic strict tools and
//! temperature, Bedrock strict tools and tool-call correlation) must never
//! durably admit an execution or reserve budget, and must never reach any
//! upstream call - on every authorized candidate, not only the first one
//! tried. Real `Store`/`Engine` on PostgreSQL, real Anthropic/Bedrock
//! adapters against mocked HTTP transports, driven end to end through the
//! `/v1/chat/completions` router (pattern: `governance::generation_defaults_tests`,
//! `providers::bedrock::tests`).
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::NewApiKey,
    governance::tests::db::{Fixture, fixture},
    inference::{Engine, EngineLimits, error::InferenceError},
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        bedrock::BedrockAdapter,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("test-secret-value".into())
    }
}

/// Background mock/gateway server task, aborted on drop so a failed
/// assertion never leaves a listener running past the test.
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn serve(app: Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, Server(task))
}

/// A mock that only counts requests. Any request at all fails a refusal
/// test's "zero upstream calls" assertion, regardless of what it returns.
fn counting_mock(hits: Arc<AtomicUsize>) -> Router {
    Router::new().fallback(move || {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "test: unexpected upstream call",
            )
                .into_response()
        }
    })
}

/// Counts of every durable row kind a deterministic refusal must never add
/// to, across admission, settlement and health observation.
async fn snapshot(pool: &PgPool) -> (i64, i64, i64, i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM inference_executions),
                (SELECT count(*) FROM governance_reservations),
                (SELECT count(*) FROM monetary_ledger),
                (SELECT count(*) FROM budget_totals),
                (SELECT count(*) FROM rate_minute_counters),
                (SELECT count(*) FROM inflight_counters),
                (SELECT count(*) FROM deployment_health)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A new, priced deployment of `provider` on `f.model`, ordered by
/// `created_at` so candidate order (query order) is deterministic and
/// controllable across test cases.
async fn add_deployment(
    f: &Fixture,
    provider: &str,
    credential_ref: &str,
    region: Option<&str>,
    upstream_model: &str,
    order: i64,
) -> Uuid {
    let connection = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,region,enabled) VALUES($1,$2,$3,$4,$5,true)")
        .bind(connection).bind(format!("{provider} test")).bind(provider).bind(credential_ref).bind(region)
        .execute(&f.store.pool).await.unwrap();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,created_at) VALUES($1,$2,$3,$4,true,clock_timestamp()+$5*interval '1 second')")
        .bind(deployment).bind(f.model).bind(connection).bind(upstream_model).bind(order)
        .execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,1000,1000,1)")
        .bind(Uuid::new_v4()).bind(deployment)
        .execute(&f.store.pool).await.unwrap();
    deployment
}

/// Issues a real API key for `f`'s workspace and points `f.principal` at it,
/// so HTTP calls can authenticate exactly as a shared-workspace holder would.
async fn issue_key(f: &mut Fixture) -> String {
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'admission validation fixture',$4)")
        .bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice())
        .execute(&f.store.pool).await.unwrap();
    f.principal.key_id = key.id;
    key.token
}

async fn start_gateway(f: &Fixture, registry: ProviderRegistry) -> (String, Server) {
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let app = crate::http::router_with_engine(f.store.clone(), None, engine);
    serve(app).await
}

fn strict_tool_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{"role":"user","content":"ping"}],
        "tools": [{"type":"function","function":{"name":"ping","parameters":{"type":"object","properties":{}},"strict":true}}],
        "stream": stream,
    })
}
fn high_temperature_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{"role":"user","content":"hi"}],
        "temperature": 1.5,
        "stream": stream,
    })
}
/// A tool-result message whose `tool_call_id` never appeared in any
/// preceding assistant tool call: a correlation failure only Bedrock's wire
/// locally rejects (Anthropic's wire has no local correlation check).
fn orphan_tool_result_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"ping","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_999","content":"orphan result"},
        ],
        "stream": stream,
    })
}

async fn post(base: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let payload: Value = response.json().await.unwrap();
    (status, payload)
}

/// One deterministic refusal, for both `stream: false` and `stream: true`
/// (rejected before any SSE is ever established either way): exact status
/// and error code, zero upstream calls, and not one new row anywhere.
async fn assert_refused(
    base: &str,
    token: &str,
    pool: &PgPool,
    hits: &Arc<AtomicUsize>,
    body: impl Fn(bool) -> Value,
    status: StatusCode,
    code: &str,
) {
    for stream in [false, true] {
        let before = snapshot(pool).await;
        let (got_status, payload) = post(base, token, &body(stream)).await;
        assert_eq!(got_status, status, "stream={stream}: {payload}");
        assert_eq!(payload["error"]["code"], code, "stream={stream}: {payload}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "stream={stream}: upstream was called"
        );
        assert_eq!(
            snapshot(pool).await,
            before,
            "stream={stream}: durable rows changed"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn anthropic_strict_tool_refusal_is_zero_write_and_zero_upstream_call(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    add_deployment(
        &f,
        "anthropic",
        "env:ANTHROPIC_NOT_RESOLVED",
        None,
        "claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (upstream_base, _upstream) = serve(counting_mock(hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{upstream_base}/v1"),
        )))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    assert_refused(
        &base,
        &token,
        &f.store.pool,
        &hits,
        |stream| strict_tool_body("company/smart", stream),
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_capability",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn anthropic_temperature_above_range_refusal_is_zero_write_and_zero_upstream_call(
    pool: PgPool,
) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    add_deployment(
        &f,
        "anthropic",
        "env:ANTHROPIC_NOT_RESOLVED",
        None,
        "claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (upstream_base, _upstream) = serve(counting_mock(hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{upstream_base}/v1"),
        )))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    assert_refused(
        &base,
        &token,
        &f.store.pool,
        &hits,
        |stream| high_temperature_body("company/smart", stream),
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_capability",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_strict_tool_refusal_is_zero_write_and_zero_upstream_call(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (upstream_base, _upstream) = serve(counting_mock(hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&upstream_base)))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    assert_refused(
        &base,
        &token,
        &f.store.pool,
        &hits,
        |stream| strict_tool_body("company/smart", stream),
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_capability",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_invalid_tool_correlation_refusal_is_zero_write_and_zero_upstream_call(
    pool: PgPool,
) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (upstream_base, _upstream) = serve(counting_mock(hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&upstream_base)))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    assert_refused(
        &base,
        &token,
        &f.store.pool,
        &hits,
        |stream| orphan_tool_result_body("company/smart", stream),
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
    )
    .await;
}

fn openai_success_mock(hits: Arc<AtomicUsize>) -> Router {
    Router::new().fallback(move || {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(json!({"id":"c1","object":"chat.completion","created":1,"model":"m",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string()))
                .unwrap()
        }
    })
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mixed_routes_refused_candidate_first_then_valid_alternative_succeeds(pool: PgPool) {
    mixed_routes_refused_and_valid_with(pool, true).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mixed_routes_valid_candidate_first_then_refused_alternative_still_succeeds(pool: PgPool) {
    mixed_routes_refused_and_valid_with(pool, false).await;
}
async fn mixed_routes_refused_and_valid_with(pool: PgPool, anthropic_first: bool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    // The fixture's own default deployment (provider "openai") would
    // otherwise be an extra, earlier-created candidate for the same
    // registered OpenAI adapter and mock, confounding which route this
    // test actually exercises.
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(f.deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (anthropic_order, openai_order) = if anthropic_first { (0, 1) } else { (1, 0) };
    add_deployment(
        &f,
        "anthropic",
        "env:ANTHROPIC_NOT_RESOLVED",
        None,
        "claude-test",
        anthropic_order,
    )
    .await;
    add_deployment(
        &f,
        "openai",
        "env:OPENAI_NOT_RESOLVED",
        None,
        "gpt-test",
        openai_order,
    )
    .await;
    let anthropic_hits = Arc::new(AtomicUsize::new(0));
    let openai_hits = Arc::new(AtomicUsize::new(0));
    let (anthropic_base, _anthropic_upstream) = serve(counting_mock(anthropic_hits.clone())).await;
    let (openai_base, _openai_upstream) = serve(openai_success_mock(openai_hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{anthropic_base}/v1"),
        )))
        .unwrap();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{openai_base}/v1"),
        )))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let before = snapshot(&f.store.pool).await;
    let (status, payload) = post(&base, &token, &strict_tool_body("company/smart", false)).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(payload["choices"][0]["message"]["content"], "ok");
    assert_eq!(
        anthropic_hits.load(Ordering::SeqCst),
        0,
        "refused candidate was called"
    );
    assert_eq!(openai_hits.load(Ordering::SeqCst), 1);
    let after = snapshot(&f.store.pool).await;
    assert_eq!(
        after.0,
        before.0 + 1,
        "exactly one execution for the one dispatched attempt"
    );
    assert_eq!(
        after.1,
        before.1 + 1,
        "exactly one reservation for the one dispatched attempt"
    );
}

/// Both candidates refuse the same request, with different error kinds
/// (Anthropic: Unsupported for an out-of-range temperature; Bedrock:
/// InvalidRequest for the same temperature, which is also outside its own
/// 0..=1 range): the overall result must be `InvalidRequest`, regardless of
/// which candidate the catalog returns first.
async fn both_refusing_mixed_kinds_with(pool: PgPool, anthropic_first: bool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    let (anthropic_order, bedrock_order) = if anthropic_first { (0, 1) } else { (1, 0) };
    add_deployment(
        &f,
        "anthropic",
        "env:ANTHROPIC_NOT_RESOLVED",
        None,
        "claude-test",
        anthropic_order,
    )
    .await;
    add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        bedrock_order,
    )
    .await;
    let anthropic_hits = Arc::new(AtomicUsize::new(0));
    let bedrock_hits = Arc::new(AtomicUsize::new(0));
    let (anthropic_base, _anthropic_upstream) = serve(counting_mock(anthropic_hits.clone())).await;
    let (bedrock_base, _bedrock_upstream) = serve(counting_mock(bedrock_hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{anthropic_base}/v1"),
        )))
        .unwrap();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&bedrock_base)))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let before = snapshot(&f.store.pool).await;
    let (status, payload) = post(
        &base,
        &token,
        &high_temperature_body("company/smart", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
    assert_eq!(
        payload["error"]["code"], "invalid_request_error",
        "{payload}"
    );
    assert_eq!(anthropic_hits.load(Ordering::SeqCst), 0);
    assert_eq!(bedrock_hits.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot(&f.store.pool).await, before);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn both_refusing_mixed_error_kinds_is_invalid_request_anthropic_first(pool: PgPool) {
    both_refusing_mixed_kinds_with(pool, true).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn both_refusing_mixed_error_kinds_is_invalid_request_bedrock_first(pool: PgPool) {
    both_refusing_mixed_kinds_with(pool, false).await;
}

/// A valid request on a single real-adapter route still creates exactly one
/// reservation per attempt, already pending when the upstream call is made
/// (dispatch happens strictly after durable admission).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn positive_control_anthropic_creates_one_reservation_before_dispatch(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    let deployment = add_deployment(
        &f,
        "anthropic",
        "env:ANTHROPIC_NOT_RESOLVED",
        None,
        "claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let mock_hits = hits.clone();
    let db_pool = f.store.pool.clone();
    let app = Router::new().fallback(move || {
        let hits = mock_hits.clone();
        let db_pool = db_pool.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let pending: (Uuid, i64) = sqlx::query_as(
                "SELECT deployment_id,reserved_tokens FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1",
            )
            .fetch_one(&db_pool)
            .await
            .unwrap();
            assert!(pending.1 > 0, "reservation must already hold tokens before dispatch");
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id":"msg_1","type":"message","role":"assistant","model":"claude-test",
                    "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
                    "usage":{"input_tokens":1,"output_tokens":1}
                }).to_string()))
                .unwrap()
        }
    });
    let (upstream_base, _upstream) = serve(app).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{upstream_base}/v1"),
        )))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let before = snapshot(&f.store.pool).await;
    let (status, payload) = post(
        &base,
        &token,
        &json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],"stream":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let after = snapshot(&f.store.pool).await;
    assert_eq!(after.0, before.0 + 1);
    assert_eq!(after.1, before.1 + 1);
    let reservations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE deployment_id=$1")
            .bind(deployment)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn positive_control_bedrock_creates_one_reservation_before_dispatch(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    let deployment = add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let mock_hits = hits.clone();
    let db_pool = f.store.pool.clone();
    let app = Router::new().fallback(move || {
        let hits = mock_hits.clone();
        let db_pool = db_pool.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let pending: (Uuid, i64) = sqlx::query_as(
                "SELECT deployment_id,reserved_tokens FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1",
            )
            .fetch_one(&db_pool)
            .await
            .unwrap();
            assert!(pending.1 > 0, "reservation must already hold tokens before dispatch");
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},
                    "stopReason":"end_turn",
                    "usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}
                }).to_string()))
                .unwrap()
        }
    });
    let (upstream_base, _upstream) = serve(app).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&upstream_base)))
        .unwrap();
    let (base, _gateway) = start_gateway(&f, registry).await;
    let before = snapshot(&f.store.pool).await;
    let (status, payload) = post(
        &base,
        &token,
        &json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],"stream":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let after = snapshot(&f.store.pool).await;
    assert_eq!(after.0, before.0 + 1);
    assert_eq!(after.1, before.1 + 1);
    let reservations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE deployment_id=$1")
            .bind(deployment)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 1);
}

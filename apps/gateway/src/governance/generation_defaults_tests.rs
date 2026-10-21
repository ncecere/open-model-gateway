//! Optional OpenAI maxima: real admission/storage and recording adapters.
use super::{tests::db::*, *};
use crate::{
    inference::{Engine, EngineLimits, repository::InferenceRepository, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_running_gateway_records_real_adapter_wire_and_refusals(
    pool: sqlx::PgPool,
) {
    use crate::providers::{
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    };
    use axum::{
        Json, Router,
        body::Body,
        http::{HeaderMap, Response, Uri},
    };
    struct Resolver;
    impl SecretResolver for Resolver {
        fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
            Secret::new("local-recording-mock".into())
        }
    }
    struct Servers(Vec<tokio::task::JoinHandle<()>>);
    impl Drop for Servers {
        fn drop(&mut self) {
            for task in &self.0 {
                task.abort();
            }
        }
    }
    let mut servers = Servers(vec![]);
    let mut f = fixture(pool).await;
    let price = f.price(1_000_000).await;
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'wire fixture',$4)").bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    f.principal.key_id = key.id;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','responses']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let pool = f.store.pool.clone();
    let mock = Router::new().fallback(move |headers: HeaderMap, uri: Uri, Json(body): Json<serde_json::Value>| {
        let pool = pool.clone(); let capture = capture.clone();
        async move {
            assert_eq!(headers["authorization"],"Bearer local-recording-mock");
            let responses = uri.path() == "/v1/responses";
            let cap = body[if responses { "max_output_tokens" } else { "max_completion_tokens" }].as_i64().unwrap();
            assert_eq!(cap,50);
            let hold: (Uuid,i64,i64) = sqlx::query_as("SELECT price_id,reserved_tokens,held_microusd FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
            assert_eq!(hold,(price,150,150));
            capture.lock().unwrap().push(body.clone());
            let (content_type, payload) = if responses {
                (if body["stream"] == true { "text/event-stream" } else { "application/json" }, native_response_fixture(body["stream"] == true))
            } else if body["stream"] == true {
                ("text/event-stream", "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n".to_owned())
            } else {
                ("application/json",serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string())
            };
            Response::builder().header("content-type",content_type).body(Body::from(payload)).unwrap()
        }
    });
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_base = format!("http://{}/v1", upstream.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(upstream, mock).await.unwrap();
    }));
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            upstream_base,
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_base = format!("http://{}", gateway.local_addr().unwrap());
    let app = crate::http::router_with_engine(f.store.clone(), None, engine);
    servers.0.push(tokio::spawn(async move {
        axum::serve(gateway, app).await.unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            let mut body = if path.ends_with("responses") {
                serde_json::json!({"model":"company/smart","input":"hello"})
            } else {
                serde_json::json!({"model":"company/smart","messages":[{"role":"user","content":"hello"}]})
            };
            body["stream"] = serde_json::json!(stream);
            let response = client
                .post(format!("{gateway_base}{path}"))
                .bearer_auth(&key.token)
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let payload = response.text().await.unwrap();
            assert_eq!(status, 200, "{path} stream={stream}: {payload}");
            assert!(payload.contains("ok"), "{payload}");
            for cap in [0, 51] {
                body[if path.ends_with("responses") {
                    "max_output_tokens"
                } else {
                    "max_completion_tokens"
                }] = serde_json::json!(cap);
                let response = client
                    .post(format!("{gateway_base}{path}"))
                    .bearer_auth(&key.token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 400);
            }
        }
    }
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
        4
    );
}

fn native_response_fixture(stream: bool) -> String {
    use serde_json::json;
    let complete = json!({"id":"private","object":"response","model":"mock-model","status":"completed","output":[{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1}});
    if !stream {
        return complete.to_string();
    }
    [
        json!({"type":"response.created","response":{"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","content":[],"status":"in_progress"}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":"ok"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"m","content_index":0,"text":"ok"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"ok","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":complete["output"][0]}),
        json!({"type":"response.completed","response":complete}),
    ].into_iter().map(|v| format!("event: {}\ndata: {v}\n\n",v["type"].as_str().unwrap())).collect()
}

struct Recording {
    pool: sqlx::PgPool,
    limit: u32,
    fail: Option<Uuid>,
    seen: Mutex<Vec<Option<u32>>>,
}
#[async_trait]
impl ProviderAdapter for Recording {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, _: ApiProtocol) -> bool {
        true
    }
    fn generation_output_limit(&self) -> u32 {
        self.limit
    }
    async fn execute(
        &self,
        target: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        self.seen.lock().unwrap().push(request.max_output_tokens);
        // This is the dispatch boundary: a pending durable reservation and its
        // hold must already match exactly the maximum sent to the provider.
        let (price, tokens, hold): (Option<Uuid>, Option<i64>, Option<i64>) = sqlx::query_as("SELECT price_id,reserved_tokens,held_microusd FROM governance_reservations WHERE deployment_id=$1 AND state='pending' ORDER BY admitted_at DESC LIMIT 1")
            .bind(target.id).fetch_one(&self.pool).await.unwrap();
        if let Some(price) = price {
            let (input, ceiling): (i64, i64) = sqlx::query_as(
                "SELECT input_token_limit,output_token_limit FROM deployment_prices WHERE id=$1",
            )
            .bind(price)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            let output = i64::from(
                request
                    .max_output_tokens
                    .expect("priced generation is bounded"),
            );
            assert!(output > 0 && output <= ceiling);
            assert_eq!(tokens, Some(input + output));
            assert_eq!(hold, tokens); // fixtures use $1/M for both token meters
        } else {
            assert_eq!((tokens, hold), (None, None));
        }
        if self.fail == Some(target.id) {
            return Err(InferenceError::UpstreamUnavailable);
        }
        let usage = Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..Default::default()
        };
        Ok(if request.stream {
            ProviderOutput::Stream(Box::pin(futures_util::stream::iter([
                Ok(ChatEvent::Finish(FinishReason::Stop)),
                Ok(ChatEvent::Usage(usage)),
                Ok(ChatEvent::Done),
            ])))
        } else {
            ProviderOutput::Complete(ChatResponse {
                content: Some("ok".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            })
        })
    }
}
fn engine(f: &Fixture, limit: u32, fail: Option<Uuid>) -> (Engine, Arc<Recording>) {
    let adapter = Arc::new(Recording {
        pool: f.store.pool.clone(),
        limit,
        fail,
        seen: Mutex::default(),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    (
        Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap(),
        adapter,
    )
}
async fn execute(
    f: &Fixture,
    engine: &Engine,
    max: Option<u32>,
    stream: bool,
    protocol: ApiProtocol,
) -> Result<(), InferenceError> {
    let mut r = request();
    r.max_output_tokens = max;
    r.stream = stream;
    match engine
        .execute_protocol(f.principal, r, Uuid::now_v7(), protocol)
        .await?
    {
        ProviderOutput::Complete(_) => {}
        ProviderOutput::Stream(mut s) => {
            while let Some(event) = s.next().await {
                event?;
            }
        }
    }
    Ok(())
}
async fn counts(f: &Fixture) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM monetary_ledger)")
        .fetch_one(&f.store.pool).await.unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_dispatch_chat_responses_and_explicit_limits(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    let price = f.price(1_000_000).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','responses']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (e, a) = engine(&f, u32::MAX, None);
    for p in [ApiProtocol::ChatCompletions, ApiProtocol::Responses] {
        for stream in [false, true] {
            execute(&f, &e, None, stream, p).await.unwrap();
            execute(&f, &e, Some(7), stream, p).await.unwrap();
        }
    }
    assert_eq!(
        *a.seen.lock().unwrap(),
        [
            Some(50),
            Some(7),
            Some(50),
            Some(7),
            Some(50),
            Some(7),
            Some(50),
            Some(7)
        ]
    );
    let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as("SELECT r.price_id,r.reserved_tokens,l.amount_microusd FROM governance_reservations r JOIN monetary_ledger l ON l.execution_id=r.execution_id WHERE l.kind='hold' ORDER BY r.admitted_at")
        .fetch_all(&f.store.pool).await.unwrap();
    assert_eq!(rows.len(), 8);
    for (i, row) in rows.iter().enumerate() {
        let bound = if i % 2 == 0 { 150 } else { 107 };
        assert_eq!(*row, (price, bound, bound));
    }
    let before = counts(&f).await;
    for max in [0, 51, u32::MAX] {
        assert_eq!(
            execute(&f, &e, Some(max), false, ApiProtocol::ChatCompletions).await,
            Err(InferenceError::InvalidRequest)
        );
    }
    assert_eq!(counts(&f).await, before);
    assert_eq!(a.seen.lock().unwrap().len(), 8);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_wire_and_invalid_server_defaults_write_nothing(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    let (e, a) = engine(&f, i32::MAX as u32, None);
    for ceiling in [0, i64::from(i32::MAX) + 1, i64::from(u32::MAX) + 1] {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,$3,1)")
            .bind(Uuid::now_v7()).bind(f.deployment).bind(ceiling).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            execute(&f, &e, None, false, ApiProtocol::ChatCompletions).await,
            Err(InferenceError::Configuration)
        );
    }
    assert_eq!(
        execute(
            &f,
            &e,
            Some(i32::MAX as u32 + 1),
            false,
            ApiProtocol::ChatCompletions
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(counts(&f).await, (0, 0, 0));
    assert!(a.seen.lock().unwrap().is_empty());
    execute(&f, &e, Some(7), false, ApiProtocol::ChatCompletions)
        .await
        .unwrap();
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_keep_unpriced_omissions_and_existing_limit_refusals(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool).await;
    let (e, a) = engine(&f, u32::MAX, None);
    execute(&f, &e, None, false, ApiProtocol::ChatCompletions)
        .await
        .unwrap();
    assert_eq!(*a.seen.lock().unwrap(), [None]);
    f.policy(
        "workspace_platform_policy_overrides",
        None,
        Some(1000),
        None,
        None,
    )
    .await;
    let before = counts(&f).await;
    assert_eq!(
        execute(&f, &e, None, false, ApiProtocol::ChatCompletions).await,
        Err(InferenceError::Configuration)
    );
    assert_eq!(counts(&f).await, before);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_compose_tpm_and_budget_without_clamping(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let (e, a) = engine(&f, u32::MAX, None);
    f.policy(
        "workspace_platform_policy_overrides",
        None,
        Some(149),
        None,
        None,
    )
    .await;
    assert_eq!(
        execute(&f, &e, None, false, ApiProtocol::ChatCompletions).await,
        Err(InferenceError::TokenReservationExceedsLimit(
            LimitScope::Workspace
        ))
    );
    f.policy(
        "workspace_platform_policy_overrides",
        None,
        Some(1000),
        None,
        None,
    )
    .await;
    sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,tokens_per_minute) VALUES($1,$2,149)").bind(f.principal.workspace_id).bind(f.principal.key_id).execute(&f.store.pool).await.unwrap();
    assert_eq!(
        execute(&f, &e, None, false, ApiProtocol::ChatCompletions).await,
        Err(InferenceError::TokenReservationExceedsLimit(
            LimitScope::ApiKey
        ))
    );
    // Remove the impossible key-TPM condition before exercising the budget;
    // otherwise that existing configuration denial correctly wins first.
    sqlx::query("UPDATE key_policies SET tokens_per_minute=1000 WHERE governance_key_id=$1")
        .bind(f.principal.key_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    f.policy(
        "workspace_platform_policy_overrides",
        None,
        Some(1000),
        None,
        Some(149),
    )
    .await;
    assert_eq!(
        execute(&f, &e, None, false, ApiProtocol::ChatCompletions).await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    assert_eq!(counts(&f).await, (0, 0, 0));
    assert!(a.seen.lock().unwrap().is_empty());
    execute(&f, &e, Some(7), false, ApiProtocol::ChatCompletions)
        .await
        .unwrap();
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_failover_uses_each_attempt_price_not_prior_cap(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    let first = f.price(1_000_000).await;
    let second = Uuid::now_v7();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,'second',true FROM deployments WHERE id=$2")
        .bind(second).bind(f.deployment).execute(&f.store.pool).await.unwrap();
    let second_price = Uuid::now_v7();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,23,1)").bind(second_price).bind(second).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,true)")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in [(f.deployment, 0), (second, 1)] {
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    let (e, a) = engine(&f, u32::MAX, Some(f.deployment));
    execute(&f, &e, None, false, ApiProtocol::ChatCompletions)
        .await
        .unwrap();
    assert_eq!(*a.seen.lock().unwrap(), [Some(50), Some(23)]);
    let pins: Vec<Uuid> =
        sqlx::query_scalar("SELECT price_id FROM governance_reservations ORDER BY admitted_at")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(pins, [first, second_price]);
}
async fn publication_barrier(pool: sqlx::PgPool, mode: locks::AdmissionMode) {
    let mut f = fixture(pool).await;
    f.store = f.store.with_admission_mode(mode);
    let old = f.price(1_000_000).await;
    let d = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let mut blocker = f.store.pool.begin().await.unwrap();
    match mode {
        locks::AdmissionMode::Scoped => locks::exclusive(
            &mut blocker,
            [locks::Scope::Workspace(f.principal.workspace_id)],
        )
        .await
        .unwrap(),
        locks::AdmissionMode::Global => {
            sqlx::query("SELECT id FROM installation FOR NO KEY UPDATE")
                .execute(&mut *blocker)
                .await
                .unwrap();
        }
    }
    let s = f.start();
    let mut r = request();
    r.max_output_tokens = None;
    let admitted = admit_generation(&f.store, &s, &r, 30, &d, u32::MAX);
    tokio::pin!(admitted);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut admitted)
            .await
            .is_err()
    );
    // Admission has the shared catalog lock even while authority is blocked.
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_xact_lock(72419502)")
            .fetch_one(&mut *blocker)
            .await
            .unwrap()
    );
    let new = Uuid::now_v7();
    let writer = sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,23,1)").bind(new).bind(f.deployment).execute(&f.store.pool);
    tokio::pin!(writer);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut writer)
            .await
            .is_err()
    );
    blocker.commit().await.unwrap();
    assert_eq!(admitted.await.unwrap().max_output_tokens, Some(50));
    writer.await.unwrap();
    let pin: (Uuid,i64,i64) = sqlx::query_as("SELECT price_id,reserved_tokens,held_microusd FROM governance_reservations WHERE execution_id=$1").bind(s.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(pin, (old, 150, 150));
    let next = f.start();
    assert_eq!(
        admit_generation(&f.store, &next, &r, 30, &d, u32::MAX)
            .await
            .unwrap()
            .max_output_tokens,
        Some(23)
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT price_id FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(next.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        new
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_publication_barrier_scoped(pool: sqlx::PgPool) {
    publication_barrier(pool, locks::AdmissionMode::Scoped).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_publication_barrier_global(pool: sqlx::PgPool) {
    publication_barrier(pool, locks::AdmissionMode::Global).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_unknown_price_keeps_unknown_hold_and_budget_refusal(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool).await;
    // This base represents unknown v3 valuation by an absent meter line.
    let lines: Vec<_> = crate::billing::v3::Meter::ALL
        .into_iter()
        .filter(|meter| *meter != crate::billing::v3::Meter::OutputTokens)
        .map(|meter| serde_json::json!({"meter":meter.as_str(),"not_applicable":true}))
        .collect();
    let price = Uuid::now_v7();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,'{}')")
        .bind(price).bind(f.deployment).bind(serde_json::json!(lines)).execute(&f.store.pool).await.unwrap();
    let d = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let mut r = request();
    r.max_output_tokens = None;
    let s = f.start();
    assert_eq!(
        admit_generation(&f.store, &s, &r, 30, &d, u32::MAX)
            .await
            .unwrap()
            .max_output_tokens,
        Some(50)
    );
    let row: (Uuid,i64,Option<i64>,bool) = sqlx::query_as("SELECT price_id,reserved_tokens,held_microusd,unbounded_cost FROM governance_reservations WHERE execution_id=$1").bind(s.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row, (price, 150, None, true));
    f.policy("workspace_local_policies", None, None, None, Some(1000))
        .await;
    let before = counts(&f).await;
    assert_eq!(
        admit_generation(&f.store, &f.start(), &r, 30, &d, u32::MAX).await,
        Err(InferenceError::PriceUnbounded)
    );
    assert_eq!(counts(&f).await, before);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_defaults_cancellation_and_expired_lease_keep_exact_holds(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let (e, a) = engine(&f, u32::MAX, None);
    let mut r = request();
    r.max_output_tokens = None;
    r.stream = true;
    let id = Uuid::now_v7();
    let output = e.execute(f.principal, r.clone(), id).await.unwrap();
    assert_eq!(*a.seen.lock().unwrap(), [Some(50)]);
    drop(output); // Before first poll: the guard must already own the hold.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let state: String = sqlx::query_scalar(
                "SELECT state FROM governance_reservations WHERE execution_id=$1",
            )
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
            if state == "unknown" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let d = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let lease = f.start();
    admit_generation(&f.store, &lease, &r, 1, &d, u32::MAX)
        .await
        .unwrap();
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE execution_id=$1").bind(lease.id).execute(&f.store.pool).await.unwrap();
    assert_eq!(reconcile_expired(&f.store, 100).await.unwrap(), 1);
    let rows: Vec<(String,i64,i64)> = sqlx::query_as("SELECT state,reserved_tokens,held_microusd FROM governance_reservations ORDER BY admitted_at").fetch_all(&f.store.pool).await.unwrap();
    assert_eq!(
        rows,
        [("unknown".into(), 150, 150), ("unknown".into(), 150, 150)]
    );
}

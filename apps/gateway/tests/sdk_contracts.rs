#![cfg(feature = "integration-tests")]
use async_trait::async_trait;
use open_model_gateway::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use std::sync::{Arc, Mutex};

struct Fixture(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ProviderAdapter for Fixture {
    fn id(&self) -> &'static str {
        "sdk_fixture"
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
    async fn execute(
        &self,
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        // Omitted optional client maxima must arrive bounded at the adapter.
        assert!(request.max_output_tokens.is_some());
        let mut text = match request.max_output_tokens {
            Some(7) => "Hello from fixture (max 7)",
            Some(100 | 32) => "Hello from fixture",
            other => panic!("unexpected SDK dispatch cap: {other:?}"),
        };
        let mut calls = vec![];
        if request
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Tool)
        {
            text = "Tool result received";
        } else if request.messages[0].content.as_deref() == Some("call ping") {
            text = "Calling ping";
            calls.push(ToolCall {
                id: "ping-call".into(),
                name: "ping".into(),
                arguments: "{}".into(),
            });
        } else if request.messages[0].content.as_deref() == Some("call weather") {
            text = "Checking weather";
            calls.push(ToolCall {
                id: "semantic-call".into(),
                name: "weather".into(),
                arguments: "{ \"city\": \"東京\" }".into(),
            });
        }
        let finish_reason = if calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        self.0.lock().unwrap().push(request.clone());
        let usage = Usage {
            input_tokens: Some(3),
            output_tokens: Some(4),
            billing: None,
            ..Default::default()
        };
        if request.stream {
            Ok(ProviderOutput::Stream(Box::pin(
                futures_util::stream::iter(vec![
                    Ok(ChatEvent::Delta {
                        text: Some(text.into()),
                        tool_calls: calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| ToolCallDelta {
                                index: index as u32,
                                id: Some(call.id.clone()),
                                name: Some(call.name.clone()),
                                arguments: Some(call.arguments.clone()),
                            })
                            .collect(),
                    }),
                    Ok(ChatEvent::Finish(finish_reason)),
                    Ok(ChatEvent::Usage(usage)),
                    Ok(ChatEvent::Done),
                ]),
            )))
        } else {
            Ok(ProviderOutput::Complete(ChatResponse {
                messages_content: None,
                content: Some(text.into()),
                tool_calls: calls,
                finish_reason,
                usage,
            }))
        }
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn real_client_sdks_accept_all_three_protocols(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='sdk_fixture',enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Exercise ordinary SDK calls against priced generation, not an unpriced
    // route which bypasses the output reservation arithmetic.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,1000,100,1 FROM deployments")
        .execute(&pool).await.unwrap();
    // A priced model for the Batch API contract (batches must be bounded).
    sqlx::raw_sql("INSERT INTO models(id,public_name,supported_protocols) SELECT gen_random_uuid(),'company/batch',ARRAY['chat_completions']; INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT gen_random_uuid(),m.id,d.provider_connection_id,d.upstream_model,true FROM models m CROSS JOIN (SELECT provider_connection_id,upstream_model FROM deployments LIMIT 1) d WHERE m.public_name='company/batch'; INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT w.id,m.id,'direct' FROM workspaces w CROSS JOIN models m WHERE m.public_name='company/batch'; INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),d.id,1000000,1000000,1000,100,1 FROM deployments d JOIN models m ON m.id=d.model_id WHERE m.public_name='company/batch';")
        .execute(&pool)
        .await
        .unwrap();
    let recorded = Arc::new(Mutex::new(vec![]));
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Fixture(recorded.clone())))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        // Files API on an encrypted in-memory store (purposes allowed below).
        let files = open_model_gateway::filestore::FileStoreRuntime::memory();
        axum::serve(
            listener,
            http::router_with_engine(store, None, engine).layer(axum::Extension(files)),
        )
        .await
        .unwrap();
    });
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/sdk-contracts.mjs");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(script)
            .env("SDK_TEST_BASE_URL", format!("http://{address}"))
            .env("SDK_TEST_API_KEY", &keys.personal_key.token)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    server.abort();
    let output = result
        .expect("SDK contract deadline")
        .expect("node and npm ci are required for SDK contracts");
    assert!(
        output.status.success(),
        "SDK contracts failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Thirty-three priced calls, including beta/cache/thinking SDK grammar.
    // Malformed/stateful/error-marked followups leave no attempts or dispatch.
    let recorded = recorded.lock().unwrap().clone();
    assert_eq!(recorded.len(), 33);
    for r in &recorded[31..] {
        let options = r.messages_options.as_ref().unwrap();
        assert_eq!(
            options.beta_headers,
            ["claude-code-20250219, extended-cache-ttl-2025-04-11, effort-2025-11-24"]
        );
        assert_eq!(options.thinking.as_ref().unwrap()["type"], "adaptive");
        assert_eq!(options.cache_controls[0]["ttl"], "1h");
    }
    let no_arguments =
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false});
    for (i, r) in recorded.iter().enumerate().take(28).skip(18) {
        assert_eq!(r.tools[0].parameters, no_arguments, "request {i}");
        assert_eq!(r.tools[0].strict, None, "omission is not strict");
        assert_eq!(r.max_output_tokens, Some(if i < 22 { 100 } else { 32 }));
    }
    for i in [19, 21, 23, 24, 26, 27] {
        let r = &recorded[i];
        let call = r
            .messages
            .iter()
            .flat_map(|m| &m.tool_calls)
            .next()
            .unwrap();
        assert_eq!(
            (&*call.id, &*call.name, &*call.arguments),
            ("ping-call", "ping", "{}")
        );
        let result = r.messages.last().unwrap();
        assert!(result.role == Role::Tool);
        assert_eq!(result.tool_call_id.as_deref(), Some("ping-call"));
        assert_eq!(result.content.as_deref(), Some(""));
    }
    let explicit_schema = serde_json::json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false});
    for (i, strict) in [(28, false), (29, true)] {
        assert_eq!(recorded[i].tools[0].parameters, explicit_schema);
        assert_eq!(recorded[i].tools[0].strict, Some(strict));
    }
    assert_eq!(
        recorded[30].messages.last().unwrap().content.as_deref(),
        Some("ok")
    );
    for (index, streaming) in [
        (10, false),
        (11, true),
        (12, false),
        (13, false),
        (14, true),
        (15, true),
    ] {
        assert_eq!(recorded[index].stream, streaming);
        assert_eq!(recorded[index].max_output_tokens, Some(100));
    }
    for index in [10, 11] {
        let r = &recorded[index];
        assert_eq!(r.messages.len(), 3);
        assert!(r.messages[0].role == Role::User);
        assert_eq!(r.messages[0].content.as_deref(), Some("hello"));
        assert!(r.messages[1].role == Role::Assistant);
        assert_eq!(r.messages[1].content.as_deref(), Some("Hello from fixture"));
        assert_eq!(r.messages[2].content.as_deref(), Some("continue"));
    }
    for index in [13, 15] {
        let r = &recorded[index];
        assert_eq!(r.messages.len(), 4);
        assert_eq!(r.messages[0].content.as_deref(), Some("call weather"));
        assert!(r.messages[1].role == Role::Assistant);
        assert_eq!(r.messages[1].content.as_deref(), Some("Checking weather"));
        assert!(r.messages[2].role == Role::Assistant);
        let call = &r.messages[2].tool_calls[0];
        assert_eq!(call.id, "semantic-call");
        assert_eq!(call.name, "weather");
        assert_eq!(call.arguments, "{ \"city\": \"東京\" }");
        assert!(r.messages[3].role == Role::Tool);
        assert_eq!(r.messages[3].tool_call_id.as_deref(), Some("semantic-call"));
        assert_eq!(r.messages[3].content.as_deref(), Some("sun"));
    }
    let attempts: (i64, i64) = sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE state='succeeded') FROM inference_executions WHERE workload_kind='generation'").fetch_one(&pool).await.unwrap();
    assert_eq!(attempts, (33, 33));
    let holds: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT r.reserved_tokens,l.amount_microusd,p.output_token_limit FROM governance_reservations r JOIN deployment_prices p ON p.id=r.price_id JOIN monetary_ledger l ON l.execution_id=r.execution_id JOIN inference_executions e ON e.id=r.execution_id WHERE l.kind='hold' AND e.workload_kind='generation' ORDER BY r.admitted_at").fetch_all(&pool).await.unwrap();
    assert_eq!(holds.len(), 33);
    for (i, (tokens, amount, ceiling)) in holds.into_iter().enumerate() {
        let cap = [
            100, 100, 7, 7, 7, 7, 7, 100, 100, 100, 100, 100, 100, 100, 100, 100, 32, 32, 100, 100,
            100, 100, 32, 32, 32, 32, 32, 32, 100, 100, 32, 32, 32,
        ][i];
        assert_eq!((tokens, amount, ceiling), (1000 + cap, 1000 + cap, 100));
    }
    let settlements: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT r.actual_microusd,r.input_tokens,r.output_tokens FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.state='settled' AND e.workload_kind='generation'").fetch_all(&pool).await.unwrap();
    assert_eq!(settlements, vec![(7, 3, 4); 33]);
    let ledger: (i64, i64) = sqlx::query_as("SELECT count(*),sum(l.amount_microusd)::bigint FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id WHERE l.kind='settlement' AND e.workload_kind='generation'").fetch_one(&pool).await.unwrap();
    assert_eq!(ledger, (33, 33 * 7));
}

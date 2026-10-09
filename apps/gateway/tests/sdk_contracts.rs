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
use std::sync::Arc;

struct Fixture;
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
                        text: Some("Hello from fixture".into()),
                        tool_calls: vec![],
                    }),
                    Ok(ChatEvent::Finish(FinishReason::Stop)),
                    Ok(ChatEvent::Usage(usage)),
                    Ok(ChatEvent::Done),
                ]),
            )))
        } else {
            Ok(ProviderOutput::Complete(ChatResponse {
                content: Some("Hello from fixture".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
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
    // A priced model for the Batch API contract (batches must be bounded);
    // the chat contracts above keep using the unpriced company/smart.
    sqlx::raw_sql("INSERT INTO models(id,public_name,supported_protocols) SELECT gen_random_uuid(),'company/batch',ARRAY['chat_completions']; INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT gen_random_uuid(),m.id,d.provider_connection_id,d.upstream_model,true FROM models m CROSS JOIN (SELECT provider_connection_id,upstream_model FROM deployments LIMIT 1) d WHERE m.public_name='company/batch'; INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT w.id,m.id,'direct' FROM workspaces w CROSS JOIN models m WHERE m.public_name='company/batch'; INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),d.id,1000000,1000000,1000,100,1 FROM deployments d JOIN models m ON m.id=d.model_id WHERE m.public_name='company/batch';")
        .execute(&pool)
        .await
        .unwrap();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(Fixture)).unwrap();
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
            .env("SDK_TEST_API_KEY", keys.personal_key.token)
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
}

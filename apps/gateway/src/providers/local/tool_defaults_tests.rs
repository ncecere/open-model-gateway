//! Priced real HTTP and ordinary SDK proof, through the actual approved local adapter.
use super::*;
use crate::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits},
    store::Store,
};
use axum::{Json, Router, http::HeaderMap, response::IntoResponse};
use serde_json::{Value, json};
use std::sync::Mutex;

struct NoSecrets;
impl SecretResolver for NoSecrets {
    fn resolve(&self, _: &str) -> Result<super::super::secrets::Secret> {
        panic!("credential-free fixture must not resolve a secret")
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn priced_tool_defaults_sdk_through_real_local_http(pool: sqlx::PgPool) {
    let capture = Arc::new(Mutex::new(vec![]));
    let recorded = capture.clone();
    let mock = Router::new().route("/v1/chat/completions", axum::routing::post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let capture = capture.clone();
        async move {
            assert!(!headers.contains_key("authorization"), "inference key never crosses upstream");
            let result = body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
            let streaming = body["stream"] == true;
            capture.lock().unwrap().push(body);
            let usage = json!({"prompt_tokens":3,"completion_tokens":4});
            let calls = json!([{"id":"ping-call","type":"function","function":{"name":"ping","arguments":"{}"}}]);
            if streaming {
                let delta = if result { json!({"content":"done"}) } else {
                    let mut calls = calls;
                    calls[0]["index"] = json!(0);
                    json!({"tool_calls":calls})
                };
                let data = format!("data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":if result {"stop"} else {"tool_calls"}}]}),
                    json!({"choices":[],"usage":usage}));
                ([("content-type","text/event-stream")], data).into_response()
            } else {
                let message = if result { json!({"role":"assistant","content":"done"}) }
                    else { json!({"role":"assistant","tool_calls":calls}) };
                Json(json!({"choices":[{"index":0,"message":message,"finish_reason":if result {"stop"} else {"tool_calls"}}],"usage":usage})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='openai_compatible',endpoint=$1,credential_ref='none',enabled=true")
        .bind(&endpoint).execute(&pool).await.unwrap();
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,1000,100,1 FROM deployments")
        .execute(&pool).await.unwrap();
    let adapter = LocalAdapter::new(
        Profile::OpenAiCompatible,
        Arc::new(NoSecrets),
        Arc::new(ApprovedEndpoints::for_test(&endpoint)),
    );
    let mut registry = crate::providers::ProviderRegistry::default();
    registry.register(Arc::new(adapter)).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, http::router_with_engine(store, None, engine))
            .await
            .unwrap();
    });
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/tool-defaults-sdk.mjs"),
            )
            .env("SDK_TEST_BASE_URL", format!("http://{address}"))
            .env("SDK_TEST_API_KEY", &keys.team_key.token)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    // The same omission-positive request must still consult live workspace authority.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let denied = reqwest::Client::new().post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(&keys.team_key.token).json(&json!({"model":"company/smart","messages":[{"role":"user","content":"ping"}],"tools":[{"type":"function","function":{"name":"ping"}}]}))
        .send().await.unwrap();
    server.abort();
    mock_task.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND);
    let capture = recorded.lock().unwrap();
    assert_eq!(
        capture.len(),
        5,
        "strict, invalid and revoked authority never dispatch or retry"
    );
    for (i, body) in capture.iter().enumerate() {
        assert_eq!(
            body["max_tokens"], 100,
            "priced server cap, no caller maximum"
        );
        let function = &body["tools"][0]["function"];
        if i == 4 {
            assert_eq!(
                function["parameters"],
                json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]})
            );
            assert_eq!(function["strict"], false);
        } else {
            assert_eq!(
                function["parameters"],
                json!({"type":"object","properties":{},"additionalProperties":false})
            );
            assert!(function.get("strict").is_none());
        }
        if [1, 3].contains(&i) {
            assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "ping-call");
            assert_eq!(body["messages"][2]["tool_call_id"], "ping-call");
            assert_eq!(body["messages"][2]["content"], "");
        }
    }
    drop(capture);
    let attempts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER(WHERE state='succeeded') FROM inference_executions",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, (5, 5));
    let reservations: Vec<(String,i64,i64,i64,i64)> = sqlx::query_as("SELECT state,reserved_tokens,actual_microusd,input_tokens,output_tokens FROM governance_reservations").fetch_all(&pool).await.unwrap();
    assert_eq!(reservations, vec![("settled".into(), 1100, 7, 3, 4); 5]);
    let ledger: Vec<(String,i64,i64)> = sqlx::query_as("SELECT kind,count(*),sum(amount_microusd)::bigint FROM monetary_ledger GROUP BY kind ORDER BY kind").fetch_all(&pool).await.unwrap();
    assert_eq!(
        ledger,
        vec![("hold".into(), 5, 5500), ("settlement".into(), 5, 35)]
    );
}

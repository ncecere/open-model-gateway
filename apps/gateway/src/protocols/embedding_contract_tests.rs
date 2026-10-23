//! Real HTTP + PostgreSQL + recording upstream + official SDK, no live providers.
use crate::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits},
    providers::{
        ProviderAdapter, ProviderRegistry,
        local::{LocalAdapter, Profile, endpoints::ApprovedEndpoints},
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::{Secret, SecretResolver},
    },
    store::Store,
};
use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Uri},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, crate::inference::error::InferenceError> {
        Secret::new("synthetic-upstream-only".into())
    }
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn ordinary_embedding_sdk_default_real_http_all_profiles(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['embeddings']")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true,upstream_model='synthetic-embed'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,0,1000,0,1 FROM deployments").execute(&pool).await.unwrap();
    let captures = Arc::new(Mutex::new(Vec::new()));
    let c = captures.clone();
    let response_override = Arc::new(Mutex::new(None::<String>));
    let o = response_override.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/v1", listener.local_addr().unwrap());
    let mock = Router::new().fallback(move |headers: HeaderMap, uri: Uri, bytes: Bytes| {
        let c = c.clone();
        let o = o.clone();
        async move {
            let native = uri.path().ends_with("/api/embed");
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            c.lock().unwrap().push((headers, uri, body));
            let default = if native { json!({"embeddings":[[1,-2,0.5],[0,4,-0.25]],"prompt_eval_count":7}) }
            else { json!({"object":"list","data":[{"object":"embedding","index":1,"embedding":[0,4,-0.25]},{"object":"embedding","index":0,"embedding":[1,-2,0.5]}],"usage":{"prompt_tokens":7,"total_tokens":7,"cost":0}}) };
            ([("content-type", "application/json")], o.lock().unwrap().clone().unwrap_or_else(|| default.to_string()))
        }
    });
    let _mock = Server(tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    }));
    for (name, profile) in [
        ("openai", None),
        ("openrouter", None),
        ("openai_compatible", Some(Profile::OpenAiCompatible)),
        ("vllm", Some(Profile::Vllm)),
        ("sglang", Some(Profile::Sglang)),
        ("ollama", Some(Profile::Ollama)),
    ] {
        sqlx::query("UPDATE provider_connections SET provider=$1,enabled=true,endpoint=$2,credential_ref=$3")
            .bind(name).bind(profile.map(|_| &upstream)).bind(if profile.is_some() { "none" } else { "env:SYNTHETIC" }).execute(&pool).await.unwrap();
        let adapter: Arc<dyn ProviderAdapter> = match profile {
            Some(p) => Arc::new(LocalAdapter::new(
                p,
                Arc::new(Resolver),
                Arc::new(ApprovedEndpoints::for_test(&upstream)),
            )),
            None if name == "openai" => Arc::new(OpenAiAdapter::for_test(
                Arc::new(Resolver),
                upstream.clone(),
            )),
            None => Arc::new(OpenRouterAdapter::for_test(
                Arc::new(Resolver),
                OpenRouterConfig::default(),
                upstream.clone(),
            )),
        };
        let mut registry = ProviderRegistry::default();
        registry.register(adapter).unwrap();
        let engine =
            Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = http::router_with_engine(store.clone(), None, engine);
        let _gateway = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new("node")
                .arg(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests/embedding-sdk-contracts.mjs"),
                )
                .env("SDK_TEST_BASE_URL", format!("http://{address}"))
                .env("SDK_TEST_API_KEY", &keys.personal_key.token)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let successful: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.provider=$1 AND e.state='succeeded' AND e.workload_kind='embeddings' AND e.input_tokens=7 AND e.output_tokens=0 AND r.state='settled' AND r.actual_microusd=7")
            .bind(name).fetch_one(&pool).await.unwrap();
        assert_eq!(successful, 5, "{name}: input-only priced settlement");
        let attempts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE provider=$1")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 5, "{name}: refusals must not admit attempts");
        let captured = std::mem::take(&mut *captures.lock().unwrap());
        assert_eq!(captured.len(), 5, "{name}: no retries");
        for (headers, uri, body) in captured {
            assert_eq!(body["model"], "synthetic-embed");
            assert!(
                headers
                    .values()
                    .all(|v| !v.to_str().unwrap().contains(&keys.personal_key.token))
            );
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                if profile.is_some() {
                    None
                } else {
                    Some("Bearer synthetic-upstream-only")
                }
            );
            if name == "ollama" {
                assert_eq!(uri.path(), "/api/embed");
                assert_eq!(body["truncate"], false);
                assert!(body.get("encoding_format").is_none());
            } else {
                assert_eq!(uri.path(), "/v1/embeddings");
                assert_eq!(body["encoding_format"], "float");
            }
        }
        invalid_results_keep_observed_usage_and_holds(
            &pool,
            &format!("http://{address}"),
            &keys.personal_key.token,
            name,
            &response_override,
        )
        .await;
        // Failure cases dispatch exactly once, except capability refusals; there
        // is no implicit retry or fallback to another endpoint/encoding.
        let count = std::mem::take(&mut *captures.lock().unwrap()).len();
        assert_eq!(
            count,
            12 + usize::from(name != "ollama")
                + usize::from(!matches!(
                    name,
                    "openai_compatible" | "ollama" | "openrouter"
                ))
                + usize::from(name == "openrouter")
        );
    }
}

fn reply(native: bool, vectors: Value, tokens: Option<u64>) -> Value {
    if native {
        json!({"embeddings":vectors,"prompt_eval_count":tokens})
    } else {
        let data: Vec<_> = vectors
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, v)| json!({"object":"embedding","index":index,"embedding":v}))
            .collect();
        json!({"object":"list","data":data,"usage":{"prompt_tokens":tokens,"total_tokens":tokens,"cost":0}})
    }
}
async fn invalid_results_keep_observed_usage_and_holds(
    pool: &sqlx::PgPool,
    base: &str,
    key: &str,
    name: &str,
    response_override: &Mutex<Option<String>>,
) {
    let native = name == "ollama";
    let normal = json!([[1, -2, 0.5], [0, 4, -0.25]]);
    let mut cases = vec![
        (
            "count",
            reply(native, json!([[1, 2, 3]]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        (
            "uniform",
            reply(native, json!([[1, 2, 3], [1, 2]]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        (
            "empty",
            reply(native, json!([[], []]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        (
            "malformed-vector",
            reply(native, json!([["bad", 2, 3], [1, 2, 3]]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        (
            "overflow-f32",
            reply(native, json!([[1e100, 2, 3], [1, 2, 3]]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        (
            "dimensions-cap",
            reply(native, json!([vec![0; 16385], vec![0; 16385]]), Some(7)).to_string(),
            2,
            None,
            Some(7),
        ),
        // JSON cannot represent NaN/Infinity; malformed JSON supplies no
        // independently validated usage, even if usage text occurs in it.
        (
            "nonfinite-json",
            reply(native, json!([["bad", 2, 3], [1, 2, 3]]), Some(7))
                .to_string()
                .replace("\"bad\"", "NaN"),
            2,
            None,
            None,
        ),
        (
            "local-size-base64",
            reply(native, json!(vec![vec![0; 12000]; 96]), Some(7)).to_string(),
            96,
            None,
            Some(7),
        ),
        (
            "local-size-float",
            reply(native, json!(vec![vec![0; 12000]; 96]), Some(7)).to_string(),
            96,
            None,
            Some(7),
        ),
        (
            "upstream-size",
            reply(native, json!(vec![vec![0; 16384]; 128]), Some(7)).to_string(),
            128,
            None,
            None,
        ),
    ];
    if !matches!(name, "openai_compatible" | "ollama" | "openrouter") {
        cases.push((
            "requested-dimensions",
            reply(native, normal.clone(), Some(7)).to_string(),
            2,
            Some(2),
            Some(7),
        ));
    }
    if !native {
        let mut duplicate = reply(false, normal.clone(), Some(7));
        duplicate["data"][1]["index"] = json!(0);
        cases.push(("duplicate-index", duplicate.to_string(), 2, None, Some(7)));
    }
    // Strict malformed metering must not be "preserved" as a guessed zero.
    let mut bad_usage = reply(native, normal.clone(), Some(7));
    if native {
        bad_usage["prompt_eval_count"] = json!(-1);
    } else {
        bad_usage["usage"]["prompt_tokens"] = json!(-1);
    }
    if name == "openrouter" {
        // Cost and token observations are independent evidence: an invalid one
        // must fail the response without erasing the valid one.
        bad_usage["usage"]["cost"] = json!(0.001);
        let mut bad_cost = reply(false, normal.clone(), Some(7));
        bad_cost["usage"]["cost"] = json!(-1);
        cases.push(("invalid-cost", bad_cost.to_string(), 2, None, Some(7)));
    }
    cases.push(("invalid-usage", bad_usage.to_string(), 2, None, None));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (label, body, count, dimensions, observed) in cases {
        // Each invalid upstream is an independent fixture, not a circuit-
        // breaker test. Reset health only; never mutate accounting evidence.
        sqlx::query("DELETE FROM deployment_health")
            .execute(pool)
            .await
            .unwrap();
        *response_override.lock().unwrap() = Some(body);
        let response = client.post(format!("{base}/v1/embeddings")).bearer_auth(key)
            .json(&json!({"model":"company/smart","input":vec!["synthetic";count],"encoding_format":if label == "local-size-float" {"float"} else {"base64"},"dimensions":dimensions}))
            .send().await.unwrap();
        assert_eq!(response.status(), 502, "{name}/{label}");
        let id =
            uuid::Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_upstream_response"
        );
        let (state, input, output, reservation, held, actual): (String, Option<i64>, Option<i64>, String, Option<i64>, Option<i64>) = sqlx::query_as("SELECT e.state,e.input_tokens,e.output_tokens,r.state,r.held_microusd,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1")
            .bind(id).fetch_one(pool).await.unwrap();
        assert_eq!(state, "failed", "{name}/{label}");
        assert_eq!(input, observed, "{name}/{label}: observed usage");
        assert_eq!(output, Some(0), "input-only semantic zero");
        assert_eq!(
            reservation, "unknown",
            "{name}/{label}: retain unresolved hold"
        );
        assert_eq!(held, Some(1000));
        assert_eq!(actual, None);
        let cost: Option<i64> = sqlx::query_scalar(
            "SELECT provider_cost_microusd FROM inference_executions WHERE id=$1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            cost,
            if name == "openrouter" && label == "invalid-usage" {
                Some(1000)
            } else if name == "openrouter"
                && !matches!(label, "invalid-cost" | "nonfinite-json" | "upstream-size")
            {
                Some(0)
            } else {
                None
            },
            "{name}/{label}: provider evidence"
        );
        let settled: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='settlement'",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(settled, 0, "{name}/{label}: no optimistic settlement");
    }
    if matches!(name, "openai_compatible" | "ollama" | "openrouter") {
        // These profiles cannot express this dimensions override; client-side
        // base64 support must not accidentally widen provider capabilities.
        let response = client.post(format!("{base}/v1/embeddings")).bearer_auth(key)
            .json(&json!({"model":"company/smart","input":["one","two"],"encoding_format":"base64","dimensions":2}))
            .send().await.unwrap();
        assert_eq!(response.status(), 501);
        let id =
            uuid::Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "unsupported_capability"
        );
        let attempts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE id=$1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(attempts, 0, "capability refusal before admission");
    }
    // Unknown usage is a valid vector response, but remains unknown accounting.
    sqlx::query("DELETE FROM deployment_health")
        .execute(pool)
        .await
        .unwrap();
    *response_override.lock().unwrap() = Some(reply(native, normal, None).to_string());
    let response = client
        .post(format!("{base}/v1/embeddings"))
        .bearer_auth(key)
        .json(&json!({"model":"company/smart","input":["one","two"],"encoding_format":"base64"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id = uuid::Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let body = response.json::<Value>().await.unwrap();
    assert!(body.get("usage").is_none());
    assert!(body["data"][0]["embedding"].is_string());
    let row: (Option<i64>, String, Option<i64>) = sqlx::query_as("SELECT e.input_tokens,r.state,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1").bind(id).fetch_one(pool).await.unwrap();
    assert_eq!(row, (None, "unknown".into(), None));
    *response_override.lock().unwrap() = None;
}

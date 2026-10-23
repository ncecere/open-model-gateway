//! Full transport, priced admission and settlement, including ordinary official SDK calls.
use super::{tests::Resolver, *};
use crate::{
    governance::tests::db::fixture,
    inference::{Engine, EngineLimits},
    providers::ProviderRegistry,
};
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Response, Uri},
};
use std::sync::Mutex;

struct Servers(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Servers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
fn fixture_body(marker: &str, streaming: bool) -> String {
    let parts: Vec<_> = marker.split(':').collect();
    let (reason, shape, evidence) = (parts[0], parts[1], parts[2]);
    let mut response = super::tests::complete();
    if evidence == "known" {
        // Anthropic input_tokens is uncached input, not the inclusive total.
        // Known billing needs explicit cache counts; omission is not zero.
        response["usage"]["cache_read_input_tokens"] = json!(0);
        response["usage"]["cache_creation_input_tokens"] = json!(0);
    }
    response["stop_reason"] = match reason {
        "missing" => Value::Null,
        "malformed" => json!({"reason":"refusal"}),
        other => json!(other),
    };
    if shape == "empty" {
        response["content"] = json!([]);
    }
    if shape == "tool" || reason == "tool_use" {
        response["content"] = json!([{"type":"tool_use","id":"semantic-call","name":"weather","input":{"city":"東京"}}]);
    }
    if evidence == "missing" {
        response.as_object_mut().unwrap().remove("usage");
    }
    if evidence == "partial" {
        response["usage"] = json!({"input_tokens":4});
    }
    if evidence == "invalid" {
        response["usage"] = json!({"input_tokens":4,"output_tokens":-1});
    }
    if !streaming {
        return response.to_string();
    }
    let mut frames = super::tests::stream();
    if evidence == "known" {
        frames[0]["message"]["usage"]["cache_read_input_tokens"] = json!(0);
        frames[0]["message"]["usage"]["cache_creation_input_tokens"] = json!(0);
    }
    if shape == "empty" {
        frames.drain(1..4);
    }
    if shape == "tool" || reason == "tool_use" {
        frames[1] = json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"semantic-call","name":"weather","input":{}}});
        frames[2] = json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":if shape == "tool" { "[broken" } else { "{\"city\":\"東京\"}" }}});
    }
    let delta = frames.len() - 2;
    frames[delta]["delta"]["stop_reason"] = if reason == "no_stop" {
        json!("refusal") // Valid finish, then EOF without message_stop.
    } else {
        response["stop_reason"].clone()
    };
    if evidence == "missing" {
        frames[0]["message"]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        frames[delta].as_object_mut().unwrap().remove("usage");
    } else if evidence == "partial" {
        // A start-event zero is not final output evidence.
        frames[delta]["usage"] = json!({"input_tokens":4});
    } else if evidence == "invalid" {
        frames[delta]["usage"] = json!({"output_tokens":-1});
    }
    if reason == "eof" {
        frames.truncate(delta);
    }
    if reason == "no_stop" {
        frames.pop();
    }
    frames
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "runtime role/maintenance ACL contract: run explicitly on a disposable PostgreSQL cluster"]
async fn terminal_fidelity_official_sdks_and_priced_http_evidence(pool: sqlx::PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'terminal fixture',$4)").bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='anthropic'")
        .execute(&pool)
        .await
        .unwrap();
    // Actual runtime ACL, not owner credentials, on every changed inference path.
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$;").execute(&pool).await.unwrap();
    sqlx::raw_sql("REVOKE CONNECT ON DATABASE postgres,template1 FROM PUBLIC,gateway_runtime")
        .execute(&pool)
        .await
        .unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime")).execute(&pool).await.unwrap();
    f.store.apply_runtime_grants().await.unwrap();
    f.store.verify_runtime_grants().await.unwrap();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE gateway_runtime")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let captured = seen.clone();
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let upstream_cancelled = cancelled.clone();
    let db = pool.clone();
    let mock = Router::new().fallback(move |headers: HeaderMap, uri: Uri, Json(body): Json<Value>| {
        let captured = captured.clone(); let db = db.clone();
        let upstream_cancelled = upstream_cancelled.clone();
        async move {
            assert_eq!(headers["x-api-key"], "mock-key");
            assert_eq!(headers["anthropic-version"], "2023-06-01");
            assert!(!headers.contains_key("authorization"));
            assert_eq!(uri.path(), "/v1/messages");
            assert_eq!(body["model"], "mock-model");
            assert_eq!(body["max_tokens"], 50);
            let hold: (i64,i64) = sqlx::query_as("SELECT reserved_tokens,held_microusd FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1").fetch_one(&db).await.unwrap();
            assert_eq!(hold, (150,150));
            let marker = body["messages"][0]["content"][0]["text"].as_str().unwrap();
            let payload = fixture_body(marker, body["stream"] == true);
            captured.lock().unwrap().push(body.clone());
            let response_body = if marker.starts_with("hold:") {
                struct Dropped(Arc<tokio::sync::Notify>);
                impl Drop for Dropped { fn drop(&mut self) { self.0.notify_one(); } }
                let guard = Dropped(upstream_cancelled);
                Body::from_stream(async_stream::stream! {
                    let _guard = guard;
                    for frame in super::tests::stream().into_iter().take(3) {
                        yield Ok::<_, std::convert::Infallible>(format!("data: {frame}\n\n"));
                    }
                    std::future::pending::<()>().await;
                })
            } else { Body::from(payload) };
            Response::builder().header("content-type", if body["stream"] == true { "text/event-stream" } else { "application/json" }).body(response_body).unwrap()
        }
    });
    let mut servers = Servers(vec![]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    }));
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            base,
        )))
        .unwrap();
    let store = crate::store::Store::new(runtime.clone());
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let app = crate::http::router_with_engine(store, None, engine.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/anthropic-terminal-contracts.mjs");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("node")
            .arg(script)
            .env("SDK_TEST_BASE_URL", &base)
            .env("SDK_TEST_API_KEY", &key.token)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        22,
        "one dispatch per SDK request, no retries"
    );
    let settled: Vec<(String,String,i64,i64,i64,i64)> = sqlx::query_as("SELECT e.state,r.state,r.actual_microusd,r.input_tokens,r.output_tokens,r.held_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.started_at").fetch_all(&pool).await.unwrap();
    assert_eq!(
        settled,
        vec![("succeeded".into(), "settled".into(), 6, 4, 2, 150); 22]
    );
    let labels: Vec<String> =
        sqlx::query_scalar("SELECT finish_reason FROM inference_executions ORDER BY started_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    // Native text/empty refusal, context, text/length/tool controls, then Chat.
    let label_pairs = [
        "content_filter",
        "content_filter",
        "length",
        "stop",
        "length",
        "tool_calls",
        "content_filter",
        "length",
        "stop",
        "length",
        "tool_calls",
    ];
    assert_eq!(
        labels,
        label_pairs
            .into_iter()
            .flat_map(|label| [label.to_owned(), label.to_owned()])
            .collect::<Vec<_>>()
    );
    let charged: (i64, i64) = sqlx::query_as(
        "SELECT count(*),sum(amount_microusd)::bigint FROM monetary_ledger WHERE kind='settlement'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(charged, (22, 132));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unsupported = client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(&key.token)
        .json(&json!({"model":"company/smart","input":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unsupported.status(),
        501,
        "interactive Anthropic Responses stays unsupported"
    );
    assert_eq!(seen.lock().unwrap().len(), 22);
    for reason in ["refusal", "model_context_window_exceeded"] {
        for evidence in ["missing", "partial", "invalid"] {
            for streaming in [false, true] {
                let marker = format!("{reason}:text:{evidence}");
                let body = json!({"model":"company/smart","messages":[{"role":"user","content":marker}],"max_tokens":50,"stream":streaming});
                let response = client
                    .post(format!("{base}/v1/messages"))
                    .header("x-api-key", &key.token)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let payload = response.text().await.unwrap();
                if evidence == "invalid" {
                    if streaming {
                        assert!(payload.contains("event: error"), "{payload}");
                        assert!(!payload.contains("event: message_stop"));
                    } else {
                        assert_eq!(status, 502, "{payload}");
                    }
                } else {
                    assert_eq!(status, 200, "{payload}");
                    assert!(payload.contains(reason), "{payload}");
                }
                let row: (String,String,Option<i64>,i64) = sqlx::query_as("SELECT e.state,r.state,r.output_tokens,r.held_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.started_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
                assert_eq!(
                    row,
                    (
                        if evidence == "invalid" {
                            "failed"
                        } else {
                            "succeeded"
                        }
                        .into(),
                        "unknown".into(),
                        None,
                        150
                    )
                );
            }
        }
    }
    for reason in [
        "unknown",
        "malformed",
        "missing",
        "pause_turn",
        "eof",
        "no_stop",
    ] {
        for streaming in [false, true] {
            let body = json!({"model":"company/smart","messages":[{"role":"user","content":format!("{reason}:text:known")}],"max_tokens":50,"stream":streaming});
            let response = client
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", &key.token)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let payload = response.text().await.unwrap();
            if streaming {
                assert!(payload.contains("event: error"), "{payload}");
                assert!(!payload.contains("event: message_stop"), "{payload}");
            } else {
                assert_eq!(status, 502, "{payload}");
            }
            let row: (String,String,i64) = sqlx::query_as("SELECT e.state,r.state,r.held_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.started_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
            assert_eq!(row, ("failed".into(), "unknown".into(), 150));
            if !streaming {
                let counts: (Option<i64>,Option<i64>) = sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions ORDER BY started_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
                assert_eq!(
                    counts,
                    (Some(4), Some(2)),
                    "failed JSON preserves valid usage evidence"
                );
            }
        }
    }
    for reason in ["refusal", "model_context_window_exceeded"] {
        let body = json!({"model":"company/smart","messages":[{"role":"user","content":format!("{reason}:tool:known")}],"max_tokens":50,"stream":true});
        let response = client
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", &key.token)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .unwrap();
        let payload = response.text().await.unwrap();
        assert!(payload.contains("event: error"));
        assert!(!payload.contains("event: message_stop"));
        let row: (String,String,i64) = sqlx::query_as("SELECT e.state,r.state,r.held_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.started_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(row, ("failed".into(), "unknown".into(), 150));
    }
    let executions: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(executions, 48);
    assert_eq!(seen.lock().unwrap().len(), 48);
    // Drop while upstream is open: cancellation retains the unresolved hold.
    let mut request = super::tests::request(true);
    request.model = "company/smart".into();
    request.messages[0].content = Some("hold:text:known".into());
    let ProviderOutput::Stream(mut stream) = engine
        .execute(f.principal, request, uuid::Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        stream.next().await,
        Some(Ok(ChatEvent::Delta { .. }))
    ));
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("upstream body cancelled on drop");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row: (String,String,i64) = sqlx::query_as("SELECT e.state,r.state,r.held_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.started_at DESC LIMIT 1").fetch_one(&pool).await.unwrap();
            if row.0 != "started" { assert_eq!(row,("cancelled".into(),"unknown".into(),150)); break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("cancelled settlement persisted");
    let ledger: (i64,i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE kind='hold'),count(*) FILTER(WHERE kind='settlement') FROM monetary_ledger").fetch_one(&pool).await.unwrap();
    assert_eq!(
        ledger,
        (49, 22),
        "unknown/failed/cancelled holds never settle or release"
    );
    let unknown_mutations: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger l JOIN governance_reservations r ON r.execution_id=l.execution_id AND r.admitted_at=l.admitted_at WHERE r.state='unknown' AND l.kind NOT IN ('hold','unknown')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        unknown_mutations, 0,
        "unknown holds have no release, settlement or other mutation"
    );
    runtime.close().await;
}

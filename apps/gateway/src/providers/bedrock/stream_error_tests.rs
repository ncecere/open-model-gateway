//! #486 end to end: the real Bedrock adapter against a mock Bedrock endpoint
//! that answers 200 and then sends an exception frame, behind the real engine
//! and HTTP routers on PostgreSQL. The response stays a 200 that ends in an
//! error (the gateway does not wait for content and does not fail over after a
//! stream is returned); what the exception type decides is the class: the
//! terminal error each client protocol sees, the recorded execution, and the
//! routing health observation.
use std::sync::Arc;

use axum::{Router, body::Body, http::Response};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{events, frame, wire};
use crate::{
    governance::tests::db::fixture,
    inference::admission_validation_tests::{add_deployment, issue_key, serve, start_gateway},
    providers::{ProviderRegistry, bedrock::BedrockAdapter},
};

/// One mock endpoint whose every ConverseStream answers 200 with `frames`.
fn mock(frames: Vec<u8>) -> Router {
    Router::new().fallback(move || {
        let frames = frames.clone();
        async move {
            Response::builder()
                .header("content-type", "application/vnd.amazon.eventstream")
                .body(Body::from(frames))
                .unwrap()
        }
    })
}

/// A streamed request; returns the HTTP status and the whole response text.
async fn stream(base: &str, token: &str, messages: bool) -> (u16, String) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = if messages {
        client.post(format!("{base}/v1/messages")).json(&json!({
            "model":"company/smart","max_tokens":16,"stream":true,
            "messages":[{"role":"user","content":"hi"}]
        }))
    } else {
        client
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({
                "model":"company/smart","stream":true,
                "messages":[{"role":"user","content":"hi"}]
            }))
    };
    let response = request
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

/// The last `data:` payload of an SSE body that is JSON.
fn last_event(body: &str) -> Value {
    body.lines()
        .rev()
        .find_map(|line| line.strip_prefix("data:"))
        .and_then(|data| serde_json::from_str(data.trim()).ok())
        .unwrap_or_else(|| panic!("no JSON event in {body}"))
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn exception_frames_end_the_stream_with_their_own_class(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment = add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    // (exception type, chat `error.code` for a non-Messages client, Messages
    // `error.type`, counts against routing health)
    let cases = [
        (
            "throttlingException",
            "rate_limit_error",
            "rate_limit_error",
            true,
        ),
        (
            "validationException",
            "upstream_rejected",
            "invalid_request_error",
            false,
        ),
        (
            "serviceUnavailableException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "internalServerException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "modelStreamErrorException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        ("modelTimeoutException", "timeout_error", "api_error", true),
        (
            "someFutureException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
    ];
    let mut problems = Vec::new();
    for (kind, chat_code, messages_type, health) in cases {
        for after_content in [false, true] {
            let mut frames = if after_content {
                wire(events(false)[..2].to_vec())
            } else {
                Vec::new()
            };
            frames.extend(frame(
                kind,
                "exception",
                json!({"message":"upstream secret prompt"}),
            ));
            let (upstream, _upstream) = serve(mock(frames)).await;
            let mut registry = ProviderRegistry::default();
            registry
                .register(Arc::new(BedrockAdapter::for_test(&upstream)))
                .unwrap();
            let (base, _gateway) = start_gateway(&f, registry).await;
            for messages in [false, true] {
                sqlx::query("DELETE FROM deployment_health")
                    .execute(&f.store.pool)
                    .await
                    .unwrap();
                let (status, body) = stream(&base, &token, messages).await;
                let error = last_event(&body);
                let (class, message) = if messages {
                    (
                        error["error"]["type"].as_str().unwrap().to_owned(),
                        error["error"]["message"].as_str().unwrap().to_owned(),
                    )
                } else {
                    (
                        error["error"]["code"].as_str().unwrap().to_owned(),
                        error["error"]["message"].as_str().unwrap().to_owned(),
                    )
                };
                let execution: (String, Option<String>) = sqlx::query_as(
                    "SELECT state,error_code FROM inference_executions WHERE deployment_id=$1 ORDER BY started_at DESC LIMIT 1",
                )
                .bind(deployment)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
                let reservation: String = sqlx::query_scalar(
                    "SELECT r.state FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.deployment_id=$1 ORDER BY e.started_at DESC LIMIT 1",
                )
                .bind(deployment)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
                let failures: i64 = sqlx::query_scalar(
                    "SELECT COALESCE((SELECT consecutive_failures FROM deployment_health WHERE deployment_id=$1),0)::bigint",
                )
                .bind(deployment)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
                println!(
                    "{kind:28} content={after_content:5} {:8} http={status} client={class} message={message:?} execution={}/{} reservation={reservation} health_failures={failures}",
                    if messages { "messages" } else { "chat" },
                    execution.0,
                    execution.1.as_deref().unwrap_or("-"),
                );
                let expected = if messages { messages_type } else { chat_code };
                let code = match (kind, messages) {
                    ("throttlingException", _) => "rate_limit_error",
                    ("validationException", _) => "upstream_rejected",
                    ("serviceUnavailableException", true) => "overloaded_error",
                    ("modelTimeoutException", _) => "timeout_error",
                    _ => "upstream_unavailable",
                };
                // Every case is checked before the test fails, so one run
                // shows the whole table.
                let mut check = |ok: bool, what: &str| {
                    if !ok {
                        problems.push(format!(
                            "{kind} content={after_content} messages={messages}: {what}"
                        ));
                    }
                };
                // The response was already returned; nothing fabricates a finish.
                check(status == 200, "status");
                check(class == expected, "client error");
                check(
                    !body.contains("[DONE]") && !body.contains("message_stop"),
                    "finish",
                );
                check(!body.contains("secret"), "upstream text");
                check(execution.0 == "failed", "execution state");
                check(execution.1.as_deref() == Some(code), "execution error code");
                // A failed stream never settles: the hold stays until reconciled.
                check(reservation != "settled", "reservation");
                check(failures == i64::from(health), "routing health");
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

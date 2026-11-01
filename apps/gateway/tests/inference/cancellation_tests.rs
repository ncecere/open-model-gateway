//! HTTP cancellation, including asynchronous durable settlement, for both
//! buffered generation frontends. No provider network calls.
use super::*;

async fn dropped_generation(pool: PgPool, path: &str, protocol: &str, body: Value) {
    let (app, keys, adapter, _) = fixture(&pool, false).await;
    workload_model(&pool, protocol).await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "authorization",
            format!("Bearer {}", keys.personal_key.token),
        )
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "generation drop request status"
    );
    drop(response);
    assert_eq!(
        adapter.stream_drops.load(Ordering::SeqCst),
        1,
        "generation transport drop count"
    );
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
    .expect("generation cancellation settlement timed out");
    let (state, actual, input, output): (String, Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT state,actual_microusd,input_tokens,output_tokens FROM governance_reservations",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(state == "unknown", "generation reservation state");
    assert!(
        actual.is_none(),
        "generation actual cost must remain unknown"
    );
    assert!(
        input.is_none() && output.is_none(),
        "generation usage must remain unknown"
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dropping_responses_http_stream_cancels_transport_and_retains_unknown_hold(pool: PgPool) {
    dropped_generation(
        pool,
        "/v1/responses",
        "responses",
        json!({"model":"company/smart","input":"hello","stream":true}),
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dropping_messages_http_stream_cancels_transport_and_retains_unknown_hold(pool: PgPool) {
    dropped_generation(pool, "/v1/messages", "messages", json!({"model":"company/smart","max_tokens":32,"messages":[{"role":"user","content":"hello"}],"stream":true})).await;
}

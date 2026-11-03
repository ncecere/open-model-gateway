//! #438: an oversized STREAMED body (no Content-Length) is refused by every
//! OpenAI-shaped handler with the one shared 413 envelope. Real gateway on
//! PostgreSQL; the request authenticates first, then the handler's JSON or
//! multipart rejection answers.
use crate::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits},
    providers::ProviderRegistry,
    store::Store,
};
use axum::body::Bytes;
use serde_json::{Value, json};
use std::sync::Arc;
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_openai_route_uses_the_shared_streamed_413_envelope(pool: sqlx::PgPool) {
    let store = Store::new(pool);
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let engine = Engine::new(
        Arc::new(store.clone()),
        ProviderRegistry::default(),
        EngineLimits::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = http::router_with_engine(store, None, engine);
    let _gateway = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = reqwest::Client::new();
    use crate::inference::types::WorkloadKind as W;
    let workloads = EngineLimits::default().workloads;
    let cap = |kind| workloads.body_bytes(kind).unwrap();
    let (json, multipart) = ("application/json", "multipart/form-data; boundary=omg438");
    let small = 2 << 20;
    // Not here: /v1/files (its own upload errors, file_too_large), /v1/messages
    // (Anthropic-shaped 413) and the realtime routes (they ignore the body).
    // Each route's own body cap: a streamed body one MiB over it reaches the
    // handler's rejection, never a parse error of the (blank) content.
    let cases = [
        ("/v1/chat/completions", json, small),
        ("/v1/responses", json, small),
        ("/v1/embeddings", json, small),
        ("/v1/batches", json, small),
        ("/v1/rerank", json, cap(W::Rerank)),
        ("/v1/images/generations", json, cap(W::Images)),
        ("/v1/systemone", json, cap(W::Systemone)),
        ("/v1/audio/speech", json, cap(W::AudioSpeech)),
        (
            "/v1/audio/transcriptions",
            multipart,
            cap(W::AudioTranscriptions),
        ),
        ("/v1/videos", json, crate::jobs::limits().video_body_bytes),
        (
            "/v1/videos",
            multipart,
            crate::jobs::limits().video_body_bytes,
        ),
    ];
    for (path, content_type, limit) in cases {
        let chunk = Bytes::from(vec![b' '; 1 << 20]);
        let chunks: Vec<Result<Bytes, std::io::Error>> = (0..limit.div_ceil(1 << 20) + 1)
            .map(|_| Ok(chunk.clone()))
            .collect();
        let request = client
            .post(format!("http://{address}{path}"))
            .bearer_auth(&keys.personal_key.token)
            .header("content-type", content_type)
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
                chunks,
            )))
            .build()
            .unwrap();
        assert!(request.headers().get("content-length").is_none());
        let response = client.execute(request).await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "{path} {content_type}"
        );
        let body: Value = response.json().await.unwrap();
        assert!(
            body == json!({"error": {
                "message": "Request body too large",
                "type": "invalid_request_error",
                "code": "payload_too_large",
                "param": null
            }}),
            "{path} {content_type}: streamed 413 must use the shared error envelope"
        );
    }
}

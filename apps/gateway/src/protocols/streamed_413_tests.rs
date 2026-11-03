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
    let json_routes = [
        "/v1/chat/completions",
        "/v1/responses",
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/images/generations",
        "/v1/systemone",
        "/v1/audio/speech",
        "/v1/batches",
        "/v1/videos",
    ];
    let multipart = "multipart/form-data; boundary=omg438";
    let cases = json_routes
        .iter()
        .map(|path| (*path, "application/json"))
        .chain([
            ("/v1/audio/transcriptions", multipart),
            ("/v1/videos", multipart),
        ]);
    for (path, content_type) in cases {
        let chunk = Bytes::from(vec![b' '; 1 << 20]);
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            (0..3).map(|_| Ok(chunk.clone())).collect();
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

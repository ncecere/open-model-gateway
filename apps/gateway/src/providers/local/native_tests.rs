use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{Response, Uri},
};
use std::{
    convert::Infallible,
    sync::atomic::{AtomicUsize, Ordering},
};

fn request() -> EmbeddingRequest {
    EmbeddingRequest {
        model: "public".into(),
        input: vec!["text".into()],
        dimensions: None,
    }
}
fn target(base: String) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "ollama".into(),
        upstream_model: "private".into(),
        credential_ref: "none".into(),
        endpoint: Some(base),
        region: None,
        supported_protocols: vec!["embeddings".into()],
    }
}
fn adapter(base: &str) -> LocalAdapter {
    LocalAdapter::new(
        Profile::Ollama,
        Arc::new(super::super::secrets::EnvSecrets::new(Vec::new())),
        Arc::new(ApprovedEndpoints::for_test(base)),
    )
}
#[tokio::test]
async fn native_body_limits_and_malformed_transport_do_not_retry() {
    for case in 0..4 {
        let calls = Arc::new(AtomicUsize::new(0));
        let capture = calls.clone();
        let app = Router::new().fallback(move |uri: Uri, bytes: Bytes| {
            capture.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(uri.path(), "/prefix/api/embed");
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["truncate"], false);
                let mut response = Response::builder();
                let body = match case {
                    0 => Body::from("not json"),
                    1 => Body::from(vec![b' '; framing::BODY_LIMIT + 1]),
                    2 => Body::from_stream(futures_util::stream::iter(
                        (0..65).map(|_| Ok::<_, Infallible>(Bytes::from(vec![b' '; 65536]))),
                    )),
                    _ => {
                        // A mid-body transport failure, not a successful EOF.
                        response = response.header("content-length", 1000);
                        Body::from_stream(futures_util::stream::iter(vec![
                            Ok(Bytes::from_static(b"{\"embeddings\":")),
                            Err(std::io::Error::other("synthetic transport failure")),
                        ]))
                    }
                };
                response.body(body).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://fixture.invalid:{}/prefix/v1",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let result = adapter(&base)
            .execute_embeddings(&target(base), request())
            .await;
        if case < 3 {
            assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
        } else {
            assert!(matches!(result, Err(InferenceError::UpstreamUnavailable)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
#[tokio::test]
async fn native_connection_failure_is_sanitized() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://fixture.invalid:{}/v1",
        listener.local_addr().unwrap().port()
    );
    drop(listener);
    assert!(matches!(
        adapter(&base)
            .execute_embeddings(&target(base), request())
            .await,
        Err(InferenceError::UpstreamUnavailable)
    ));
}

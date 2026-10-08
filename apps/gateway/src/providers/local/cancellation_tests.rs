use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{Response, Uri},
};
use futures_util::StreamExt;
use std::{convert::Infallible, time::Duration};
struct Guard(Arc<tokio::sync::Notify>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
#[tokio::test]
async fn pending_local_embeddings_and_chat_stream_drop_cancel_http_body() {
    for embedding in [true, false] {
        let began = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(tokio::sync::Notify::new());
        let b = began.clone();
        let d = dropped.clone();
        let app=Router::new().fallback(move |uri: Uri, bytes: Bytes| {
            let began=b.clone();let guard=Guard(d.clone());
            async move {
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if embedding {
                    assert_eq!(uri.path(), "/api/embed");
                    assert_eq!(value["truncate"], false);
                } else {
                    assert_eq!(uri.path(), "/v1/chat/completions");
                }
                let body=async_stream::stream! {
                    let _guard=guard;began.notify_one();
                    let data=if embedding {b"{\"embeddings\":".as_slice()} else {b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n".as_slice()};
                    yield Ok::<_,Infallible>(Bytes::copy_from_slice(data));
                    std::future::pending::<()>().await;
                };
                Response::builder().header("content-type",if embedding{"application/json"}else{"text/event-stream"}).body(Body::from_stream(body)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "http://fixture.invalid:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let adapter = Arc::new(LocalAdapter::new(
            Profile::Ollama,
            Arc::new(super::super::secrets::EnvSecrets::new(Vec::new())),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        ));
        let target = Deployment {
            id: uuid::Uuid::new_v4(),
            provider: "ollama".into(),
            upstream_model: "private".into(),
            credential_ref: "none".into(),
            endpoint: Some(base),
            region: None,
            supported_protocols: vec!["chat_completions".into(), "embeddings".into()],
        };
        if embedding {
            let task = tokio::spawn(async move {
                adapter
                    .execute_embeddings(
                        &target,
                        EmbeddingRequest {
                            model: "public".into(),
                            input: vec!["hello".into()],
                            dimensions: None,
                        },
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(3), began.notified())
                .await
                .unwrap();
            task.abort();
            let _ = task.await;
        } else {
            let r = ChatRequest {
                model: "public".into(),
                messages: vec![Message {
                    role: Role::User,
                    content: Some("hello".into()),
                    tool_calls: vec![],
                    tool_call_id: None,
                }],
                tools: vec![],
                tool_choice: None,
                temperature: None,
                max_output_tokens: Some(7),
                stream: true,
            };
            let ProviderOutput::Stream(mut output) = adapter.execute(&target, r).await.unwrap()
            else {
                panic!()
            };
            assert!(matches!(
                output.next().await,
                Some(Ok(ChatEvent::Delta { .. }))
            ));
            drop(output);
        }
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .expect("dropping local work must release upstream body");
        server.abort();
    }
}

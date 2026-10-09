//! Mock-upstream contract of Anthropic Message Batches (127.0.0.1 only).
use super::*;
use crate::providers::{
    ProviderAdapter,
    secrets::{Secret, SecretResolver},
};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Response, Uri},
};
use std::sync::{Arc, Mutex};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> std::result::Result<Secret, InferenceError> {
        Secret::new("mock-key".into())
    }
}
type Seen = Arc<Mutex<Vec<(Method, String, HeaderMap, Vec<u8>)>>>;
struct Mock {
    adapter: AnthropicAdapter,
    seen: Seen,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn mock(replies: Vec<(u16, &'static str, Vec<u8>)>) -> Mock {
    let seen: Seen = Arc::default();
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let capture = seen.clone();
    let app = Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let capture = capture.clone();
            let queue = queue.clone();
            async move {
                capture
                    .lock()
                    .unwrap()
                    .push((method, uri.to_string(), headers, body.to_vec()));
                let (status, ct, body) = queue.lock().unwrap().pop_front().expect("reply");
                Response::builder()
                    .status(status)
                    .header("content-type", ct)
                    .body(Body::from(body))
                    .unwrap()
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock {
        adapter: AnthropicAdapter::for_test(Arc::new(Resolver), base),
        seen,
        task,
    }
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "anthropic".into(),
        upstream_model: "claude-private".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["messages".into()],
    }
}
fn batch(status: &str, counts: Value, extra: Value) -> Vec<u8> {
    let mut v = json!({"id":"msgbatch_01","type":"message_batch","processing_status":status,
        "request_counts":counts,"ended_at":null,"created_at":"2026-10-09T10:00:00Z",
        "expires_at":"2026-10-10T10:00:00Z","cancel_initiated_at":null,
        "results_url":null});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::to_vec(&v).unwrap()
}
fn counts(processing: u32, succeeded: u32, errored: u32, canceled: u32, expired: u32) -> Value {
    json!({"processing":processing,"succeeded":succeeded,"errored":errored,"canceled":canceled,"expired":expired})
}
fn request() -> BatchRequest {
    BatchRequest::Chat(crate::inference::types::ChatRequest {
        model: "company/claude".into(),
        messages: vec![crate::inference::types::Message {
            role: crate::inference::types::Role::User,
            content: Some("hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(32),
        stream: false,
    })
}

#[test]
fn lines_are_message_params_with_gateway_ids() {
    let t = target();
    let v: Value = serde_json::from_slice(
        &encode_native_line(&t, BatchEndpoint::ChatCompletions, "l3", &request()).unwrap(),
    )
    .unwrap();
    assert_eq!(v["custom_id"], "l3");
    assert_eq!(v["params"]["model"], "claude-private");
    assert_eq!(v["params"]["max_tokens"], 32);
    assert!(v["params"].get("stream").is_none());
    // Embeddings have no native Anthropic batch; custom endpoints are not native.
    assert!(!native_batch(&t, BatchEndpoint::Embeddings));
    let mut proxied = target();
    proxied.endpoint = Some("https://proxy.invalid".into());
    assert!(!native_batch(&proxied, BatchEndpoint::Messages));
}

#[tokio::test]
async fn create_streams_one_json_body_and_maps_status() {
    let m = mock(vec![(
        200,
        "application/json",
        batch("in_progress", counts(2, 0, 0, 0, 0), json!({})),
    )])
    .await;
    let records: ByteStream = Box::pin(futures_util::stream::iter(vec![
        Ok(Bytes::from_static(b"{\"custom_id\":\"l0\",\"params\":{}}")),
        Ok(Bytes::from_static(b"{\"custom_id\":\"l1\",\"params\":{}}")),
    ]));
    let b = m
        .adapter
        .submit_native_batch(&target(), BatchEndpoint::Messages, records)
        .await
        .unwrap();
    assert_eq!(b.status, BatchStatus::InProgress);
    assert_eq!(b.counts.unwrap().total, 2);
    let (method, uri, headers, body) = m.seen.lock().unwrap().pop().unwrap();
    assert_eq!(
        (method, uri.as_str()),
        (Method::POST, "/v1/messages/batches")
    );
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["x-api-key"], "mock-key");
    let sent: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(sent["requests"].as_array().unwrap().len(), 2);
    assert_eq!(sent["requests"][1]["custom_id"], "l1");
    // An aborted record stream never yields a complete body.
    let m = mock(vec![(
        200,
        "application/json",
        batch("in_progress", counts(1, 0, 0, 0, 0), json!({})),
    )])
    .await;
    let records: ByteStream = Box::pin(futures_util::stream::iter(vec![
        Ok(Bytes::from_static(b"{}")),
        Err(InferenceError::InvalidRequest),
    ]));
    assert!(
        m.adapter
            .submit_native_batch(&target(), BatchEndpoint::Messages, records)
            .await
            .is_err()
    );
}

#[test]
fn processing_status_maps_to_batch_states() {
    let parse = |status: &str, c: Value, extra: Value| {
        parse_batch(&serde_json::from_slice(&batch(status, c, extra)).unwrap()).unwrap()
    };
    assert_eq!(
        parse("canceling", counts(1, 0, 0, 0, 0), json!({})).status,
        BatchStatus::Cancelling
    );
    let ended = parse(
        "ended",
        counts(0, 3, 1, 0, 0),
        json!({"ended_at":"2026-10-09T11:00:00Z"}),
    );
    assert_eq!(ended.status, BatchStatus::Completed);
    let c = ended.counts.unwrap();
    assert_eq!((c.total, c.completed, c.failed), (4, 3, 1));
    assert!(ended.completed_at.is_some());
    assert_eq!(
        parse(
            "ended",
            counts(0, 1, 0, 2, 0),
            json!({"cancel_initiated_at":"2026-10-09T10:30:00Z","ended_at":"2026-10-09T11:00:00Z"})
        )
        .status,
        BatchStatus::Cancelled
    );
    assert_eq!(
        parse("ended", counts(0, 0, 0, 0, 2), json!({})).status,
        BatchStatus::Expired
    );
    assert!(parse_batch(&json!({"type":"message_batch","id":"x","processing_status":"bogus","request_counts":counts(0,0,0,0,0)})).is_err());
}

#[tokio::test]
async fn retrieve_cancel_results_and_delete() {
    let results = [
        json!({"custom_id":"l0","result":{"type":"succeeded","message":{"id":"msg","type":"message","role":"assistant","model":"claude-private","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":3}}}}),
        json!({"custom_id":"l1","result":{"type":"errored","error":{"type":"error","error":{"type":"invalid_request_error","message":"private detail"}}}}),
        json!({"custom_id":"l2","result":{"type":"expired"}}),
        json!({"custom_id":"l3","result":{"type":"canceled"}}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n")
    .into_bytes();
    let m = mock(vec![
        (
            200,
            "application/json",
            batch(
                "ended",
                counts(0, 1, 1, 1, 1),
                json!({"ended_at":"2026-10-09T11:00:00Z"}),
            ),
        ),
        (
            200,
            "application/json",
            batch("canceling", counts(1, 0, 0, 0, 0), json!({})),
        ),
        (200, "application/binary", results),
        (
            200,
            "application/json",
            b"{\"id\":\"msgbatch_01\",\"type\":\"message_batch_deleted\"}".to_vec(),
        ),
    ])
    .await;
    let t = target();
    let id = UpstreamId::parse("msgbatch_01").unwrap();
    let b = m.adapter.retrieve_batch(&t, &id).await.unwrap();
    assert_eq!(b.status, BatchStatus::Completed);
    let c = m.adapter.cancel_batch(&t, &id).await.unwrap();
    assert_eq!(c.status, BatchStatus::Cancelling);
    let mut stream = m.adapter.native_batch_results(&t, &b).await.unwrap();
    let mut all = Vec::new();
    while let Some(chunk) = stream.next().await {
        all.extend_from_slice(&chunk.unwrap());
    }
    let lines: Vec<&[u8]> = all.split(|b| *b == b'\n').collect();
    let first = decode_native_result(BatchEndpoint::Messages, lines[0]).unwrap();
    let NativeOutcome::Succeeded(response) = first.outcome else {
        panic!("expected a success")
    };
    let BatchResponse::Chat(r) = *response else {
        panic!("expected a chat result")
    };
    assert_eq!(
        (r.usage.input_tokens, r.usage.output_tokens),
        (Some(7), Some(3))
    );
    match decode_native_result(BatchEndpoint::Messages, lines[1])
        .unwrap()
        .outcome
    {
        NativeOutcome::Failed { status, code } => {
            assert_eq!((status, code.as_str()), (400, "invalid_request_error"))
        }
        _ => panic!("expected an errored line"),
    }
    assert!(matches!(
        decode_native_result(BatchEndpoint::Messages, lines[2])
            .unwrap()
            .outcome,
        NativeOutcome::Expired
    ));
    assert!(matches!(
        decode_native_result(BatchEndpoint::Messages, lines[3])
            .unwrap()
            .outcome,
        NativeOutcome::Cancelled
    ));
    m.adapter.delete_native_batch(&t, &b).await.unwrap();
    let uris: Vec<(Method, String)> = m
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.0.clone(), s.1.clone()))
        .collect();
    assert_eq!(
        uris,
        [
            (Method::GET, "/v1/messages/batches/msgbatch_01".to_owned()),
            (
                Method::POST,
                "/v1/messages/batches/msgbatch_01/cancel".to_owned()
            ),
            (
                Method::GET,
                "/v1/messages/batches/msgbatch_01/results".to_owned()
            ),
            (
                Method::DELETE,
                "/v1/messages/batches/msgbatch_01".to_owned()
            ),
        ]
    );
}

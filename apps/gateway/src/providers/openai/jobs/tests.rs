//! Mock-upstream contract tests for OpenAI videos, files and batches (no
//! network beyond 127.0.0.1, no paid calls).
use super::*;
use crate::providers::{
    ProviderAdapter,
    openai::OpenAiAdapter,
    secrets::{Secret, SecretResolver},
};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Method, Response, Uri, header},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Resolver(AtomicUsize);
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> std::result::Result<Secret, InferenceError> {
        assert_eq!(reference, "env:OPENAI_TEST");
        self.0.fetch_add(1, Ordering::SeqCst);
        Secret::new("local-mock-key".into())
    }
}

struct Captured {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}
fn json_reply(v: Value) -> Reply {
    Reply {
        status: 200,
        content_type: "application/json",
        body: serde_json::to_vec(&v).unwrap(),
    }
}
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new(replies: Vec<Reply>) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        let capture = seen.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, raw: Bytes| {
                let capture = capture.clone();
                let queue = queue.clone();
                async move {
                    capture.lock().unwrap().push(Captured {
                        method,
                        uri: uri.to_string(),
                        headers,
                        body: raw.to_vec(),
                    });
                    let reply = queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected request");
                    Response::builder()
                        .status(reply.status)
                        .header(header::CONTENT_TYPE, reply.content_type)
                        .header(header::LOCATION, "https://example.invalid/elsewhere")
                        .body(Body::from(reply.body))
                        .unwrap()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, seen, task }
    }
    fn adapter(&self) -> OpenAiAdapter {
        OpenAiAdapter::for_test(Arc::new(Resolver(AtomicUsize::new(0))), self.base.clone())
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn last(&self) -> Captured {
        self.seen.lock().unwrap().pop().expect("a request")
    }
}
fn target(model: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: model.into(),
        credential_ref: "env:OPENAI_TEST".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["videos".into()],
    }
}
fn video_request() -> VideoRequest {
    VideoRequest {
        model: "company/video".into(),
        prompt: "a calm lake".into(),
        seconds: 8,
        size: VideoSize::P1280x720,
    }
}
fn video(status: &str) -> Value {
    json!({"id":"video_abc123","object":"video","model":"sora-2","status":status,"progress":40,
           "created_at":1791432289,"completed_at":null,"expires_at":null,"seconds":"8","size":"1280x720",
           "prompt":"a calm lake","remixed_from_video_id":null,"error":null})
}

#[tokio::test]
async fn video_create_sends_explicit_multipart_and_parses_the_job() {
    let mock = Mock::new(vec![json_reply(video("queued"))]).await;
    let got = mock
        .adapter()
        .create_video(&target("sora-2"), video_request())
        .await
        .unwrap();
    assert_eq!(got.id.as_str(), "video_abc123");
    assert_eq!(got.state, JobState::Queued);
    assert_eq!((got.seconds, got.progress), (Some(8), Some(40)));
    assert_eq!(got.size.unwrap().as_str(), "1280x720");
    let sent = mock.last();
    assert_eq!(
        (sent.method, sent.uri.as_str()),
        (Method::POST, "/v1/videos")
    );
    assert_eq!(sent.headers["authorization"], "Bearer local-mock-key");
    let ct = sent.headers["content-type"].to_str().unwrap().to_owned();
    let boundary = crate::protocols::audio::multipart::boundary(&ct).unwrap();
    let parts = crate::protocols::audio::multipart::parse(&sent.body, &boundary, 8).unwrap();
    let fields: Vec<(String, String)> = parts
        .iter()
        .map(|p| (p.name.clone(), String::from_utf8(p.data.to_vec()).unwrap()))
        .collect();
    assert_eq!(
        fields,
        [
            ("model".into(), "sora-2".into()),
            ("prompt".into(), "a calm lake".into()),
            ("seconds".into(), "8".into()),
            ("size".into(), "1280x720".into()),
        ]
    );
}

#[tokio::test]
async fn video_unsupported_family_and_bad_connections_never_dispatch() {
    let mock = Mock::new(vec![]).await;
    let adapter = mock.adapter();
    assert!(!adapter.supports_video_request(&target("gpt-image-1"), &video_request()));
    assert_eq!(
        adapter
            .create_video(&target("gpt-image-1"), video_request())
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    let mut cloud = target("sora-2");
    cloud.endpoint = Some("https://example.invalid/v1".into());
    assert_eq!(
        adapter.create_video(&cloud, video_request()).await.err(),
        Some(InferenceError::Configuration)
    );
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn video_statuses_errors_and_identity_are_strict() {
    let failed = json!({"id":"video_abc123","object":"video","status":"failed","progress":0,"created_at":1,"seconds":"8","size":"1280x720",
        "error":{"code":"moderation_blocked","message":"prompt text echoed here"}});
    let mock = Mock::new(vec![
        json_reply(failed),
        json_reply(video("completed")),
        json_reply(json!({"id":"video_abc123","object":"video","status":"weird"})),
        json_reply(json!({"id":"video_other","object":"video","status":"queued"})),
        json_reply(json!({"id":"video_abc123","object":"video","status":"completed","seconds":"lots","size":"1280x720"})),
    ])
    .await;
    let a = mock.adapter();
    let t = target("sora-2");
    let id = UpstreamId::parse("video_abc123").unwrap();
    let f = a.retrieve_video(&t, &id).await.unwrap();
    assert_eq!(f.state, JobState::Failed);
    assert_eq!(f.error.unwrap().as_str(), "moderation_blocked");
    assert_eq!(mock.last().uri, "/v1/videos/video_abc123");
    assert_eq!(
        a.retrieve_video(&t, &id).await.unwrap().state,
        JobState::Completed
    );
    assert_eq!(
        a.retrieve_video(&t, &id).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
    assert_eq!(
        a.retrieve_video(&t, &id).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
    // Unparseable duration is unknown, never guessed.
    assert_eq!(a.retrieve_video(&t, &id).await.unwrap().seconds, None);
}

#[tokio::test]
async fn error_statuses_map_without_reading_bodies_or_following_redirects() {
    let reply = |status: u16| Reply {
        status,
        content_type: "application/json",
        body: br#"{"error":{"message":"secret upstream detail"}}"#.to_vec(),
    };
    let mock = Mock::new(vec![
        reply(401),
        reply(429),
        reply(500),
        reply(400),
        reply(302),
    ])
    .await;
    let a = mock.adapter();
    let t = target("sora-2");
    for expected in [
        InferenceError::Configuration,
        InferenceError::Busy,
        InferenceError::UpstreamUnavailable,
        InferenceError::UpstreamRejected,
        InferenceError::Configuration,
    ] {
        assert_eq!(
            a.create_video(&t, video_request()).await.err(),
            Some(expected)
        );
    }
    // One request per call: no retries, no redirect follow-up.
    assert_eq!(mock.count(), 5);
}

#[tokio::test]
async fn video_content_streams_allowlisted_media_and_delete_is_confirmed() {
    let mock = Mock::new(vec![
        Reply {
            status: 200,
            content_type: "video/mp4",
            body: b"\x00\x00\x00\x18ftypmp42".to_vec(),
        },
        Reply {
            status: 200,
            content_type: "text/html",
            body: b"<html>".to_vec(),
        },
        json_reply(json!({"id":"video_abc123","object":"video.deleted","deleted":true})),
        json_reply(json!({"id":"video_abc123","object":"video.deleted","deleted":false})),
    ])
    .await;
    let a = mock.adapter();
    let t = target("sora-2");
    let id = UpstreamId::parse("video_abc123").unwrap();
    let content = a.video_content(&t, &id, VideoAsset::Video).await.unwrap();
    assert_eq!(content.content_type, "video/mp4");
    let body: Vec<u8> = content.body.map(|c| c.unwrap().to_vec()).concat().await;
    assert_eq!(body, b"\x00\x00\x00\x18ftypmp42");
    assert_eq!(
        mock.last().uri,
        "/v1/videos/video_abc123/content?variant=video"
    );
    assert_eq!(
        a.video_content(&t, &id, VideoAsset::Thumbnail).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
    a.delete_video(&t, &id).await.unwrap();
    assert_eq!(mock.last().method, Method::DELETE);
    assert_eq!(
        a.delete_video(&t, &id).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
}

fn batch(status: &str) -> Value {
    json!({"id":"batch_up1","object":"batch","endpoint":"/v1/chat/completions","model":"gpt-x",
        "input_file_id":"file-in1","completion_window":"24h","status":status,
        "output_file_id":"file-out1","error_file_id":null,"created_at":1791432289,"in_progress_at":1791432300,
        "expires_at":1791518689,"completed_at":1791433000,"request_counts":{"total":3,"completed":3,"failed":0},
        "metadata":{"team":"search"},
        "usage":{"input_tokens":250,"input_tokens_details":{"cached_tokens":50},"output_tokens":40,
                 "output_tokens_details":{"reasoning_tokens":5},"total_tokens":290}})
}

fn records(lines: &[&'static [u8]]) -> ByteStream {
    Box::pin(futures_util::stream::iter(
        lines
            .iter()
            .map(|l| Ok(axum::body::Bytes::from_static(l)))
            .collect::<Vec<_>>(),
    ))
}
fn chat_request() -> BatchRequest {
    BatchRequest::Chat(crate::inference::types::ChatRequest {
        model: "company/chat".into(),
        messages: vec![crate::inference::types::Message {
            role: crate::inference::types::Role::User,
            content: Some("hi".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(16),
        stream: false,
    })
}

#[test]
fn native_lines_use_gateway_ids_and_the_upstream_model() {
    let t = target("gpt-x");
    let chat: Value = serde_json::from_slice(
        &encode_native_line(&t, BatchEndpoint::Responses, "l7", &chat_request()).unwrap(),
    )
    .unwrap();
    assert_eq!(chat["custom_id"], "l7");
    assert_eq!(chat["url"], "/v1/chat/completions");
    assert_eq!(chat["body"]["model"], "gpt-x");
    assert_eq!(chat["body"]["max_completion_tokens"], 16);
    assert!(chat["body"].get("stream_options").is_none());
    let embeddings = BatchRequest::Embeddings(crate::inference::types::EmbeddingRequest {
        model: "company/embed".into(),
        input: vec!["a".into(), "b".into()],
        dimensions: None,
    });
    let e: Value = serde_json::from_slice(
        &encode_native_line(&t, BatchEndpoint::Embeddings, "l0", &embeddings).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (e["url"].as_str(), e["body"]["model"].as_str()),
        (Some("/v1/embeddings"), Some("gpt-x"))
    );
    // A chat line can't go to the embeddings endpoint and vice versa.
    assert!(encode_native_line(&t, BatchEndpoint::Embeddings, "l0", &chat_request()).is_err());
    // Only the fixed OpenAI origin is native.
    let mut other = target("gpt-x");
    other.endpoint = Some("https://proxy.example".into());
    assert!(!native_batch(&other));
    assert!(native_batch(&t));
}

#[tokio::test]
async fn native_submit_uploads_the_records_then_creates_the_batch() {
    let mock = Mock::new(vec![
        json_reply(
            json!({"id":"file-in1","object":"file","bytes":12,"created_at":1,"filename":"batch.jsonl","purpose":"batch","status":"processed"}),
        ),
        json_reply(batch("validating")),
    ])
    .await;
    let a = mock.adapter();
    let t = target("gpt-x");
    let b = a
        .submit_native_batch(
            &t,
            BatchEndpoint::ChatCompletions,
            records(&[b"{\"a\":1}", b"{\"b\":2}"]),
        )
        .await
        .unwrap();
    assert_eq!(b.status, BatchStatus::Validating);
    assert_eq!(b.input_file.unwrap().as_str(), "file-in1");
    let created = mock.last();
    assert_eq!(
        (created.method, created.uri.as_str()),
        (Method::POST, "/v1/batches")
    );
    // No client metadata is ever sent upstream.
    assert_eq!(
        serde_json::from_slice::<Value>(&created.body).unwrap(),
        json!({"input_file_id":"file-in1","endpoint":"/v1/chat/completions","completion_window":"24h"})
    );
    let upload = mock.last();
    assert_eq!(upload.uri, "/v1/files");
    let ct = upload.headers["content-type"].to_str().unwrap().to_owned();
    let boundary = crate::protocols::audio::multipart::boundary(&ct).unwrap();
    let parts = crate::protocols::audio::multipart::parse(&upload.body, &boundary, 4).unwrap();
    assert_eq!(parts[0].name, "purpose");
    assert_eq!(parts[0].data, b"batch");
    assert_eq!(parts[1].data, b"{\"a\":1}\n{\"b\":2}\n");
}

#[tokio::test]
async fn native_submit_failures_abort_or_clean_up() {
    // A failing record stream never yields a complete upstream form.
    let aborted = Mock::new(vec![json_reply(json!({"id":"file-x","object":"file"}))]).await;
    let content: ByteStream = Box::pin(futures_util::stream::iter(vec![
        Ok(axum::body::Bytes::from_static(b"{\"a\":1}")),
        Err(InferenceError::InvalidRequest),
    ]));
    let t = target("gpt-x");
    assert!(
        aborted
            .adapter()
            .submit_native_batch(&t, BatchEndpoint::ChatCompletions, content)
            .await
            .is_err()
    );
    // A refused batch deletes the uploaded copy; one attempt, no retry.
    let mock = Mock::new(vec![
        json_reply(json!({"id":"file-in1","object":"file","purpose":"batch"})),
        Reply {
            status: 400,
            content_type: "application/json",
            body: b"{\"error\":{\"message\":\"secret detail\"}}".to_vec(),
        },
        json_reply(json!({"id":"file-in1","object":"file","deleted":true})),
    ])
    .await;
    assert_eq!(
        mock.adapter()
            .submit_native_batch(&t, BatchEndpoint::ChatCompletions, records(&[b"{}"]))
            .await
            .err(),
        Some(InferenceError::UpstreamRejected)
    );
    let deleted = mock.last();
    assert_eq!(
        (deleted.method, deleted.uri.as_str()),
        (Method::DELETE, "/v1/files/file-in1")
    );
    assert_eq!(mock.count(), 2);
}

#[tokio::test]
async fn native_retrieve_cancel_and_usage() {
    let mock = Mock::new(vec![
        json_reply(batch("completed")),
        json_reply(batch("cancelling")),
        json_reply(json!({"id":"batch_up1","object":"batch","status":"completed","endpoint":"/v1/responses"})),
    ])
    .await;
    let a = mock.adapter();
    let t = target("gpt-x");
    let id = UpstreamId::parse("batch_up1").unwrap();
    let done = a.retrieve_batch(&t, &id).await.unwrap();
    assert_eq!(done.status, BatchStatus::Completed);
    assert_eq!(
        done.counts,
        Some(RequestCounts {
            total: 3,
            completed: 3,
            failed: 0
        })
    );
    let u = done.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (Some(250), Some(40)));
    let billing = u.billing.unwrap();
    assert_eq!(
        (
            billing.cache_read_input_tokens,
            billing.uncached_input_tokens
        ),
        (Some(50), Some(200))
    );
    assert_eq!(done.output_file.unwrap().as_str(), "file-out1");
    assert_eq!(done.input_file.unwrap().as_str(), "file-in1");
    let c = a.cancel_batch(&t, &id).await.unwrap();
    assert_eq!(c.status, BatchStatus::Cancelling);
    assert_eq!(mock.last().uri, "/v1/batches/batch_up1/cancel");
    assert_eq!(
        a.retrieve_batch(&t, &id).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
    // Malformed usage is unknown, not zero.
    let mut bad = batch("completed");
    bad["usage"]["total_tokens"] = json!(1);
    assert_eq!(parse_batch(&bad).unwrap().usage, None);
    bad["usage"] = json!({"input_tokens": 5});
    assert_eq!(parse_batch(&bad).unwrap().usage, None);
}

fn jsonl_reply(lines: &[Value]) -> Reply {
    let mut body = Vec::new();
    for l in lines {
        body.extend_from_slice(&serde_json::to_vec(l).unwrap());
        body.push(b'\n');
    }
    Reply {
        status: 200,
        content_type: "application/jsonl",
        body,
    }
}

#[tokio::test]
async fn native_results_join_output_and_error_files_and_decode_lines() {
    let ok = json!({"id":"r1","custom_id":"l0","response":{"status_code":200,"request_id":"q","body":{"id":"x","model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","content":"answer","refusal":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}},"error":null});
    let rejected = json!({"id":"r2","custom_id":"l1","response":{"status_code":400,"request_id":"q","body":{"error":{"message":"private text","code":"invalid_value"}}},"error":null});
    let expired = json!({"id":"r3","custom_id":"l2","response":null,"error":{"code":"batch_expired","message":"x"}});
    let mock = Mock::new(vec![jsonl_reply(&[ok]), jsonl_reply(&[rejected, expired])]).await;
    let a = mock.adapter();
    let t = target("gpt-x");
    let mut upstream = parse_batch(&batch("completed")).unwrap();
    upstream.error_file = UpstreamId::parse("file-err1");
    let mut stream = a.native_batch_results(&t, &upstream).await.unwrap();
    let mut all = Vec::new();
    while let Some(chunk) = stream.next().await {
        all.extend_from_slice(&chunk.unwrap());
    }
    let lines: Vec<&[u8]> = all
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 3);
    let first = decode_native_result(BatchEndpoint::ChatCompletions, lines[0]).unwrap();
    assert_eq!(first.custom_id, "l0");
    let NativeOutcome::Succeeded(response) = first.outcome else {
        panic!("expected a success")
    };
    let BatchResponse::Chat(r) = *response else {
        panic!("expected a chat result")
    };
    assert_eq!(
        (r.usage.input_tokens, r.usage.output_tokens),
        (Some(10), Some(2))
    );
    match decode_native_result(BatchEndpoint::ChatCompletions, lines[1])
        .unwrap()
        .outcome
    {
        NativeOutcome::Failed { status, code } => {
            assert_eq!((status, code.as_str()), (400, "invalid_value"))
        }
        _ => panic!("expected a failure"),
    }
    assert!(matches!(
        decode_native_result(BatchEndpoint::ChatCompletions, lines[2])
            .unwrap()
            .outcome,
        NativeOutcome::Expired
    ));
    assert!(decode_native_result(BatchEndpoint::ChatCompletions, b"{}").is_err());
    let uris: Vec<String> = mock
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.uri.clone())
        .collect();
    assert_eq!(
        uris,
        ["/v1/files/file-out1/content", "/v1/files/file-err1/content"]
    );
}

#[tokio::test]
async fn native_cleanup_deletes_every_provider_copy() {
    let mock = Mock::new(vec![
        json_reply(json!({"id":"file-in1","object":"file","deleted":true})),
        Reply {
            status: 404,
            content_type: "application/json",
            body: b"{}".to_vec(),
        },
    ])
    .await;
    let upstream = parse_batch(&batch("completed")).unwrap();
    mock.adapter()
        .delete_native_batch(&target("gpt-x"), &upstream)
        .await
        .unwrap();
    let uris: Vec<(Method, String)> = mock
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|c| (c.method.clone(), c.uri.clone()))
        .collect();
    assert_eq!(
        uris,
        [
            (Method::DELETE, "/v1/files/file-in1".to_owned()),
            (Method::DELETE, "/v1/files/file-out1".to_owned()),
        ]
    );
}

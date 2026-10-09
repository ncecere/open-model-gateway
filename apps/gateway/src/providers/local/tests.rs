use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
struct Resolver(AtomicUsize);
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<super::super::secrets::Secret> {
        self.0.fetch_add(1, Ordering::SeqCst);
        super::super::secrets::Secret::new("fixture-only".into())
    }
}
struct Mock {
    base: String,
    capture: Arc<Mutex<Vec<(HeaderMap, Uri, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new(status: u16, unfinished: bool) -> Self {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let c = capture.clone();
        let app=Router::new().fallback(move |headers:HeaderMap,uri:Uri,bytes:Bytes| {
            let c=c.clone(); async move {
                let v:Value=serde_json::from_slice(&bytes).unwrap();let stream=v["stream"]==true;
                let embeddings=uri.path().ends_with("/embeddings");
                let native=uri.path().ends_with("/api/embed");
                let count=v["input"].as_array().map_or(0, Vec::len);
                c.lock().unwrap().push((headers,uri,v));
                let data=if native {json!({"model":"private","embeddings":vec![vec![0.1,0.2];count],"prompt_eval_count":4}).to_string()}
                else if embeddings {json!({"object":"list","model":"private","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":4,"total_tokens":4}}).to_string()}
                else if stream {format!("data: {}\n\ndata: {}\n\ndata: {}\n\n{}",json!({"model":"served/local-1","choices":[{"index":0,"delta":{"role":"assistant","content":"世界"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3,"created_cache_tokens":4}}}),if unfinished{""}else{"data: [DONE]\n\n"})}
                else {json!({"model":"served/local-1","choices":[{"index":0,"message":{"role":"assistant","content":"世界"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3,"created_cache_tokens":4}}}).to_string()};
                let chunks=data.into_bytes().into_iter().map(|b|Ok::<_,Infallible>(Bytes::from(vec![b]))).collect::<Vec<_>>();
                Response::builder().status(status).header("content-type",if stream{"text/event-stream"}else{"application/json"}).header("location","http://169.254.169.254/latest/meta-data/").body(Body::from_stream(futures_util::stream::iter(chunks))).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // This hostname does not resolve publicly. Only the explicit pinned mapping works.
        let base = format!(
            "http://fixture.invalid:{}/proxy/tenant/v1",
            listener.local_addr().unwrap().port()
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            capture,
            task,
        }
    }
    fn adapter(&self, profile: Profile, resolver: Arc<Resolver>) -> LocalAdapter {
        LocalAdapter::new(
            profile,
            resolver,
            Arc::new(ApprovedEndpoints::for_test(&self.base)),
        )
    }
    fn target(&self, profile: Profile) -> Deployment {
        Deployment {
            id: uuid::Uuid::new_v4(),
            provider: profile.id().into(),
            upstream_model: "private".into(),
            credential_ref: "none".into(),
            endpoint: Some(self.base.clone()),
            region: None,
            supported_protocols: vec!["chat_completions".into(), "embeddings".into()],
        }
    }
}
fn request(stream: bool) -> ChatRequest {
    ChatRequest {
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
        stream,
    }
}
#[tokio::test]
async fn all_profiles_mock_chat_and_embedding_contracts_with_pinned_host() {
    for profile in [
        Profile::OpenAiCompatible,
        Profile::Vllm,
        Profile::Sglang,
        Profile::Ollama,
    ] {
        let mock = Mock::new(200, false).await;
        let resolver = Arc::new(Resolver(AtomicUsize::new(0)));
        let adapter = mock.adapter(profile, resolver.clone());
        let target = mock.target(profile);
        // The served model comes from `model` (here only on the first stream
        // chunk); these servers report no reasoning breakdown, so it is unknown.
        super::super::contract::assert_text_chat_contract(
            &adapter,
            &target,
            request(false),
            super::super::contract::Telemetry {
                reported_model: Some("served/local-1"),
                reasoning_tokens: None,
            },
        )
        .await;
        let ProviderOutput::Complete(response) =
            adapter.execute(&target, request(false)).await.unwrap()
        else {
            panic!()
        };
        let b = response.usage.billing.unwrap();
        assert_eq!(b.total_input_tokens, Some(10));
        assert_eq!(b.cache_read_input_tokens, Some(3));
        if matches!(profile, Profile::Vllm) {
            assert_eq!(b.cache_write_input_tokens, Some(4));
            assert_eq!(b.uncached_input_tokens, Some(3));
        } else {
            assert_eq!(b.cache_write_input_tokens, None);
            assert_eq!(b.uncached_input_tokens, None);
        }
        let r = adapter
            .execute_embeddings(
                &target,
                EmbeddingRequest {
                    model: "public".into(),
                    input: vec!["hello".into()],
                    dimensions: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.embeddings[0].len(), 2);
        assert_eq!(r.usage.input_tokens, Some(4));
        assert_eq!(r.usage.output_tokens, Some(0));
        assert!(!adapter.supports_protocol(ApiProtocol::Responses));
        assert!(!adapter.supports_protocol(ApiProtocol::Messages));
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
        let c = mock.capture.lock().unwrap();
        for (h, uri, v) in c.iter() {
            assert!(!h.contains_key("authorization"));
            assert!(!h.contains_key("x-api-key"));
            assert!(h["host"].to_str().unwrap().starts_with("fixture.invalid:"));
            assert_eq!(v["model"], "private");
            assert!(v.get("max_completion_tokens").is_none());
            if uri.path().ends_with("chat/completions") {
                assert_eq!(uri.path(), "/proxy/tenant/v1/chat/completions");
                assert_eq!(v["max_tokens"], 7);
            } else if matches!(profile, Profile::Ollama) {
                assert_eq!(uri.path(), "/proxy/tenant/api/embed");
                assert_eq!(v["truncate"], false);
                assert_eq!(v["input"], json!(["hello"]));
                assert!(v.get("encoding_format").is_none());
                assert!(v.get("dimensions").is_none());
            } else {
                assert_eq!(uri.path(), "/proxy/tenant/v1/embeddings");
            }
        }
    }
}
#[tokio::test]
async fn invalid_approval_or_auth_never_resolves_secret_or_sends_request() {
    let mock = Mock::new(200, false).await;
    let resolver = Arc::new(Resolver(AtomicUsize::new(0)));
    let adapter = mock.adapter(Profile::Ollama, resolver.clone());
    let mut target = mock.target(Profile::Ollama);
    let embedding = EmbeddingRequest {
        model: "public".into(),
        input: vec!["hello".into()],
        dimensions: None,
    };
    target.endpoint = Some("http://unapproved.invalid/v1".into());
    target.credential_ref = "env:SECRET".into();
    assert!(adapter.execute(&target, request(false)).await.is_err());
    assert!(
        adapter
            .execute_embeddings(&target, embedding.clone())
            .await
            .is_err()
    );
    assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    target.endpoint = Some(mock.base.clone());
    target.credential_ref = "aws:default".into();
    assert!(adapter.execute(&target, request(false)).await.is_err());
    assert!(
        adapter
            .execute_embeddings(&target, embedding.clone())
            .await
            .is_err()
    );
    assert!(mock.capture.lock().unwrap().is_empty());
    target.credential_ref = "env:FIXTURE".into();
    assert!(adapter.execute(&target, request(false)).await.is_ok());
    assert!(adapter.execute_embeddings(&target, embedding).await.is_ok());
    assert_eq!(resolver.0.load(Ordering::SeqCst), 2);
    let capture = mock.capture.lock().unwrap();
    assert_eq!(capture.len(), 2);
    assert_eq!(capture[1].1.path(), "/proxy/tenant/api/embed");
    for (headers, _, _) in capture.iter() {
        assert_eq!(headers["authorization"], "Bearer fixture-only");
        assert!(!headers.contains_key("x-api-key"));
    }
}
#[tokio::test]
async fn redirects_and_eof_do_not_succeed_or_follow_credentials() {
    let mock = Mock::new(302, false).await;
    let adapter = mock.adapter(Profile::Vllm, Arc::new(Resolver(AtomicUsize::new(0))));
    assert!(matches!(
        adapter
            .execute(&mock.target(Profile::Vllm), request(false))
            .await,
        Err(InferenceError::Configuration)
    ));
    assert_eq!(mock.capture.lock().unwrap().len(), 1);
    let mock = Mock::new(200, true).await;
    let adapter = mock.adapter(Profile::Sglang, Arc::new(Resolver(AtomicUsize::new(0))));
    let ProviderOutput::Stream(stream) = adapter
        .execute(&mock.target(Profile::Sglang), request(true))
        .await
        .unwrap()
    else {
        panic!()
    };
    let events: Vec<_> = stream.collect().await;
    assert!(events.last().unwrap().is_err());
    assert!(!events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))));
}
#[tokio::test]
async fn ollama_and_generic_uncertified_controls_rejected_before_dispatch() {
    let mock = Mock::new(200, false).await;
    let adapter = mock.adapter(Profile::Ollama, Arc::new(Resolver(AtomicUsize::new(0))));
    for choice in [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Function("f".into()),
    ] {
        let mut r = request(false);
        r.tool_choice = Some(choice);
        assert!(!adapter.supports_chat_request(&r));
        assert!(matches!(
            adapter.execute(&mock.target(Profile::Ollama), r).await,
            Err(InferenceError::Unsupported)
        ));
    }
    let mut r = request(false);
    r.tools.push(FunctionTool {
        name: "f".into(),
        description: None,
        parameters: json!({"type":"object"}),
        strict: Some(true),
    });
    assert!(!adapter.supports_chat_request(&r));
    assert!(matches!(
        adapter.execute(&mock.target(Profile::Ollama), r).await,
        Err(InferenceError::Unsupported)
    ));
    assert!(mock.capture.lock().unwrap().is_empty());
}

#[tokio::test]
async fn preadmission_checks_and_execute_defense_need_no_connection_or_handler() {
    // No listener, endpoint approval, real environment secret, or upstream handler.
    // Request-specific support must be a pure check independent of execution.
    let approvals = Arc::new(ApprovedEndpoints::parse("[]", "production").unwrap());
    let resolver = Arc::new(Resolver(AtomicUsize::new(0)));
    for profile in [
        Profile::OpenAiCompatible,
        Profile::Vllm,
        Profile::Sglang,
        Profile::Ollama,
    ] {
        let adapter = LocalAdapter::new(profile, resolver.clone(), approvals.clone());
        let target = Deployment {
            id: uuid::Uuid::new_v4(),
            provider: profile.id().into(),
            upstream_model: "private".into(),
            credential_ref: "env:NOT_READ".into(),
            endpoint: None,
            region: None,
            supported_protocols: vec![],
        };
        assert!(adapter.supports_chat_request(&request(false)));
        let mut strict = request(false);
        strict.tools.push(FunctionTool {
            name: "f".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: Some(true),
        });
        assert!(!adapter.supports_chat_request(&strict));
        assert!(matches!(
            adapter.execute(&target, strict).await,
            Err(InferenceError::Unsupported)
        ));
        for choice in [
            ToolChoice::Auto,
            ToolChoice::None,
            ToolChoice::Required,
            ToolChoice::Function("f".into()),
        ] {
            let mut r = request(false);
            r.tool_choice = Some(choice);
            let supported = match profile {
                Profile::Ollama => false,
                Profile::OpenAiCompatible => {
                    matches!(r.tool_choice, Some(ToolChoice::Auto | ToolChoice::None))
                }
                _ => true,
            };
            assert_eq!(adapter.supports_chat_request(&r), supported);
            if !supported {
                assert!(matches!(
                    adapter.execute(&target, r).await,
                    Err(InferenceError::Unsupported)
                ));
            }
        }
        let mut embedding = EmbeddingRequest {
            model: "public".into(),
            input: vec!["text".into()],
            dimensions: None,
        };
        assert!(adapter.supports_embedding_request(&embedding));
        embedding.dimensions = Some(2);
        let supported = matches!(profile, Profile::Vllm | Profile::Sglang);
        assert_eq!(adapter.supports_embedding_request(&embedding), supported);
        if !supported {
            assert!(matches!(
                adapter.execute_embeddings(&target, embedding.clone()).await,
                Err(InferenceError::Unsupported)
            ));
        }
        embedding.dimensions = Some(0);
        assert!(!adapter.supports_embedding_request(&embedding));
        assert!(matches!(
            adapter.execute_embeddings(&target, embedding).await,
            Err(InferenceError::InvalidRequest)
        ));
    }
    assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_embeddings_batch_and_errors_never_retry_or_use_compatibility_endpoint() {
    for status in [200, 400, 413, 429, 500, 302] {
        let mock = Mock::new(status, false).await;
        let resolver = Arc::new(Resolver(AtomicUsize::new(0)));
        let adapter = mock.adapter(Profile::Ollama, resolver.clone());
        let result = adapter
            .execute_embeddings(
                &mock.target(Profile::Ollama),
                EmbeddingRequest {
                    model: "public".into(),
                    input: vec!["one".into(), "two".into()],
                    dimensions: None,
                },
            )
            .await;
        if status == 200 {
            let response = result.unwrap();
            assert_eq!(response.embeddings.len(), 2);
            assert_eq!(
                response.usage.billing.unwrap().uncached_input_tokens,
                Some(4)
            );
        } else {
            assert!(result.is_err());
        }
        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.len(), 1);
        assert_eq!(capture[0].1.path(), "/proxy/tenant/api/embed");
        assert_eq!(capture[0].2["truncate"], false);
        assert_eq!(capture[0].2["input"], json!(["one", "two"]));
        assert!(!capture[0].0.contains_key("authorization"));
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    }
}

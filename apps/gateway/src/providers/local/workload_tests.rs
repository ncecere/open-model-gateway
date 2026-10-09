//! Rerank and System One on local profiles against in-process mock servers.
//! Fixture bodies follow the vLLM/SGLang/Ollama sources named in `workloads`
//! and the shapes a live Jina rerank and OpenJev System One server returned
//! on 2026-10-09.
use super::super::contract::{assert_rerank_contract, assert_systemone_contract};
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Resolver(AtomicUsize);
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<super::super::secrets::Secret> {
        self.0.fetch_add(1, Ordering::SeqCst);
        super::super::secrets::Secret::new("fixture-only".into())
    }
}
type Captured = Arc<Mutex<Vec<(HeaderMap, Uri, Value)>>>;
struct Mock {
    base: String,
    capture: Captured,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new(status: u16, body: Value) -> Self {
        let capture: Captured = Arc::default();
        let c = capture.clone();
        let bytes = serde_json::to_vec(&body).unwrap();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let (c, bytes) = (c.clone(), bytes.clone());
            async move {
                c.lock().unwrap().push((
                    headers,
                    uri,
                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                ));
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .header("location", "http://169.254.169.254/latest/meta-data/")
                    .body(Body::from(bytes))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Unresolvable host: only the approval's pinned address can reach the mock.
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
    fn target(&self, profile: Profile, protocol: &str) -> Deployment {
        Deployment {
            id: uuid::Uuid::new_v4(),
            provider: profile.id().into(),
            upstream_model: "private".into(),
            credential_ref: "none".into(),
            endpoint: Some(self.base.clone()),
            region: None,
            supported_protocols: vec![protocol.into()],
        }
    }
    fn requests(&self) -> Vec<(HeaderMap, Uri, Value)> {
        self.capture.lock().unwrap().clone()
    }
}
fn resolver() -> Arc<Resolver> {
    Arc::new(Resolver(AtomicUsize::new(0)))
}
fn rerank(top_n: Option<u32>) -> RerankRequest {
    RerankRequest {
        model: "public".into(),
        query: "capital of France".into(),
        documents: vec![
            "Paris is the capital of France.".into(),
            "Bananas are yellow.".into(),
            "France borders Spain.".into(),
        ],
        top_n,
    }
}
fn systemone() -> SystemoneRequest {
    SystemoneRequest {
        model: "public".into(),
        state: json!("The sky is blue on a clear day."),
        questions: BTreeMap::from([
            (
                "blue".to_owned(),
                Question {
                    kind: QuestionKind::Noul,
                    instructions: json!("Is the sky blue on a clear day?"),
                    criteria: None,
                },
            ),
            (
                "tone".to_owned(),
                Question {
                    kind: QuestionKind::Score,
                    instructions: json!("How certain is the statement?"),
                    criteria: Some(json!(["unsure", "fairly sure", "certain"])),
                },
            ),
        ]),
    }
}
fn systemone_body() -> Value {
    json!({"model":"cygnet-gemma4-12b-nvfp4","answers":{
        "blue":{"type":"noul","noul":0.99},
        "tone":{"type":"score","score":1.8,"legend":{"0":"unsure","1":"fairly sure","2":"certain"},
                "probabilities":{"0":0.05,"1":0.1,"2":0.85},"confidence":0.8}},
        "usage":{"input_tokens":122,"output_tokens":1}})
}
fn jina_body() -> Value {
    // Live Jina-compatible server (rerank.bitop.dev): no document echo, total only.
    json!({"model":"jina-reranker-v3.5","usage":{"total_tokens":15},
        "results":[{"index":0,"relevance_score":0.4991665780544281},{"index":2,"relevance_score":0.02}]})
}
fn vllm_body() -> Value {
    // vLLM RerankResponse: id, model, usage{prompt_tokens,total_tokens}, results with document echo.
    json!({"id":"rerank-1","model":"BAAI/bge-reranker-v2-m3","usage":{"prompt_tokens":31,"total_tokens":31},
        "results":[{"index":0,"document":{"text":"Paris is the capital of France.","multi_modal":null},"relevance_score":0.99},
                   {"index":2,"document":{"text":"France borders Spain.","multi_modal":null},"relevance_score":0.2}]})
}
fn sglang_body() -> Value {
    // SGLang List[RerankResponse], sorted, `document` omitted under return_documents:false.
    json!([{"score":0.99,"index":0,"meta_info":{"id":"a","prompt_tokens":12,"e2e_latency":0.01}},
           {"score":0.2,"index":2,"meta_info":{"id":"b","prompt_tokens":10}},
           {"score":-3.5,"index":1,"meta_info":{"id":"c","prompt_tokens":9}}])
}

#[tokio::test]
async fn rerank_profiles_map_wire_paths_usage_and_contract() {
    for (profile, body, path, input) in [
        (
            Profile::OpenAiCompatible,
            jina_body(),
            "/proxy/tenant/v1/rerank",
            Some(15),
        ),
        (Profile::Vllm, vllm_body(), "/proxy/tenant/rerank", Some(31)),
        (
            Profile::Sglang,
            sglang_body(),
            "/proxy/tenant/v1/rerank",
            Some(31),
        ),
    ] {
        let mock = Mock::new(200, body).await;
        let resolver = resolver();
        let adapter = mock.adapter(profile, resolver.clone());
        let target = mock.target(profile, "rerank");
        let response =
            assert_rerank_contract(&adapter, &target, rerank(Some(2)), input, None).await;
        assert_eq!(
            response.results.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one upstream request, no retries");
        let (headers, uri, sent) = &requests[0];
        assert_eq!(uri.path(), path);
        assert!(!headers.contains_key("authorization"));
        assert!(
            headers["host"]
                .to_str()
                .unwrap()
                .starts_with("fixture.invalid:")
        );
        assert_eq!(sent["model"], "private");
        assert_eq!(sent["query"], "capital of France");
        assert_eq!(sent["documents"].as_array().unwrap().len(), 3);
        if matches!(profile, Profile::Sglang) {
            // The gateway ranks and truncates so every pair's tokens are observed.
            assert_eq!(
                sent,
                &json!({"model":"private","query":"capital of France","documents":rerank(None).documents,"return_documents":false})
            );
        } else {
            assert_eq!(sent["top_n"], 2);
            assert_eq!(sent.as_object().unwrap().len(), 4);
        }
    }
    // top_n is clamped to the document count; omitted when the client omits it.
    let mock = Mock::new(200, jina_body()).await;
    let adapter = mock.adapter(Profile::Vllm, resolver());
    let target = mock.target(Profile::Vllm, "rerank");
    adapter
        .execute_rerank(&target, rerank(Some(9)))
        .await
        .unwrap();
    adapter.execute_rerank(&target, rerank(None)).await.unwrap();
    let requests = mock.requests();
    assert_eq!(requests[0].2["top_n"], 3);
    assert!(requests[1].2.get("top_n").is_none());
}

#[tokio::test]
async fn rerank_unknown_usage_stays_unknown() {
    // No usage object: tokens unknown, never zero.
    let mut body = jina_body();
    body.as_object_mut().unwrap().remove("usage");
    let mock = Mock::new(200, body).await;
    let adapter = mock.adapter(Profile::OpenAiCompatible, resolver());
    let target = mock.target(Profile::OpenAiCompatible, "rerank");
    let r = assert_rerank_contract(&adapter, &target, rerank(None), None, None).await;
    assert_eq!(r.usage.billing.unwrap().uncached_input_tokens, None);
    // Search units are observed when a server reports them.
    let mut body = jina_body();
    body["usage"] = json!({"search_units":1});
    let mock = Mock::new(200, body).await;
    let adapter = mock.adapter(Profile::OpenAiCompatible, resolver());
    let target = mock.target(Profile::OpenAiCompatible, "rerank");
    assert_rerank_contract(&adapter, &target, rerank(None), None, Some(1)).await;
    // An SGLang pair without meta_info (decoder rerankers) makes the sum unknown.
    let mut body = sglang_body();
    body[1].as_object_mut().unwrap().remove("meta_info");
    let mock = Mock::new(200, body).await;
    let adapter = mock.adapter(Profile::Sglang, resolver());
    let target = mock.target(Profile::Sglang, "rerank");
    let r = assert_rerank_contract(&adapter, &target, rerank(Some(1)), None, None).await;
    assert_eq!(r.results.len(), 1);
    assert_eq!(r.results[0].index, 0);
}

#[tokio::test]
async fn invalid_rerank_bodies_fail_and_keep_usage_evidence() {
    for (profile, mutate) in [
        (
            Profile::OpenAiCompatible,
            (|v: &mut Value| v["results"][1]["index"] = json!(0)) as fn(&mut Value),
        ),
        (Profile::OpenAiCompatible, |v| {
            v["results"][0]["extra"] = json!(1)
        }),
        (Profile::OpenAiCompatible, |v| v["results"] = json!([])),
        (Profile::Vllm, |v| {
            v["results"][0]["relevance_score"] = json!("high")
        }),
        (Profile::Vllm, |v| v["results"][0]["index"] = json!(3)),
        (Profile::Sglang, |v| v[1]["index"] = json!(0)),
        (Profile::Sglang, |v| v[1]["score"] = json!(null)),
        (Profile::Sglang, |v| {
            v.as_array_mut().unwrap().pop();
        }),
        (Profile::Sglang, |v| v[0]["relevance_score"] = json!(1)),
    ] {
        let mut body = match profile {
            Profile::Sglang => sglang_body(),
            Profile::Vllm => vllm_body(),
            _ => jina_body(),
        };
        mutate(&mut body);
        let mock = Mock::new(200, body).await;
        let adapter = mock.adapter(profile, resolver());
        let target = mock.target(profile, "rerank");
        let (result, evidence) =
            crate::inference::evidence::capture(adapter.execute_rerank(&target, rerank(None)))
                .await;
        assert!(
            matches!(result, Err(InferenceError::InvalidUpstream)),
            "{}",
            profile.id()
        );
        assert!(evidence.unwrap().input_tokens.is_some(), "{}", profile.id());
    }
    // Malformed usage fails outright.
    for (profile, body) in [
        (
            Profile::Vllm,
            json!({"results":[{"index":0,"relevance_score":0.5}],"usage":{"prompt_tokens":3,"total_tokens":4}}),
        ),
        (
            Profile::OpenAiCompatible,
            json!({"results":[{"index":0,"relevance_score":0.5}],"usage":{"total_tokens":-1}}),
        ),
        (
            Profile::Sglang,
            json!([{"index":0,"score":0.5,"meta_info":{"prompt_tokens":"3"}}]),
        ),
        (Profile::OpenAiCompatible, json!([])),
        (Profile::Sglang, json!({"results":[]})),
    ] {
        let mock = Mock::new(200, body).await;
        let adapter = mock.adapter(profile, resolver());
        let r = adapter
            .execute_rerank(&mock.target(profile, "rerank"), rerank(None))
            .await;
        assert!(matches!(r, Err(InferenceError::InvalidUpstream)));
    }
}

#[tokio::test]
async fn systemone_profiles_wire_usage_and_contract() {
    for profile in [Profile::OpenAiCompatible, Profile::Ollama] {
        let mock = Mock::new(200, systemone_body()).await;
        let resolver = resolver();
        let adapter = mock.adapter(profile, resolver.clone());
        let target = mock.target(profile, "systemone");
        let response = assert_systemone_contract(&adapter, &target, systemone(), 122, 1).await;
        assert!(response.answers["blue"] == Answer::Noul { noul: 0.99 });
        let b = response.usage.billing.unwrap();
        assert_eq!(
            (b.uncached_input_tokens, b.cache_read_input_tokens),
            (Some(122), Some(0))
        );
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let (headers, uri, sent) = &requests[0];
        assert_eq!(uri.path(), "/proxy/tenant/v1/systemone");
        assert!(!headers.contains_key("authorization"));
        // Exactly the TypeSafe wire: no provider preferences or client fields.
        assert_eq!(sent, &systemone().wire("private"));
    }
    // Ollama's cached input is a subset of input_tokens: a cache-read category,
    // with no write category reported (left unknown, not zero).
    let mut body = systemone_body();
    body["prompt_eval_cached_count"] = json!(100);
    let mock = Mock::new(200, body).await;
    let adapter = mock.adapter(Profile::Ollama, resolver());
    let target = mock.target(Profile::Ollama, "systemone");
    let u = assert_systemone_contract(&adapter, &target, systemone(), 122, 1)
        .await
        .usage;
    let b = u.billing.unwrap();
    assert_eq!(
        (
            b.total_input_tokens,
            b.cache_read_input_tokens,
            b.cache_write_input_tokens,
            b.uncached_input_tokens
        ),
        (Some(122), Some(100), None, None)
    );
    let mut body = systemone_body();
    body["prompt_eval_cached_count"] = json!(123);
    let mock = Mock::new(200, body).await;
    let adapter = mock.adapter(Profile::Ollama, resolver());
    assert!(matches!(
        adapter
            .execute_systemone(&mock.target(Profile::Ollama, "systemone"), systemone())
            .await,
        Err(InferenceError::InvalidUpstream)
    ));
}

#[tokio::test]
async fn invalid_systemone_bodies_fail_and_keep_usage_evidence() {
    for (mutate, evidence_kept) in [
        (
            (|v: &mut Value| v["answers"]["blue"]["noul"] = json!(1.5)) as fn(&mut Value),
            true,
        ),
        (|v| v["answers"]["tone"]["legend"] = json!({"0":"x"}), true),
        (
            |v| {
                v["answers"].as_object_mut().unwrap().remove("tone");
            },
            true,
        ),
        (|v| v["answers"]["blue"]["extra"] = json!(1), true),
        (
            |v| {
                v["usage"].as_object_mut().unwrap().remove("output_tokens");
            },
            false,
        ),
        (
            |v| {
                v.as_object_mut().unwrap().remove("usage");
            },
            false,
        ),
        (|v| *v = json!([]), false),
    ] {
        let mut body = systemone_body();
        mutate(&mut body);
        let mock = Mock::new(200, body).await;
        let adapter = mock.adapter(Profile::OpenAiCompatible, resolver());
        let target = mock.target(Profile::OpenAiCompatible, "systemone");
        let (result, evidence) =
            crate::inference::evidence::capture(adapter.execute_systemone(&target, systemone()))
                .await;
        assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
        assert_eq!(evidence.is_some(), evidence_kept);
    }
}

#[tokio::test]
async fn unsupported_profiles_never_resolve_credentials_or_send() {
    let mock = Mock::new(200, json!({})).await;
    let resolver = resolver();
    for (profile, rerank_ok, systemone_ok) in [
        (Profile::OpenAiCompatible, true, true),
        (Profile::Vllm, true, false),
        (Profile::Sglang, true, false),
        (Profile::Ollama, false, true),
    ] {
        let adapter = mock.adapter(profile, resolver.clone());
        assert_eq!(adapter.supports_protocol(ApiProtocol::Rerank), rerank_ok);
        assert_eq!(
            adapter.supports_protocol(ApiProtocol::Systemone),
            systemone_ok
        );
        for p in [
            ApiProtocol::Responses,
            ApiProtocol::Messages,
            ApiProtocol::Images,
            ApiProtocol::AudioSpeech,
            ApiProtocol::Realtime,
        ] {
            assert!(!adapter.supports_protocol(p));
        }
        let mut target = mock.target(profile, "rerank");
        target.credential_ref = "env:FIXTURE".into();
        if !rerank_ok {
            assert!(matches!(
                adapter.execute_rerank(&target, rerank(None)).await,
                Err(InferenceError::Unsupported)
            ));
        }
        if !systemone_ok {
            assert!(matches!(
                adapter.execute_systemone(&target, systemone()).await,
                Err(InferenceError::Unsupported)
            ));
        }
        // Invalid requests fail before any connection or credential use.
        let mut bad = rerank(None);
        bad.top_n = Some(0);
        assert!(matches!(
            adapter.execute_rerank(&target, bad).await,
            Err(InferenceError::InvalidRequest)
        ));
        let mut image = systemone();
        image.state =
            json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]);
        assert!(matches!(
            adapter.execute_systemone(&target, image).await,
            Err(InferenceError::Unsupported)
        ));
        // An unapproved endpoint fails before the credential is resolved.
        target.endpoint = Some("http://unapproved.invalid/v1".into());
        if rerank_ok {
            assert!(matches!(
                adapter.execute_rerank(&target, rerank(None)).await,
                Err(InferenceError::Configuration)
            ));
        }
        if systemone_ok {
            assert!(matches!(
                adapter.execute_systemone(&target, systemone()).await,
                Err(InferenceError::Configuration)
            ));
        }
    }
    assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn statuses_redirects_and_bearer_auth() {
    for (status, expected) in [
        (302, InferenceError::Configuration),
        (401, InferenceError::Configuration),
        (403, InferenceError::Configuration),
        (422, InferenceError::UpstreamRejected),
        (429, InferenceError::Busy),
        (529, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::new(status, systemone_body()).await;
        let adapter = mock.adapter(Profile::OpenAiCompatible, resolver());
        assert_eq!(
            adapter
                .execute_systemone(
                    &mock.target(Profile::OpenAiCompatible, "systemone"),
                    systemone()
                )
                .await
                .err(),
            Some(expected)
        );
        assert_eq!(
            adapter
                .execute_rerank(
                    &mock.target(Profile::OpenAiCompatible, "rerank"),
                    rerank(None)
                )
                .await
                .err(),
            Some(expected)
        );
        // Redirect targets are never followed: exactly the two requests above.
        assert_eq!(mock.requests().len(), 2);
    }
    // env: credentials become a dedicated upstream Bearer value.
    for (profile, body, protocol) in [
        (Profile::OpenAiCompatible, jina_body(), "rerank"),
        (Profile::Ollama, systemone_body(), "systemone"),
    ] {
        let mock = Mock::new(200, body).await;
        let resolver = resolver();
        let adapter = mock.adapter(profile, resolver.clone());
        let mut target = mock.target(profile, protocol);
        target.credential_ref = "env:FIXTURE".into();
        if protocol == "rerank" {
            adapter.execute_rerank(&target, rerank(None)).await.unwrap();
        } else {
            adapter
                .execute_systemone(&target, systemone())
                .await
                .unwrap();
        }
        assert_eq!(resolver.0.load(Ordering::SeqCst), 1);
        assert_eq!(mock.requests()[0].0["authorization"], "Bearer fixture-only");
    }
}

#[tokio::test]
async fn dropping_rerank_or_systemone_cancels_the_upstream_body() {
    struct Guard(Arc<tokio::sync::Notify>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    for systemone_call in [false, true] {
        let began = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(tokio::sync::Notify::new());
        let (b, d) = (began.clone(), dropped.clone());
        let app = Router::new().fallback(move || {
            let (began, guard) = (b.clone(), Guard(d.clone()));
            async move {
                let body = async_stream::stream! {
                    let _guard = guard;
                    began.notify_one();
                    yield Ok::<_, Infallible>(Bytes::from_static(b"{\"results\":"));
                    std::future::pending::<()>().await;
                };
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from_stream(body))
                    .unwrap()
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
            Profile::OpenAiCompatible,
            resolver(),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        ));
        let target = Deployment {
            id: uuid::Uuid::new_v4(),
            provider: "openai_compatible".into(),
            upstream_model: "private".into(),
            credential_ref: "none".into(),
            endpoint: Some(base),
            region: None,
            supported_protocols: vec![],
        };
        let task = tokio::spawn(async move {
            if systemone_call {
                adapter.execute_systemone(&target, systemone()).await.err()
            } else {
                adapter.execute_rerank(&target, rerank(None)).await.err()
            }
        });
        tokio::time::timeout(Duration::from_secs(3), began.notified())
            .await
            .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .expect("dropping local work must release the upstream body");
        server.abort();
    }
}

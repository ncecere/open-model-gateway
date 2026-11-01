//! Real-handler fixtures of the completed inference areas: the real router,
//! authentication, catalog, routing and admission over a disposable PostgreSQL
//! database, with a deterministic provider adapter (no network, no paid
//! provider). Every exchange is captured as status, headers and raw bytes and
//! validated against the artifact BEFORE any JSON decoding; the #81 coverage
//! and HTTP boundary-vector gates then judge the executed set.
//!
//! The existing `apps/gateway/tests/*` fixtures are external crates and cannot
//! reach this test-only validator, so the adapter is a minimal copy of theirs.
use super::{
    responses::{self as r, Fixtures, capture, embedding_encoding_matches, probe},
    workload_tests::{embeddings_valid, images_valid, rerank_valid, systemone_valid},
};
use crate::{
    billing::{BillingUsage, MeterUsage},
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, images::fixtures::PNG_1X1, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc};
use tower::ServiceExt;

/// Deterministic provider: shapes only, never a prompt echo.
struct Adapter {
    pool: PgPool,
}
#[async_trait]
impl ProviderAdapter for Adapter {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::Embeddings
                | ApiProtocol::Rerank
                | ApiProtocol::Systemone
                | ApiProtocol::Images
                | ApiProtocol::AudioSpeech
                | ApiProtocol::AudioTranscriptions
        )
    }
    fn supports_transcription_request(&self, _: &Deployment, _: &TranscriptionRequest) -> bool {
        true
    }
    fn supports_speech_request(&self, _: &Deployment, _: &SpeechRequest) -> bool {
        true
    }
    async fn execute_audio_transcription(
        &self,
        _: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, InferenceError> {
        let mut usage = Usage {
            meters: Some(crate::inference::audio::transcription_meters(Some(1000))),
            ..Default::default()
        };
        if request.language.as_deref() == Some("en") {
            usage.input_tokens = Some(2);
            usage.output_tokens = Some(1);
        }
        Ok(TranscriptionResponse {
            text: "fixture transcript".into(),
            usage,
        })
    }
    async fn execute_audio_speech(
        &self,
        _: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse, InferenceError> {
        Ok(SpeechResponse {
            audio: Box::pin(futures::stream::iter([Ok(Bytes::from_static(
                b"fixture audio",
            ))])),
            content_type: request.response_format.content_type(),
            usage: request.usage(),
        })
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn cancel_batch(
        &self,
        _: &Deployment,
        id: &crate::jobs::types::UpstreamId,
    ) -> Result<crate::jobs::types::UpstreamBatch, InferenceError> {
        use crate::jobs::types::{BatchStatus, UpstreamBatch};
        let state = id
            .as_str()
            .split('_')
            .nth(1)
            .and_then(BatchStatus::parse)
            .ok_or(InferenceError::InvalidUpstream)?;
        // Deterministic poller race: while the native cancellation is awaiting
        // its provider, another observer commits a terminal state. The real
        // handler must reload it rather than invent a cancelling snapshot.
        sqlx::query("UPDATE async_jobs SET state=$2,completed_at=clock_timestamp(),in_progress_at=clock_timestamp(),finalizing_at=clock_timestamp() WHERE upstream_id=$1").bind(id.as_str()).bind(state.state().as_str()).execute(&self.pool).await.map_err(|_|InferenceError::Storage)?;
        Ok(UpstreamBatch {
            id: id.clone(),
            status: state,
            input_file: None,
            output_file: None,
            error_file: None,
            counts: None,
            usage: None,
            created_at: None,
            in_progress_at: None,
            finalizing_at: None,
            completed_at: None,
            failed_at: None,
            expired_at: None,
            cancelling_at: None,
            cancelled_at: None,
            expires_at: None,
            metadata: None,
        })
    }
    async fn execute_embeddings(
        &self,
        _: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        let dimensions = request.dimensions.unwrap_or(2) as usize;
        Ok(EmbeddingResponse {
            embeddings: (0..request.input.len())
                .map(|row| {
                    (0..dimensions)
                        .map(|i| 0.25 * (row + i + 1) as f32)
                        .collect()
                })
                .collect(),
            usage: Usage {
                input_tokens: Some(7),
                // Embeddings have no output-token workload; this zero is semantic.
                output_tokens: Some(0),
                billing: Some(BillingUsage {
                    total_input_tokens: Some(7),
                    uncached_input_tokens: Some(5),
                    cache_read_input_tokens: Some(2),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                ..Default::default()
            },
        })
    }
    async fn execute_rerank(
        &self,
        _: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse, InferenceError> {
        let count = request.result_limit();
        Ok(RerankResponse {
            results: (0..count)
                .map(|index| RerankResult {
                    index,
                    relevance_score: 1.0 / (index + 1) as f64,
                })
                .collect(),
            usage: Usage {
                input_tokens: Some(25),
                output_tokens: Some(0),
                billing: Some(BillingUsage {
                    total_input_tokens: Some(25),
                    uncached_input_tokens: Some(25),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                meters: Some(MeterUsage {
                    output_images: Some(0),
                    input_characters: Some(0),
                    input_audio_seconds_ms: Some(0),
                    output_audio_seconds_ms: Some(0),
                    search_units: Some(1),
                    requests: Some(1),
                    output_video_seconds_ms: None,
                }),
                ..Default::default()
            },
        })
    }
    async fn execute_systemone(
        &self,
        _: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse, InferenceError> {
        let mut answers = BTreeMap::new();
        for (key, question) in &request.questions {
            let answer = match (question.kind, &question.criteria) {
                (QuestionKind::Noul, _) => Answer::Noul { noul: 0.9566 },
                (QuestionKind::Choice, Some(Value::Object(options))) => {
                    let share = 1.0 / options.len() as f64;
                    Answer::Choice {
                        choice: options.keys().next().unwrap().clone(),
                        probabilities: options.keys().map(|k| (k.clone(), share)).collect(),
                        confidence: Some(0.5),
                    }
                }
                (QuestionKind::Score, Some(Value::Array(levels))) => {
                    let share = 1.0 / levels.len() as f64;
                    // A real System One answer's legend values are provider JSON
                    // that parse_answers copies unvalidated: rotate every JSON kind
                    // across calls (the criterion echoed, null, a boolean, a number,
                    // an object, an array), so each is observed in a real response.
                    static CALL: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let call = CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Answer::Score {
                        score: 1.0,
                        legend: levels
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let value = match (i + call) % 6 {
                                    0 => v.clone(),
                                    1 => Value::Null,
                                    2 => json!(true),
                                    3 => json!(1),
                                    4 => json!({"provider":"note"}),
                                    _ => json!(["provider"]),
                                };
                                (i.to_string(), value)
                            })
                            .collect(),
                        probabilities: (0..levels.len()).map(|i| (i.to_string(), share)).collect(),
                        confidence: None,
                    }
                }
                _ => return Err(InferenceError::InvalidUpstream),
            };
            answers.insert(key.clone(), answer);
        }
        Ok(SystemoneResponse {
            answers,
            usage: Usage {
                input_tokens: Some(145),
                output_tokens: Some(0),
                ..Default::default()
            },
        })
    }
    fn supports_image_request(&self, _: &Deployment, _: &ImageRequest) -> bool {
        true
    }
    async fn execute_images(
        &self,
        _: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse, InferenceError> {
        Ok(ImageResponse {
            created: 1_791_432_289,
            images: (0..request.n)
                .map(|i| GeneratedImage {
                    b64_json: PNG_1X1.into(),
                    media_type: ImageMediaType::Png,
                    revised_prompt: (i == 0).then(|| "a red dot, centered".to_owned()),
                })
                .collect(),
            usage: Usage {
                input_tokens: Some(9),
                output_tokens: Some(272),
                billing: Some(BillingUsage {
                    total_input_tokens: Some(9),
                    uncached_input_tokens: Some(9),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                meters: Some(MeterUsage {
                    output_images: Some(u64::from(request.n)),
                    input_characters: Some(0),
                    input_audio_seconds_ms: Some(0),
                    output_audio_seconds_ms: Some(0),
                    search_units: Some(0),
                    requests: Some(1),
                    output_video_seconds_ms: None,
                }),
                ..Default::default()
            },
        })
    }
}

/// A non-generation model serves exactly one protocol (`valid_model_protocols`),
/// so the one seeded model is switched to the workload under test.
pub(super) async fn serve(pool: &PgPool, protocol: &str) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY[$1]")
        .bind(protocol)
        .execute(pool)
        .await
        .unwrap();
}

pub(super) async fn gateway(pool: &PgPool) -> (axum::Router, DevelopmentKeys) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    for sql in [
        "UPDATE provider_connections SET provider='test_vendor', enabled=true",
        "UPDATE deployments SET enabled=true, upstream_model='private-upstream-id'",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
    serve(pool, "embeddings").await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter { pool: pool.clone() }))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    (http::router_with_engine(store, None, engine), keys)
}

pub(super) fn request(
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<&Value>,
) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

/// An oversized body: declared (the body-limit layer answers, no handler runs)
/// or streamed without a length (the handler's own extractor answers).
fn oversized(path: &str, key: &str, streamed: bool) -> Request<Body> {
    let chunk = Bytes::from(vec![b'x'; 1 << 20]);
    let builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if streamed {
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            (0..3).map(|_| Ok(chunk.clone())).collect();
        builder
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap()
    } else {
        let size = 2 * 1024 * 1024 + 1;
        builder
            .header("content-length", size)
            .body(Body::from(vec![b'x'; size]))
            .unwrap()
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_completed_area_fixtures(pool: PgPool) {
    let (app, keys) = gateway(&pool).await;
    let key = keys.personal_key.token.clone();
    let mut fixtures = Fixtures::default();

    // ---- GET /v1/models: the catalog of the key's workspace.
    let listing = capture(
        "GET",
        "/v1/models",
        app.clone()
            .oneshot(request("GET", "/v1/models", Some(&key), None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&listing.bytes).unwrap();
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "company/smart")
    );
    fixtures.observe(&listing);

    // ---- Refusals every completed operation shares (captured and validated;
    // a refusal earns no coverage).
    for (method, path) in [
        ("GET", "/v1/models"),
        ("POST", "/v1/embeddings"),
        ("POST", "/v1/rerank"),
        ("POST", "/v1/images/generations"),
        ("POST", "/v1/systemone"),
        ("POST", "/v1/realtime/client_secrets"),
        ("POST", "/v1/realtime/calls"),
    ] {
        for credential in [None, Some("omg_live_not_a_real_key")] {
            let body = (method == "POST").then(|| json!({}));
            let refused = capture(
                method,
                path,
                app.clone()
                    .oneshot(request(method, path, credential, body.as_ref()))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_eq!(refused.headers["www-authenticate"], "Bearer");
            fixtures.observe(&refused);
        }
    }
    // Oversized bodies, declared and streamed, on every body-bearing completed route.
    for path in [
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/images/generations",
        "/v1/systemone",
        "/v1/realtime/client_secrets",
    ] {
        for streamed in [false, true] {
            let too_large = capture(
                "POST",
                path,
                app.clone()
                    .oneshot(oversized(path, &key, streamed))
                    .await
                    .unwrap(),
            )
            .await;
            // The realtime refusal ignores its body, so only a declared length trips the cap.
            let expected = if path.starts_with("/v1/realtime") && streamed {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            };
            assert_eq!(too_large.status, expected, "{path} streamed={streamed}");
            fixtures.observe(&too_large);
        }
    }

    // ---- The two realtime refusals: always 400 unsupported_capability.
    for path in ["/v1/realtime/client_secrets", "/v1/realtime/calls"] {
        for body in [None, Some(json!({"model":"gpt-realtime"}))] {
            let refusal = capture(
                "POST",
                path,
                app.clone()
                    .oneshot(request("POST", path, Some(&key), body.as_ref()))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(refusal.status, StatusCode::BAD_REQUEST, "{path}");
            let parsed: Value = serde_json::from_slice(&refusal.bytes).unwrap();
            assert_eq!(parsed["error"]["code"], "unsupported_capability");
            fixtures.observe(&refusal);
        }
    }

    // ---- /v1/messages: the documented transport exception, on the real stack.
    let wrong_method = app
        .clone()
        .oneshot(request("GET", "/v1/messages", Some(&key), None))
        .await
        .unwrap();
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    let (parts, body) = wrong_method.into_parts();
    let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
    assert_eq!(parts.headers["allow"], "POST");
    r::validate_documented(
        r::artifact(),
        "POST",
        "/v1/messages",
        parts.status,
        &parts.headers,
        &bytes,
    )
    .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["error"]["type"], "invalid_request_error");

    // ---- Body vectors: one run per wire form AND per union arm the valid
    // body does not carry. Each send validates the real response; an accepted
    // embedding is cross-checked against the encoding it asked for.
    let send = |path: &'static str| {
        let app = app.clone();
        let key = key.clone();
        async move |body: Value| {
            let response = app
                .clone()
                .oneshot(request("POST", path, Some(&key), Some(&body)))
                .await
                .unwrap();
            let captured = capture("POST", path, response).await;
            if path == "/v1/embeddings" && captured.status == StatusCode::OK {
                let reply: Value = serde_json::from_slice(&captured.bytes).unwrap();
                embedding_encoding_matches(&body, &reply)
                    .unwrap_or_else(|e| crate::management::openapi::responses::refuse(e));
            }
            probe(captured)
        }
    };

    // embeddings: float string, base64 array (object and positional forms).
    serve(&pool, "embeddings").await;
    let embeddings_array = json!({"model":"company/smart","input":["a","b"],"encoding_format":"base64","dimensions":null});
    for valid in [
        embeddings_valid(),
        embeddings_array,
        json!(["company/smart", "hello", "float", 2]),
        json!(["company/smart", ["a", "b"], "base64", null]),
    ] {
        fixtures
            .body_vectors(
                "POST",
                "/v1/embeddings",
                valid,
                &[],
                &[],
                send("/v1/embeddings"),
            )
            .await;
    }

    // rerank.
    serve(&pool, "rerank").await;
    for valid in [
        rerank_valid(),
        json!(["company/smart", "cat", ["kitten", "airplane", "dog"], 2]),
    ] {
        fixtures
            .body_vectors("POST", "/v1/rerank", valid, &[], &[], send("/v1/rerank"))
            .await;
    }

    // images: `size` has no derivable bounds, so the fixture supplies the
    // straddling candidates (16384 per side is the last accepted value).
    serve(&pool, "images").await;
    let straddle = |at: &'static str| {
        [
            (at, json!("16384x16384")),
            (at, json!("16385x16384")),
            (at, json!("1x1")),
            (at, json!("0x1")),
        ]
    };
    fixtures
        .body_vectors(
            "POST",
            "/v1/images/generations",
            images_valid(),
            &[],
            &straddle("/size"),
            send("/v1/images/generations"),
        )
        .await;
    fixtures
        .body_vectors(
            "POST",
            "/v1/images/generations",
            json!([
                "company/smart",
                "a red dot",
                2,
                "1024x1024",
                "low",
                "b64_json",
                7
            ]),
            &[],
            &straddle("/3"),
            send("/v1/images/generations"),
        )
        .await;

    // systemone: a string state and a JSON state, each in both wire forms. A
    // non-blank string has no length bounds to derive, so the fixture supplies
    // the straddling candidates (accepted `x`, refused ` `) at every string
    // location of the valid body.
    serve(&pool, "systemone").await;
    let questions = systemone_valid()["questions"].clone();
    // Two elements, so the one-element boundary differs from the valid body.
    let json_state = json!([{"type":"text","text":"hello"},{"type":"text","text":"there"}]);
    let blank = |positional: bool, string_state: bool| -> Vec<(String, Value)> {
        let q = if positional { "/2" } else { "/questions" };
        let mut at = vec![
            format!("{q}/is_q/instructions"),
            format!("{q}/is_q/criteria/true"),
            format!("{q}/is_q/criteria/false"),
            format!("{q}/lang/instructions"),
            format!("{q}/lang/criteria/en"),
            format!("{q}/lang/criteria/fr"),
            format!("{q}/lang/criteria/x"),
            format!("{q}/tone/instructions"),
            format!("{q}/tone/criteria/0"),
        ];
        if string_state {
            at.push(if positional { "/1" } else { "/state" }.to_owned());
        }
        at.into_iter()
            .flat_map(|at| [(at.clone(), json!("x")), (at, json!(" "))])
            .collect()
    };
    for (valid, positional, string_state) in [
        (systemone_valid(), false, true),
        (
            json!({"model":"company/smart","state":json_state,"questions":questions}),
            false,
            false,
        ),
        (
            json!(["company/smart", "hello there", questions]),
            true,
            true,
        ),
        (json!(["company/smart", json_state, questions]), true, false),
    ] {
        let owned = blank(positional, string_state);
        let candidates: Vec<(&str, Value)> = owned
            .iter()
            .map(|(at, v)| (at.as_str(), v.clone()))
            .collect();
        fixtures
            .body_vectors(
                "POST",
                "/v1/systemone",
                valid,
                &[],
                &candidates,
                send("/v1/systemone"),
            )
            .await;
    }

    // Every operation of every completed area earned positive credit, and
    // coverage and boundary vectors hold.
    fixtures.finish(r::SIMPLE_JSON_AREAS);
}

/// Generation adapter: deterministic wire shapes and real admission-time
/// Messages policy, never credentials/network or caller content in an error.
struct GenerationAdapter;
#[async_trait]
impl ProviderAdapter for GenerationAdapter {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::ChatCompletions | ApiProtocol::Responses | ApiProtocol::Messages
        )
    }
    fn validate_chat_request(&self, request: &ChatRequest) -> Result<(), InferenceError> {
        let translated = request
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("translated"));
        crate::providers::messages::validate(
            request,
            if translated {
                crate::providers::messages::ProviderClass::Local
            } else {
                crate::providers::messages::ProviderClass::Anthropic
            },
        )?;
        if self.supports_chat_request(request) {
            Ok(())
        } else {
            Err(InferenceError::Unsupported)
        }
    }
    async fn execute(
        &self,
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        let mode = request
            .messages
            .last()
            .and_then(|m| m.content.as_deref())
            .unwrap_or("tools");
        let tools = matches!(mode, "tools" | "both" | "native");
        let content = (mode != "tools").then(|| "hello".to_owned());
        let calls = if tools {
            vec![ToolCall {
                id: "call_0".into(),
                name: "ping".into(),
                arguments: if mode == "native" {
                    json!({"nested":[null,true,1,"text",{},[]]}).to_string()
                } else {
                    "{}".into()
                },
            }]
        } else {
            vec![]
        };
        let finish_reason = match mode {
            "length" => FinishReason::Length,
            "context" => FinishReason::ContextWindowExceeded,
            "filter" => FinishReason::ContentFilter,
            "refusal" => FinishReason::Refusal,
            "tools" | "both" | "native" => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        };
        let usage = if matches!(mode, "unknown" | "error") {
            Usage::default()
        } else {
            Usage {
                input_tokens: Some(12),
                output_tokens: Some(u64::from(request.max_output_tokens.unwrap_or(2).min(2))),
                billing: Some(BillingUsage {
                    total_input_tokens: Some(12),
                    uncached_input_tokens: Some(7),
                    cache_read_input_tokens: Some(2),
                    cache_write_input_tokens: Some(3),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(1),
                    cache_write_1h_input_tokens: Some(2),
                }),
                ..Default::default()
            }
        };
        if !request.stream {
            let messages_content = (mode == "native" && request.messages_options.is_some()).then(|| vec![
                json!({"type":"thinking","thinking":"reasoning","signature":"opaque"}),
                json!({"type":"redacted_thinking","data":"opaque"}),
                json!({"type":"text","text":"hello"}),
                json!({"type":"tool_use","id":"call_0","name":"ping","input":{"nested":[null,true,1,"text",{},[]]}}),
            ]);
            return Ok(ProviderOutput::Complete(ChatResponse {
                messages_content,
                content,
                tool_calls: calls,
                finish_reason,
                usage,
            }));
        }
        let mut events = vec![];
        if mode == "native" && request.messages_options.is_some() {
            for block in [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
                json!({"type":"content_block_stop","index":2}),
                json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call_0","name":"ping","input":{}}}),
                json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":calls[0].arguments}}),
                json!({"type":"content_block_stop","index":3}),
            ] {
                events.push(Ok(ChatEvent::MessagesBlock(block)));
            }
        }

        if let Some(text) = content {
            events.push(Ok(ChatEvent::Delta {
                text: Some(text),
                tool_calls: vec![],
            }));
        }
        if mode == "error" {
            events.push(Err(InferenceError::InvalidUpstream));
        } else {
            for (index, call) in calls.into_iter().enumerate() {
                events.push(Ok(ChatEvent::Delta {
                    text: None,
                    tool_calls: vec![ToolCallDelta {
                        index: index as u32,
                        id: Some(call.id),
                        name: Some(call.name),
                        arguments: Some(call.arguments),
                    }],
                }));
            }
            events.extend([
                Ok(ChatEvent::Finish(finish_reason)),
                Ok(ChatEvent::Usage(usage)),
                Ok(ChatEvent::Done),
            ]);
        }
        Ok(ProviderOutput::Stream(Box::pin(
            futures_util::stream::iter(events),
        )))
    }
}

async fn generation_gateway(pool: &PgPool) -> (axum::Router, DevelopmentKeys) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='test_vendor', enabled=true")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true, upstream_model='private-upstream-id'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
    .execute(pool)
    .await
    .unwrap();
    let cache = json!({"read":{"status":"priced","microusd_per_million":"500000"},"write":{"status":"priced","microusd_per_million":"1000000"},"write_5m":{"status":"priced","microusd_per_million":"1250000"},"write_1h":{"status":"priced","microusd_per_million":"2000000"}});
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) SELECT gen_random_uuid(),id,1000000,2000000,1048576,4294967295,2,$1 FROM deployments")
        .bind(cache).execute(pool).await.unwrap();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(GenerationAdapter)).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    (http::router_with_engine(store, None, engine), keys)
}

fn generation_body(path: &str, mode: &str, stream: bool) -> Value {
    match path {
        "/v1/chat/completions" => {
            json!({"model":"company/smart","messages":[{"role":"user","content":mode}],"stream":stream})
        }
        "/v1/responses" => json!({"model":"company/smart","input":mode,"stream":stream}),
        _ => {
            json!({"model":"company/smart","max_tokens":32,"messages":[{"role":"user","content":mode}],"stream":stream})
        }
    }
}
fn generation_http(path: &str, key: &str, body: &Value) -> Request<Body> {
    let mut req = request("POST", path, Some(key), Some(body));
    req.headers_mut()
        .insert("anthropic-version", "2023-06-01".parse().unwrap());
    req
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_real_handler_fixtures(pool: PgPool) {
    let (app, keys) = generation_gateway(&pool).await;
    let key = keys.personal_key.token;
    let mut fixtures = Fixtures::default();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let mut event_kinds = std::collections::BTreeSet::new();
        for stream in [false, true] {
            for mode in [
                "text", "tools", "both", "unknown", "length", "context", "filter", "refusal",
                "native", "error",
            ] {
                if mode == "error" && !stream {
                    continue;
                }
                let body = generation_body(path, mode, stream);
                super::generation::validate_request(r::artifact(), path, &body)
                    .expect("valid generation fixture request");
                let captured = generation_capture(
                    path,
                    &body,
                    app.clone()
                        .oneshot(generation_http(path, &key, &body))
                        .await
                        .unwrap(),
                )
                .await;
                let expected_status = if path == "/v1/messages" && mode == "filter" && !stream {
                    StatusCode::NOT_IMPLEMENTED
                } else {
                    StatusCode::OK
                };
                assert_eq!(
                    captured.status, expected_status,
                    "generation fixture status"
                );
                if stream {
                    let outcome = super::generation::validate_sse(
                        r::artifact(),
                        path,
                        &captured.bytes,
                        false,
                    )
                    .expect("generation stream contract");
                    if path == "/v1/chat/completions" {
                        event_kinds.insert(if mode == "error" {
                            "error".to_owned()
                        } else {
                            "message".to_owned()
                        });
                    } else {
                        for line in std::str::from_utf8(&captured.bytes)
                            .expect("validated SSE UTF-8")
                            .lines()
                        {
                            if let Some(kind) = line.strip_prefix("event: ") {
                                event_kinds.insert(kind.to_owned());
                            }
                        }
                    }
                    let expected =
                        if mode == "error" || (path == "/v1/messages" && mode == "filter") {
                            super::sse::Outcome::Errored
                        } else if path == "/v1/responses"
                            && matches!(mode, "length" | "context" | "filter" | "refusal")
                        {
                            super::sse::Outcome::Incomplete
                        } else {
                            super::sse::Outcome::Completed
                        };
                    assert_eq!(outcome, expected, "generation terminal kind");
                }
                fixtures.observe(&captured);
            }
        }
        let declared = r::artifact()["paths"][path]["post"]["responses"]["200"]["content"]["text/event-stream"]["x-omg-sse"]["events"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(event_kinds, declared, "SSE event fixture inventory differs");
        let body = generation_body(path, "text", false);
        let send = async |body: Value| {
            let response = app
                .clone()
                .oneshot(generation_http(path, &key, &body))
                .await
                .unwrap();
            probe(generation_capture(path, &body, response).await)
        };
        fixtures
            .body_vectors("POST", path, body, &[], &[], send)
            .await;
        for valid in generation_full_requests(path) {
            super::generation::validate_request(r::artifact(), path, &valid)
                .expect("full generation request fixture");
            let send = async |body: Value| {
                let response = app
                    .clone()
                    .oneshot(generation_http(path, &key, &body))
                    .await
                    .unwrap();
                probe(generation_capture(path, &body, response).await)
            };
            fixtures
                .body_vectors("POST", path, valid, &[], &[], send)
                .await;
        }
    }
    fixtures.finish(r::GENERATION_AREAS);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_refusals_and_beta_headers(pool: PgPool) {
    let (app, keys) = generation_gateway(&pool).await;
    let key = keys.personal_key.token;
    for (path, body, status) in [
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],"max_tokens":1,"max_completion_tokens":2}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","input":"hi","store":true}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","input":"hi","previous_response_id":"resp_previous"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/messages",
            json!({"model":"company/smart","max_tokens":32,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_0","content":"translated","is_error":true}]}]}),
            StatusCode::NOT_IMPLEMENTED,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(generation_http(path, &key, &body))
            .await
            .unwrap();
        let captured = capture("POST", path, response).await;
        assert_eq!(captured.status, status, "generation refusal status");
    }
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let mut body = generation_body(path, "text", false);
        body["invented"] = json!(true);
        let captured = capture(
            "POST",
            path,
            app.clone()
                .oneshot(generation_http(path, &key, &body))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            captured.status,
            StatusCode::BAD_REQUEST,
            "unknown generation field status"
        );
        let body = generation_body(path, "text", false);
        let captured = capture(
            "POST",
            path,
            app.clone()
                .oneshot(generation_http(path, "not-an-inference-key", &body))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            captured.status,
            StatusCode::UNAUTHORIZED,
            "generation authentication status"
        );
        let mut req = oversized(path, &key, false);
        req.headers_mut()
            .insert("anthropic-version", "2023-06-01".parse().unwrap());
        let captured = capture("POST", path, app.clone().oneshot(req).await.unwrap()).await;
        assert_eq!(
            captured.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "generation declared body cap status"
        );
    }
    for (path, body) in super::generation_tests::integer_float_cases() {
        let captured = capture(
            "POST",
            path,
            app.clone()
                .oneshot(generation_http(path, &key, &body))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            captured.status,
            StatusCode::BAD_REQUEST,
            "floating-point integer wire kind status"
        );
    }
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 0, "refused generation requests must not execute");
    let body = generation_body("/v1/messages", "text", false);
    for (beta, status) in [
        (
            "future-custom-beta,extended-cache-ttl-2025-04-11",
            StatusCode::OK,
        ),
        ("CoNtExT-1m-2025-08-07", StatusCode::BAD_REQUEST),
        ("files-api-2025-04-14", StatusCode::BAD_REQUEST),
        ("one,,two", StatusCode::BAD_REQUEST),
    ] {
        let mut req = generation_http("/v1/messages", &key, &body);
        req.headers_mut()
            .insert("anthropic-beta", beta.parse().unwrap());
        let header_verdict = super::generation::validate_request_headers(
            r::artifact(),
            "/v1/messages",
            req.headers(),
        );
        assert_eq!(
            header_verdict.is_ok(),
            status == StatusCode::OK,
            "Messages beta request contract verdict"
        );
        let captured = capture(
            "POST",
            "/v1/messages",
            app.clone().oneshot(req).await.unwrap(),
        )
        .await;
        assert_eq!(captured.status, status, "Messages beta header status");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "only accepted beta request executes"
    );
    assert_eq!(
        crate::providers::messages::DENIED_BETA_PREFIXES,
        r::artifact()["paths"]["/v1/messages"]["post"]["parameters"][1]["x-omg-denied-prefixes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .as_slice(),
        "Messages beta denylist drift"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_large_message_boundaries(pool: PgPool) {
    let (app, keys) = generation_gateway(&pool).await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for (count, status) in [(4096, StatusCode::OK), (4097, StatusCode::BAD_REQUEST)] {
            let messages = vec![json!({"role":"user","content":"x"}); count];
            let mut body = generation_body(path, "text", false);
            body[if path == "/v1/responses" {
                "input"
            } else {
                "messages"
            }] = json!(messages);
            for body in [body.clone(), generation_positional(path, &body)] {
                let captured = capture(
                    "POST",
                    path,
                    app.clone()
                        .oneshot(generation_http(path, &keys.personal_key.token, &body))
                        .await
                        .unwrap(),
                )
                .await;
                assert_eq!(
                    captured.status, status,
                    "generation message-count boundary status"
                );
            }
        }
    }
}

fn generation_positional(path: &str, body: &Value) -> Value {
    let fields: &[&str] = match path {
        "/v1/chat/completions" => &[
            "model",
            "messages",
            "stream",
            "stream_options",
            "temperature",
            "max_completion_tokens",
            "max_tokens",
            "n",
            "tools",
            "tool_choice",
            "user",
            "metadata",
        ],
        "/v1/responses" => &[
            "model",
            "input",
            "instructions",
            "stream",
            "store",
            "max_output_tokens",
            "temperature",
            "tools",
            "tool_choice",
            "text",
            "user",
            "metadata",
        ],
        _ => &[
            "model",
            "messages",
            "system",
            "max_tokens",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "tools",
            "tool_choice",
            "metadata",
            "thinking",
            "output_config",
            "context_management",
        ],
    };
    let length = fields.len();
    Value::Array(
        fields[..length]
            .iter()
            .map(|key| {
                body.get(key).cloned().unwrap_or_else(|| match *key {
                    "tools" => json!([]),
                    "stream" | "store" => json!(false),
                    _ => Value::Null,
                })
            })
            .collect(),
    )
}

fn generation_full_requests(path: &str) -> Vec<Value> {
    let parameters = json!({"type":"object","properties":{"value":{"type":"string"}}});
    let cache = json!({"type":"ephemeral","ttl":"5m"});
    let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_1X1},"cache_control":cache});
    let mut full = match path {
        "/v1/chat/completions" => json!({"model":"company/smart","messages":[
            {"role":"system","content":"instructions"},{"role":"developer","content":"instructions"},
            {"role":"user","content":"text"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_0","type":"function","function":{"name":"ping","arguments":"{}"}}],"refusal":null},
            {"role":"tool","content":"result","tool_call_id":"call_0"},
            {"role":"assistant","content":"both","tool_calls":[]}],
            "stream":true,"stream_options":{"include_usage":true},"temperature":1,"max_completion_tokens":32,"max_tokens":32,"n":1,
            "tools":[{"type":"function","function":{"name":"ping","description":"Ping","parameters":parameters,"strict":false}}],
            "tool_choice":{"type":"function","function":{"name":"ping"}},"user":"client-case","metadata":{"session_id":"fixture"}}),
        "/v1/responses" => json!({"model":"company/smart","input":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"earlier","annotations":[]}],"id":"msg_prior","status":"completed"},
            {"type":"function_call","call_id":"call_0","name":"ping","arguments":"{}","id":"fc_prior","status":"completed"},
            {"type":"function_call_output","call_id":"call_0","output":"result","id":null,"status":null},
            {"type":"function_call","call_id":"call_1","name":"ping","arguments":"{}","id":"fc_prior_1","status":"completed"},
            {"type":"function_call_output","call_id":"call_1","output":"result","id":"result_1","status":"completed"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"earlier"}],"id":"msg_user","status":"completed"},
            {"type":"message","role":"user","content":"both","id":"msg_last","status":"completed"}],
            "instructions":"instructions","stream":true,"store":false,"max_output_tokens":32,"temperature":1,
            "tools":[{"type":"function","name":"ping","description":"Ping","parameters":parameters,"strict":false}],
            "tool_choice":{"type":"function","name":"ping"},"text":{"format":{"type":"text"}},"user":"client-case","metadata":{"session_id":"fixture"}}),
        _ => json!({"model":"company/smart","messages":[
            {"role":"user","content":"earlier"}, {"role":"assistant","content":"earlier"},
            {"role":"assistant","content":[{"type":"thinking","thinking":"reasoning","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"},
                {"type":"text","text":"earlier","cache_control":cache},{"type":"tool_use","id":"call_0","name":"ping","input":{"nested":[null,true,1,"text",{},[]]},"cache_control":{"type":"ephemeral","ttl":"1h"}},
                {"type":"tool_use","id":"call_1","name":"ping","input":{},"cache_control":cache},image]},
            {"role":"user","content":[image,{"type":"tool_result","tool_use_id":"call_0","content":[{"type":"text","text":"result"},image],"is_error":false,"cache_control":cache},{"type":"tool_result","tool_use_id":"call_1","content":"result","is_error":true,"cache_control":cache},{"type":"text","text":"native","cache_control":cache}]}],
            "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "max_tokens":2048,"stream":true,"temperature":0.5,"top_p":0.5,"top_k":5,"stop_sequences":["END"],
            "tools":[{"type":"custom","name":"ping","description":"Ping","input_schema":parameters,"defer_loading":false,"cache_control":cache}],
            "tool_choice":{"type":"tool","name":"ping"},"metadata":{"user_id":"client-case"},
            "thinking":{"type":"enabled","budget_tokens":1024,"display":"summarized"},
            "output_config":{"effort":"medium","task_budget":{"type":"tokens","total":64000,"remaining":32000}},
            "context_management":{"edits":[{"type":"clear_tool_uses_20250919","trigger":{"type":"input_tokens","value":0},"keep":{"type":"tool_uses","value":3},"clear_at_least":{"type":"thinking_turns","value":1},"clear_tool_inputs":true,"exclude_tools":["ping"]},{"type":"clear_thinking_20251015","keep":"all"}]}}),
    };
    // Probe the token aliases independently as well as their equality rule.
    if path == "/v1/chat/completions" {
        full.as_object_mut().unwrap().remove("max_tokens");
    }
    let mut basic = generation_body(path, "text", false);
    basic["tools"] = match path {
        "/v1/chat/completions" => json!([{"type":"function","function":{"name":"ping"}}]),
        "/v1/responses" => json!([{"type":"function","name":"ping","parameters":{}}]),
        _ => json!([{"name":"ping","input_schema":{}}]),
    };
    let mut cases = vec![generation_positional(path, &basic)];
    if path == "/v1/chat/completions" {
        // Cross product of outer call/message and nested function serde forms;
        // all-tuple and all-object bodies alone do not reach mixed branches.
        let calls = json!([
            {"id":"call_0","type":"function","function":{"name":"ping","arguments":"{}"}},
            {"id":"call_1","type":"function","function":["ping","{}"]},
            ["call_2","function",{"name":"ping","arguments":"{}"}],
            ["call_3","function",["ping","{}"]]
        ]);
        for nested in [false, true] {
            let mut mixed = if nested {
                generation_nested_positional(path, full.clone())
            } else {
                full.clone()
            };
            if nested {
                mixed["messages"][3][2] = calls.clone();
            } else {
                mixed["messages"][3]["tool_calls"] = calls.clone();
            }
            let messages = mixed["messages"].as_array_mut().unwrap();
            messages.splice(
                5..5,
                (1..4).map(|i| {
                    let id = format!("call_{i}");
                    if nested {
                        json!(["tool", "result", [], id])
                    } else {
                        json!({"role":"tool","content":"result","tool_call_id":id})
                    }
                }),
            );
            mixed["tool_choice"] = if nested {
                json!(["function",{"name":"ping"}])
            } else {
                json!({"type":"function","function":["ping"]})
            };
            cases.push(mixed.clone());
            cases.push(generation_positional(path, &mixed));
        }
        let mut equal = full.clone();
        equal["max_tokens"] = json!(32);
        cases.push(equal); // Equal aliases are supported, not just unequal refusals.
    }
    let mut variants = vec![full.clone()];
    let mut nulls = full.clone();
    let optional: &[&str] = match path {
        "/v1/chat/completions" => &[
            "stream",
            "stream_options",
            "temperature",
            "max_completion_tokens",
            "max_tokens",
            "n",
            "tool_choice",
            "user",
            "metadata",
        ],
        "/v1/responses" => &[
            "instructions",
            "stream",
            "store",
            "max_output_tokens",
            "temperature",
            "tool_choice",
            "text",
            "user",
            "metadata",
        ],
        _ => &[
            "system",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "tool_choice",
            "metadata",
            "thinking",
            "output_config",
            "context_management",
        ],
    };
    for key in optional {
        nulls[*key] = Value::Null;
    }
    cases.push(nulls.clone());
    cases.push(generation_positional(path, &nulls));
    let mut child_nulls = full.clone();
    match path {
        "/v1/chat/completions" => {
            child_nulls["tools"][0]["function"]["description"] = Value::Null;
            child_nulls["tools"][0]["function"]["strict"] = Value::Null;
            child_nulls["messages"][3]["tool_call_id"] = Value::Null;
            let mut legacy = full.clone();
            legacy
                .as_object_mut()
                .unwrap()
                .remove("max_completion_tokens");
            legacy["max_tokens"] = json!(32);
            variants.push(legacy);
        }
        "/v1/responses" => {
            child_nulls["tools"][0]["description"] = Value::Null;
            child_nulls["tools"][0]["strict"] = Value::Null;
        }
        _ => {
            child_nulls["metadata"]["user_id"] = Value::Null;
        }
    }
    variants.push(child_nulls);
    let mut automatic = full.clone();
    automatic["tool_choice"] = if path == "/v1/messages" {
        json!({"type":"auto"})
    } else {
        json!("auto")
    };
    if path == "/v1/messages" {
        automatic["system"] = json!("instructions");
        automatic["thinking"] = json!({"type":"adaptive","display":"omitted"});
    }
    cases.push(automatic.clone());
    cases.push(generation_positional(path, &automatic));
    for variant in variants {
        cases.push(variant.clone());
        cases.push(generation_positional(path, &variant));
        let nested = generation_nested_positional(path, variant);
        cases.push(nested.clone());
        cases.push(generation_positional(path, &nested));
    }
    cases
}

fn generation_nested_positional(path: &str, mut full: Value) -> Value {
    match path {
        "/v1/chat/completions" => {
            for message in full["messages"].as_array_mut().unwrap() {
                let role = message["role"].clone();
                let calls = message["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| {
                        json!([
                            c["id"],
                            c["type"],
                            [c["function"]["name"], c["function"]["arguments"]]
                        ])
                    })
                    .collect::<Vec<_>>();
                let mut positional = vec![
                    role.clone(),
                    message["content"].clone(),
                    json!(calls),
                    message["tool_call_id"].clone(),
                ];
                if role == "assistant" {
                    positional.push(Value::Null);
                }
                *message = json!(positional);
            }
            full["stream_options"] = json!([true]);
            let f = &full["tools"][0]["function"];
            full["tools"] = json!([[
                "function",
                [f["name"], f["description"], f["parameters"], f["strict"]]
            ]]);
            full["tool_choice"] = json!(["function", ["ping"]]);
        }
        "/v1/responses" => {
            let f = &full["tools"][0];
            full["tools"] = json!([[
                f["type"],
                f["name"],
                f["description"],
                f["parameters"],
                f["strict"]
            ]]);
            full["text"] = json!([["text"]]);
        }
        _ => {
            for message in full["messages"].as_array_mut().unwrap() {
                *message = json!([message["role"], message["content"]]);
            }
            full["metadata"] = json!([full["metadata"]["user_id"]]);
        }
    }
    full
}

async fn generation_capture(
    path: &str,
    request: &Value,
    response: axum::response::Response,
) -> crate::management::openapi::responses::Captured {
    let captured = capture("POST", path, response).await;
    if captured.status == StatusCode::OK {
        super::generation::validate_request(r::artifact(), path, request)
            .expect("accepted generation request contract");
        let is_sse = captured
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap()
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            });
        let stream_index = match path {
            "/v1/chat/completions" => 2,
            "/v1/responses" => 3,
            _ => 4,
        };
        let requested_stream = request
            .get("stream")
            .or_else(|| request.get(stream_index))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert_eq!(
            is_sse, requested_stream,
            "generation request/response wire mode"
        );
        if is_sse {
            let options = request.get("stream_options").or_else(|| request.get(3));
            let include_usage = options
                .and_then(|v| v.get("include_usage").or_else(|| v.get(0)))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let outcome = super::generation::validate_sse(
                r::artifact(),
                path,
                &captured.bytes,
                include_usage,
            )
            .expect("request-aware generation SSE contract");
            assert!(
                outcome.positive(),
                "request-aware generation stream truncated"
            );
        }
    }
    captured
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_distinct_tool_boundaries(pool: PgPool) {
    let (app, keys) = generation_gateway(&pool).await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for (count, status) in [(128, StatusCode::OK), (129, StatusCode::BAD_REQUEST)] {
            let mut body = generation_body(path, "text", false);
            body["tools"] = Value::Array(
                (0..count)
                    .map(|i| {
                        let name = format!("tool_{i}");
                        match path {
                            "/v1/chat/completions" => {
                                json!({"type":"function","function":{"name":name}})
                            }
                            "/v1/responses" => {
                                json!({"type":"function","name":name,"parameters":{}})
                            }
                            _ => json!({"name":name,"input_schema":{}}),
                        }
                    })
                    .collect(),
            );
            let captured = generation_capture(
                path,
                &body,
                app.clone()
                    .oneshot(generation_http(path, &keys.personal_key.token, &body))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(
                captured.status, status,
                "distinct tool-count boundary status"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_native_image_count_boundary(pool: PgPool) {
    let (app, keys) = generation_gateway(&pool).await;
    for (count, status) in [(20, StatusCode::OK), (21, StatusCode::BAD_REQUEST)] {
        let body = json!({"model":"company/smart","max_tokens":32,"messages":[{"role":"user","content":vec![json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG_1X1}});count]}]});
        let captured = generation_capture(
            "/v1/messages",
            &body,
            app.clone()
                .oneshot(generation_http(
                    "/v1/messages",
                    &keys.personal_key.token,
                    &body,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            captured.status, status,
            "aggregate native image count status"
        );
    }
    assert_eq!(
        r::artifact()["paths"]["/v1/messages"]["post"]["x-omg-limits"]["images"]["maximum"],
        json!(20),
        "image-count contract drift"
    );
}

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
struct Adapter;
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

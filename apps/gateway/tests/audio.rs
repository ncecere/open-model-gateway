#![cfg(feature = "integration-tests")]
//! Speech routes over real HTTP + PostgreSQL with a fixture adapter:
//! multipart transcription, streamed speech, v3 settlement, cancellation,
//! auth/catalog/key/protocol enforcement and configured caps.

use async_trait::async_trait;
use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use open_model_gateway::{
    billing::MeterUsage,
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    calls: AtomicUsize,
    hang: bool,
    dropped: Arc<AtomicBool>,
}
struct DropSignal(Arc<AtomicBool>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl ProviderAdapter for Fixture {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::AudioTranscriptions | ApiProtocol::AudioSpeech
        )
    }
    fn supports_transcription_request(&self, _: &Deployment, _: &TranscriptionRequest) -> bool {
        true
    }
    fn supports_speech_request(&self, _: &Deployment, _: &SpeechRequest) -> bool {
        true
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_audio_transcription(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        assert_eq!(request.duration_ms, Some(850));
        if request.language.as_deref() == Some("xx") {
            // e.g. OpenRouter 402 without credit.
            return Err(InferenceError::Configuration);
        }
        Ok(TranscriptionResponse {
            text: "hello world".into(),
            usage: Usage {
                meters: Some(MeterUsage {
                    output_images: Some(0),
                    input_characters: Some(0),
                    input_audio_seconds_ms: Some(1000),
                    output_audio_seconds_ms: Some(0),
                    search_units: Some(0),
                    requests: Some(1),
                    output_video_seconds_ms: None,
                }),
                ..Default::default()
            },
        })
    }
    async fn execute_audio_speech(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        let signal = DropSignal(self.dropped.clone());
        let hang = self.hang;
        Ok(SpeechResponse {
            audio: Box::pin(async_stream::stream! {
                let _signal = signal;
                yield Ok(Bytes::from_static(b"\xff\xf3\xc4"));
                if hang {
                    std::future::pending::<()>().await;
                }
                yield Ok(Bytes::from_static(b"tail"));
            }),
            content_type: request.response_format.content_type(),
            usage: request.usage(),
        })
    }
}

async fn setup(
    pool: &PgPool,
    protocol: &str,
    hang: bool,
    limits: EngineLimits,
) -> (axum::Router, DevelopmentKeys, Arc<Fixture>) {
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
    model_protocol(pool, protocol).await;
    let adapter = Arc::new(Fixture {
        calls: AtomicUsize::new(0),
        hang,
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, limits).unwrap();
    (http::router_with_engine(store, None, engine), keys, adapter)
}
async fn model_protocol(pool: &PgPool, protocol: &str) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY[$1]")
        .bind(protocol)
        .execute(pool)
        .await
        .unwrap();
}
async fn v3_price(pool: &PgPool, priced: Value) {
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(pool)
        .await
        .unwrap();
    let na = |m: &str| json!({"meter":m,"not_applicable":true});
    let mut lines: Vec<Value> = [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
    ]
    .into_iter()
    .filter(|m| priced["meter"] != *m)
    .map(na)
    .collect();
    lines.push(priced);
    lines.push(json!({"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,0,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(pool).await.unwrap();
}

/// PCM WAV: 16 kHz mono 16-bit silence.
fn wav(ms: u64) -> Vec<u8> {
    let data = (16_000 * 2 * ms / 1000) as u32;
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for (v, w) in [
        (16u32, 4),
        (1, 2),
        (1, 2),
        (16_000, 4),
        (32_000, 4),
        (2, 2),
        (16, 2),
    ] {
        b.extend_from_slice(&v.to_le_bytes()[..w]);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(b.len() + data as usize, 0);
    b
}
const BOUNDARY: &str = "audio-test-boundary";
fn multipart(fields: &[(&str, &str)], filename: &str, audio: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}
fn transcription_request(key: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header("authorization", format!("Bearer {key}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}
fn speech_request(key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/audio/speech")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn speech_body() -> Value {
    json!({"model":"company/smart","input":"Hello world!","voice":"alloy"})
}
async fn read(response: axum::response::Response) -> (StatusCode, String, Vec<u8>) {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (
        status,
        content_type,
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
}
fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn transcription_reaches_http_with_per_minute_settlement(pool: PgPool) {
    let (app, keys, adapter) = setup(
        &pool,
        "audio_transcriptions",
        false,
        EngineLimits::default(),
    )
    .await;
    v3_price(
        &pool,
        json!({"meter":"input_audio_seconds_ms","microusd_per_batch":"6000","batch":60000,"unit_label":"/minute","sku_label":"Audio"}),
    )
    .await;
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',200)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let audio = wav(850);
    let response = app
        .clone()
        .oneshot(transcription_request(
            &keys.personal_key.token,
            multipart(
                &[("model", "company/smart")],
                "../secret/meeting.wav",
                &audio,
            ),
        ))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let (status, content_type, body) = read(response).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(content_type.starts_with("application/json"));
    assert_eq!(
        json_of(&body),
        json!({"text":"hello world","usage":{"type":"duration","seconds":1.0}})
    );
    let text = String::from_utf8_lossy(&body);
    for leaked in ["private-upstream-id", "meeting", "secret"] {
        assert!(!text.contains(leaked), "{leaked}");
    }
    let row: (String, String, i64, i64, Value) = sqlx::query_as("SELECT e.workload_kind,r.state,r.held_microusd,r.actual_microusd,r.meter_usage FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1::uuid")
        .bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2, row.3),
        ("audio_transcriptions", "settled", 200, 100)
    );
    assert_eq!(row.4["input_audio_seconds_ms"], "1000");
    // Text format; the remaining 100 µUSD only cover a measured second... the
    // 2 s ceiling hold (200) is now denied.
    let (status, _, body) = read(
        app.clone()
            .oneshot(transcription_request(
                &keys.personal_key.token,
                multipart(
                    &[("model", "company/smart"), ("response_format", "text")],
                    "a.wav",
                    &audio,
                ),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_of(&body)["error"]["code"], "budget_exceeded");
    sqlx::query("DELETE FROM policy_budgets WHERE layer='installation'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, content_type, body) = read(
        app.clone()
            .oneshot(transcription_request(
                &keys.personal_key.token,
                multipart(
                    &[("model", "company/smart"), ("response_format", "text")],
                    "a.wav",
                    &audio,
                ),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/plain; charset=utf-8");
    assert_eq!(body, b"hello world");
    // Sanitized provider configuration error (e.g. upstream 402).
    let (status, _, body) = read(
        app.oneshot(transcription_request(
            &keys.personal_key.token,
            multipart(
                &[("model", "company/smart"), ("language", "xx")],
                "a.wav",
                &audio,
            ),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_of(&body)["error"]["code"],
        "provider_configuration_error"
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn speech_streams_binary_audio_with_per_character_settlement(pool: PgPool) {
    let (app, keys, _) = setup(&pool, "audio_speech", false, EngineLimits::default()).await;
    v3_price(
        &pool,
        json!({"meter":"input_characters","microusd_per_batch":"15000000","batch":1000000,"unit_label":"/M characters","sku_label":"Characters"}),
    )
    .await;
    let response = app
        .clone()
        .oneshot(speech_request(&keys.personal_key.token, speech_body()))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let (status, content_type, body) = read(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "audio/mpeg");
    assert_eq!(body, b"\xff\xf3\xc4tail");
    let row: (String, bool, String, i64, i64) = sqlx::query_as("SELECT e.workload_kind,e.streamed,r.state,r.held_microusd,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1::uuid")
        .bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        ("audio_speech".into(), true, "settled".into(), 180, 180)
    );
    // Explicit format → explicit content type.
    let mut wav_body = speech_body();
    wav_body["response_format"] = json!("wav");
    let (status, content_type, _) = read(
        app.oneshot(speech_request(&keys.personal_key.token, wav_body))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, content_type.as_str()),
        (StatusCode::OK, "audio/wav")
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dropping_http_speech_body_cancels_upstream_and_keeps_the_hold(pool: PgPool) {
    let (app, keys, adapter) = setup(&pool, "audio_speech", true, EngineLimits::default()).await;
    v3_price(
        &pool,
        json!({"meter":"input_characters","microusd_per_batch":"15000000","batch":1000000,"unit_label":"/M characters","sku_label":"Characters"}),
    )
    .await;
    let response = app
        .oneshot(speech_request(&keys.personal_key.token, speech_body()))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut body = response.into_body();
    let frame = body.frame().await.unwrap().unwrap();
    assert_eq!(&frame.into_data().unwrap()[..], b"\xff\xf3\xc4");
    assert!(!adapter.dropped.load(Ordering::SeqCst));
    drop(body);
    assert!(adapter.dropped.load(Ordering::SeqCst));
    let mut state = String::new();
    for _ in 0..100 {
        state = sqlx::query_scalar("SELECT state FROM inference_executions WHERE id=$1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if state != "started" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state, "cancelled");
    let held: (String, i64) = sqlx::query_as(
        "SELECT state,held_microusd FROM governance_reservations WHERE execution_id=$1::uuid",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held, ("unknown".into(), 180));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn audio_routes_enforce_auth_catalog_key_protocol_and_caps(pool: PgPool) {
    let mut limits = EngineLimits::default();
    limits.audio.max_upload_bytes = 16 * 1024;
    limits.audio.max_speech_input_chars = 12;
    limits.workloads.audio_transcriptions_body_bytes = 64 * 1024;
    let (app, keys, adapter) = setup(&pool, "audio_speech", false, limits).await;
    let audio = wav(100);
    let form = || multipart(&[("model", "company/smart")], "a.wav", &audio);
    // Unauthenticated and invalid keys.
    for request in [
        transcription_request("omg_live_not_a_real_key", form()),
        speech_request("omg_live_not_a_real_key", speech_body()),
    ] {
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    // A speech model is not a transcription model: explicit 400, no dispatch.
    let (status, _, body) = read(
        app.clone()
            .oneshot(transcription_request(&keys.personal_key.token, form()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&body)["error"]["code"], "unsupported_capability");
    // Configured caps: 13 characters, a 32 KiB file, a 100 KiB body.
    let mut long = speech_body();
    long["input"] = json!("\u{e9}".repeat(13));
    let (status, _, body) = read(
        app.clone()
            .oneshot(speech_request(&keys.personal_key.token, long))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_of(&body)["error"]["code"], "invalid_request_error");
    model_protocol(&pool, "audio_transcriptions").await;
    for (bytes, expected) in [
        (wav(1000), StatusCode::PAYLOAD_TOO_LARGE),
        (wav(3200), StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let response = app
            .clone()
            .oneshot(transcription_request(
                &keys.personal_key.token,
                multipart(&[("model", "company/smart")], "a.wav", &bytes),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    // Malformed multipart and unsupported options never dispatch.
    for (body, code) in [
        (b"not multipart".to_vec(), "invalid_request_error"),
        (
            multipart(&[("model", "company/smart")], "a.txt", &audio),
            "invalid_request_error",
        ),
        (
            multipart(
                &[
                    ("model", "company/smart"),
                    ("response_format", "verbose_json"),
                ],
                "a.wav",
                &audio,
            ),
            "unsupported_capability",
        ),
    ] {
        let (status, _, body) = read(
            app.clone()
                .oneshot(transcription_request(&keys.personal_key.token, body))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            (status, json_of(&body)["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code))
        );
    }
    // Workspace catalog denial.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, body) = read(
        app.clone()
            .oneshot(transcription_request(&keys.team_key.token, form()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("model_not_found"))
    );
    // Key model restriction without a selection.
    let governance: Uuid = sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
        .bind(keys.personal_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)")
        .bind(keys.personal_workspace_id)
        .bind(governance)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(transcription_request(&keys.personal_key.token, form()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

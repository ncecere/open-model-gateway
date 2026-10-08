//! OpenAI speech mock contracts: multipart re-encoding, usage normalization,
//! binary passthrough, cancellation and sanitized errors.
use super::*;
use crate::{
    inference::audio::{AudioStream, TranscriptionFormat},
    providers::{
        ProviderAdapter,
        secrets::{Secret, SecretResolver},
    },
};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use futures_util::StreamExt;
use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "test-key-reference");
        Secret::new("local-mock-key".into())
    }
}
struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Vec<u8>,
}
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    dropped: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct DropSignal(Arc<AtomicBool>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl Mock {
    /// Reply with `chunks`; `hang` keeps the body open after them.
    async fn serve(
        status: u16,
        content_type: &'static str,
        chunks: Vec<Vec<u8>>,
        hang: bool,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicBool::new(false));
        let (capture, signal) = (requests.clone(), dropped.clone());
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let (capture, signal, chunks) = (capture.clone(), signal.clone(), chunks.clone());
            async move {
                capture.lock().unwrap().push(Captured {
                    headers,
                    uri,
                    body: body.to_vec(),
                });
                let guard = DropSignal(signal);
                let stream = async_stream::stream! {
                    let _guard = guard;
                    for chunk in chunks {
                        yield Ok::<_, Infallible>(Bytes::from(chunk));
                    }
                    if hang {
                        std::future::pending::<()>().await;
                    }
                };
                Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            dropped,
            task,
        }
    }
    fn adapter(&self) -> OpenAiAdapter {
        OpenAiAdapter::for_test(Arc::new(Resolver), self.base.clone())
    }
}
fn target(model: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: model.into(),
        credential_ref: "test-key-reference".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![],
    }
}
fn wav() -> Vec<u8> {
    crate::inference::audio::tests_support::wav(850)
}
fn transcription() -> TranscriptionRequest {
    TranscriptionRequest::new(
        "company/whisper".into(),
        Arc::from(wav()),
        AudioFormat::Wav,
        Some("en".into()),
        Some("Glossary: Ada".into()),
        Some(0.2),
        TranscriptionFormat::Text,
    )
}
fn speech(format: SpeechFormat) -> SpeechRequest {
    SpeechRequest {
        model: "company/tts".into(),
        input: "Hello".into(),
        voice: "alloy".into(),
        response_format: format,
        speed: Some(1.25),
    }
}
/// `name → (filename, content type, data)` from a gateway-built form.
type FormPart = (String, Option<String>, Option<String>, Vec<u8>);
fn form(c: &Captured) -> Vec<FormPart> {
    let ct = c.headers[header::CONTENT_TYPE].to_str().unwrap();
    let boundary = crate::protocols::audio::multipart::boundary(ct).unwrap();
    crate::protocols::audio::multipart::parse(&c.body, &boundary, 16)
        .unwrap()
        .into_iter()
        .map(|p| (p.name, p.filename, p.content_type, p.data.to_vec()))
        .collect()
}

#[tokio::test]
async fn transcription_reencodes_multipart_and_normalizes_duration_usage() {
    let mock = Mock::serve(
        200,
        "application/json",
        vec![br#"{"text":"hi there","usage":{"type":"duration","seconds":1}}"#.to_vec()],
        false,
    )
    .await;
    let adapter = mock.adapter();
    assert!(adapter.supports_protocol(ApiProtocol::AudioTranscriptions));
    let response = adapter
        .execute_audio_transcription(&target("whisper-1"), transcription())
        .await
        .unwrap();
    assert_eq!(response.text, "hi there");
    let meters = response.usage.meters.unwrap();
    assert_eq!(
        (meters.input_audio_seconds_ms, meters.requests),
        (Some(1000), Some(1))
    );
    assert_eq!(response.usage.input_tokens, None);
    let requests = mock.requests.lock().unwrap();
    let c = &requests[0];
    assert_eq!(c.uri.path(), "/v1/audio/transcriptions");
    assert_eq!(c.headers[header::AUTHORIZATION], "Bearer local-mock-key");
    let parts = form(c);
    let names: Vec<_> = parts.iter().map(|p| p.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "model",
            "response_format",
            "language",
            "prompt",
            "temperature",
            "file"
        ]
    );
    let value =
        |n: &str| String::from_utf8(parts.iter().find(|p| p.0 == n).unwrap().3.clone()).unwrap();
    assert_eq!(value("model"), "whisper-1");
    // Upstream always returns JSON (usage); text is rendered by the gateway.
    assert_eq!(value("response_format"), "json");
    assert_eq!(value("temperature"), "0.2");
    let file = parts.iter().find(|p| p.0 == "file").unwrap();
    assert_eq!(file.1.as_deref(), Some("audio.wav"));
    assert_eq!(file.2.as_deref(), Some("audio/wav"));
    assert_eq!(file.3, wav());
}

#[tokio::test]
async fn token_billed_transcription_records_tokens_and_measured_duration() {
    let mock = Mock::serve(
        200,
        "application/json",
        vec![br#"{"text":"hi","usage":{"type":"tokens","total_tokens":14,"input_tokens":8,"input_token_details":{"text_tokens":0,"audio_tokens":8},"output_tokens":6}}"#.to_vec()],
        false,
    )
    .await;
    let response = mock
        .adapter()
        .execute_audio_transcription(&target("gpt-4o-mini-transcribe"), transcription())
        .await
        .unwrap();
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens),
        (Some(8), Some(6))
    );
    assert_eq!(
        response.usage.meters.unwrap().input_audio_seconds_ms,
        Some(850)
    );
}

#[tokio::test]
async fn transcription_errors_are_sanitized_and_bounded() {
    for (status, content_type, body, expected) in [
        (
            401,
            "application/json",
            r#"{"error":{"message":"sk-secret"}}"#,
            InferenceError::Configuration,
        ),
        (429, "application/json", "{}", InferenceError::Busy),
        (
            400,
            "application/json",
            r#"{"error":{"message":"bad file"}}"#,
            InferenceError::UpstreamRejected,
        ),
        (
            503,
            "text/html",
            "<html>",
            InferenceError::UpstreamUnavailable,
        ),
        (302, "text/html", "", InferenceError::Configuration),
        (200, "text/html", "<html>", InferenceError::InvalidUpstream),
        (
            200,
            "application/json",
            r#"{"text":"x","segments":[{"text":"x"}]}"#,
            InferenceError::InvalidUpstream,
        ),
        (
            200,
            "application/json",
            r#"{"error":{"message":"x"}}"#,
            InferenceError::InvalidUpstream,
        ),
    ] {
        let mock = Mock::serve(status, content_type, vec![body.as_bytes().to_vec()], false).await;
        let error = mock
            .adapter()
            .execute_audio_transcription(&target("whisper-1"), transcription())
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected, "{status} {body}");
    }
    let huge = vec![b' '; 5 * 1024 * 1024];
    let mock = Mock::serve(200, "application/json", vec![huge], false).await;
    assert_eq!(
        mock.adapter()
            .execute_audio_transcription(&target("whisper-1"), transcription())
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    // Invalid connections fail before any request or credential use.
    let mock = Mock::serve(200, "application/json", vec![], false).await;
    let mut bad = target("whisper-1");
    bad.endpoint = Some("https://evil.example/v1".into());
    assert_eq!(
        mock.adapter()
            .execute_audio_transcription(&bad, transcription())
            .await
            .err(),
        Some(InferenceError::Configuration)
    );
    assert!(mock.requests.lock().unwrap().is_empty());
}

async fn drain(mut audio: AudioStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = audio.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn speech_passes_binary_audio_through_with_explicit_content_type() {
    let mock = Mock::serve(
        200,
        "audio/mpeg",
        vec![vec![], b"\xff\xf3\xc4".to_vec(), b"rest".to_vec()],
        false,
    )
    .await;
    let adapter = mock.adapter();
    let request = speech(SpeechFormat::Mp3);
    assert!(adapter.supports_speech_request(&target("tts-1"), &request));
    let response = adapter
        .execute_audio_speech(&target("tts-1"), request)
        .await
        .unwrap();
    assert_eq!(response.content_type, "audio/mpeg");
    let meters = response.usage.meters.unwrap();
    assert_eq!(
        (
            meters.input_characters,
            meters.output_audio_seconds_ms,
            meters.requests
        ),
        (Some(5), None, Some(1))
    );
    assert_eq!(drain(response.audio).await, b"\xff\xf3\xc4rest");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].uri.path(), "/v1/audio/speech");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({"model":"tts-1","input":"Hello","voice":"alloy","response_format":"mp3","speed":1.25})
    );
}

#[tokio::test]
async fn speech_rejects_wrong_media_types_and_empty_bodies() {
    for (content_type, chunks) in [
        (
            "application/json",
            vec![br#"{"error":{"message":"x"}}"#.to_vec()],
        ),
        ("audio/wav", vec![b"RIFF".to_vec()]),
        ("text/html", vec![b"<html>".to_vec()]),
        ("audio/mpeg", vec![]),
        ("audio/mpeg", vec![vec![]]),
    ] {
        let mock = Mock::serve(200, content_type, chunks, false).await;
        assert_eq!(
            mock.adapter()
                .execute_audio_speech(&target("tts-1"), speech(SpeechFormat::Mp3))
                .await
                .err(),
            Some(InferenceError::InvalidUpstream),
            "{content_type}"
        );
    }
    let mock = Mock::serve(200, "audio/pcm", vec![b"\0\0".to_vec()], false).await;
    let response = mock
        .adapter()
        .execute_audio_speech(&target("gpt-4o-mini-tts"), speech(SpeechFormat::Pcm))
        .await
        .unwrap();
    assert_eq!(response.content_type, "audio/pcm");
    let mock = Mock::serve(400, "application/json", vec![b"{}".to_vec()], false).await;
    assert_eq!(
        mock.adapter()
            .execute_audio_speech(&target("tts-1"), speech(SpeechFormat::Wav))
            .await
            .err(),
        Some(InferenceError::UpstreamRejected)
    );
}

#[tokio::test]
async fn dropping_the_speech_stream_closes_the_upstream_connection() {
    let mock = Mock::serve(200, "audio/mpeg", vec![b"\xff\xf3".to_vec()], true).await;
    let mut response = mock
        .adapter()
        .execute_audio_speech(&target("tts-1"), speech(SpeechFormat::Mp3))
        .await
        .unwrap();
    assert_eq!(
        &response.audio.next().await.unwrap().unwrap()[..],
        b"\xff\xf3"
    );
    assert!(!mock.dropped.load(Ordering::SeqCst));
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !mock.dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("upstream response body was not dropped after client cancellation");
}

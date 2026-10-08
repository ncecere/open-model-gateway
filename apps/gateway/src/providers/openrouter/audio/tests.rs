//! OpenRouter speech mock contracts.
use super::*;
use crate::{
    inference::audio::{AudioStream, TranscriptionFormat},
    providers::{
        ProviderAdapter,
        openrouter::{DataCollection, OpenRouterConfig},
        secrets::{Secret, SecretResolver},
    },
};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use futures_util::StreamExt;
use reqwest::header;
use serde_json::Value;
use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const USER_ID: &str = "user_2xSecretAccountIdentifier";

#[derive(Default)]
struct Resolver {
    calls: AtomicUsize,
}
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "env:OPENROUTER_TEST_KEY");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Secret::new("local-mock-openrouter-key".into())
    }
}
struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Value,
}
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn serve(status: u16, content_type: &'static str, chunks: Vec<Vec<u8>>) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let capture = requests.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let (capture, chunks) = (capture.clone(), chunks.clone());
            async move {
                capture.lock().unwrap().push(Captured {
                    headers,
                    uri,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let stream = futures_util::stream::iter(
                    chunks
                        .into_iter()
                        .map(|c| Ok::<_, Infallible>(Bytes::from(c))),
                );
                Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }
    fn adapter(&self) -> OpenRouterAdapter {
        OpenRouterAdapter::for_test(
            Arc::new(Resolver::default()),
            OpenRouterConfig::new(None, None, DataCollection::Deny).unwrap(),
            self.base.clone(),
        )
    }
}
fn target(model: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openrouter".into(),
        upstream_model: model.into(),
        credential_ref: "env:OPENROUTER_TEST_KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![],
    }
}
fn transcription(prompt: Option<&str>) -> TranscriptionRequest {
    TranscriptionRequest::new(
        "company/whisper".into(),
        Arc::from(crate::inference::audio::tests_support::wav(850)),
        AudioFormat::Wav,
        Some("en".into()),
        prompt.map(str::to_owned),
        None,
        TranscriptionFormat::Json,
    )
}
fn speech(format: SpeechFormat) -> SpeechRequest {
    SpeechRequest {
        model: "company/voice".into(),
        input: "Hi there".into(),
        voice: "en-US-Harper:MAI-Voice-2".into(),
        response_format: format,
        speed: None,
    }
}

#[test]
fn base64_matches_rfc4648_vectors() {
    for (raw, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(raw.as_bytes()), encoded);
    }
    assert_eq!(base64(&[0xFB, 0xFF]), "+/8=");
}

#[tokio::test]
async fn transcription_sends_base64_json_and_captures_seconds_and_cost() {
    let mock = Mock::serve(
        200,
        "application/json",
        vec![br#"{"text":"hello","usage":{"seconds":0.85,"total_tokens":3,"input_tokens":2,"output_tokens":1,"cost":0.0000028305}}"#.to_vec()],
    )
    .await;
    let adapter = mock.adapter();
    assert!(adapter.supports_protocol(ApiProtocol::AudioTranscriptions));
    let response = adapter
        .execute_audio_transcription(
            &target("openai/whisper-large-v3-turbo"),
            transcription(None),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "hello");
    assert_eq!(
        response.usage.meters.unwrap().input_audio_seconds_ms,
        Some(850)
    );
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens),
        (Some(2), Some(1))
    );
    assert_eq!(response.usage.provider_cost_microusd, Some(3));
    let requests = mock.requests.lock().unwrap();
    let c = &requests[0];
    assert_eq!(c.uri.path(), "/api/v1/audio/transcriptions");
    assert_eq!(
        c.headers[header::AUTHORIZATION],
        "Bearer local-mock-openrouter-key"
    );
    assert_eq!(c.body["model"], "openai/whisper-large-v3-turbo");
    assert_eq!(c.body["response_format"], "json");
    assert_eq!(c.body["language"], "en");
    assert_eq!(c.body["provider"], json!({"data_collection":"deny"}));
    assert_eq!(c.body["input_audio"]["format"], "wav");
    assert_eq!(
        c.body["input_audio"]["data"],
        base64(&crate::inference::audio::tests_support::wav(850))
    );
    assert!(c.body.get("prompt").is_none() && c.body.get("temperature").is_none());
}

#[tokio::test]
async fn transcription_without_credit_is_a_configuration_error_and_prompt_is_unsupported() {
    let body = format!(
        r#"{{"error":{{"message":"This request requires at least $0.50 in balance for audio","code":402,"metadata":{{"limit_source":"openrouter_credits"}}}},"user_id":"{USER_ID}"}}"#
    );
    let mock = Mock::serve(402, "application/json", vec![body.into_bytes()]).await;
    let adapter = mock.adapter();
    let error = adapter
        .execute_audio_transcription(
            &target("openai/whisper-large-v3-turbo"),
            transcription(None),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, InferenceError::Configuration);
    assert_eq!(error.code(), "provider_configuration_error");
    assert!(!error.message().contains(USER_ID));
    // A 200 envelope carrying an error is mapped by code too.
    let mock = Mock::serve(
        200,
        "application/json",
        vec![br#"{"error":{"code":402,"message":"x"},"user_id":"u"}"#.to_vec()],
    )
    .await;
    assert_eq!(
        mock.adapter()
            .execute_audio_transcription(&target("m"), transcription(None))
            .await
            .err(),
        Some(InferenceError::Configuration)
    );
    // OpenRouter ignores `prompt`: never silently dropped, never sent.
    let request = transcription(Some("Glossary"));
    assert!(!adapter.supports_transcription_request(&target("m"), &request));
    assert_eq!(
        adapter
            .execute_audio_transcription(&target("m"), request)
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn speech_streams_raw_mp3_and_values_characters_locally() {
    let mock = Mock::serve(
        200,
        "audio/mpeg",
        vec![b"\xff\xf3\xe4\xc4".to_vec(), b"more".to_vec()],
    )
    .await;
    let adapter = mock.adapter();
    let response = adapter
        .execute_audio_speech(
            &target("microsoft/mai-voice-2-flash"),
            speech(SpeechFormat::Mp3),
        )
        .await
        .unwrap();
    assert_eq!(response.content_type, "audio/mpeg");
    assert_eq!(response.usage.provider_cost_microusd, None);
    assert_eq!(response.usage.meters.unwrap().input_characters, Some(8));
    let mut audio: AudioStream = response.audio;
    let mut bytes = Vec::new();
    while let Some(chunk) = audio.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes, b"\xff\xf3\xe4\xc4more");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].uri.path(), "/api/v1/audio/speech");
    assert_eq!(
        requests[0].body,
        json!({"model":"microsoft/mai-voice-2-flash","input":"Hi there","voice":"en-US-Harper:MAI-Voice-2","response_format":"mp3","provider":{"data_collection":"deny"}})
    );
}

#[tokio::test]
async fn speech_formats_errors_and_json_envelopes() {
    let mock = Mock::serve(200, "audio/mpeg", vec![b"x".to_vec()]).await;
    let adapter = mock.adapter();
    for format in [SpeechFormat::Wav, SpeechFormat::Opus] {
        let request = speech(format);
        assert!(!adapter.supports_speech_request(&target("m"), &request));
        assert_eq!(
            adapter
                .execute_audio_speech(&target("m"), request)
                .await
                .err(),
            Some(InferenceError::Unsupported)
        );
    }
    assert!(mock.requests.lock().unwrap().is_empty());
    for (status, content_type, body, expected) in [
        (
            200,
            "application/json",
            r#"{"error":{"code":429,"message":"x"}}"#,
            InferenceError::Busy,
        ),
        (
            200,
            "application/json",
            r#"{"ok":true}"#,
            InferenceError::InvalidUpstream,
        ),
        (200, "audio/wav", "RIFF", InferenceError::InvalidUpstream),
        (
            401,
            "application/json",
            r#"{"error":{"code":401}}"#,
            InferenceError::Configuration,
        ),
        (
            400,
            "application/json",
            r#"{"error":{"code":400}}"#,
            InferenceError::UpstreamRejected,
        ),
        (
            502,
            "text/html",
            "<html>",
            InferenceError::UpstreamUnavailable,
        ),
    ] {
        let mock = Mock::serve(status, content_type, vec![body.as_bytes().to_vec()]).await;
        assert_eq!(
            mock.adapter()
                .execute_audio_speech(&target("m"), speech(SpeechFormat::Mp3))
                .await
                .err(),
            Some(expected),
            "{status} {body}"
        );
    }
    let mock = Mock::serve(200, "audio/pcm", vec![b"\0\0".to_vec()]).await;
    assert_eq!(
        mock.adapter()
            .execute_audio_speech(&target("m"), speech(SpeechFormat::Pcm))
            .await
            .unwrap()
            .content_type,
        "audio/pcm"
    );
}

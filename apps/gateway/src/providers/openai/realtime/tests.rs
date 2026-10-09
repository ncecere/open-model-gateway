use std::{sync::atomic::Ordering, time::Duration};

use super::{
    mock::{Init, Mock, Reply, SERVER_SECRET, UPSTREAM_MODEL, usage as mock_usage},
    *,
};
use crate::providers::secrets::{Secret, SecretResolver};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "env:OPENAI_API_KEY");
        Secret::new(SERVER_SECRET.into())
    }
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: UPSTREAM_MODEL.into(),
        credential_ref: "env:OPENAI_API_KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["realtime".into()],
    }
}
fn setup() -> RealtimeSetup {
    RealtimeSetup {
        window_output_tokens: 100,
        connect_timeout: Duration::from_secs(5),
    }
}

#[test]
fn usage_is_complete_and_consistent_or_unknown() {
    let u = usage(&mock_usage(10, 4, 20, 5, 10, 40)).unwrap();
    assert_eq!(
        u,
        RealtimeUsage {
            input_text_tokens: 10,
            cached_text_tokens: 4,
            input_audio_tokens: 20,
            cached_audio_tokens: 5,
            output_text_tokens: 10,
            output_audio_tokens: 40,
        }
    );
    // No cache details is fine only when nothing was cached.
    let mut v = mock_usage(10, 0, 20, 0, 1, 2);
    v["input_token_details"]
        .as_object_mut()
        .unwrap()
        .remove("cached_tokens_details");
    assert!(usage(&v).is_some());
    let mut v = mock_usage(10, 4, 20, 0, 1, 2);
    v["input_token_details"]
        .as_object_mut()
        .unwrap()
        .remove("cached_tokens_details");
    assert_eq!(usage(&v), None);
    for broken in [
        |v: &mut Value| v["input_tokens"] = json!(31),
        |v: &mut Value| v["output_tokens"] = json!(49),
        |v: &mut Value| v["total_tokens"] = json!(1),
        |v: &mut Value| v["input_token_details"]["image_tokens"] = json!(3),
        |v: &mut Value| v["input_token_details"]["cached_tokens"] = json!(8),
        |v: &mut Value| v["input_token_details"]["audio_tokens"] = json!("20"),
        |v: &mut Value| v["output_token_details"] = Value::Null,
        |v: &mut Value| {
            v["input_token_details"]["cached_tokens_details"]["text_tokens"] = json!(11)
        },
        |v: &mut Value| v["input_token_details"]["text_tokens"] = json!(-1),
    ] {
        let mut v = mock_usage(10, 4, 20, 5, 10, 40);
        broken(&mut v);
        assert_eq!(usage(&v), None, "{v}");
    }
    assert_eq!(usage(&Value::Null), None);
}

#[test]
fn classification_bounds_identifiers_and_never_forwards_upstream_messages() {
    let done = json!({"type":"response.done","response":{"id":"resp_1","status":"cancelled","usage":mock_usage(1,0,0,0,1,0)}}).to_string();
    match classify(&done).unwrap() {
        UpstreamEvent::ResponseDone {
            response_id,
            status,
            usage,
            text,
        } => {
            assert_eq!(response_id, "resp_1");
            assert_eq!(status, Some(ResponseStatus::Cancelled));
            assert!(usage.is_some());
            assert_eq!(text, done);
        }
        _ => panic!("response.done"),
    }
    let done = json!({"type":"response.done","response":{"id":"resp_1","status":"in_progress"}});
    assert!(matches!(
        classify(&done.to_string()).unwrap(),
        UpstreamEvent::ResponseDone {
            status: None,
            usage: None,
            ..
        }
    ));
    assert!(classify(&json!({"type":"response.created","response":{}}).to_string()).is_err());
    let error = json!({"type":"error","error":{"type":"invalid_request_error","code":"bad value","message":"org-123","param":"session.model","event_id":"evt 1"}});
    match classify(&error.to_string()).unwrap() {
        UpstreamEvent::Error {
            kind,
            code,
            param,
            event_id,
        } => {
            assert_eq!(kind.as_deref(), Some("invalid_request_error"));
            assert_eq!(code, None, "spaces are not a bounded token");
            assert_eq!(param.as_deref(), Some("session.model"));
            assert_eq!(event_id, None);
        }
        _ => panic!("error"),
    }
    assert!(matches!(
        classify(r#"{"type":"rate_limits.updated","rate_limits":[]}"#).unwrap(),
        UpstreamEvent::Filtered
    ));
    assert!(matches!(
        classify(r#"{"type":"conversation.item.input_audio_transcription.completed","usage":{}}"#)
            .unwrap(),
        UpstreamEvent::Unaccounted
    ));
    assert!(matches!(
        classify(r#"{"type":"response.output_audio.delta","delta":"AA=="}"#).unwrap(),
        UpstreamEvent::Forward(_)
    ));
    assert!(classify("not json").is_err());
    assert!(classify(r#"{"no":"type"}"#).is_err());
}

#[test]
fn only_sessions_without_self_started_work_are_safe() {
    let safe = json!({"audio":{"input":{"turn_detection":{"type":"server_vad","create_response":false,"idle_timeout_ms":null},"transcription":null}}});
    assert!(session_safe(&safe));
    assert!(session_safe(
        &json!({"audio":{"input":{"turn_detection":null}}})
    ));
    assert!(session_safe(&json!({"type":"realtime"})));
    for unsafe_session in [
        json!({"audio":{"input":{"turn_detection":{"type":"server_vad","create_response":true}}}}),
        json!({"audio":{"input":{"turn_detection":{"type":"semantic_vad"}}}}),
        json!({"audio":{"input":{"turn_detection":{"type":"server_vad","create_response":false,"idle_timeout_ms":5000}}}}),
        json!({"audio":{"input":{"turn_detection":null,"transcription":{"model":"gpt-4o-transcribe"}}}}),
    ] {
        assert!(!session_safe(&unsafe_session), "{unsafe_session}");
    }
    let init = init_event(&setup());
    assert_eq!(init["session"]["max_output_tokens"], 100);
    assert!(session_safe(&init["session"]));
}

#[tokio::test]
async fn connect_uses_server_credential_enforces_configuration_and_closes_on_drop() {
    let mock = Mock::start(Init::Safe, vec![Reply::Done(mock_usage(1, 0, 2, 0, 3, 4))]).await;
    let adapter = test_adapter(std::sync::Arc::new(Resolver), mock.base.clone());
    let mut upstream = adapter
        .connect_realtime_for_test(&target(), &setup())
        .await
        .unwrap();
    {
        let handshakes = mock.shared.handshakes.lock().unwrap();
        let (uri, headers) = &handshakes[0];
        assert_eq!(uri, &format!("/v1/realtime?model={UPSTREAM_MODEL}"));
        let auth: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k == "authorization")
            .collect();
        assert_eq!(
            auth,
            [&(
                "authorization".to_owned(),
                format!("Bearer {SERVER_SECRET}")
            )]
        );
        assert!(
            !headers
                .iter()
                .any(|(k, _)| k == "sec-websocket-protocol" || k == "openai-beta")
        );
    }
    assert_eq!(mock.shared.received_types(), ["session.update"]);
    // session.created then the acknowledging session.updated are replayed in order.
    let first = upstream.events.next().await.unwrap().unwrap();
    assert!(
        matches!(&first, UpstreamEvent::Session { event, .. } if event["type"] == "session.created")
    );
    let second = upstream.events.next().await.unwrap().unwrap();
    assert!(
        matches!(&second, UpstreamEvent::Session { event, safe: true } if event["type"] == "session.updated")
    );
    upstream
        .sink
        .send(
            json!({"type":"response.create","event_id":"e1","response":{"max_output_tokens":100}})
                .to_string(),
        )
        .await
        .unwrap();
    assert!(matches!(
        upstream.events.next().await.unwrap().unwrap(),
        UpstreamEvent::ResponseCreated { .. }
    ));
    assert!(matches!(
        upstream.events.next().await.unwrap().unwrap(),
        UpstreamEvent::Forward(_)
    ));
    // Upstream rate limits are filtered, not forwarded.
    assert!(matches!(
        upstream.events.next().await.unwrap().unwrap(),
        UpstreamEvent::Filtered
    ));
    assert!(matches!(
        upstream.events.next().await.unwrap().unwrap(),
        UpstreamEvent::ResponseDone { usage: Some(_), .. }
    ));
    drop(upstream);
    assert!(
        mock.shared.wait_closed().await,
        "dropping the session closes upstream"
    );
}

#[tokio::test]
async fn connect_fails_closed_on_rejected_or_unsafe_configuration_and_maps_handshake_status() {
    let adapter = |mock: &Mock| test_adapter(std::sync::Arc::new(Resolver), mock.base.clone());
    let mock = Mock::start(Init::Reject, vec![]).await;
    assert_eq!(
        adapter(&mock)
            .connect_realtime_for_test(&target(), &setup())
            .await
            .err(),
        Some(InferenceError::UpstreamRejected)
    );
    let mock = Mock::start(Init::Unsafe, vec![]).await;
    assert_eq!(
        adapter(&mock)
            .connect_realtime_for_test(&target(), &setup())
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    for (status, error) in [
        (401, InferenceError::Configuration),
        (429, InferenceError::Busy),
        (503, InferenceError::UpstreamUnavailable),
        (302, InferenceError::Configuration),
    ] {
        let mock = Mock::start(Init::Status(status), vec![]).await;
        assert_eq!(
            adapter(&mock)
                .connect_realtime_for_test(&target(), &setup())
                .await
                .err(),
            Some(error),
            "{status}"
        );
    }
    // Connection validation happens before any credential or network use.
    let mock = Mock::start(Init::Safe, vec![]).await;
    for bad in [
        Deployment {
            credential_ref: "none".into(),
            ..target()
        },
        Deployment {
            endpoint: Some("https://evil.example/v1".into()),
            ..target()
        },
        Deployment {
            provider: "openrouter".into(),
            ..target()
        },
        Deployment {
            upstream_model: " ".into(),
            ..target()
        },
    ] {
        assert_eq!(
            adapter(&mock)
                .connect_realtime_for_test(&bad, &setup())
                .await
                .err(),
            Some(InferenceError::Configuration)
        );
    }
    assert_eq!(mock.shared.connections.load(Ordering::SeqCst), 0);
}

impl OpenAiAdapter {
    async fn connect_realtime_for_test(
        &self,
        target: &Deployment,
        setup: &RealtimeSetup,
    ) -> Result<RealtimeUpstream> {
        connect(self, target, setup).await
    }
}

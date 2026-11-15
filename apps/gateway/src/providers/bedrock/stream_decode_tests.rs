//! #486: how a ConverseStream exception frame is classified, from the real
//! frame bytes through the SDK's decoder. The gateway reads the class from the
//! frame's `:exception-type` header alone; AWS sends `{"message": ...}` as the
//! payload, so no payload field or message text is ever consulted.
use super::*;

/// An `exception` frame with exactly these headers and this raw payload.
fn exception_with(headers: &[(&str, Hdr)], payload: &[u8]) -> Vec<u8> {
    let mut all = vec![(":message-type", Hdr::Str("exception"))];
    all.extend_from_slice(headers);
    raw_frame(&all, payload)
}
/// A well-formed exception frame of this type.
fn exception(kind: &str) -> Vec<u8> {
    frame(
        kind,
        "exception",
        json!({"message":"upstream secret prompt"}),
    )
}
/// Runs a streamed request against `frames` and returns the events the client
/// would have received before the stream's single terminal error.
async fn stream_failure(frames: Vec<u8>, messages: bool) -> (Vec<ChatEvent>, InferenceError) {
    let mock = Mock::serve(complete(false), frames, 200).await;
    let mut req = request();
    req.stream = true;
    if messages {
        req.messages_options = Some(Box::default());
    }
    let Ok(ProviderOutput::Stream(mut output)) = mock.adapter().execute(&target(), req).await
    else {
        panic!("a 200 response must return a stream, however it later fails");
    };
    let mut seen = Vec::new();
    while let Some(event) = output.next().await {
        match event {
            Ok(ChatEvent::Done) => panic!("a failed stream emitted Done"),
            Ok(event) => seen.push(event),
            Err(error) => {
                assert!(output.next().await.is_none(), "nothing follows the error");
                return (seen, error);
            }
        }
    }
    panic!("the stream ended without an error");
}
/// The class for both client protocols.
async fn classes(frames: Vec<u8>) -> (InferenceError, InferenceError) {
    (
        stream_failure(frames.clone(), false).await.1,
        stream_failure(frames, true).await.1,
    )
}

/// The five types ConverseStream documents, plus `modelTimeoutException`, which
/// is documented only as an operation error and so reaches the decoder as an
/// unmodeled frame named by its header. Content already delivered stays
/// delivered, and no usage or finish is invented.
#[tokio::test]
async fn exception_frames_are_classified_by_their_type_before_and_after_content() {
    use InferenceError as E;
    // (exception type, class for chat completions, class for Messages)
    let cases = [
        ("throttlingException", E::Busy, E::Busy),
        (
            "validationException",
            E::UpstreamRejected,
            E::UpstreamRejected,
        ),
        // Messages alone reports provider unavailability as an overload, as
        // `sdk_messages_error` does for the same exception in an HTTP response.
        (
            "serviceUnavailableException",
            E::UpstreamUnavailable,
            E::Overloaded,
        ),
        (
            "internalServerException",
            E::UpstreamUnavailable,
            E::UpstreamUnavailable,
        ),
        (
            "modelStreamErrorException",
            E::UpstreamUnavailable,
            E::UpstreamUnavailable,
        ),
        ("modelTimeoutException", E::Timeout, E::Timeout),
        (
            "someFutureException",
            E::UpstreamUnavailable,
            E::UpstreamUnavailable,
        ),
    ];
    for (kind, chat, messages) in cases {
        for (is_messages, expected) in [(false, chat), (true, messages)] {
            // Before any content.
            let (seen, error) = stream_failure(exception(kind), is_messages).await;
            assert!(seen.is_empty(), "{kind}");
            assert_eq!(error, expected, "{kind} before content, {is_messages}");
            // After some content: what was delivered stays delivered.
            let mut bytes = wire(events(false)[..2].to_vec());
            bytes.extend(exception(kind));
            let (seen, error) = stream_failure(bytes, is_messages).await;
            assert_eq!(error, expected, "{kind} after content, {is_messages}");
            assert!(matches!(
                seen.as_slice(),
                [ChatEvent::Delta { text: Some(text), .. }] if text == "Hello"
            ));
            // After the terminal metadata (usage was delivered): still a
            // failure, never a late success.
            let mut bytes = wire(events(false));
            bytes.extend(exception(kind));
            let (seen, error) = stream_failure(bytes, is_messages).await;
            assert_eq!(error, expected, "{kind} after usage, {is_messages}");
            assert!(seen.iter().any(|e| matches!(e, ChatEvent::Usage(_))));
        }
    }
}

/// A code or type inside the payload, or words in the message, never
/// reclassify a frame whose `:exception-type` says otherwise.
#[tokio::test]
async fn exception_payload_text_never_changes_the_class() {
    for payload in [
        json!({"message":"throttling: too many requests","__type":"ThrottlingException"}),
        json!({"message":"validation failed","code":"validationException"}),
        json!({"message":"Input is too long for requested model."}),
    ] {
        let bytes = frame("internalServerException", "exception", payload);
        assert_eq!(
            classes(bytes).await,
            (
                InferenceError::UpstreamUnavailable,
                InferenceError::UpstreamUnavailable
            )
        );
    }
    // An unknown type with a known name in its payload stays unknown.
    let bytes = frame(
        "someFutureException",
        "exception",
        json!({"message":"x","__type":"ThrottlingException"}),
    );
    assert_eq!(
        stream_failure(bytes, false).await.1,
        InferenceError::UpstreamUnavailable
    );
}

/// The type header must be present and a string. An absent header, or one of
/// any other type, is rejected by the SDK's header parsing before any
/// exception exists (a response error, so `InvalidUpstream`); an empty string
/// is a type nobody knows (`UpstreamUnavailable`).
#[tokio::test]
async fn exception_type_header_edge_cases() {
    let body = br#"{"message":"x"}"#;
    let json = (":content-type", Hdr::Str("application/json"));
    let invalid = (
        InferenceError::InvalidUpstream,
        InferenceError::InvalidUpstream,
    );
    assert_eq!(classes(exception_with(&[json], body)).await, invalid);
    for value in [
        Hdr::Bytes(b"throttlingException"),
        Hdr::Int(429),
        Hdr::Time(0),
        Hdr::True,
    ] {
        let bytes = exception_with(&[(":exception-type", value), json], body);
        assert_eq!(classes(bytes).await, invalid);
    }
    let bytes = exception_with(&[(":exception-type", Hdr::Str("")), json], body);
    assert_eq!(
        classes(bytes).await,
        (
            InferenceError::UpstreamUnavailable,
            InferenceError::UpstreamUnavailable
        )
    );
}

/// The SDK keeps the last of repeated `:exception-type` headers; so does the
/// fallback for an unmodeled name.
#[tokio::test]
async fn the_last_of_duplicate_exception_type_headers_names_the_exception() {
    let body = br#"{"message":"x"}"#;
    let pair = |first: &'static str, second: &'static str| {
        exception_with(
            &[
                (":exception-type", Hdr::Str(first)),
                (":exception-type", Hdr::Str(second)),
            ],
            body,
        )
    };
    // Unmodeled last: the first header is not used.
    assert_eq!(
        classes(pair("throttlingException", "someFutureException")).await,
        (
            InferenceError::UpstreamUnavailable,
            InferenceError::UpstreamUnavailable
        )
    );
    assert_eq!(
        classes(pair("someFutureException", "throttlingException")).await,
        (InferenceError::Busy, InferenceError::Busy)
    );
    // An unmodeled last name that the fallback does know.
    assert_eq!(
        classes(pair("someFutureException", "modelTimeoutException")).await,
        (InferenceError::Timeout, InferenceError::Timeout)
    );
    assert_eq!(
        classes(pair("modelTimeoutException", "someFutureException")).await,
        (
            InferenceError::UpstreamUnavailable,
            InferenceError::UpstreamUnavailable
        )
    );
}

/// In a stream only the documented lowerCamel names are classified. Any other
/// name, in any casing or form, is an unknown failure: not evidence of a
/// client fault or a configuration fault (`Configuration` and
/// `UpstreamRejected` do not count against routing health, and a rejection can
/// settle as a free known zero), so it is `UpstreamUnavailable`. The table the
/// HTTP error path uses (`service_error`) is not applied to frames.
#[tokio::test]
async fn unlisted_exception_names_are_upstream_unavailable() {
    let long = "a".repeat(65_535);
    let names = [
        "ThrottlingException",
        "ValidationException",
        "ServiceUnavailableException",
        "ModelTimeoutException",
        "AccessDeniedException",
        "accessDeniedException",
        "ExpiredTokenException",
        "UnrecognizedClientException",
        "InvalidSignatureException",
        "ResourceNotFoundException",
        "resourceNotFoundException",
        "ServiceQuotaExceededException",
        "serviceQuotaExceededException",
        "modelNotReadyException",
        "throttlingexception",
        "THROTTLINGEXCEPTION",
        " throttlingException",
        "throttlingException ",
        "throttlingException\0",
        "throttling\u{0435}xception",
        "throttlingException\u{200b}",
        "modelTimeoutException\n",
        "validationException:http://internal.amazon.com/",
        "x",
        long.as_str(),
    ];
    for name in names {
        let bytes = frame(name, "exception", json!({"message":"x"}));
        assert_eq!(
            classes(bytes).await,
            (
                InferenceError::UpstreamUnavailable,
                InferenceError::UpstreamUnavailable
            ),
            "{:?}",
            &name[..name.len().min(40)]
        );
    }
}

/// A modeled exception whose JSON payload has the wrong shape fails the SDK's
/// own deserializer, so the frame is a response error (`InvalidUpstream`: fail
/// closed). A payload that is not JSON at all reaches the SDK's `Unhandled`
/// path, and the header classifies it exactly as a well-formed frame, for
/// Messages too: the class never depends on the payload being well formed.
#[tokio::test]
async fn exception_payload_shape_does_not_change_the_class_except_by_failing_closed() {
    use InferenceError as E;
    let invalid = (E::InvalidUpstream, E::InvalidUpstream);
    for payload in [
        &b"null"[..],
        b"[]",
        b"\"s\"",
        br#"{"message":5}"#,
        br#"{"message":{"a":1}}"#,
    ] {
        let bytes = exception_with(
            &[(":exception-type", Hdr::Str("throttlingException"))],
            payload,
        );
        assert_eq!(classes(bytes).await, invalid, "{payload:?}");
    }
    for payload in [&b"not json"[..], b"", b"{", &[0xff, 0xfe][..]] {
        for (kind, expected) in [
            ("throttlingException", (E::Busy, E::Busy)),
            (
                "validationException",
                (E::UpstreamRejected, E::UpstreamRejected),
            ),
            (
                "internalServerException",
                (E::UpstreamUnavailable, E::UpstreamUnavailable),
            ),
            (
                "serviceUnavailableException",
                (E::UpstreamUnavailable, E::Overloaded),
            ),
            ("modelTimeoutException", (E::Timeout, E::Timeout)),
        ] {
            let bytes = exception_with(&[(":exception-type", Hdr::Str(kind))], payload);
            assert_eq!(classes(bytes).await, expected, "{kind} {payload:?}");
        }
    }
}

/// The `:message-type` `error` form is not a modeled exception: the SDK
/// reports it as an undecodable response and the code in the frame is not
/// read. It fails closed as an invalid response (no routing-health effect).
#[tokio::test]
async fn error_message_type_frames_fail_closed_as_invalid_upstream() {
    for code in ["ThrottlingException", "InternalFailure", ""] {
        let bytes = raw_frame(
            &[
                (":message-type", Hdr::Str("error")),
                (":error-code", Hdr::Str(code)),
                (":error-message", Hdr::Str("upstream secret")),
            ],
            b"",
        );
        let (seen, error) = stream_failure(bytes, false).await;
        assert!(seen.is_empty());
        assert_eq!(error, InferenceError::InvalidUpstream, "{code}");
    }
}

//! #622: how a ConverseStream that was already returned ends when the failure
//! is not an exception frame. A failure of the transport (a reset, a body that
//! ends inside a frame or before the terminal sequence, a frame the
//! event-stream decoder cannot decode) is `UpstreamUnavailable`, like a
//! connection failure before the stream; a frame that arrived whole and
//! decodes but whose content is wrong (or a bound of the gateway) stays
//! `InvalidUpstream`. Exception frames keep their own class (#486).
use super::*;
use crate::providers::stream_class_tests::{Step, serve, upstream};

/// Runs one streamed request against a mock that sends `steps` as an event
/// stream with a 200: the events delivered, and how the stream ended.
async fn run(steps: Vec<Step>, messages: bool) -> (Vec<ChatEvent>, Result<()>) {
    let (base, _server) = serve(upstream(200, "application/vnd.amazon.eventstream", steps)).await;
    let mut req = request();
    req.stream = true;
    if messages {
        req.messages_options = Some(Box::default());
    }
    let output = BedrockAdapter::for_test(&base)
        .execute(&target(), req)
        .await;
    let Ok(ProviderOutput::Stream(mut stream)) = output else {
        panic!("a 200 with a first frame returns a stream, however it later ends");
    };
    let mut seen = Vec::new();
    while let Some(next) = stream.next().await {
        match next {
            Ok(ChatEvent::Done) => return (seen, Ok(())),
            Ok(event) => seen.push(event),
            Err(error) => return (seen, Err(error)),
        }
    }
    panic!("the stream ended with neither Done nor an error");
}
fn text(events: &[ChatEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::Delta { text, .. } => text.clone(),
            _ => None,
        })
        .collect()
}
/// The frames of a valid stream, one by one: messageStart, a text delta,
/// contentBlockStop, messageStop, metadata.
fn frames() -> Vec<Vec<u8>> {
    events(false)
        .into_iter()
        .map(|(kind, value)| frame(kind, "event", value))
        .collect()
}
fn bytes(frames: &[Vec<u8>]) -> Step {
    Step::Bytes(frames.concat())
}

#[tokio::test]
async fn a_valid_stream_completes() {
    for messages in [false, true] {
        let (seen, end) = run(vec![bytes(&frames())], messages).await;
        assert_eq!(end, Ok(()));
        assert_eq!(text(&seen), "Hello");
    }
}

/// A failure of the transport, wherever it happens: the class is the same
/// for chat completions and Messages (no Messages split for an unavailable
/// transport), what was delivered stays delivered, and nothing is invented.
#[tokio::test]
async fn transport_failures_are_an_unavailable_provider() {
    let f = frames();
    let half = |frame: &[u8]| frame[..frame.len() / 2].to_vec();
    let mut corrupt_crc = f.clone();
    let last = corrupt_crc[2].len() - 1;
    corrupt_crc[2][last] ^= 1; // the message CRC of the third frame
    let mut corrupt_payload = f.clone();
    corrupt_payload[2][20] ^= 1; // a header byte: the CRC no longer matches
    // Three bytes of a length prefix that no frame can have.
    let impossible_length = vec![0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0];
    let cases: Vec<(&str, Vec<Step>, &str)> = vec![
        (
            "reset after content",
            vec![bytes(&f[..2]), Step::Reset],
            "Hello",
        ),
        (
            "reset inside a frame",
            vec![
                Step::Bytes([f[..2].concat(), half(&f[2])].concat()),
                Step::Reset,
            ],
            "Hello",
        ),
        (
            "reset before any content",
            vec![bytes(&f[..1]), Step::Reset],
            "",
        ),
        (
            "end inside a frame",
            vec![Step::Bytes([f[..2].concat(), half(&f[2])].concat())],
            "Hello",
        ),
        (
            "end at a frame boundary before messageStop",
            vec![bytes(&f[..3])],
            "Hello",
        ),
        (
            "end after messageStop, before metadata",
            vec![bytes(&f[..4])],
            "Hello",
        ),
        (
            "a frame with a failed message CRC",
            vec![bytes(&corrupt_crc)],
            "Hello",
        ),
        (
            "a frame with a corrupt header",
            vec![bytes(&corrupt_payload)],
            "Hello",
        ),
        (
            "a length prefix no frame can have",
            vec![bytes(&f[..2]), Step::Bytes(impossible_length)],
            "Hello",
        ),
    ];
    for messages in [false, true] {
        for (place, steps, delivered) in &cases {
            let (seen, end) = run(steps.clone(), messages).await;
            assert_eq!(
                end,
                Err(InferenceError::UpstreamUnavailable),
                "{place} messages={messages}"
            );
            assert_eq!(&text(&seen), delivered, "{place} messages={messages}");
        }
    }
}

/// A frame that arrived whole and decodes, but whose content is wrong, is the
/// provider's protocol violation; so is a frame the gateway's bound refuses.
#[tokio::test]
async fn a_whole_frame_with_wrong_content_stays_an_invalid_response() {
    let f = frames();
    let mut bad_metadata = f.clone();
    bad_metadata[4] = frame(
        "metadata",
        "event",
        json!({"usage":{"inputTokens":3,"outputTokens":5}}),
    );
    let mut duplicate_event_type = f.clone();
    duplicate_event_type[1] = raw_frame(
        &[
            (":message-type", Hdr::Str("event")),
            (":event-type", Hdr::Str("contentBlockDelta")),
            (":event-type", Hdr::Str("contentBlockDelta")),
        ],
        br#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
    );
    let mut not_json = f.clone();
    not_json[1] = raw_frame(
        &[
            (":message-type", Hdr::Str("event")),
            (":event-type", Hdr::Str("contentBlockDelta")),
        ],
        b"{not json",
    );
    let mut unknown_event = f.clone();
    unknown_event[1] = frame("banana", "event", json!({}));
    let mut twice = f.clone();
    twice.insert(4, f[3].clone());
    // A prefix longer than the wire limit.
    let over_the_limit = Step::Bytes((u32::MAX).to_be_bytes().into_iter().chain([0; 8]).collect());
    let cases: Vec<(&str, Vec<Step>)> = vec![
        (
            "a usage envelope with a missing count",
            vec![bytes(&bad_metadata)],
        ),
        ("duplicate event types", vec![bytes(&duplicate_event_type)]),
        ("a payload that is not JSON", vec![bytes(&not_json)]),
        ("an unknown event", vec![bytes(&unknown_event)]),
        ("messageStop twice", vec![bytes(&twice)]),
        (
            "a frame over the wire limit",
            vec![bytes(&f[..2]), over_the_limit],
        ),
    ];
    for messages in [false, true] {
        for (place, steps) in &cases {
            let (_, end) = run(steps.clone(), messages).await;
            assert_eq!(
                end,
                Err(InferenceError::InvalidUpstream),
                "{place} messages={messages}"
            );
        }
    }
}

/// Exception frames are still classified by their header (#486): a transport
/// rule must not swallow them.
#[tokio::test]
async fn an_exception_frame_keeps_its_class() {
    let f = frames();
    let throttled = frame(
        "throttlingException",
        "exception",
        json!({"message":"slow down"}),
    );
    for messages in [false, true] {
        let (seen, end) = run(
            vec![Step::Bytes([f[..2].concat(), throttled.clone()].concat())],
            messages,
        )
        .await;
        assert_eq!(end, Err(InferenceError::Busy), "messages={messages}");
        assert_eq!(text(&seen), "Hello");
    }
}

#[test]
fn a_body_failure_is_classified_by_where_it_arose() {
    use super::super::wire::Failure;
    for (failure, class) in [
        (Failure::Body, InferenceError::UpstreamUnavailable),
        (Failure::Truncated, InferenceError::UpstreamUnavailable),
        (Failure::Corrupt, InferenceError::UpstreamUnavailable),
        // The transport's own read or total timeout is the class of the
        // stream's idle timeout.
        (Failure::Timeout, InferenceError::Timeout),
        (Failure::Invalid, InferenceError::InvalidUpstream),
    ] {
        assert_eq!(failure.class(), class, "{failure:?}");
    }
}

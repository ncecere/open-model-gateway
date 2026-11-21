//! #614 and #622 across the SSE adapters (Anthropic, OpenAI chat, OpenAI
//! Responses, OpenRouter and a local profile), against a mock upstream that
//! answers 200 and then does what the case says:
//!
//! - **In-stream errors** are classified like the same error as an HTTP
//!   response: for every documented name, the class of the stream's `error`
//!   frame equals the class of a real HTTP error response with that name's
//!   status, for each client protocol the adapter serves, before and after
//!   content. An unrecognised error is `UpstreamUnavailable`, never a success.
//! - **Transport failures** inside a returned stream (reset, a clean end
//!   before the terminal frame, a partial frame) are
//!   `UpstreamUnavailable`. A frame that arrived whole but is not JSON, has the
//!   wrong shape, or exceeds the frame bound stays `InvalidUpstream`.
//!
//! The same scripts drive the engine end to end in `stream_class_e2e_tests`.
use super::{
    ProviderAdapter,
    anthropic::AnthropicAdapter,
    local::{LocalAdapter, Profile, endpoints::ApprovedEndpoints},
    openai::OpenAiAdapter,
    openrouter::{OpenRouterAdapter, OpenRouterConfig},
    secrets::{EnvSecrets, Secret, SecretResolver},
};
use crate::inference::{error::InferenceError, types::*};
use axum::{
    Router,
    body::{Body, Bytes},
    http::Response,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub(crate) struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("test-secret-value".into())
    }
}

/// The adapter under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Anthropic,
    OpenAi,
    Responses,
    OpenRouter,
    Local,
}
pub(crate) const KINDS: [Kind; 5] = [
    Kind::Anthropic,
    Kind::OpenAi,
    Kind::Responses,
    Kind::OpenRouter,
    Kind::Local,
];

/// One step of a mock upstream's body.
#[derive(Clone)]
pub(crate) enum Step {
    Bytes(Vec<u8>),
    /// The connection fails mid-body.
    Reset,
    /// The body stalls for good (until the client goes away).
    Hang,
}

/// The mock's response: `status`, `content_type` and the body the steps make.
pub(crate) fn response(
    status: u16,
    content_type: &'static str,
    steps: Vec<Step>,
) -> Response<Body> {
    let body = async_stream::stream! {
        for step in steps {
            match step {
                Step::Bytes(bytes) => yield Ok::<_, std::io::Error>(Bytes::from(bytes)),
                Step::Reset => {
                    // Let what was written reach the client first.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    yield Err(std::io::Error::other("reset"));
                }
                Step::Hang => std::future::pending::<()>().await,
            }
        }
    };
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from_stream(body))
        .unwrap()
}

/// A mock upstream that answers every request with the same response.
pub(crate) fn upstream(status: u16, content_type: &'static str, steps: Vec<Step>) -> Router {
    Router::new().fallback(move || {
        let steps = steps.clone();
        async move { response(status, content_type, steps) }
    })
}
pub(crate) fn sse(frames: &[Value]) -> Vec<u8> {
    frames
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn chunk(delta: Value, reason: Value) -> Value {
    json!({"id":"c","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
}
fn responses_complete() -> Value {
    json!({"id":"private","object":"response","model":"served","status":"completed","output":[{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello","annotations":[]}]}],"usage":{"input_tokens":3,"output_tokens":2}})
}

impl Kind {
    pub(crate) fn provider(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi | Self::Responses => "openai",
            Self::OpenRouter => "openrouter",
            Self::Local => "vllm",
        }
    }
    /// The client protocols the adapter serves: `messages` or not.
    pub(crate) fn clients(self) -> &'static [bool] {
        match self {
            Self::Responses => &[false],
            _ => &[false, true],
        }
    }
    pub(crate) fn credential(self) -> &'static str {
        match self {
            Self::Local => "none",
            _ => "env:STREAM_CLASS_KEY",
        }
    }
    pub(crate) fn protocol(self) -> ApiProtocol {
        match self {
            Self::Responses => ApiProtocol::Responses,
            _ => ApiProtocol::ChatCompletions,
        }
    }
    /// The base URL a Local deployment carries (an approved host name that
    /// resolves to the mock); `None` for the others.
    fn local_endpoint(self, upstream: &str) -> Option<String> {
        (self == Self::Local)
            .then(|| format!("{}/v1", upstream.replace("127.0.0.1", "fixture.invalid")))
    }
    pub(crate) fn adapter(self, upstream: &str) -> Arc<dyn ProviderAdapter> {
        let base = format!("{upstream}/v1");
        match self {
            Self::Anthropic => Arc::new(AnthropicAdapter::for_test(Arc::new(Resolver), base)),
            Self::OpenAi | Self::Responses => {
                Arc::new(OpenAiAdapter::for_test(Arc::new(Resolver), base))
            }
            Self::OpenRouter => Arc::new(OpenRouterAdapter::for_test(
                Arc::new(Resolver),
                OpenRouterConfig::default(),
                base,
            )),
            Self::Local => Arc::new(LocalAdapter::new(
                Profile::Vllm,
                Arc::new(EnvSecrets::new(Vec::new())),
                Arc::new(ApprovedEndpoints::for_test(
                    &self.local_endpoint(upstream).unwrap(),
                )),
            )),
        }
    }
    pub(crate) fn target(self, upstream: &str) -> Deployment {
        Deployment {
            id: uuid::Uuid::new_v4(),
            provider: self.provider().into(),
            upstream_model: "private-model".into(),
            credential_ref: self.credential().into(),
            endpoint: self.local_endpoint(upstream),
            region: None,
            supported_protocols: vec!["chat_completions".into(), "messages".into()],
        }
    }
    /// A valid stream up to and including some content, and the rest of it,
    /// ending with the protocol's terminal frame.
    pub(crate) fn valid(self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        match self {
            Self::Anthropic => (
                sse(&[
                    json!({"type":"message_start","message":{"id":"x","type":"message","role":"assistant","model":"private","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
                ]),
                sse(&[
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
                ]),
                sse(&[json!({"type":"message_stop"})]),
            ),
            Self::Responses => (
                sse(&[
                    json!({"type":"response.created","response":{"status":"in_progress"}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","content":[],"status":"in_progress"}}),
                    json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
                    json!({"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":"Hello"}),
                ]),
                sse(&[
                    json!({"type":"response.output_text.done","output_index":0,"item_id":"m","content_index":0,"text":"Hello"}),
                    json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello","annotations":[]}}),
                    json!({"type":"response.output_item.done","output_index":0,"item":responses_complete()["output"][0]}),
                ]),
                sse(&[json!({"type":"response.completed","response":responses_complete()})]),
            ),
            Self::OpenAi | Self::OpenRouter | Self::Local => (
                sse(&[chunk(
                    json!({"role":"assistant","content":"Hello"}),
                    Value::Null,
                )]),
                sse(&[
                    chunk(json!({}), json!("stop")),
                    json!({"id":"c","object":"chat.completion.chunk","model":"m","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
                ]),
                b"data: [DONE]\n\n".to_vec(),
            ),
        }
    }
    /// The frames that carry the provider error `name`, in each form the
    /// protocol has (Responses has two).
    pub(crate) fn errors(self, name: &str) -> Vec<Value> {
        match self {
            Self::Anthropic => {
                vec![json!({"type":"error","error":{"type":name,"message":"secret provider text"}})]
            }
            Self::Responses => vec![
                json!({"type":"error","code":name,"message":"secret provider text","param":null,"sequence_number":3}),
                json!({"type":"response.failed","response":{"status":"failed","error":{"code":name,"message":"secret provider text"}}}),
            ],
            Self::OpenAi | Self::OpenRouter | Self::Local => {
                vec![json!({"error":{"message":"secret provider text","param":null,"code":name}})]
            }
        }
    }
    /// A frame that arrived whole and is JSON, but means nothing here.
    pub(crate) fn wrong_shape(self) -> Vec<u8> {
        match self {
            Self::Anthropic | Self::Responses => sse(&[json!({"type":"banana"})]),
            _ => sse(&[json!({"choices":"banana"})]),
        }
    }
}

/// The ChatRequest a client of `messages` or chat completions sends.
pub(crate) fn request(messages: bool) -> ChatRequest {
    ChatRequest {
        messages_options: messages.then(Box::default),
        model: "public".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
            tool_result_is_error: false,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(50),
        stream: true,
    }
}

pub(crate) enum Outcome {
    /// `execute` itself failed (an HTTP error status).
    Refused(InferenceError),
    /// The stream was returned, delivered these events and ended in this error.
    Failed(Vec<ChatEvent>, InferenceError),
    /// The stream ended with `Done`.
    Completed(Vec<ChatEvent>),
}
/// `ChatEvent` has no `Debug`: an outcome shows its class and what was delivered.
impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "refused {e:?}"),
            Self::Failed(events, e) => write!(f, "failed {e:?} after {} events", events.len()),
            Self::Completed(events) => write!(f, "completed with {} events", events.len()),
        }
    }
}
impl Outcome {
    pub(crate) fn class(&self) -> Option<InferenceError> {
        match self {
            Self::Refused(e) | Self::Failed(_, e) => Some(*e),
            Self::Completed(_) => None,
        }
    }
}

/// A mock server task, aborted on drop.
pub(crate) struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(crate) async fn serve(app: Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, Server(task))
}

/// Runs one streamed request of `kind` against a mock that answers `status`.
pub(crate) async fn run(
    kind: Kind,
    messages: bool,
    status: u16,
    content_type: &'static str,
    steps: Vec<Step>,
) -> Outcome {
    let (base, _server) = serve(upstream(status, content_type, steps)).await;
    let adapter = kind.adapter(&base);
    let output = adapter
        .execute_protocol(&kind.target(&base), request(messages), kind.protocol())
        .await;
    let mut stream = match output {
        Ok(ProviderOutput::Stream(stream)) => stream,
        Ok(ProviderOutput::Complete(_)) => panic!("a stream was requested"),
        Err(error) => return Outcome::Refused(error),
    };
    let mut events = Vec::new();
    while let Some(next) = stream.next().await {
        match next {
            Ok(ChatEvent::Done) => {
                events.push(ChatEvent::Done);
                return Outcome::Completed(events);
            }
            Ok(event) => events.push(event),
            Err(error) => {
                assert!(stream.next().await.is_none(), "nothing follows the error");
                return Outcome::Failed(events, error);
            }
        }
    }
    panic!("the stream ended with neither Done nor an error");
}
async fn sse_run(kind: Kind, messages: bool, steps: Vec<Step>) -> Outcome {
    run(kind, messages, 200, "text/event-stream", steps).await
}
fn delivered_text(events: &[ChatEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::Delta { text, .. } => text.clone(),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_complete_stream_completes() {
    for kind in KINDS {
        for &messages in kind.clients() {
            let (content, rest, terminal) = kind.valid();
            let steps = vec![Step::Bytes([content, rest, terminal].concat())];
            let Outcome::Completed(events) = sse_run(kind, messages, steps).await else {
                panic!("{kind:?} messages={messages}: not completed");
            };
            assert_eq!(delivered_text(&events), "Hello", "{kind:?}");
        }
    }
}

pub(crate) use super::stream_error::DOCUMENTED as NAMES;

/// What the HTTP response of `status` is classified as, restated by hand from
/// the adapters' documented tables, as an oracle independent of the code under
/// test.
pub(crate) fn http_class(kind: Kind, messages: bool, name: &str, status: u16) -> InferenceError {
    use InferenceError as E;
    match status {
        // OpenRouter keeps its own meanings (`status_error`).
        401 | 402 if kind == Kind::OpenRouter => E::Configuration,
        403 if kind == Kind::OpenRouter => E::UpstreamRejected,
        529 if messages && kind != Kind::OpenRouter => E::Overloaded,
        400 if messages
            && matches!(
                name,
                "context_length_exceeded" | "prompt_too_long" | "too_many_tokens"
            ) =>
        {
            E::ContextLength
        }
        503 if messages && kind != Kind::OpenRouter => E::Overloaded,
        408 | 524 if kind == Kind::OpenRouter => E::Timeout,
        408 => E::Timeout,
        429 => E::Busy,
        401 | 403 => E::Configuration,
        400..=499 => E::UpstreamRejected,
        _ => E::UpstreamUnavailable,
    }
}

/// For every documented name, each client protocol, before and after content:
/// the class of the in-stream error equals the class of the HTTP response with
/// that name's status, and both equal the hand-written oracle.
#[tokio::test]
async fn in_stream_errors_have_the_class_of_the_same_http_error() {
    let mut problems = Vec::new();
    for kind in KINDS {
        for &messages in kind.clients() {
            let (content, _, _) = kind.valid();
            for (name, status) in NAMES {
                let expected = http_class(kind, messages, name, *status);
                for frame in kind.errors(name) {
                    let bytes = sse(std::slice::from_ref(&frame));
                    // The same error as an HTTP response.
                    let http = run(
                        kind,
                        messages,
                        *status,
                        "application/json",
                        vec![Step::Bytes(frame.to_string().into_bytes())],
                    )
                    .await;
                    let Outcome::Refused(http) = http else {
                        panic!("{kind:?}: an HTTP {status} is refused");
                    };
                    for (place, steps) in [
                        ("before content", vec![Step::Bytes(bytes.clone())]),
                        (
                            "after content",
                            vec![Step::Bytes([content.clone(), bytes.clone()].concat())],
                        ),
                    ] {
                        let at = format!("{kind:?} messages={messages} {name} {place} {frame}");
                        match sse_run(kind, messages, steps).await {
                            Outcome::Failed(events, class) => {
                                if class != http || class != expected {
                                    problems.push(format!(
                                        "{at}: stream {class:?}, http {http:?}, expected {expected:?}"
                                    ));
                                }
                                if (place == "after content") != !events.is_empty() {
                                    problems
                                        .push(format!("{at}: delivered {} events", events.len()));
                                }
                            }
                            other => problems.push(format!("{at}: {other:?}")),
                        }
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// A name nobody documents, an error with no readable name, and a name that is
/// a client-fault word in the message text: the provider failed, in the one
/// way that counts against routing health, and never succeeded.
#[tokio::test]
async fn an_unrecognised_in_stream_error_is_an_unavailable_provider() {
    for kind in KINDS {
        for &messages in kind.clients() {
            let (content, _, _) = kind.valid();
            let mut frames = kind.errors("brand_new_error_name");
            frames.extend(kind.errors("Rate_Limit_Error"));
            // Neither a name nor a status.
            frames.push(match kind {
                Kind::Anthropic => json!({"type":"error","error":{"message":"rate_limit_error"}}),
                Kind::Responses => json!({"type":"error","message":"rate_limit_error"}),
                _ => json!({"error":{"message":"prompt is too long"}}),
            });
            for frame in frames {
                for prefix in [Vec::new(), content.clone()] {
                    let steps = vec![Step::Bytes(
                        [prefix, sse(std::slice::from_ref(&frame))].concat(),
                    )];
                    let at = format!("{kind:?} messages={messages} {frame}");
                    match sse_run(kind, messages, steps).await {
                        Outcome::Failed(_, class) => {
                            assert_eq!(class, InferenceError::UpstreamUnavailable, "{at}");
                        }
                        other => panic!("{at}: {other:?}"),
                    }
                }
            }
        }
    }
}

/// The class does not depend on the order of `code` and `type`, and an
/// explicit `code` wins over a `type` that says otherwise.
#[tokio::test]
async fn an_explicit_code_wins_over_the_type() {
    let frame = json!({"error":{"message":"x","type":"rate_limit_error","code":"server_error"}});
    for kind in [Kind::OpenAi, Kind::OpenRouter, Kind::Local] {
        let outcome = sse_run(
            kind,
            false,
            vec![Step::Bytes(sse(std::slice::from_ref(&frame)))],
        )
        .await;
        assert_eq!(
            outcome.class(),
            Some(InferenceError::UpstreamUnavailable),
            "{kind:?}"
        );
    }
}

/// #622: a returned stream whose connection fails mid-body, whose body ends
/// before the protocol's terminal frame, or ends with an incomplete frame is
/// an unavailable provider. A complete invalid UTF-8 line remains InvalidUpstream.
#[tokio::test]
async fn transport_failures_inside_a_stream_are_an_unavailable_provider() {
    let mut problems = Vec::new();
    for kind in KINDS {
        for &messages in kind.clients() {
            let (content, rest, terminal) = kind.valid();
            let cut = rest[..rest.len() / 2].to_vec();
            let cases: Vec<(&str, Vec<Step>, bool)> = vec![
                (
                    "reset after content",
                    vec![Step::Bytes(content.clone()), Step::Reset],
                    true,
                ),
                (
                    "reset before any event",
                    vec![Step::Bytes(b": keep-alive\n\n".to_vec()), Step::Reset],
                    false,
                ),
                (
                    "reset inside a frame",
                    vec![
                        Step::Bytes([content.clone(), cut.clone()].concat()),
                        Step::Reset,
                    ],
                    true,
                ),
                (
                    "end after content",
                    vec![Step::Bytes(content.clone())],
                    true,
                ),
                (
                    "end inside a frame",
                    vec![Step::Bytes([content.clone(), cut].concat())],
                    true,
                ),
                (
                    "end before the terminal frame",
                    vec![Step::Bytes([content.clone(), rest.clone()].concat())],
                    true,
                ),
                (
                    "end with no bytes at all",
                    vec![Step::Bytes(Vec::new())],
                    false,
                ),
            ];
            for (place, steps, delivered) in cases {
                let at = format!("{kind:?} messages={messages} {place}");
                match sse_run(kind, messages, steps).await {
                    Outcome::Failed(events, class) => {
                        if class != InferenceError::UpstreamUnavailable {
                            problems.push(format!("{at}: {class:?}"));
                        }
                        if delivered != !events.is_empty() {
                            problems.push(format!("{at}: delivered {} events", events.len()));
                        }
                    }
                    other => problems.push(format!("{at}: {other:?}")),
                }
            }
            // The terminal frame arriving is what completes it (control).
            let steps = vec![Step::Bytes([content, rest, terminal].concat())];
            assert!(matches!(
                sse_run(kind, messages, steps).await,
                Outcome::Completed(_)
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// #622: a frame that is completely received but whose content is wrong is the
/// provider's protocol violation, not a transport failure: it does not count
/// against routing health.
#[tokio::test]
async fn a_whole_frame_with_wrong_content_stays_an_invalid_response() {
    let mut problems = Vec::new();
    for kind in KINDS {
        for &messages in kind.clients() {
            let (content, rest, _) = kind.valid();
            let oversized = format!("data: {}\n\n", "x".repeat(1024 * 1024 + 16)).into_bytes();
            let cases: Vec<(&str, Vec<u8>)> = vec![
                ("not JSON", b"data: {not json\n\n".to_vec()),
                ("invalid UTF-8 line", b"data: \xff\xfe\n\n".to_vec()),
                ("JSON of the wrong shape", kind.wrong_shape()),
                ("a frame over the bound", oversized),
                ("the closing frames twice", [rest.clone(), rest].concat()),
            ];
            for (place, bytes) in cases {
                let steps = vec![Step::Bytes([content.clone(), bytes].concat())];
                let at = format!("{kind:?} messages={messages} {place}");
                match sse_run(kind, messages, steps).await {
                    Outcome::Failed(_, InferenceError::InvalidUpstream) => {}
                    other => problems.push(format!("{at}: {other:?}")),
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn a_json_200_for_a_stream_request_is_not_an_sse_error_event() {
    for kind in KINDS {
        for &messages in kind.clients() {
            let body = br#"{"error":{"code":"rate_limit_exceeded"}}"#.to_vec();
            assert!(matches!(
                run(
                    kind,
                    messages,
                    200,
                    "application/json",
                    vec![Step::Bytes(body)]
                )
                .await,
                Outcome::Refused(InferenceError::InvalidUpstream)
            ));
        }
    }
}

#[tokio::test]
async fn numeric_codes_and_string_precedence_match_http_statuses() {
    for kind in KINDS {
        for &messages in kind.clients() {
            for (code, typ, status) in [
                (json!(408), "rate_limit_error", 408),
                (json!(429), "invalid_request_error", 429),
                (json!(503), "invalid_request_error", 503),
                (json!(524), "invalid_request_error", 524),
                (json!("400"), "rate_limit_error", 400),
                (json!("429"), "invalid_request_error", 429),
                (json!(""), "invalid_request_error", 400),
                (json!("new_name"), "invalid_request_error", 400),
            ] {
                let error = json!({"code":code,"type":typ});
                let frame = match kind {
                    Kind::Anthropic => json!({"type":"error","error":error}),
                    Kind::Responses => {
                        json!({"type":"response.failed","response":{"status":"failed","error":error}})
                    }
                    _ => json!({"error":error}),
                };
                let http = run(
                    kind,
                    messages,
                    status,
                    "application/json",
                    vec![Step::Bytes(frame.to_string().into_bytes())],
                )
                .await;
                assert_eq!(
                    http.class(),
                    Some(http_class(kind, messages, "", status)),
                    "HTTP {status} {kind:?}"
                );
                let outcome = sse_run(kind, messages, vec![Step::Bytes(sse(&[frame]))]).await;
                assert_eq!(
                    outcome.class(),
                    http.class(),
                    "{kind:?} messages={messages} code={code}"
                );
            }
        }
    }
}

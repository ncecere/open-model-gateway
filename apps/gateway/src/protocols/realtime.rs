//! `GET /v1/realtime?model=…`: OpenAI Realtime (GA) WebSocket frontend.
//!
//! The inference key comes from exactly one of `Authorization: Bearer` or the
//! browser subprotocol `openai-insecure-api-key.<key>`; the workspace derives
//! from the key only and the key is never forwarded (the upstream uses the
//! server-side credential reference). Admission and the upstream connection
//! happen before the 101 response, so their failures are ordinary JSON errors.
//!
//! After the upgrade, client events pass an allowlist (anything else is an
//! `error` event and a close, never a silent drop), `response.create` reserves
//! a budget window before it is forwarded and gets an explicit per-response
//! `max_output_tokens`, and upstream `response.done` usage settles each
//! response. No event content is logged or stored.
use std::time::Duration;

use axum::{
    Extension,
    extract::{
        State,
        ws::{
            CloseFrame, Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
        },
    },
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::time::{Instant, sleep_until, timeout};
use uuid::Uuid;

use crate::{
    http::RequestId,
    inference::{
        Engine,
        client::ClientMetadata,
        error::InferenceError,
        realtime::{RealtimeSession, RealtimeUpstream, UpstreamEvent},
        repository::{FinishLabel, Outcome},
    },
    store::Store,
};

const KEY_PROTOCOL: &str = "openai-insecure-api-key.";
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EVENT_ID: usize = 128;

// ---------------------------------------------------------------- HTTP ----

fn http_error(status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"message":message,"type":kind,"code":code,"param":null}})),
    )
        .into_response()
}
fn unauthorized() -> Response {
    let mut response = http_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "authentication_error",
        "Invalid or missing API key",
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer"),
    );
    response
}
fn unsupported(message: &str) -> Response {
    http_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "unsupported_capability",
        message,
    )
}

/// Requested subprotocols, in order (comma-separated across header lines).
fn subprotocols(headers: &HeaderMap) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for value in headers.get_all(header::SEC_WEBSOCKET_PROTOCOL) {
        for p in value.to_str().ok()?.split(',') {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }
            out.push(p.to_owned());
        }
    }
    Some(out)
}

/// Exactly one inference key: a Bearer header or the key subprotocol.
pub(crate) fn credential(headers: &HeaderMap) -> Option<String> {
    let authorization: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
    if authorization.len() > 1 || headers.contains_key("x-api-key") {
        return None;
    }
    let bearer = match authorization.first() {
        Some(value) => {
            let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
            if !scheme.eq_ignore_ascii_case("bearer")
                || token.is_empty()
                || token.bytes().any(|b| b.is_ascii_whitespace())
            {
                return None;
            }
            Some(token.to_owned())
        }
        None => None,
    };
    let protocols = subprotocols(headers)?;
    let keys: Vec<&str> = protocols
        .iter()
        .filter_map(|p| p.strip_prefix(KEY_PROTOCOL))
        .collect();
    match (bearer, keys.as_slice()) {
        (Some(token), []) => Some(token),
        (None, [key]) if !key.is_empty() => Some((*key).to_owned()),
        _ => None,
    }
}

/// The single `model` query parameter (percent-decoded).
fn model_param(uri: &Uri) -> Result<String, &'static str> {
    let mut model = None;
    for pair in uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "model" {
            return Err("Only the model query parameter is supported");
        }
        let value = percent_encoding::percent_decode_str(&value.replace('+', " "))
            .decode_utf8()
            .map_err(|_| "The model query parameter is invalid")?
            .into_owned();
        if model.replace(value).is_some() {
            return Err("The model query parameter must appear once");
        }
    }
    model
        .filter(|m| !m.trim().is_empty() && m.len() <= 200 && !m.chars().any(char::is_control))
        .ok_or("The model query parameter is required")
}

/// `POST /v1/realtime/client_secrets` and `/v1/realtime/calls`: explicit.
pub async fn unsupported_route() -> Response {
    unsupported(
        "Ephemeral client secrets and WebRTC/SIP calls are not supported; connect to GET /v1/realtime over WebSocket with an inference key",
    )
}

pub async fn handle(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some(token) = credential(&headers) else {
        return unauthorized();
    };
    let principal = match store.authenticate(&token).await {
        Ok(Some(principal)) => principal,
        Ok(None) => return unauthorized(),
        Err(_) => {
            tracing::error!("authentication database lookup failed");
            return http_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "api_error",
                "Authentication service unavailable",
            );
        }
    };
    drop(token);
    if headers
        .get_all("openai-beta")
        .iter()
        .any(|v| v.to_str().map_or(true, |v| v.contains("realtime")))
    {
        return unsupported("The beta Realtime interface is not supported; use the GA interface");
    }
    let protocols = subprotocols(&headers).unwrap_or_default();
    if protocols
        .iter()
        .any(|p| p != "realtime" && !p.starts_with(KEY_PROTOCOL))
    {
        return unsupported("Unsupported WebSocket subprotocol");
    }
    let model = match model_param(&uri) {
        Ok(model) => model,
        Err(message) => {
            return http_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request_error",
                message,
            );
        }
    };
    let Ok(upgrade) = upgrade else {
        return http_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_error",
            "A WebSocket upgrade is required",
        );
    };
    let session = ClientMetadata::from_headers(&headers)
        .scope(engine.open_realtime(principal, &model, request_id))
        .await;
    let session = match session {
        Ok(session) => session,
        Err(error) => return super::workload_error(error, StatusCode::BAD_REQUEST),
    };
    let limits = session.limits();
    upgrade
        .protocols(["realtime"])
        .max_message_size(limits.max_message_bytes)
        .max_frame_size(limits.max_message_bytes)
        .on_upgrade(move |socket| proxy(socket, session))
}

// ------------------------------------------------------- Client events ----

/// A rejected client event: an `error` event, then (if `fatal`) a close.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub kind: &'static str,
    pub code: &'static str,
    pub message: &'static str,
    pub param: Option<String>,
    pub event_id: Option<String>,
    pub fatal: bool,
}
fn invalid(param: &str, message: &'static str) -> Rejection {
    Rejection {
        kind: "invalid_request_error",
        code: "invalid_request_error",
        message,
        param: Some(param.to_owned()),
        event_id: None,
        fatal: true,
    }
}
fn refused(param: &str, message: &'static str) -> Rejection {
    Rejection {
        kind: "invalid_request_error",
        code: "unsupported_capability",
        message,
        param: Some(param.to_owned()),
        event_id: None,
        fatal: true,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientAction {
    Forward(String),
    /// Forward after a budget window is reserved; `event_id` correlates an
    /// upstream rejection.
    ResponseCreate {
        text: String,
        event_id: String,
    },
}

fn object<'a>(value: &'a Value, param: &str) -> Result<&'a Map<String, Value>, Rejection> {
    value
        .as_object()
        .ok_or_else(|| invalid(param, "Expected an object"))
}
fn only(map: &Map<String, Value>, allowed: &[&str], param: &str) -> Result<(), Rejection> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(refused(
            &format!("{param}.{k}"),
            "This field is not supported by the gateway",
        )),
        None => Ok(()),
    }
}
fn string(value: Option<&Value>, param: &str, required: bool) -> Result<(), Rejection> {
    match value {
        None if !required => Ok(()),
        Some(Value::String(_)) => Ok(()),
        _ => Err(invalid(param, "Expected a string")),
    }
}
fn modalities(value: Option<&Value>, param: &str) -> Result<(), Rejection> {
    match value {
        None => Ok(()),
        Some(Value::Array(a)) if a.len() == 1 && (a[0] == "text" || a[0] == "audio") => Ok(()),
        _ => Err(invalid(param, "Expected [\"text\"] or [\"audio\"]")),
    }
}
fn max_output(value: Option<&Value>, window: u32, param: &str) -> Result<(), Rejection> {
    match value {
        None => Ok(()),
        Some(v)
            if v.as_u64()
                .is_some_and(|n| (1..=u64::from(window)).contains(&n)) =>
        {
            Ok(())
        }
        _ => Err(refused(
            param,
            "max_output_tokens must be an integer within the gateway's per-response ceiling",
        )),
    }
}
fn tools(value: Option<&Value>, param: &str) -> Result<(), Rejection> {
    match value {
        None => Ok(()),
        Some(Value::Array(tools)) => tools.iter().try_for_each(|t| {
            if t["type"] == "function" {
                Ok(())
            } else {
                Err(refused(param, "Only function tools are supported"))
            }
        }),
        _ => Err(invalid(param, "Expected an array")),
    }
}
fn tool_choice(value: Option<&Value>, param: &str) -> Result<(), Rejection> {
    match value {
        None => Ok(()),
        Some(Value::String(s)) if matches!(s.as_str(), "auto" | "none" | "required") => Ok(()),
        Some(Value::Object(o)) if o.get("type").is_some_and(|t| t == "function") => Ok(()),
        _ => Err(refused(
            param,
            "Only auto, none, required or a function tool choice is supported",
        )),
    }
}
fn audio_output(value: Option<&Value>, allowed: &[&str], param: &str) -> Result<(), Rejection> {
    if let Some(output) = value {
        only(object(output, param)?, allowed, param)?;
    }
    Ok(())
}

/// `session.update.session`: realtime sessions that cannot start billable
/// work by themselves (no automatic responses, no idle-timeout responses, no
/// input transcription) and no per-session overrides the gateway cannot bound.
pub(crate) fn validate_session(session: &Value, window: u32) -> Result<(), Rejection> {
    let s = object(session, "session")?;
    only(
        s,
        &[
            "type",
            "instructions",
            "output_modalities",
            "max_output_tokens",
            "audio",
            "tools",
            "tool_choice",
            "truncation",
        ],
        "session",
    )?;
    if s.get("type").is_none_or(|t| t != "realtime") {
        return Err(refused(
            "session.type",
            "Only realtime sessions are supported (transcription sessions are not)",
        ));
    }
    string(s.get("instructions"), "session.instructions", false)?;
    modalities(s.get("output_modalities"), "session.output_modalities")?;
    max_output(
        s.get("max_output_tokens"),
        window,
        "session.max_output_tokens",
    )?;
    tools(s.get("tools"), "session.tools")?;
    tool_choice(s.get("tool_choice"), "session.tool_choice")?;
    if let Some(audio) = s.get("audio") {
        let audio = object(audio, "session.audio")?;
        only(audio, &["input", "output"], "session.audio")?;
        audio_output(
            audio.get("output"),
            &["format", "voice", "speed"],
            "session.audio.output",
        )?;
        if let Some(input) = audio.get("input") {
            let input = object(input, "session.audio.input")?;
            only(
                input,
                &[
                    "format",
                    "noise_reduction",
                    "turn_detection",
                    "transcription",
                ],
                "session.audio.input",
            )?;
            if input.get("transcription").is_some_and(|t| !t.is_null()) {
                return Err(refused(
                    "session.audio.input.transcription",
                    "Input audio transcription is billed separately and is not supported",
                ));
            }
            match input.get("turn_detection") {
                None | Some(Value::Null) => {}
                Some(detection) => {
                    let d = object(detection, "session.audio.input.turn_detection")?;
                    only(
                        d,
                        &[
                            "type",
                            "create_response",
                            "interrupt_response",
                            "prefix_padding_ms",
                            "silence_duration_ms",
                            "threshold",
                            "eagerness",
                            "idle_timeout_ms",
                        ],
                        "session.audio.input.turn_detection",
                    )?;
                    if !d
                        .get("type")
                        .is_some_and(|t| t == "server_vad" || t == "semantic_vad")
                    {
                        return Err(invalid(
                            "session.audio.input.turn_detection.type",
                            "Expected server_vad or semantic_vad",
                        ));
                    }
                    if d.get("create_response") != Some(&Value::Bool(false)) {
                        return Err(refused(
                            "session.audio.input.turn_detection.create_response",
                            "Automatic responses are not supported; set create_response to false and send response.create",
                        ));
                    }
                    if d.get("idle_timeout_ms").is_some_and(|v| !v.is_null()) {
                        return Err(refused(
                            "session.audio.input.turn_detection.idle_timeout_ms",
                            "Idle-timeout responses are not supported",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// `response.create.response` with an explicit `max_output_tokens`.
pub(crate) fn validate_response(response: Option<&Value>, window: u32) -> Result<Value, Rejection> {
    let mut r = match response {
        None | Some(Value::Null) => Map::new(),
        Some(v) => object(v, "response")?.clone(),
    };
    only(
        &r,
        &[
            "instructions",
            "output_modalities",
            "max_output_tokens",
            "metadata",
            "tools",
            "tool_choice",
            "audio",
            "conversation",
        ],
        "response",
    )?;
    if r.get("conversation").is_some_and(|c| c != "auto") {
        return Err(refused(
            "response.conversation",
            "Out-of-band responses are not supported",
        ));
    }
    string(r.get("instructions"), "response.instructions", false)?;
    modalities(r.get("output_modalities"), "response.output_modalities")?;
    max_output(
        r.get("max_output_tokens"),
        window,
        "response.max_output_tokens",
    )?;
    tools(r.get("tools"), "response.tools")?;
    tool_choice(r.get("tool_choice"), "response.tool_choice")?;
    if let Some(audio) = r.get("audio") {
        let audio = object(audio, "response.audio")?;
        only(audio, &["output"], "response.audio")?;
        audio_output(
            audio.get("output"),
            &["format", "voice"],
            "response.audio.output",
        )?;
    }
    if let Some(metadata) = r.get("metadata") {
        let m = object(metadata, "response.metadata")?;
        if m.len() > 16
            || m.iter().any(|(k, v)| {
                !(1..=64).contains(&k.chars().count())
                    || v.as_str().is_none_or(|v| v.chars().count() > 512)
            })
        {
            return Err(invalid(
                "response.metadata",
                "At most 16 string pairs (keys up to 64, values up to 512 characters)",
            ));
        }
    }
    r.entry("max_output_tokens").or_insert(json!(window));
    Ok(Value::Object(r))
}

/// `conversation.item.create.item`: messages, function calls and outputs;
/// image input has no realtime meter and is refused.
pub(crate) fn validate_item(item: &Value) -> Result<(), Rejection> {
    let i = object(item, "item")?;
    match i.get("type").and_then(Value::as_str) {
        Some("function_call" | "function_call_output") => Ok(()),
        Some("message") => {
            if !i
                .get("role")
                .is_some_and(|r| r == "user" || r == "assistant" || r == "system")
            {
                return Err(invalid("item.role", "Expected user, assistant or system"));
            }
            let Some(Value::Array(content)) = i.get("content") else {
                return Err(invalid("item.content", "Expected an array"));
            };
            content
                .iter()
                .try_for_each(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "input_audio" | "output_text" | "output_audio") => Ok(()),
                    Some("input_image") => Err(refused(
                        "item.content",
                        "Image input is not supported on realtime sessions",
                    )),
                    _ => Err(invalid("item.content", "Unsupported content part type")),
                })
        }
        _ => Err(refused("item.type", "Unsupported conversation item type")),
    }
}

fn event_id(e: &Map<String, Value>) -> Result<Option<String>, Rejection> {
    match e.get("event_id") {
        None => Ok(None),
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= MAX_EVENT_ID
                && s.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err(invalid("event_id", "Expected a printable event id")),
    }
}

/// Validate one client text frame against the allowlist.
pub(crate) fn validate(text: &str, window: u32) -> Result<ClientAction, Rejection> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| invalid("event", "Events must be JSON objects"))?;
    let e = object(&value, "event")?;
    let id = event_id(e)?;
    let with_id = |mut r: Rejection| {
        r.event_id = id.clone();
        r
    };
    let kind = e.get("type").and_then(Value::as_str).unwrap_or_default();
    let forward = || Ok(ClientAction::Forward(text.to_owned()));
    let checked = match kind {
        "session.update" => only(e, &["type", "event_id", "session"], "event")
            .and_then(|_| validate_session(e.get("session").unwrap_or(&Value::Null), window))
            .and_then(|_| forward()),
        "input_audio_buffer.append" => only(e, &["type", "event_id", "audio"], "event")
            .and_then(|_| string(e.get("audio"), "audio", true))
            .and_then(|_| forward()),
        "input_audio_buffer.commit" | "input_audio_buffer.clear" | "output_audio_buffer.clear" => {
            only(e, &["type", "event_id"], "event").and_then(|_| forward())
        }
        "conversation.item.create" => only(
            e,
            &["type", "event_id", "previous_item_id", "item"],
            "event",
        )
        .and_then(|_| string(e.get("previous_item_id"), "previous_item_id", false))
        .and_then(|_| validate_item(e.get("item").unwrap_or(&Value::Null)))
        .and_then(|_| forward()),
        "conversation.item.delete" | "conversation.item.retrieve" => {
            only(e, &["type", "event_id", "item_id"], "event")
                .and_then(|_| string(e.get("item_id"), "item_id", true))
                .and_then(|_| forward())
        }
        "conversation.item.truncate" => only(
            e,
            &[
                "type",
                "event_id",
                "item_id",
                "content_index",
                "audio_end_ms",
            ],
            "event",
        )
        .and_then(|_| string(e.get("item_id"), "item_id", true))
        .and_then(|_| forward()),
        "response.cancel" => only(e, &["type", "event_id", "response_id"], "event")
            .and_then(|_| string(e.get("response_id"), "response_id", false))
            .and_then(|_| forward()),
        "response.create" => only(e, &["type", "event_id", "response"], "event")
            .and_then(|_| validate_response(e.get("response"), window))
            .map(|response| {
                let event_id = id
                    .clone()
                    .unwrap_or_else(|| format!("event_gw_{}", Uuid::new_v4().simple()));
                let text =
                    json!({"type":"response.create","event_id":event_id,"response":response})
                        .to_string();
                ClientAction::ResponseCreate { text, event_id }
            }),
        _ => Err(Rejection {
            kind: "invalid_request_error",
            code: "unsupported_event",
            message: "This client event type is not supported by the gateway",
            param: Some("type".into()),
            event_id: None,
            fatal: true,
        }),
    };
    checked.map_err(with_id)
}

// --------------------------------------------------------------- Proxy ----

fn error_text(
    kind: &str,
    code: &str,
    message: &str,
    param: Option<&str>,
    event_id: Option<&str>,
) -> String {
    json!({
        "type": "error",
        "event_id": format!("event_gw_{}", Uuid::new_v4().simple()),
        "error": {"type": kind, "code": code, "message": message, "param": param, "event_id": event_id}
    })
    .to_string()
}
fn inference_error_text(error: InferenceError) -> String {
    let body = super::openai_error_body(error);
    let e = &body["error"];
    error_text(
        e["type"].as_str().unwrap_or("api_error"),
        error.code(),
        error.message(),
        None,
        None,
    )
}

/// How a session ended (decides the close frame and the attempt outcome).
enum End {
    ClientClosed,
    ClientGone,
    Idle,
    MaxDuration,
    Rejected(Rejection),
    Binary,
    /// A client message above `GATEWAY_REALTIME_MAX_MESSAGE_BYTES`.
    TooLarge,
    RateLimited,
    Denied(InferenceError),
    Upstream(InferenceError),
}

/// Token bucket over client events (burst = 2 × rate).
struct Bucket {
    tokens: f64,
    rate: f64,
    at: Instant,
}
impl Bucket {
    fn new(rate: u32) -> Self {
        Self {
            tokens: f64::from(rate) * 2.0,
            rate: f64::from(rate),
            at: Instant::now(),
        }
    }
    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate)
            .min(self.rate * 2.0);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The client sent a message or frame above the configured cap.
fn too_large(error: &axum::Error) -> bool {
    std::error::Error::source(error)
        .and_then(|e| e.downcast_ref::<tokio_tungstenite::tungstenite::Error>())
        .is_some_and(|e| matches!(e, tokio_tungstenite::tungstenite::Error::Capacity(_)))
}
async fn send_client(
    client: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    text: String,
) -> bool {
    matches!(
        timeout(SEND_TIMEOUT, client.send(Message::Text(text.into()))).await,
        Ok(Ok(()))
    )
}
async fn send_upstream(
    upstream: &mut RealtimeUpstream,
    text: String,
) -> Result<(), InferenceError> {
    timeout(SEND_TIMEOUT, upstream.sink.send(text))
        .await
        .map_err(|_| InferenceError::Timeout)?
}

/// Rewrite the upstream model id to the public alias.
fn session_text(mut event: Value, alias: &str) -> String {
    if let Some(session) = event.get_mut("session").and_then(Value::as_object_mut)
        && session.contains_key("model")
    {
        session.insert("model".into(), json!(alias));
    }
    event.to_string()
}
fn is_output_delta(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Peek<'a> {
        #[serde(rename = "type", borrow)]
        kind: std::borrow::Cow<'a, str>,
    }
    serde_json::from_str::<Peek>(text).is_ok_and(|p| {
        matches!(
            p.kind.as_ref(),
            "response.output_audio.delta"
                | "response.output_text.delta"
                | "response.output_audio_transcript.delta"
        )
    })
}

async fn proxy(socket: WebSocket, mut session: RealtimeSession) {
    let Some(mut upstream) = session.take_upstream() else {
        let _ = session
            .finish(Outcome::Failed, Some(InferenceError::Storage), None)
            .await;
        return;
    };
    let limits = session.limits();
    let window = session.window_output_tokens();
    let alias = session.model().to_owned();
    let (mut client_tx, mut client_rx) = socket.split();
    let deadline = Instant::now() + limits.max_session;
    let mut idle = Instant::now() + limits.idle;
    let mut bucket = Bucket::new(limits.max_events_per_second);
    let mut requested: Option<String> = None;
    let mut first_output = false;
    let end = loop {
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => break End::MaxDuration,
            _ = sleep_until(idle) => break End::Idle,
            message = client_rx.next() => {
                idle = Instant::now() + limits.idle;
                let text = match message {
                    Some(Err(error)) if too_large(&error) => break End::TooLarge,
                    None | Some(Err(_)) => break End::ClientGone,
                    Some(Ok(Message::Close(_))) => break End::ClientClosed,
                    Some(Ok(Message::Binary(_))) => break End::Binary,
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                };
                if !bucket.take() {
                    break End::RateLimited;
                }
                match validate(text.as_str(), window) {
                    Err(rejection) => break End::Rejected(rejection),
                    Ok(ClientAction::Forward(text)) => {
                        if let Err(error) = send_upstream(&mut upstream, text).await {
                            break End::Upstream(error);
                        }
                    }
                    Ok(ClientAction::ResponseCreate { text, event_id }) => {
                        if session.response_open() || requested.is_some() {
                            // Upstream allows one active response; refuse
                            // explicitly without forwarding.
                            let error = error_text(
                                "invalid_request_error",
                                "conversation_already_has_active_response",
                                "A response is already in progress",
                                None,
                                Some(&event_id),
                            );
                            if !send_client(&mut client_tx, error).await {
                                break End::ClientGone;
                            }
                            continue;
                        }
                        if let Err(error) = session.reserve_window().await {
                            break End::Denied(error);
                        }
                        requested = Some(event_id);
                        if let Err(error) = send_upstream(&mut upstream, text).await {
                            break End::Upstream(error);
                        }
                    }
                }
            }
            event = upstream.events.next() => {
                idle = Instant::now() + limits.idle;
                let text = match event {
                    None => break End::Upstream(InferenceError::UpstreamUnavailable),
                    Some(Err(error)) => break End::Upstream(error),
                    Some(Ok(UpstreamEvent::Forward(text))) => {
                        if !first_output && is_output_delta(&text) {
                            first_output = true;
                            session.first_output();
                        }
                        text
                    }
                    Some(Ok(UpstreamEvent::Session { event, safe })) => {
                        // `session.created` shows upstream defaults from before the
                        // gateway's acknowledged configuration; every update is checked.
                        if !safe && event["type"] == "session.updated" {
                            break End::Upstream(InferenceError::InvalidUpstream);
                        }
                        session_text(event, &alias)
                    }
                    Some(Ok(UpstreamEvent::ResponseCreated { text, response_id })) => {
                        if let Err(error) = session.open_response(response_id).await {
                            break End::Upstream(error);
                        }
                        requested = None;
                        text
                    }
                    Some(Ok(UpstreamEvent::ResponseDone { text, response_id, status, usage })) => {
                        if let Err(error) = session.settle_response(&response_id, status, usage).await {
                            break End::Upstream(error);
                        }
                        text
                    }
                    Some(Ok(UpstreamEvent::Error { kind, code, param, event_id })) => {
                        if event_id.is_some() && event_id == requested {
                            requested = None;
                            session.request_rejected();
                        }
                        error_text(
                            kind.as_deref().unwrap_or("server_error"),
                            code.as_deref().unwrap_or("upstream_error"),
                            "The provider reported an error for this session",
                            param.as_deref(),
                            event_id.as_deref(),
                        )
                    }
                    Some(Ok(UpstreamEvent::Filtered)) => continue,
                    Some(Ok(UpstreamEvent::Unaccounted)) => {
                        break End::Upstream(InferenceError::InvalidUpstream)
                    }
                };
                if !send_client(&mut client_tx, text).await {
                    break End::ClientGone;
                }
            }
        }
    };
    // Upstream work stops first: dropping the socket closes it immediately.
    // A best-effort close frame is sent only when the client is still there
    // to be told why; it never waits on a slow upstream.
    let in_flight = session.response_open() || requested.is_some();
    let client_present = !matches!(end, End::ClientClosed | End::ClientGone);
    let client_closed = matches!(end, End::ClientClosed);
    if client_present {
        let _ = timeout(Duration::from_millis(500), upstream.sink.close()).await;
    }
    drop(upstream);
    let (close, outcome, error, finish) = match end {
        End::ClientClosed | End::ClientGone if in_flight => (None, Outcome::Cancelled, None, None),
        End::ClientClosed | End::ClientGone => {
            (None, Outcome::Succeeded, None, Some(FinishLabel::Stop))
        }
        End::Idle | End::MaxDuration if in_flight => (
            Some((
                1000,
                "session_expired",
                inference_error_text(InferenceError::Timeout),
            )),
            Outcome::Failed,
            Some(InferenceError::Timeout),
            None,
        ),
        End::Idle => (
            Some((
                1000,
                "idle_timeout",
                error_text(
                    "invalid_request_error",
                    "idle_timeout",
                    "The session was idle for too long",
                    None,
                    None,
                ),
            )),
            Outcome::Succeeded,
            None,
            Some(FinishLabel::Stop),
        ),
        End::MaxDuration => (
            Some((
                1000,
                "session_expired",
                error_text(
                    "invalid_request_error",
                    "session_expired",
                    "The session reached its maximum duration",
                    None,
                    None,
                ),
            )),
            Outcome::Succeeded,
            None,
            Some(FinishLabel::Length),
        ),
        End::Rejected(r) => (
            Some((
                1008,
                r.code,
                error_text(
                    r.kind,
                    r.code,
                    r.message,
                    r.param.as_deref(),
                    r.event_id.as_deref(),
                ),
            )),
            Outcome::Failed,
            Some(
                if r.code == "unsupported_capability" || r.code == "unsupported_event" {
                    InferenceError::Unsupported
                } else {
                    InferenceError::InvalidRequest
                },
            ),
            None,
        ),
        End::Binary => (
            Some((
                1003,
                "unsupported_data",
                error_text(
                    "invalid_request_error",
                    "unsupported_event",
                    "Binary frames are not supported; send JSON text events",
                    None,
                    None,
                ),
            )),
            Outcome::Failed,
            Some(InferenceError::Unsupported),
            None,
        ),
        End::TooLarge => (
            Some((
                1009,
                "message_too_big",
                error_text(
                    "invalid_request_error",
                    "message_too_big",
                    "The event exceeds the gateway's message size limit",
                    None,
                    None,
                ),
            )),
            Outcome::Failed,
            Some(InferenceError::InvalidRequest),
            None,
        ),
        End::RateLimited => (
            Some((
                1008,
                "rate_limit_error",
                inference_error_text(InferenceError::Busy),
            )),
            Outcome::Failed,
            Some(InferenceError::Busy),
            None,
        ),
        End::Denied(error) => (
            Some((1008, error.code(), inference_error_text(error))),
            Outcome::Failed,
            Some(error),
            None,
        ),
        End::Upstream(error) => (
            Some((1011, error.code(), inference_error_text(error))),
            Outcome::Failed,
            Some(error),
            None,
        ),
    };
    if let Some((code, reason, text)) = close
        && send_client(&mut client_tx, text).await
        && matches!(
            timeout(
                Duration::from_secs(1),
                client_tx.send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                }))),
            )
            .await,
            Ok(Ok(()))
        )
    {
        // Wait (bounded) for the client's closing reply, discarding what it
        // sent first. Dropping the socket with unread input (e.g. the events
        // that tripped the rate limit) makes the kernel reset the connection,
        // which can destroy the error event and close frame still in flight.
        let _ = timeout(Duration::from_secs(1), async {
            while let Some(Ok(message)) = client_rx.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
    }
    if client_closed {
        // Complete the closing handshake the client started.
        let _ = timeout(Duration::from_millis(500), client_tx.close()).await;
    }
    drop(client_tx);
    drop(client_rx);
    let _ = session.finish(outcome, error, finish).await;
}

#[cfg(test)]
mod tests;

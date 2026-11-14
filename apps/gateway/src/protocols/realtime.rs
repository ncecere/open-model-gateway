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
        realtime::{
            RealtimeSession, RealtimeUpstream, UpstreamEvent,
            context::{EventSize, base64_decoded_bytes, input_format_rate, session_config},
        },
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
        /// The explicit per-response output bound (within the window).
        max_output_tokens: u32,
    },
}

/// Base64 characters of the audio payloads one validated event carries.
fn audio_chars(e: &Map<String, Value>) -> usize {
    match e.get("type").and_then(Value::as_str) {
        Some("input_audio_buffer.append") => {
            e.get("audio").and_then(Value::as_str).map_or(0, str::len)
        }
        Some("conversation.item.create") => e
            .get("item")
            .and_then(|i| i.get("content"))
            .and_then(Value::as_array)
            .map_or(0, |parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("audio").and_then(Value::as_str))
                    .map(str::len)
                    .sum()
            }),
        _ => 0,
    }
}
/// What a validated event can add to the conversation, for hold sizing
/// (`inference::realtime::context`): audio payloads as decoded audio bytes,
/// everything else of the forwarded text as text bytes.
fn event_size(e: &Map<String, Value>, forwarded: &str) -> EventSize {
    let audio = audio_chars(e).min(forwarded.len());
    EventSize {
        text_bytes: (forwarded.len() - audio) as u64,
        audio_bytes: base64_decoded_bytes(audio),
        audio_bytes_per_second: (e.get("type").and_then(Value::as_str) == Some("session.update"))
            .then(|| input_format_rate(&e["session"]["audio"]["input"]["format"]))
            .flatten(),
    }
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
#[cfg(test)]
pub(crate) fn validate(text: &str, window: u32) -> Result<ClientAction, Rejection> {
    validate_measured(text, window).map(|(action, _)| action)
}
/// [`validate`], plus the size of what the forwarded event can add to the
/// conversation.
pub(crate) fn validate_measured(
    text: &str,
    window: u32,
) -> Result<(ClientAction, EventSize), Rejection> {
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
                // Validated within 1..=window above (or set to the window).
                let max_output_tokens = response["max_output_tokens"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(window);
                let text =
                    json!({"type":"response.create","event_id":event_id,"response":response})
                        .to_string();
                ClientAction::ResponseCreate {
                    text,
                    event_id,
                    max_output_tokens,
                }
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
    checked.map_err(with_id).map(|action| {
        let size = match &action {
            ClientAction::Forward(t) | ClientAction::ResponseCreate { text: t, .. } => {
                event_size(e, t)
            }
        };
        (action, size)
    })
}

// --------------------------------------------------------------- Proxy ----

mod relay;
use relay::{End, Fault, inference_error_text, invalid_text, rejection_error, rejection_text};

async fn proxy(socket: WebSocket, session: RealtimeSession) {
    let Some(ended) = relay::relay(socket, session).await else {
        return;
    };
    let (end, in_flight) = (ended.end, ended.in_flight);
    let (close, outcome, error, finish) = match end {
        End::ClientClosed if !in_flight => {
            (None, Outcome::Succeeded, None, Some(FinishLabel::Stop))
        }
        // Never a clean stop (#410): a close mid-response or a lost connection.
        End::ClientClosed | End::ClientGone => (None, Outcome::Cancelled, None, None),
        End::ClientProtocol(fault @ Fault::InvalidUtf8) => {
            let (outcome, error, text) = fault.ending();
            (Some((1007, "invalid_payload", text)), outcome, error, None)
        }
        End::ClientProtocol(fault @ Fault::Violation) => {
            let (outcome, error, text) = fault.ending();
            (Some((1002, "protocol_error", text)), outcome, error, None)
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
                invalid_text("idle_timeout", relay::IDLE_MESSAGE),
            )),
            Outcome::Succeeded,
            None,
            Some(FinishLabel::Stop),
        ),
        End::MaxDuration => (
            Some((
                1000,
                "session_expired",
                invalid_text("session_expired", relay::EXPIRED_MESSAGE),
            )),
            Outcome::Succeeded,
            None,
            Some(FinishLabel::Length),
        ),
        End::Rejected(r) => (
            Some((1008, r.code, rejection_text(&r))),
            Outcome::Failed,
            Some(rejection_error(&r)),
            None,
        ),
        End::Binary => (
            Some((
                1003,
                "unsupported_data",
                invalid_text("unsupported_event", relay::BINARY_MESSAGE),
            )),
            Outcome::Failed,
            Some(InferenceError::Unsupported),
            None,
        ),
        End::TooLarge => (
            Some((
                1009,
                "message_too_big",
                invalid_text("message_too_big", relay::TOO_LARGE_MESSAGE),
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
    ended.closing.finish(close, outcome, error, finish).await;
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "integration-tests"))]
pub(crate) use tests::inference_openapi_handshake_fixtures;

#[cfg(test)]
use relay::{is_output_delta, protocol_fault, session_text, too_large};

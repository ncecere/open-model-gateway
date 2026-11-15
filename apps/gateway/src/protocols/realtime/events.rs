//! The client-event allowlist: every client text frame is validated here before
//! it is forwarded. A rejected event is returned as a [`Rejection`]; `relay` turns
//! it into an `error` event and a close.
use super::*;

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

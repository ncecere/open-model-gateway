//! OpenAI Realtime (GA interface) over WebSocket at the fixed origin:
//! `wss://api.openai.com/v1/realtime?model=<upstream model>` with the server
//! credential (`Authorization: Bearer`). Client credentials, headers,
//! subprotocols and query parameters are never forwarded.
//!
//! Before the session is returned the adapter sends its own `session.update`
//! (server VAD without automatic responses, input transcription off, the
//! per-response output ceiling) and waits for the acknowledging
//! `session.updated`, so no billable work can start that the gateway did not
//! reserve. Every later `session.updated` is re-checked. Redirects (tungstenite
//! treats them as handshake errors), ambient proxies and retries are absent;
//! handshake error bodies are never read.
use std::{borrow::Cow, sync::Arc};

use futures_util::{SinkExt, StreamExt, future, stream};
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{self, Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

use super::{BASE, OpenAiAdapter, check_status};
use crate::inference::{
    error::InferenceError,
    realtime::{
        MAX_UPSTREAM_MESSAGE_BYTES, RealtimeSetup, RealtimeUpstream, RealtimeUsage, ResponseStatus,
        UpstreamEvent,
    },
    types::Deployment,
};

type Result<T> = std::result::Result<T, InferenceError>;
/// Event id of the gateway's own configuration update.
const INIT_EVENT_ID: &str = "gw_session_init";

/// Validate the connection before resolving credentials.
fn authorization(adapter: &OpenAiAdapter, target: &Deployment) -> Result<HeaderValue> {
    if target.provider != "openai"
        || target.credential_ref == "none"
        || target
            .endpoint
            .as_deref()
            .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
        || target.region.as_deref().is_some_and(|v| !v.is_empty())
        || target.upstream_model.trim().is_empty()
        || target.upstream_model.len() > 256
    {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| InferenceError::Configuration)?;
    auth.set_sensitive(true);
    Ok(auth)
}

/// `wss://…/realtime?model=…` from the fixed base. Plain `ws` exists only for
/// the loopback test constructor.
fn endpoint(adapter: &OpenAiAdapter, target: &Deployment) -> Result<(String, bool)> {
    let mut url = reqwest::Url::parse(&format!("{}/realtime", adapter.base.trim_end_matches('/')))
        .map_err(|_| InferenceError::Configuration)?;
    let secure = match url.scheme() {
        "https" => true,
        "http" if cfg!(test) => false,
        _ => return Err(InferenceError::Configuration),
    };
    url.set_scheme(if secure { "wss" } else { "ws" })
        .map_err(|_| InferenceError::Configuration)?;
    url.query_pairs_mut()
        .append_pair("model", &target.upstream_model);
    Ok((url.into(), secure))
}

/// Explicit ring-backed rustls with the webpki roots (certificate checks on).
fn tls() -> Result<Arc<rustls::ClientConfig>> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| InferenceError::Configuration)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn connect_error(error: tungstenite::Error) -> InferenceError {
    match error {
        // The handshake response body is never read.
        tungstenite::Error::Http(response) => match check_status(response.status()) {
            Ok(()) => InferenceError::InvalidUpstream,
            Err(error) => error,
        },
        tungstenite::Error::Url(_) => InferenceError::Configuration,
        _ => InferenceError::UpstreamUnavailable,
    }
}

/// The gateway's enforced session configuration.
pub(crate) fn init_event(setup: &RealtimeSetup) -> Value {
    json!({
        "type": "session.update",
        "event_id": INIT_EVENT_ID,
        "session": {
            "type": "realtime",
            "max_output_tokens": setup.window_output_tokens,
            "audio": {"input": {
                "turn_detection": {"type": "server_vad", "create_response": false},
                "transcription": null
            }}
        }
    })
}

pub(super) async fn connect(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    setup: &RealtimeSetup,
) -> Result<RealtimeUpstream> {
    let deadline = Instant::now() + setup.connect_timeout;
    let auth = authorization(adapter, target)?;
    let (url, secure) = endpoint(adapter, target)?;
    let mut request = url
        .into_client_request()
        .map_err(|_| InferenceError::Configuration)?;
    request
        .headers_mut()
        .insert(reqwest::header::AUTHORIZATION, auth);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_UPSTREAM_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_UPSTREAM_MESSAGE_BYTES));
    let connector = if secure {
        Connector::Rustls(tls()?)
    } else {
        Connector::Plain
    };
    let (socket, _response) = timeout_at(
        deadline,
        connect_async_tls_with_config(request, Some(config), true, Some(connector)),
    )
    .await
    .map_err(|_| InferenceError::Timeout)?
    .map_err(connect_error)?;
    let (mut sink, mut upstream) = socket.split();
    timeout_at(
        deadline,
        sink.send(Message::Text(init_event(setup).to_string().into())),
    )
    .await
    .map_err(|_| InferenceError::Timeout)?
    .map_err(|_| InferenceError::UpstreamUnavailable)?;
    // Wait for the acknowledgement; earlier events are replayed in order.
    let mut buffered = Vec::new();
    let config;
    loop {
        let message = timeout_at(deadline, upstream.next())
            .await
            .map_err(|_| InferenceError::Timeout)?;
        let event = match message {
            Some(Ok(message)) => match classify_message(message)? {
                Some(event) => event,
                None => continue,
            },
            _ => return Err(InferenceError::UpstreamUnavailable),
        };
        match event {
            UpstreamEvent::Error { event_id, .. } if event_id.as_deref() == Some(INIT_EVENT_ID) => {
                return Err(InferenceError::UpstreamRejected);
            }
            UpstreamEvent::Session { event, safe } if event["type"] == "session.updated" => {
                if !safe {
                    return Err(InferenceError::InvalidUpstream);
                }
                // The acknowledged configuration sizes the first response's
                // hold (instructions, tools, input audio format).
                config = crate::inference::realtime::context::session_config(&event["session"]);
                buffered.push(Ok(UpstreamEvent::Session { event, safe }));
                break;
            }
            other => buffered.push(Ok(other)),
        }
    }
    let live = upstream.filter_map(|message| {
        future::ready(match message {
            Ok(message) => classify_message(message).transpose(),
            Err(_) => Some(Err(InferenceError::UpstreamUnavailable)),
        })
    });
    let sink = sink
        .sink_map_err(|_| InferenceError::UpstreamUnavailable)
        .with(|text: String| future::ready(Ok::<_, InferenceError>(Message::Text(text.into()))));
    Ok(RealtimeUpstream {
        sink: Box::pin(sink),
        events: Box::pin(stream::iter(buffered).chain(live)),
        config,
    })
}

/// `None` for control frames. A close frame or binary data from upstream is
/// an error: the session never ends "cleanly" because upstream went away.
fn classify_message(message: Message) -> Result<Option<UpstreamEvent>> {
    match message {
        Message::Text(text) => classify(text.as_str()).map(Some),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
        Message::Close(_) => Err(InferenceError::UpstreamUnavailable),
        Message::Binary(_) => Err(InferenceError::InvalidUpstream),
    }
}

#[derive(Deserialize)]
struct Peek<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
}

/// A bounded printable token (codes, params, event ids), else unknown.
fn token(value: &Value, max: usize) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned)
}
fn response_id(event: &Value) -> Result<String> {
    token(&event["response"]["id"], 128).ok_or(InferenceError::InvalidUpstream)
}

pub(crate) fn classify(text: &str) -> Result<UpstreamEvent> {
    let peek: Peek = serde_json::from_str(text).map_err(|_| InferenceError::InvalidUpstream)?;
    let parse = || serde_json::from_str::<Value>(text).map_err(|_| InferenceError::InvalidUpstream);
    Ok(match peek.kind.as_ref() {
        "session.created" | "session.updated" => {
            let event = parse()?;
            let safe = session_safe(&event["session"]);
            UpstreamEvent::Session { event, safe }
        }
        "response.created" => UpstreamEvent::ResponseCreated {
            response_id: response_id(&parse()?)?,
            text: text.to_owned(),
        },
        "response.done" => {
            let event = parse()?;
            UpstreamEvent::ResponseDone {
                response_id: response_id(&event)?,
                status: event["response"]["status"]
                    .as_str()
                    .and_then(ResponseStatus::parse),
                usage: usage(&event["response"]["usage"]),
                text: text.to_owned(),
            }
        }
        "error" => {
            let event = parse()?;
            let error = &event["error"];
            UpstreamEvent::Error {
                kind: token(&error["type"], 64),
                code: token(&error["code"], 64),
                param: token(&error["param"], 128),
                event_id: token(&error["event_id"], 128),
            }
        }
        // Upstream account-level rate limits are not tenant information.
        "rate_limits.updated" => UpstreamEvent::Filtered,
        // Transcription is disabled by the gateway; its usage is billed
        // separately and cannot be accounted, so its appearance fails closed.
        kind if kind.starts_with("conversation.item.input_audio_transcription.") => {
            UpstreamEvent::Unaccounted
        }
        _ => UpstreamEvent::Forward(text.to_owned()),
    })
}

/// Effective session configuration starts no billable work by itself.
pub(crate) fn session_safe(session: &Value) -> bool {
    let input = &session["audio"]["input"];
    let detection = &input["turn_detection"];
    let detection_ok = detection.is_null()
        || (detection["create_response"] == Value::Bool(false)
            && detection.get("idle_timeout_ms").is_none_or(Value::is_null));
    detection_ok && input["transcription"].is_null()
}

/// `response.usage` normalized per modality; anything missing, non-integer or
/// inconsistent is unknown (`None`), never zero. Image input is refused by the
/// gateway, so image tokens must be zero.
pub(crate) fn usage(u: &Value) -> Option<RealtimeUsage> {
    let n = |v: &Value| v.as_u64().filter(|n| *n <= i64::MAX as u64);
    let optional_zero = |v: &Value| if v.is_null() { Some(0) } else { n(v) };
    let input = n(&u["input_tokens"])?;
    let output = n(&u["output_tokens"])?;
    let d = &u["input_token_details"];
    let text = n(&d["text_tokens"])?;
    let audio = n(&d["audio_tokens"])?;
    let image = optional_zero(&d["image_tokens"])?;
    let cached = n(&d["cached_tokens"])?;
    let c = &d["cached_tokens_details"];
    let (cached_text, cached_audio, cached_image) = if c.is_object() {
        (
            n(&c["text_tokens"])?,
            n(&c["audio_tokens"])?,
            optional_zero(&c["image_tokens"])?,
        )
    } else if cached == 0 {
        (0, 0, 0)
    } else {
        return None;
    };
    let o = &u["output_token_details"];
    let output_text = n(&o["text_tokens"])?;
    let output_audio = n(&o["audio_tokens"])?;
    let consistent = image == 0
        && cached_image == 0
        && text.checked_add(audio) == Some(input)
        && cached_text.checked_add(cached_audio) == Some(cached)
        && output_text.checked_add(output_audio) == Some(output)
        && u.get("total_tokens")
            .is_none_or(|t| n(t).is_some_and(|t| input.checked_add(output) == Some(t)));
    let usage = RealtimeUsage {
        input_text_tokens: text,
        cached_text_tokens: cached_text,
        input_audio_tokens: audio,
        cached_audio_tokens: cached_audio,
        output_text_tokens: output_text,
        output_audio_tokens: output_audio,
    };
    (consistent && usage.valid()).then_some(usage)
}

/// Loopback test adapter (ws://127.0.0.1) for realtime contract tests.
#[cfg(test)]
pub(crate) fn test_adapter(
    resolver: Arc<dyn crate::providers::secrets::SecretResolver>,
    base: String,
) -> OpenAiAdapter {
    OpenAiAdapter::for_test(resolver, base)
}

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

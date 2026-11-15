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

mod events;
use events::*;

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

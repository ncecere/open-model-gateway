//! The realtime WebSocket session loop: the client and upstream relays, the
//! error events and the closing handshake that `proxy` reports with.
use std::ops::ControlFlow::{self, Break, Continue};

use super::*;
use crate::inference::realtime::RealtimeLimits;

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
pub(super) fn inference_error_text(error: InferenceError) -> String {
    let body = crate::protocols::openai_error_body(error);
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
pub(super) enum End {
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
/// Axum erases its browser parser's error type; use that exact pinned crate,
/// never the independently upgraded upstream transport's `tungstenite` re-export.
/// The loopback regression exercises Axum itself to detect any pin mismatch.
pub(super) fn too_large(error: &axum::Error) -> bool {
    std::error::Error::source(error)
        .and_then(|e| e.downcast_ref::<browser_tungstenite::Error>())
        .is_some_and(|e| matches!(e, browser_tungstenite::Error::Capacity(_)))
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
pub(super) fn session_text(mut event: Value, alias: &str) -> String {
    if let Some(session) = event.get_mut("session").and_then(Value::as_object_mut)
        && session.contains_key("model")
    {
        session.insert("model".into(), json!(alias));
    }
    event.to_string()
}
pub(super) fn is_output_delta(text: &str) -> bool {
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

type ClientTx = futures_util::stream::SplitSink<WebSocket, Message>;
type ClientRx = futures_util::stream::SplitStream<WebSocket>;

/// The session loop's state, shared by the client and upstream branches.
struct Relay {
    session: RealtimeSession,
    upstream: RealtimeUpstream,
    client_tx: ClientTx,
    limits: RealtimeLimits,
    window: u32,
    alias: String,
    idle: Instant,
    bucket: Bucket,
    requested: Option<String>,
    first_output: bool,
}

impl Relay {
    /// One client message: `Continue` keeps the loop running.
    async fn client(&mut self, message: Option<Result<Message, axum::Error>>) -> ControlFlow<End> {
        self.idle = Instant::now() + self.limits.idle;
        let text = match message {
            Some(Err(error)) if too_large(&error) => return Break(End::TooLarge),
            None | Some(Err(_)) => return Break(End::ClientGone),
            Some(Ok(Message::Close(_))) => return Break(End::ClientClosed),
            Some(Ok(Message::Binary(_))) => return Break(End::Binary),
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(_)) => return Continue(()),
        };
        if !self.bucket.take() {
            return Break(End::RateLimited);
        }
        match validate_measured(text.as_str(), self.window) {
            Err(rejection) => Break(End::Rejected(rejection)),
            Ok((ClientAction::Forward(text), size)) => {
                self.session.client_event(size);
                if let Err(error) = send_upstream(&mut self.upstream, text).await {
                    return Break(End::Upstream(error));
                }
                Continue(())
            }
            Ok((
                ClientAction::ResponseCreate {
                    text,
                    event_id,
                    max_output_tokens,
                },
                size,
            )) => {
                self.response_create(text, event_id, max_output_tokens, size)
                    .await
            }
        }
    }

    /// A validated `response.create`: reserve its window, then forward it.
    async fn response_create(
        &mut self,
        text: String,
        event_id: String,
        max_output_tokens: u32,
        size: EventSize,
    ) -> ControlFlow<End> {
        if self.session.response_open() || self.requested.is_some() {
            // Upstream allows one active response; refuse
            // explicitly without forwarding.
            let error = error_text(
                "invalid_request_error",
                "conversation_already_has_active_response",
                "A response is already in progress",
                None,
                Some(&event_id),
            );
            if !send_client(&mut self.client_tx, error).await {
                return Break(End::ClientGone);
            }
            return Continue(());
        }
        self.session.client_event(size);
        if let Err(error) = self.session.reserve_window(max_output_tokens).await {
            return Break(End::Denied(error));
        }
        self.requested = Some(event_id);
        if let Err(error) = send_upstream(&mut self.upstream, text).await {
            return Break(End::Upstream(error));
        }
        Continue(())
    }

    /// One upstream event: relay it to the client unless it is filtered.
    async fn upstream(
        &mut self,
        event: Option<Result<UpstreamEvent, InferenceError>>,
    ) -> ControlFlow<End> {
        self.idle = Instant::now() + self.limits.idle;
        let text = match event {
            None => return Break(End::Upstream(InferenceError::UpstreamUnavailable)),
            Some(Err(error)) => return Break(End::Upstream(error)),
            Some(Ok(event)) => self.upstream_text(event).await?,
        };
        let Some(text) = text else {
            return Continue(());
        };
        if !send_client(&mut self.client_tx, text).await {
            return Break(End::ClientGone);
        }
        Continue(())
    }

    /// Account for one upstream event and give the text the client receives
    /// (`None`: the event is filtered and nothing is sent).
    async fn upstream_text(&mut self, event: UpstreamEvent) -> ControlFlow<End, Option<String>> {
        let text = match event {
            UpstreamEvent::Forward(text) => {
                if !self.first_output && is_output_delta(&text) {
                    self.first_output = true;
                    self.session.first_output();
                }
                text
            }
            UpstreamEvent::Session { event, safe } => {
                // `session.created` shows upstream defaults from before the
                // gateway's acknowledged configuration; every update is checked.
                if !safe && event["type"] == "session.updated" {
                    return Break(End::Upstream(InferenceError::InvalidUpstream));
                }
                self.session
                    .session_config(session_config(&event["session"]));
                session_text(event, &self.alias)
            }
            UpstreamEvent::ResponseCreated { text, response_id } => {
                if let Err(error) = self.session.open_response(response_id).await {
                    return Break(End::Upstream(error));
                }
                self.requested = None;
                text
            }
            UpstreamEvent::ResponseDone {
                text,
                response_id,
                status,
                usage,
            } => {
                if let Err(error) = self
                    .session
                    .settle_response(&response_id, status, usage)
                    .await
                {
                    return Break(End::Upstream(error));
                }
                text
            }
            UpstreamEvent::Error {
                kind,
                code,
                param,
                event_id,
            } => self.upstream_error(kind, code, param, event_id),
            UpstreamEvent::Filtered => return Continue(None),
            UpstreamEvent::Unaccounted => {
                return Break(End::Upstream(InferenceError::InvalidUpstream));
            }
        };
        Continue(Some(text))
    }

    /// An upstream `error` event, rebuilt from its bounded tokens.
    fn upstream_error(
        &mut self,
        kind: Option<String>,
        code: Option<String>,
        param: Option<String>,
        event_id: Option<String>,
    ) -> String {
        if event_id.is_some() && event_id == self.requested {
            self.requested = None;
            self.session.request_rejected();
        }
        error_text(
            kind.as_deref().unwrap_or("server_error"),
            code.as_deref().unwrap_or("upstream_error"),
            "The provider reported an error for this session",
            param.as_deref(),
            event_id.as_deref(),
        )
    }
}

/// How the session loop ended, with what `proxy` still needs to report it.
pub(super) struct Ended {
    pub end: End,
    /// A response was open or requested when the loop ended.
    pub in_flight: bool,
    pub closing: Closing,
}

/// The client side and the session, kept until the end is reported.
pub(super) struct Closing {
    session: RealtimeSession,
    client_tx: ClientTx,
    client_rx: ClientRx,
    client_closed: bool,
}

impl Closing {
    /// Tell the client why the session ended, then record the attempt.
    pub(super) async fn finish(
        self,
        close: Option<(u16, &'static str, String)>,
        outcome: Outcome,
        error: Option<InferenceError>,
        finish: Option<FinishLabel>,
    ) {
        let Self {
            session,
            mut client_tx,
            mut client_rx,
            client_closed,
        } = self;
        close_client(&mut client_tx, &mut client_rx, close, client_closed).await;
        drop(client_tx);
        drop(client_rx);
        let _ = session.finish(outcome, error, finish).await;
    }
}

/// Relay events both ways until the session ends, then stop the upstream
/// (`None`: there was no upstream and the attempt is already finished).
pub(super) async fn relay(socket: WebSocket, mut session: RealtimeSession) -> Option<Ended> {
    let Some(upstream) = session.take_upstream() else {
        let _ = session
            .finish(Outcome::Failed, Some(InferenceError::Storage), None)
            .await;
        return None;
    };
    let limits = session.limits();
    let window = session.window_output_tokens();
    let alias = session.model().to_owned();
    let (client_tx, mut client_rx) = socket.split();
    let deadline = Instant::now() + limits.max_session;
    let mut relay = Relay {
        session,
        upstream,
        client_tx,
        limits,
        window,
        alias,
        idle: Instant::now() + limits.idle,
        bucket: Bucket::new(limits.max_events_per_second),
        requested: None,
        first_output: false,
    };
    let end = loop {
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => break End::MaxDuration,
            _ = sleep_until(relay.idle) => break End::Idle,
            message = client_rx.next() => {
                if let Break(end) = relay.client(message).await {
                    break end;
                }
            }
            event = relay.upstream.events.next() => {
                if let Break(end) = relay.upstream(event).await {
                    break end;
                }
            }
        }
    };
    let Relay {
        session,
        mut upstream,
        client_tx,
        requested,
        ..
    } = relay;
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
    Some(Ended {
        end,
        in_flight,
        closing: Closing {
            session,
            client_tx,
            client_rx,
            client_closed,
        },
    })
}

/// The error event for a gateway-side `invalid_request_error` close.
pub(super) fn invalid_text(code: &str, message: &str) -> String {
    error_text("invalid_request_error", code, message, None, None)
}

/// The error event for a rejected client event.
pub(super) fn rejection_text(r: &Rejection) -> String {
    error_text(
        r.kind,
        r.code,
        r.message,
        r.param.as_deref(),
        r.event_id.as_deref(),
    )
}

/// The attempt error recorded for a rejected client event.
pub(super) fn rejection_error(r: &Rejection) -> InferenceError {
    if r.code == "unsupported_capability" || r.code == "unsupported_event" {
        InferenceError::Unsupported
    } else {
        InferenceError::InvalidRequest
    }
}

/// Send the error event and close frame (if any), then finish the client's
/// side of the closing handshake.
async fn close_client(
    client_tx: &mut ClientTx,
    client_rx: &mut ClientRx,
    close: Option<(u16, &'static str, String)>,
    client_closed: bool,
) {
    if let Some((code, reason, text)) = close
        && send_client(client_tx, text).await
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
}

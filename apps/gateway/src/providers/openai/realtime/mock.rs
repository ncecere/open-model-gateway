//! Scripted mock of the OpenAI Realtime WebSocket (loopback, no paid calls).
//! Records handshake requests and every received text frame, and whether the
//! gateway closed the socket.
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Notify};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
    },
};

pub(crate) const SERVER_SECRET: &str = "sk-mock-server-secret";
pub(crate) const UPSTREAM_MODEL: &str = "gpt-realtime-private";

/// How the mock answers the gateway's own `session.update`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Init {
    Safe,
    /// Acknowledges a configuration with automatic responses still on.
    Unsafe,
    /// Rejects the configuration with an error carrying private details.
    Reject,
    /// Refuses the handshake with this HTTP status.
    Status(u16),
}
/// Reply to one `response.create`.
#[derive(Clone)]
pub(crate) enum Reply {
    /// `response.created`, one audio delta, `response.done` with this usage.
    Done(Value),
    /// `response.done` without a usage object.
    NoUsage,
    /// `response.created` and a delta, then nothing.
    Hang,
    /// An `error` event for the request's event id (no response).
    Reject,
}
pub(crate) fn usage(
    text: u64,
    cached_text: u64,
    audio: u64,
    cached_audio: u64,
    out_text: u64,
    out_audio: u64,
) -> Value {
    json!({
        "total_tokens": text + audio + out_text + out_audio,
        "input_tokens": text + audio,
        "output_tokens": out_text + out_audio,
        "input_token_details": {
            "text_tokens": text, "audio_tokens": audio, "image_tokens": 0,
            "cached_tokens": cached_text + cached_audio,
            "cached_tokens_details": {"text_tokens": cached_text, "audio_tokens": cached_audio, "image_tokens": 0}
        },
        "output_token_details": {"text_tokens": out_text, "audio_tokens": out_audio}
    })
}

/// One handshake: request URI and headers.
pub(crate) type Handshake = (String, Vec<(String, String)>);
#[derive(Default)]
pub(crate) struct Shared {
    pub handshakes: Mutex<Vec<Handshake>>,
    pub received: Mutex<Vec<String>>,
    pub replies: Mutex<VecDeque<Reply>>,
    pub connections: AtomicUsize,
    pub closed: AtomicBool,
    /// Signalled when `closed` is set, so tests wait instead of sleeping.
    closed_signal: Notify,
    pub init: Mutex<Option<Init>>,
}
impl Shared {
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closed_signal.notify_waiters();
    }
    /// Wait until the gateway closed an upstream socket. The bound only
    /// turns a hang into a failure; it is not a timing assumption.
    pub async fn wait_closed(&self) -> bool {
        let closed = async {
            loop {
                // Register before checking so a concurrent close is not missed.
                let notified = self.closed_signal.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.closed.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(60), closed)
            .await
            .is_ok()
    }
    pub fn received_types(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|t| {
                serde_json::from_str::<Value>(t).unwrap()["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
    pub fn received_of(&self, kind: &str) -> Vec<Value> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|t| serde_json::from_str::<Value>(t).unwrap())
            .filter(|v| v["type"] == kind)
            .collect()
    }
}

pub(crate) struct Mock {
    /// `http://127.0.0.1:<port>/v1` (the adapter derives `ws://…/v1/realtime`).
    pub base: String,
    pub shared: Arc<Shared>,
}
fn session(create_response: bool) -> Value {
    json!({"type":"realtime","object":"realtime.session","id":"sess_private","model":UPSTREAM_MODEL,
        "output_modalities":["audio"],"max_output_tokens":"inf",
        "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,
            "turn_detection":{"type":"server_vad","create_response":create_response,"interrupt_response":true,"idle_timeout_ms":null}},
            "output":{"voice":"marin"}}})
}
impl Mock {
    pub async fn start(init: Init, replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let shared = Arc::new(Shared {
            replies: Mutex::new(replies.into()),
            init: Mutex::new(Some(init)),
            ..Shared::default()
        });
        let state = shared.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move { serve(tcp, state).await });
            }
        });
        Self { base, shared }
    }
}
async fn serve(tcp: tokio::net::TcpStream, state: Arc<Shared>) {
    state.connections.fetch_add(1, Ordering::SeqCst);
    let init = state.init.lock().unwrap().unwrap_or(Init::Safe);
    let record = state.clone();
    #[allow(clippy::result_large_err)] // tungstenite's callback signature
    let callback =
        move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let headers = request
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_owned(),
                        v.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            record
                .handshakes
                .lock()
                .unwrap()
                .push((request.uri().to_string(), headers));
            if let Init::Status(code) = init {
                let mut refused = ErrorResponse::new(Some("refused".into()));
                *refused.status_mut() = StatusCode::from_u16(code).unwrap();
                return Err(refused);
            }
            Ok(response)
        };
    let Ok(socket) = accept_hdr_async(tcp, callback).await else {
        return;
    };
    let (mut tx, mut rx) = socket.split();
    let send = |v: Value| Message::Text(v.to_string().into());
    if tx
        .send(send(
            json!({"type":"session.created","event_id":"event_1","session":session(true)}),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut responses = 0;
    while let Some(message) = rx.next().await {
        let Ok(Message::Text(text)) = message else {
            break;
        };
        state.received.lock().unwrap().push(text.to_string());
        let event: Value = serde_json::from_str(text.as_str()).unwrap();
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "session.update" if event["event_id"] == "gw_session_init" => match init {
                Init::Reject => out.push(json!({"type":"error","event_id":"event_e","error":{"type":"invalid_request_error","code":"invalid_value","message":"org-private-123 cannot use gpt-realtime-private","param":"session","event_id":"gw_session_init"}})),
                Init::Unsafe => out.push(json!({"type":"session.updated","event_id":"event_2","session":session(true)})),
                _ => out.push(json!({"type":"session.updated","event_id":"event_2","session":session(false)})),
            },
            "session.update" => {
                out.push(json!({"type":"session.updated","event_id":"event_3","session":session(false)}))
            }
            "response.create" => {
                responses += 1;
                let id = format!("resp_{responses}");
                let reply = state.replies.lock().unwrap().pop_front().unwrap_or(Reply::NoUsage);
                let created = json!({"type":"response.created","event_id":"event_c","response":{"id":id,"object":"realtime.response","status":"in_progress"}});
                let delta = json!({"type":"response.output_audio.delta","event_id":"event_d","response_id":id,"item_id":"item_1","output_index":0,"content_index":0,"delta":"UklGRg=="});
                let done = |usage: Option<Value>| {
                    let mut response = json!({"id":id,"object":"realtime.response","status":"completed","output":[]});
                    if let Some(u) = usage {
                        response["usage"] = u;
                    }
                    json!({"type":"response.done","event_id":"event_f","response":response})
                };
                match reply {
                    Reply::Done(u) => out.extend([created, delta, json!({"type":"rate_limits.updated","event_id":"event_r","rate_limits":[{"name":"tokens","limit":1000000,"remaining":999,"reset_seconds":1}]}), done(Some(u))]),
                    Reply::NoUsage => out.extend([created, done(None)]),
                    Reply::Hang => out.extend([created, delta]),
                    Reply::Reject => out.push(json!({"type":"error","event_id":"event_x","error":{"type":"invalid_request_error","code":"conversation_already_has_active_response","message":"private","event_id":event["event_id"]}})),
                }
            }
            _ => {}
        }
        for v in out {
            if tx.send(send(v)).await.is_err() {
                state.mark_closed();
                return;
            }
        }
    }
    state.mark_closed();
}

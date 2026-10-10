//! Deterministic OpenAI-compatible mock upstream for load tests (scale plan P0).
//!
//! TEST ONLY. It never forwards anything and never calls a provider. It serves
//! `POST /v1/chat/completions` (complete and streamed) with configurable
//! latency, time to first token, completion length and a deterministic error
//! rate, and reports exact, deterministic usage so every successful attempt
//! settles at a known cost. Prompts are never logged or stored: the only
//! request data kept is an optional `nonce:<id>` marker found in the message
//! text, so a load generator can match every upstream call to a client request
//! (and through the gateway's `x-request-id`, to a durable reservation).
//!
//! Admin endpoints (unauthenticated; bind to a private address):
//! `GET /__mock/stats`, `GET /__mock/calls` (JSON array of [`CallRecord`]) and
//! `POST /__mock/reset`.
use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Longest nonce kept from a prompt (`[A-Za-z0-9_-]`).
pub const MAX_NONCE_CHARS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Complete (non-stream) responses: time until the whole response is sent.
    pub latency_ms: u64,
    /// Streams: time until the first content delta.
    pub ttft_ms: u64,
    /// Streams: delay between content deltas.
    pub inter_token_ms: u64,
    /// Completion tokens (and stream deltas) per response, capped by the
    /// request's `max_completion_tokens`/`max_tokens`.
    pub completion_tokens: u32,
    /// Reported prompt tokens, independent of the prompt (deterministic usage).
    pub prompt_tokens: u32,
    /// Fraction of calls answered with `error_status`, chosen deterministically
    /// from `seed` and the request nonce (or call number without a nonce).
    pub error_rate: f64,
    pub error_status: u16,
    pub seed: u64,
    /// Calls kept for `/__mock/calls`; later calls are counted, not kept.
    pub max_records: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latency_ms: 20,
            ttft_ms: 10,
            inter_token_ms: 5,
            completion_tokens: 4,
            prompt_tokens: 12,
            error_rate: 0.0,
            error_status: 500,
            seed: 1,
            max_records: 5_000_000,
        }
    }
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.error_rate),
            "error rate must be between 0 and 1"
        );
        anyhow::ensure!(
            (400..=599).contains(&self.error_status),
            "error status must be 4xx or 5xx"
        );
        anyhow::ensure!(
            self.latency_ms <= 600_000 && self.ttft_ms <= 600_000 && self.inter_token_ms <= 60_000,
            "latencies are capped at 10 minutes"
        );
        anyhow::ensure!(
            self.completion_tokens <= 100_000,
            "too many completion tokens"
        );
        Ok(())
    }
}

/// One upstream call as observed by the mock.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallRecord {
    pub nonce: Option<String>,
    pub stream: bool,
    /// HTTP status sent.
    pub status: u16,
    pub completion_tokens: u32,
    /// Streams: whether `[DONE]` was sent (false when the client went away).
    pub completed: bool,
    /// Request received to last byte produced, in microseconds.
    pub service_us: u64,
}

#[derive(Serialize)]
pub struct Stats {
    pub calls: u64,
    pub errors: u64,
    pub in_flight: u64,
    pub records: usize,
    pub dropped_records: u64,
    pub config: Config,
}

pub struct Mock {
    config: Config,
    calls: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicU64,
    dropped: AtomicU64,
    records: Mutex<Vec<CallRecord>>,
}

impl Mock {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            config,
            calls: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            records: Mutex::new(Vec::new()),
        }))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn record(&self, record: CallRecord) {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if records.len() < self.config.max_records {
            records.push(record);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn calls(&self) -> Vec<CallRecord> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            calls: self.calls.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            records: self.records.lock().unwrap_or_else(|e| e.into_inner()).len(),
            dropped_records: self.dropped.load(Ordering::Relaxed),
            config: self.config.clone(),
        }
    }

    pub fn reset(&self) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.calls.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
    }

    /// Deterministic per-call error decision.
    pub fn fails(&self, nonce: Option<&str>, call: u64) -> bool {
        if self.config.error_rate <= 0.0 {
            return false;
        }
        let mut hash = Sha256::new();
        hash.update(self.config.seed.to_le_bytes());
        match nonce {
            Some(nonce) => hash.update(nonce.as_bytes()),
            None => hash.update(call.to_le_bytes()),
        }
        let digest = hash.finalize();
        let value = u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"));
        (value % 1_000_000) < (self.config.error_rate * 1_000_000.0) as u64
    }
}

/// Counts in-flight calls and records the call when the response (or stream)
/// is finished or dropped.
struct CallGuard {
    mock: Arc<Mock>,
    started: Instant,
    record: Option<CallRecord>,
}
impl CallGuard {
    fn new(mock: Arc<Mock>, record: CallRecord) -> Self {
        mock.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            mock,
            started: Instant::now(),
            record: Some(record),
        }
    }
    fn complete(&mut self) {
        if let Some(record) = self.record.as_mut() {
            record.completed = true;
        }
    }
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.mock.in_flight.fetch_sub(1, Ordering::Relaxed);
        if let Some(mut record) = self.record.take() {
            record.service_us = self.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
            self.mock.record(record);
        }
    }
}

pub fn router(mock: Arc<Mock>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .route("/health", get(|| async { "ok\n" }))
        .route("/__mock/stats", get(stats))
        .route("/__mock/calls", get(calls))
        .route("/__mock/reset", post(reset))
        .layer(DefaultBodyLimit::max(2 << 20))
        .with_state(mock)
}

async fn stats(State(mock): State<Arc<Mock>>) -> Json<Stats> {
    Json(mock.stats())
}
async fn calls(State(mock): State<Arc<Mock>>) -> Json<Vec<CallRecord>> {
    Json(mock.calls())
}
async fn reset(State(mock): State<Arc<Mock>>) -> StatusCode {
    mock.reset();
    StatusCode::NO_CONTENT
}
async fn models() -> Json<Value> {
    Json(json!({"object":"list","data":[{"id":"mock-chat","object":"model","owned_by":"mock"}]}))
}

fn error(status: StatusCode, message: &str, kind: &str) -> Response {
    let mut response = (
        status,
        Json(json!({"error":{"message":message,"type":kind,"code":kind}})),
    )
        .into_response();
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("uuid header"),
    );
    response
}

/// `nonce:<id>` anywhere in the text of the messages (last one wins).
pub fn find_nonce(messages: &Value) -> Option<String> {
    let mut found = None;
    let mut scan = |text: &str| {
        for (index, _) in text.match_indices("nonce:") {
            let id: String = text[index + 6..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(MAX_NONCE_CHARS)
                .collect();
            if !id.is_empty() {
                found = Some(id);
            }
        }
    };
    for message in messages.as_array().into_iter().flatten() {
        match &message["content"] {
            Value::String(text) => scan(text),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .for_each(&mut scan),
            _ => {}
        }
    }
    found
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The deterministic usage object for `completion` tokens.
pub fn usage(config: &Config, completion: u32) -> Value {
    json!({
        "prompt_tokens": config.prompt_tokens,
        "completion_tokens": completion,
        "total_tokens": u64::from(config.prompt_tokens) + u64::from(completion),
        // Complete cache split, so the attempt settles at a known cost.
        "prompt_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}
    })
}

async fn chat(State(mock): State<Arc<Mock>>, body: Bytes) -> Response {
    let call = mock.calls.fetch_add(1, Ordering::Relaxed);
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid JSON",
            "invalid_request_error",
        );
    };
    let (Some(model), Some(messages)) = (request["model"].as_str(), request.get("messages")) else {
        return error(
            StatusCode::BAD_REQUEST,
            "model and messages are required",
            "invalid_request_error",
        );
    };
    let model = model.to_owned();
    let stream = request["stream"].as_bool().unwrap_or(false);
    let nonce = find_nonce(messages);
    let cap = request["max_completion_tokens"]
        .as_u64()
        .or_else(|| request["max_tokens"].as_u64())
        .unwrap_or(u64::MAX);
    let tokens = u64::from(mock.config.completion_tokens).min(cap) as u32;
    let fails = mock.fails(nonce.as_deref(), call);
    let mut guard = CallGuard::new(
        mock.clone(),
        CallRecord {
            nonce,
            stream,
            status: if fails { mock.config.error_status } else { 200 },
            completion_tokens: if fails { 0 } else { tokens },
            completed: false,
            service_us: 0,
        },
    );
    let config = mock.config.clone();
    if fails {
        mock.errors.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(config.ttft_ms.min(config.latency_ms))).await;
        guard.complete();
        let status =
            StatusCode::from_u16(config.error_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return error(status, "injected mock failure", "mock_injected_error");
    }
    let id = format!("chatcmpl-mock{call}");
    if !stream {
        tokio::time::sleep(Duration::from_millis(config.latency_ms)).await;
        let body = json!({
            "id": id,
            "object": "chat.completion",
            "created": unix_now(),
            "model": model,
            "choices": [{"index":0,"message":{"role":"assistant","content":"tok ".repeat(tokens as usize)},"finish_reason":"stop"}],
            "usage": usage(&config, tokens),
        });
        guard.complete();
        let mut response = Json(body).into_response();
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("uuid header"),
        );
        return response;
    }
    let created = unix_now();
    let frames = async_stream::stream! {
        // The guard moves into the stream: a client disconnect drops it with
        // `completed: false`.
        let mut guard = guard;
        let chunk = |delta: Value, finish: Value| {
            Bytes::from(format!(
                "data: {}\n\n",
                json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                       "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            ))
        };
        tokio::time::sleep(Duration::from_millis(config.ttft_ms)).await;
        for n in 0..tokens {
            if n > 0 {
                tokio::time::sleep(Duration::from_millis(config.inter_token_ms)).await;
            }
            let delta = if n == 0 {
                json!({"role": "assistant", "content": "tok "})
            } else {
                json!({"content": "tok "})
            };
            yield Ok::<_, Infallible>(chunk(delta, Value::Null));
        }
        yield Ok(chunk(json!({}), json!("stop")));
        yield Ok(Bytes::from(format!(
            "data: {}\n\n",
            json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                   "choices": [], "usage": usage(&config, tokens)})
        )));
        guard.complete();
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    let mut response = Response::new(Body::from_stream(frames));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        "x-request-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("uuid header"),
    );
    response
}

/// Bind and serve until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    mock: Arc<Mock>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(mock))
        .with_graceful_shutdown(shutdown)
        .await
}

#[cfg(test)]
mod tests;

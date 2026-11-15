//! Per-client pool idle timeouts and the lost-connection counter (#483).
//!
//! An upstream (or a load balancer in front of it) closes an idle keep-alive
//! connection on its own timeout. A request written at that moment is lost,
//! and the gateway does not resend it. Two things narrow and measure that:
//!
//! - every pooled outbound client sets a pool idle timeout below its
//!   upstream's, so the pool drops a connection before the upstream does
//!   ([`UpstreamClient::pool_idle_timeout`]);
//! - [`UpstreamClient::lost_before_response`] counts the failure class that
//!   remains, in `gateway_upstream_connection_lost_total{client}`.
//!
//! Neither resends anything. `reqwest::retry::never()` governs only
//! reqwest's own retry policy; the connection pool below it can re-dispatch
//! a request it never wrote (see [`connection_lost_before_response`]).
//! What the counter proves, and what it does not, is stated there.
//!
//! `tests` in `egress_wiring_tests.rs` pin which client each call site names.

use std::{error::Error as _, time::Duration};

use crate::metrics::Metrics;

/// The outbound clients that keep a connection pool, as a fixed metric label
/// and as the source of each client's pool idle timeout. OIDC is not here:
/// identity requests use one connection each (#417). Nothing derived from a
/// request, host or error text is ever a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpstreamClient {
    Openai,
    Openrouter,
    Anthropic,
    Bedrock,
    Local,
    S3,
    /// The shared export client: OTLP traces, metrics and logs, the audit
    /// export (webhook and OTLP) and alert webhooks.
    Export,
}

impl UpstreamClient {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::Openai,
        Self::Openrouter,
        Self::Anthropic,
        Self::Bedrock,
        Self::Local,
        Self::S3,
        Self::Export,
    ];

    /// The `client` label value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::Bedrock => "bedrock",
            Self::Local => "local",
            Self::S3 => "s3",
            Self::Export => "export",
        }
    }

    /// How long the pool keeps an unused connection. It must stay below the
    /// shortest idle timeout on the path to the upstream, with a margin for
    /// latency: the pool checks a connection's age when it takes one, so a
    /// connection older than this is never reused. The providers do not
    /// publish their idle timeouts; every figure below is a conservative
    /// choice to be tuned with `gateway_upstream_connection_lost_total`
    /// (docs/operations.md, "Lost connections").
    ///
    /// - OpenAI, OpenRouter, Anthropic, Bedrock: 20 s. AWS documents 60 s as
    ///   the Application Load Balancer default idle timeout, and Cloudflare
    ///   documents 400 s of keep-alive toward clients. Whether each provider
    ///   sits behind either is not verified.
    /// - S3: 3 s. Not published, and the reports conflict (3 to 5 s in one
    ///   community answer, about 20 s in another); 3 s is the conservative
    ///   pick and unverified.
    /// - Local: 4 s. uvicorn's `timeout_keep_alive` defaults to 5 s
    ///   (`uvicorn/config.py`), and vLLM passes `VLLM_HTTP_TIMEOUT_KEEP_ALIVE`
    ///   (default 5) to it. llama.cpp's server sets no keep-alive timeout,
    ///   so its cpp-httplib default applies (5 s, and at most 100 requests
    ///   per connection). Ollama's Go `http.Server` sets no `IdleTimeout`, so
    ///   it does not close idle connections. A server with a shorter
    ///   keep-alive (gunicorn's `keepalive` defaults to 2 s) still loses
    ///   requests; `client="local"` shows it.
    /// - Export: 10 s. The OpenTelemetry Collector's confighttp
    ///   `keepalive::idle_timeout` defaults to 1 minute. Pushes are seconds
    ///   to minutes apart, so most open a connection anyway.
    pub(crate) const fn pool_idle_timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Openai | Self::Openrouter | Self::Anthropic | Self::Bedrock => 20,
            Self::S3 => 3,
            Self::Local => 4,
            Self::Export => 10,
        })
    }

    /// Counts `error` if it is a connection lost before a complete response
    /// head. Call it where a send error is mapped; it changes nothing about
    /// the error the caller returns.
    pub(crate) fn lost_before_response(self, error: &reqwest::Error) {
        #[cfg(test)]
        if let Some(sink) = SINK.with(std::cell::Cell::get) {
            self.count_on(sink, error);
            return;
        }
        self.count_on(&crate::metrics::METRICS, error);
    }

    /// A `map_err` function that counts the error, then returns `value`:
    /// the mapping a call site already had, with the counting added.
    pub(crate) fn count_then<T>(self, value: T) -> impl FnOnce(reqwest::Error) -> T {
        move |error| {
            self.lost_before_response(&error);
            value
        }
    }

    pub(super) fn count_on(self, metrics: &Metrics, error: &reqwest::Error) -> bool {
        let lost = connection_lost_before_response(error);
        if lost {
            metrics.upstream_connection_lost(self.as_str());
        }
        lost
    }
}

#[cfg(test)]
thread_local! {
    static SINK: std::cell::Cell<Option<&'static Metrics>> = const { std::cell::Cell::new(None) };
}

/// Test only: counts on this thread go to `metrics` until the guard drops,
/// so a test asserts an exact count that no parallel test can change.
#[cfg(test)]
pub(crate) fn count_here(metrics: &'static Metrics) -> impl Drop {
    struct Restore(Option<&'static Metrics>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SINK.with(|sink| sink.set(self.0));
        }
    }
    Restore(SINK.with(|sink| sink.replace(Some(metrics))))
}

/// True when a request failed after the gateway handed it to a connection
/// and before a complete response head arrived, because the connection was
/// lost: a request error (not connect, redirect, body or decode), not a
/// timeout, not the gateway's own request body failing, whose cause chain
/// holds a closed, incomplete or canceled hyper message or a reset, aborted,
/// broken-pipe or end-of-file I/O error.
///
/// This is what reqwest 0.12 and hyper 1 let us establish, and no more:
///
/// - It does not prove the connection was reused. Neither reqwest nor
///   hyper-util says whether a failed request used a pooled connection. A
///   stale idle connection is one cause among several. The class also holds
///   a fresh connection reset or closed (an accept and close is counted
///   with the server seeing no byte), an upstream crash, restart or drain
///   mid-request, a middlebox or NAT cutting a long non-streaming wait, a
///   reset during a large upload, and a partial response head (partial
///   status line or headers).
/// - It does not say whether the upstream saw the request. A counted
///   request may have been fully written and processed, or never written.
/// - A request error from the gateway's own request body stream is a
///   `hyper::Error` that `is_user()`, whatever its I/O kind, and is not
///   counted.
/// - HTTP/1 only: the gateway's reqwest has no `http2` feature.
///
/// Nothing is resent by the gateway or by reqwest (`retry::never()`). The
/// connection pool beneath reqwest can re-dispatch a request that had not
/// left it: hyper-util 0.1.21's `send_request` loops while a failure is
/// retryable, the connection was reused and `retry_canceled_requests` (on by
/// default, and reqwest 0.12.28 offers no switch) is true, so a request
/// handed back unwritten may go to a new connection, once per stale idle
/// connection until a fresh one is used. It is never written twice. That
/// predates #483, such a request is not an error and is not counted, and a
/// shorter pool idle timeout makes it rarer.
pub(crate) fn connection_lost_before_response(error: &reqwest::Error) -> bool {
    if !error.is_request() || error.is_connect() || error.is_timeout() {
        return false;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(cause) = source {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
            // The gateway's own request body failed: not the connection.
            if hyper.is_user() {
                return false;
            }
            if hyper.is_closed() || hyper.is_incomplete_message() || hyper.is_canceled() {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
            )
        {
            return true;
        }
        // io::Error::source skips its boxed custom error; inspect it directly.
        source = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .map(|inner| inner as &(dyn std::error::Error + 'static))
            .or_else(|| cause.source());
    }
    false
}

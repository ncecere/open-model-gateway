//! Per-client pool idle timeouts and the lost-connection counter (#483).
//!
//! An upstream (or a load balancer in front of it) closes an idle keep-alive
//! connection on its own timeout. A request written at that moment is lost,
//! and with no retry (`reqwest::retry::never()`) the caller sees a failed
//! request. Two things narrow and measure that, and neither resends anything:
//!
//! - every outbound client sets a pool idle timeout below its upstream's, so
//!   the pool drops a connection before the upstream does
//!   ([`UpstreamClient::pool_idle_timeout`]);
//! - [`UpstreamClient::lost_before_response`] counts the failure class that
//!   remains, in `gateway_upstream_connection_lost_total{client}`.
//!
//! What the counter proves, and what it does not, is stated on
//! [`connection_lost_before_response`].

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
    /// export (webhook and OTLP) and alert webhooks. Its pool idle timeout
    /// applies to all of them; only the OTLP pushes and alert webhooks are
    /// counted (the audit export has its own failure categories, and its
    /// file is at its size ceiling).
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
    /// latency: the pool checks a connection's age when it is taken, so a
    /// connection older than this is never reused. No upstream below
    /// publishes its idle timeout except where stated; every other figure is
    /// a conservative choice, to be tuned with
    /// `gateway_upstream_connection_lost_total` (docs/operations.md).
    ///
    /// - OpenAI, OpenRouter, Anthropic: not published. 20 s is under the
    ///   60 s that AWS documents as the Application Load Balancer default
    ///   (unverified for these providers' front ends).
    /// - Bedrock runtime: not published. 20 s, as above.
    /// - S3: not published; a community report (AWS re:Post, unverified)
    ///   sees S3 close idle connections after 3 to 5 s, so 3 s.
    /// - Local: operator-run servers. uvicorn's `timeout_keep_alive` defaults
    ///   to 5 s, which is what vLLM's OpenAI server runs on; llama.cpp's
    ///   HTTP server also defaults to 5 s (both from their documentation,
    ///   not re-verified here). 4 s keeps a 1 s margin.
    /// - Export: collectors differ and pushes are seconds to a minute apart,
    ///   so most pushes open a connection anyway; 10 s keeps the busy trace
    ///   exporter's reuse and drops a connection before a typical 30 s to
    ///   60 s collector timeout (unverified).
    pub(crate) const fn pool_idle_timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Openai | Self::Openrouter | Self::Anthropic | Self::Bedrock => 20,
            Self::S3 => 3,
            Self::Local => 4,
            Self::Export => 10,
        })
    }

    /// Counts `error` if it is a connection lost before any response byte.
    /// Call it where a send error is mapped; it changes nothing about the
    /// error the caller returns.
    pub(crate) fn lost_before_response(self, error: &reqwest::Error) {
        self.count_on(&crate::metrics::METRICS, error);
    }

    pub(super) fn count_on(self, metrics: &Metrics, error: &reqwest::Error) -> bool {
        let lost = connection_lost_before_response(error);
        if lost {
            metrics.upstream_connection_lost(self.as_str());
        }
        lost
    }
}

/// True when a request failed after it was handed to a connection and before
/// any response byte arrived, because the connection was lost: the error is a
/// request error (not a connect, redirect, body or decode error), not a
/// timeout, and its cause chain holds a closed or incomplete hyper message or
/// a reset, aborted, broken-pipe or end-of-file I/O error.
///
/// This is what reqwest 0.12 and hyper 1 let us establish, and no more:
///
/// - It does not prove the connection was reused. Neither reqwest nor
///   hyper-util says whether a failed request used a pooled connection, so a
///   fresh connection that the upstream reset mid-request is counted too.
///   (hyper-util itself resends, once, a request it never began to write on a
///   reused connection; such a request is not an error and is not counted.)
/// - It does not prove the upstream never began the work. The request may
///   have been fully written before the connection was lost.
/// - The class is the one a stale pooled connection produces, so a rise that
///   follows a pool idle timeout near the upstream's points at that cause.
///   It is not proof of it.
/// - HTTP/1 only: the gateway's reqwest has no `http2` feature.
pub(crate) fn connection_lost_before_response(error: &reqwest::Error) -> bool {
    if !error.is_request() || error.is_connect() || error.is_timeout() {
        return false;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(cause) = source {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>()
            && (hyper.is_closed() || hyper.is_incomplete_message() || hyper.is_canceled())
        {
            return true;
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

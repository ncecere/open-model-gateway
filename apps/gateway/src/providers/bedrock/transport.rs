//! Bounded transport for SDK-signed requests; never constructs AWS authentication.
use aws_smithy_runtime_api::client::{
    http::{
        HttpConnector, HttpConnectorFuture, SharedHttpClient, SharedHttpConnector, http_client_fn,
    },
    orchestrator::{HttpRequest, HttpResponse},
    result::ConnectorError,
};
use aws_smithy_types::body::SdkBody;
use futures_util::StreamExt;
use std::time::Duration;

// Includes JSON envelope / event framing overhead, not just normalized model output.
pub(super) const WIRE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct Connector {
    client: crate::egress::HttpClient,
    streaming_read: Option<Duration>,
}
impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BedrockBoundedConnector")
    }
}
fn failure() -> ConnectorError {
    ConnectorError::other(
        Box::new(std::io::Error::other("Provider transport failed")),
        None,
    )
}
// This marker is created only when an active HTTP-body read expires. Provider
// payloads and unrelated SDK/decoding errors cannot manufacture its class.
#[derive(Debug)]
struct ReadTimeout;
impl std::fmt::Display for ReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Provider body timeout")
    }
}
impl std::error::Error for ReadTimeout {}

pub(super) fn is_read_timeout(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if error.is::<ReadTimeout>()
            || error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .is_some_and(|inner| inner.is::<ReadTimeout>())
        {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.client.clone();
        let streaming_read = self.streaming_read;
        HttpConnectorFuture::new(async move {
            let method =
                reqwest::Method::from_bytes(request.method().as_bytes()).map_err(|_| failure())?;
            let bytes = request.body().bytes().ok_or_else(failure)?;
            if bytes.len() > WIRE_LIMIT {
                return Err(failure());
            }
            let mut builder = client.request(method, request.uri()).body(bytes.to_vec());
            for (name, value) in request.headers().iter() {
                let mut value =
                    reqwest::header::HeaderValue::from_str(value).map_err(|_| failure())?;
                if matches!(name, "authorization" | "x-amz-security-token") {
                    value.set_sensitive(true);
                }
                builder = builder.header(name, value);
            }
            // Bound the active wait for headers without attaching a wall-clock
            // timer to the returned streaming response body.
            let response = if let Some(read) = streaming_read {
                tokio::time::timeout(read, builder.send())
                    .await
                    .map_err(|_| {
                        ConnectorError::timeout(Box::new(std::io::Error::other("Provider timeout")))
                    })?
            } else {
                builder.send().await
            }
            .map_err(|error| {
                crate::egress::UpstreamClient::Bedrock.lost_before_response(&error);
                if error.is_timeout() {
                    ConnectorError::timeout(Box::new(std::io::Error::other("Provider timeout")))
                } else {
                    failure()
                }
            })?;
            if response
                .content_length()
                .is_some_and(|n| n > WIRE_LIMIT as u64)
            {
                return Err(failure());
            }
            let status = response
                .status()
                .as_u16()
                .try_into()
                .map_err(|_| failure())?;
            let headers = response.headers().clone();
            let successful = response.status().is_success();
            let event_stream = headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| h.split(';').next() == Some("application/vnd.amazon.eventstream"));
            let body = async_stream::try_stream! {
                let mut stream = response.bytes_stream();
                let mut size = 0usize;
                let mut guard = super::wire::StreamGuard::default();
                let mut complete = Vec::new();
                loop {
                    // Ready bytes win over an elapsed timer when a reader resumes.
                    // Each chunk's progress starts a fresh active-read budget,
                    // including partial AWS frames buffered by the wire guard.
                    let next = if let Some(read) = streaming_read {
                        tokio::time::timeout(read, stream.next()).await
                            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, ReadTimeout))?
                    } else { stream.next().await };
                    let Some(chunk) = next else { break; };
                    let chunk = chunk.map_err(|_| std::io::Error::other("Provider body failed"))?;
                    if chunk.len() > WIRE_LIMIT - size { Err(std::io::Error::other("Provider body limit"))?; }
                    size += chunk.len();
                    if !successful { yield http_body::Frame::data(chunk); }
                    else if event_stream {
                        for frame in guard.push(&chunk)? { yield http_body::Frame::data(axum::body::Bytes::from(frame)); }
                    } else { complete.extend_from_slice(&chunk); }
                }
                if successful {
                    if event_stream { guard.end()?; }
                    else { super::wire::complete(&complete)?; yield http_body::Frame::data(axum::body::Bytes::from(complete)); }
                }
            };
            let body: std::pin::Pin<
                Box<
                    dyn futures_util::Stream<Item = std::result::Result<_, std::io::Error>>
                        + Send
                        + Sync,
                >,
            > = Box::pin(body);
            let mut result = HttpResponse::new(
                status,
                SdkBody::from_body_1_x(http_body_util::StreamBody::new(body)),
            );
            for (name, value) in &headers {
                let value = value.to_str().map_err(|_| failure())?;
                result
                    .headers_mut()
                    .try_insert(name.as_str().to_owned(), value.to_owned())
                    .map_err(|_| failure())?;
            }
            Ok(result)
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Timeouts {
    pub read: Duration,
    pub total: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            read: Duration::from_secs(60),
            total: Duration::from_secs(300),
        }
    }
}

pub(super) fn client(timeouts: Timeouts, streaming: bool) -> anyhow::Result<SharedHttpClient> {
    // SDK config cannot override these bounded settings. No redirects, hidden retries,
    // ambient proxies or detached producer tasks. Dropping the body cancels its socket work.
    // Every resolved address must pass the egress policy (#33).
    let builder =
        crate::egress::client_builder(crate::egress::resolver(crate::egress::Purpose::Provider))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(crate::egress::UpstreamClient::Bedrock.pool_idle_timeout());
    // ConverseStream's lifetime belongs to the engine watchdog. A reqwest total
    // timer also runs while the consumer is idle and would falsely fail a healthy
    // stream at this fixed boundary. Keep the total cap for non-stream Converse.
    // The ready-first active body-read timer replaces reqwest's timer-first
    // read timer. Its budget resets on wire progress, before frame assembly.
    let builder = if streaming {
        builder
    } else {
        builder.read_timeout(timeouts.read).timeout(timeouts.total)
    };
    let client = builder
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
    let connector = SharedHttpConnector::new(Connector {
        client,
        streaming_read: streaming.then_some(timeouts.read),
    });
    Ok(http_client_fn(move |_, _| connector.clone()))
}

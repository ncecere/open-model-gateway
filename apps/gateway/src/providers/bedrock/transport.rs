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
struct Connector(reqwest::Client);
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
impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.0.clone();
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
            let response = builder.send().await.map_err(|error| {
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
            let body = async_stream::try_stream! {
                let mut stream = response.bytes_stream();
                let mut size = 0usize;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| std::io::Error::other("Provider body failed"))?;
                    if chunk.len() > WIRE_LIMIT - size { Err(std::io::Error::other("Provider body limit"))?; }
                    size += chunk.len();
                    yield http_body::Frame::data(chunk);
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

pub(super) fn client() -> anyhow::Result<SharedHttpClient> {
    // SDK config cannot override these bounded settings. No redirects, hidden retries,
    // ambient proxies or detached producer tasks. Dropping the body cancels its socket work.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
    let connector = SharedHttpConnector::new(Connector(client));
    Ok(http_client_fn(move |_, _| connector.clone()))
}

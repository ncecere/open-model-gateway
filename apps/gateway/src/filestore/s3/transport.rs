//! Bounded reqwest transport for SDK-signed S3 requests. No redirects, no ambient
//! proxy, no reqwest retries, explicit connect/read timeouts. Request bodies are
//! always in memory (single PUT or one multipart part); successful GetObject
//! bodies stream unbounded (they are the object), everything else is capped.
use std::time::Duration;

use aws_smithy_runtime_api::client::{
    http::{
        HttpConnector, HttpConnectorFuture, SharedHttpClient, SharedHttpConnector, http_client_fn,
    },
    orchestrator::{HttpRequest, HttpResponse},
    result::ConnectorError,
};
use aws_smithy_types::body::SdkBody;
use futures::StreamExt;

/// XML responses (multipart create/complete, errors) are small.
const SMALL_BODY_LIMIT: usize = 1024 * 1024;
/// Largest request body: one multipart part plus slack.
const REQUEST_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct Connector(reqwest::Client);
impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileStoreS3Connector")
    }
}

fn failure() -> ConnectorError {
    ConnectorError::other(
        Box::new(std::io::Error::other("File store transport failed")),
        None,
    )
}

impl HttpConnector for Connector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.0.clone();
        HttpConnectorFuture::new(async move {
            let method =
                reqwest::Method::from_bytes(request.method().as_bytes()).map_err(|_| failure())?;
            let streaming_get = method == reqwest::Method::GET;
            let bytes = request.body().bytes().ok_or_else(failure)?;
            if bytes.len() > REQUEST_LIMIT {
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
                    ConnectorError::timeout(Box::new(std::io::Error::other("File store timeout")))
                } else if error.is_connect() {
                    ConnectorError::io(Box::new(std::io::Error::other("File store connect failed")))
                } else {
                    failure()
                }
            })?;
            let successful = response.status().is_success();
            let limit = if successful && streaming_get {
                usize::MAX
            } else {
                SMALL_BODY_LIMIT
            };
            if response.content_length().is_some_and(|n| n > limit as u64) {
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
                    let chunk = chunk.map_err(|_| std::io::Error::other("File store body failed"))?;
                    if chunk.len() > limit - size {
                        Err(std::io::Error::other("File store body limit"))?;
                    }
                    size += chunk.len();
                    yield http_body::Frame::data(chunk);
                }
            };
            let body: std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = std::result::Result<_, std::io::Error>>
                        + Send
                        + Sync,
                >,
            > = Box::pin(body);
            let mut result = HttpResponse::new(
                status,
                SdkBody::from_body_1_x(http_body_util::StreamBody::new(body)),
            );
            for (name, value) in &headers {
                let Ok(value) = value.to_str() else { continue };
                result
                    .headers_mut()
                    .try_insert(name.as_str().to_owned(), value.to_owned())
                    .map_err(|_| failure())?;
            }
            Ok(result)
        })
    }
}

/// `ca_pem`: optional extra trust anchor(s) for a private S3-compatible endpoint.
pub(super) fn client(ca_pem: Option<&[u8]>) -> anyhow::Result<SharedHttpClient> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60));
    if let Some(pem) = ca_pem {
        for cert in reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|_| anyhow::anyhow!("GATEWAY_S3_CA_FILE is not a PEM certificate bundle"))?
        {
            builder = builder.add_root_certificate(cert);
        }
    }
    let client = builder
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to initialize file store transport"))?;
    let connector = SharedHttpConnector::new(Connector(client));
    Ok(http_client_fn(move |_, _| connector.clone()))
}

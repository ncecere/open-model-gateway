//! S3 and S3-compatible backend (AWS S3, MinIO, RustFS, Cloudflare R2, Wasabi,
//! Ceph RGW, GCS interoperability) through `aws-sdk-s3`.
//!
//! - Identity: Bedrock's `aws:default` / `aws:profile:<name>` / `aws:role:<arn>`
//!   references (same parsing, same `GATEWAY_AWS_PROFILE_ALLOWLIST`), or `static`
//!   keys read from two environment-variable references at startup.
//! - Endpoint: unset means AWS S3 for the region. Any explicit endpoint must
//!   exactly match `GATEWAY_S3_ENDPOINT_ALLOWLIST`; that entry is the approval for
//!   `http://` (private networks only). Shared AWS endpoint configuration
//!   (`AWS_ENDPOINT_URL*`, profile endpoints) is never inherited.
//! - Compatibility: flexible request checksums and response validation run only
//!   "when required" (no `x-amz-checksum-*`/`aws-chunked` trailers that older
//!   MinIO/RustFS/Ceph releases reject), S3 Express session auth and multi-region
//!   access points are off, path-style addressing defaults on with an endpoint.
//! - One attempt per request (no implicit retries), no redirects or ambient proxy,
//!   10 s connect, 60 s read and 300 s operation timeouts.
//! - Objects at most [`PART`] bytes of ciphertext use one PutObject; larger ones a
//!   multipart upload in [`PART`]-byte parts, aborted on any failure or cancellation.
use std::time::Duration;

use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Credentials, Region, RequestChecksumCalculation,
        ResponseChecksumValidation, SharedCredentialsProvider, StalledStreamProtectionConfig,
        retry::RetryConfig, timeout::TimeoutConfig,
    },
    error::{ProvideErrorMetadata, SdkError},
    primitives::ByteStream as SdkStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use zeroize::Zeroizing;

use super::{Backend, BackendKind, ByteStream, FileStoreError, ObjectKey, crypto::valid_env_name};
use crate::providers::bedrock::auth::{self as aws_auth, AwsAuth};

mod transport;

/// Multipart part size (ciphertext). 10,000 parts bound one object to ~78 GiB.
pub(crate) const PART: usize = 8 * 1024 * 1024;
const MAX_PARTS: i32 = 10_000;

pub const BUCKET_ENV: &str = "GATEWAY_S3_BUCKET";
pub const PREFIX_ENV: &str = "GATEWAY_S3_PREFIX";
pub const REGION_ENV: &str = "GATEWAY_S3_REGION";
pub const ENDPOINT_ENV: &str = "GATEWAY_S3_ENDPOINT";
pub const PATH_STYLE_ENV: &str = "GATEWAY_S3_FORCE_PATH_STYLE";
pub const AUTH_ENV: &str = "GATEWAY_S3_AUTH";
pub const ACCESS_KEY_ENV: &str = "GATEWAY_S3_ACCESS_KEY_ID_ENV";
pub const SECRET_KEY_ENV: &str = "GATEWAY_S3_SECRET_ACCESS_KEY_ENV";
pub const ENDPOINT_ALLOWLIST_ENV: &str = "GATEWAY_S3_ENDPOINT_ALLOWLIST";
pub const CA_FILE_ENV: &str = "GATEWAY_S3_CA_FILE";

/// How the backend authenticates. No Debug: static keys and external IDs.
pub(crate) enum S3Auth {
    Aws(AwsAuth),
    Static {
        access_key_id: Zeroizing<String>,
        secret_access_key: Zeroizing<String>,
    },
}

impl S3Auth {
    pub(crate) fn mode(&self) -> &'static str {
        match self {
            Self::Aws(AwsAuth::Default) => "aws_default",
            Self::Aws(AwsAuth::Profile(_)) => "aws_profile",
            Self::Aws(AwsAuth::Role { .. }) => "aws_role",
            Self::Static { .. } => "static",
        }
    }
}

/// Validated S3 configuration. No Debug.
pub struct S3Settings {
    pub(crate) bucket: String,
    pub(crate) prefix: Option<String>,
    pub(crate) region: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) path_style: bool,
    pub(crate) auth: S3Auth,
    pub(crate) ca_pem: Option<Vec<u8>>,
}

fn charset(value: &str, extra: &[u8]) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// S3 bucket naming rules (lowercase DNS-compatible, 3-63 characters, not an IP).
pub(crate) fn valid_bucket(bucket: &str) -> bool {
    let b = bucket.as_bytes();
    (3..=63).contains(&b.len())
        && bucket
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && !bucket.contains("..")
        && !bucket.contains(".-")
        && !bucket.contains("-.")
        && bucket.parse::<std::net::Ipv4Addr>().is_err()
}

/// Normalized key prefix: segments of `[A-Za-z0-9._-]`, joined by `/`, no `.`/`..`.
pub(crate) fn normalize_prefix(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.len() > 256 {
        return None;
    }
    trimmed
        .split('/')
        .all(|s| !s.is_empty() && s != "." && s != ".." && charset(s, b"._-"))
        .then(|| trimmed.to_owned())
}

/// Exact `http(s)://host[:port]` origin in canonical form (no credentials, path,
/// query, fragment or trailing slash beyond the root).
pub(crate) fn canonical_origin(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    let canonical = url.as_str().trim_end_matches('/').to_owned();
    (matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|h| !h.is_empty() && charset(h, b".-[]:"))
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && (endpoint == canonical || endpoint == format!("{canonical}/")))
    .then_some(canonical)
}

/// A region code: an AWS region for AWS S3; for S3-compatible endpoints any
/// short lowercase code (`us-east-1`, `auto` for R2, `us-central1` for GCS).
fn valid_region(region: &str, custom_endpoint: bool) -> bool {
    if custom_endpoint {
        (1..=32).contains(&region.len())
            && region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    } else {
        aws_auth::valid_region(region)
    }
}

fn secret_value(
    lookup: &dyn Fn(&str) -> Option<String>,
    reference_env: &str,
) -> anyhow::Result<Zeroizing<String>> {
    let name = lookup(reference_env)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("{reference_env} is required with GATEWAY_S3_AUTH=static")
        })?;
    anyhow::ensure!(
        valid_env_name(&name),
        "{reference_env} must name an environment variable (A-Z, 0-9, _)"
    );
    let value = Zeroizing::new(
        lookup(&name)
            .ok_or_else(|| anyhow::anyhow!("the variable named by {reference_env} is not set"))?,
    );
    anyhow::ensure!(
        (1..=1024).contains(&value.len())
            && !value.chars().any(|c| c.is_control() || c.is_whitespace()),
        "the variable named by {reference_env} does not hold a valid credential"
    );
    Ok(value)
}

impl S3Settings {
    /// Reads and validates the `GATEWAY_S3_*` variables through `lookup`.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let bucket = get(BUCKET_ENV).ok_or_else(|| {
            anyhow::anyhow!("{BUCKET_ENV} is required with GATEWAY_FILE_STORE=s3")
        })?;
        anyhow::ensure!(
            valid_bucket(&bucket),
            "{BUCKET_ENV} is not a valid bucket name"
        );
        let prefix = match get(PREFIX_ENV) {
            None => None,
            Some(p) => Some(normalize_prefix(&p).ok_or_else(|| {
                anyhow::anyhow!("{PREFIX_ENV} must be path segments of A-Z a-z 0-9 . _ - (at most 256 characters)")
            })?),
        };
        let endpoint = match get(ENDPOINT_ENV) {
            None => None,
            Some(e) => {
                let canonical = canonical_origin(&e).ok_or_else(|| {
                    anyhow::anyhow!("{ENDPOINT_ENV} must be an exact http(s)://host[:port] origin")
                })?;
                let mut allowed = Vec::new();
                for entry in get(ENDPOINT_ALLOWLIST_ENV).unwrap_or_default().split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        continue;
                    }
                    allowed.push(canonical_origin(entry).ok_or_else(|| {
                        anyhow::anyhow!("{ENDPOINT_ALLOWLIST_ENV} entries must be exact http(s)://host[:port] origins")
                    })?);
                }
                anyhow::ensure!(
                    allowed.contains(&canonical),
                    "{ENDPOINT_ENV} must be listed in {ENDPOINT_ALLOWLIST_ENV}"
                );
                Some(canonical)
            }
        };
        let region = get(REGION_ENV).unwrap_or_else(|| "us-east-1".into());
        anyhow::ensure!(
            valid_region(&region, endpoint.is_some()),
            "{REGION_ENV} is not a valid region code"
        );
        let path_style = match get(PATH_STYLE_ENV).as_deref() {
            None => endpoint.is_some(),
            Some("true") => true,
            Some("false") => false,
            Some(_) => anyhow::bail!("{PATH_STYLE_ENV} must be true or false"),
        };
        let auth_ref = get(AUTH_ENV).unwrap_or_else(|| "aws:default".into());
        let auth = if auth_ref == "static" {
            S3Auth::Static {
                access_key_id: secret_value(lookup, ACCESS_KEY_ENV)?,
                secret_access_key: secret_value(lookup, SECRET_KEY_ENV)?,
            }
        } else {
            anyhow::ensure!(
                get(ACCESS_KEY_ENV).is_none() && get(SECRET_KEY_ENV).is_none(),
                "{ACCESS_KEY_ENV}/{SECRET_KEY_ENV} are only used with GATEWAY_S3_AUTH=static"
            );
            let auth = AwsAuth::parse(&auth_ref).ok_or_else(|| {
                anyhow::anyhow!(
                    "{AUTH_ENV} must be static, aws:default, aws:profile:<name> or aws:role:<role-arn>"
                )
            })?;
            let profiles = get(aws_auth::PROFILE_ALLOWLIST_ENV).unwrap_or_default();
            let policy = aws_auth::Policy::new(profiles.split(','), std::iter::empty());
            anyhow::ensure!(
                policy.allows(&auth),
                "the AWS profile in {AUTH_ENV} is not on {}",
                aws_auth::PROFILE_ALLOWLIST_ENV
            );
            S3Auth::Aws(auth)
        };
        let ca_pem = match get(CA_FILE_ENV) {
            None => None,
            Some(path) => Some(
                std::fs::read(&path)
                    .map_err(|_| anyhow::anyhow!("{CA_FILE_ENV} could not be read"))?,
            ),
        };
        Ok(Self {
            bucket,
            prefix,
            region,
            endpoint,
            path_style,
            auth,
            ca_pem,
        })
    }

    /// Endpoint host only (no scheme, port or path), for display.
    pub fn endpoint_host(&self) -> Option<String> {
        self.endpoint
            .as_deref()
            .and_then(|e| reqwest::Url::parse(e).ok())
            .and_then(|u| u.host_str().map(str::to_owned))
    }
    pub fn endpoint_tls(&self) -> bool {
        self.endpoint
            .as_deref()
            .is_none_or(|e| e.starts_with("https://"))
    }
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
    pub fn region(&self) -> &str {
        &self.region
    }
    pub fn path_style(&self) -> bool {
        self.path_style
    }
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }
    pub fn auth_mode(&self) -> &'static str {
        self.auth.mode()
    }
}

pub(crate) struct S3Backend {
    client: Client,
    bucket: String,
    prefix: Option<String>,
}

fn sdk_error<E: ProvideErrorMetadata>(
    error: &SdkError<E, aws_smithy_runtime_api::client::orchestrator::HttpResponse>,
) -> FileStoreError {
    match error {
        SdkError::TimeoutError(_) => FileStoreError::Timeout,
        SdkError::DispatchFailure(d) if d.is_timeout() => FileStoreError::Timeout,
        SdkError::ServiceError(e) => {
            let status = e.raw().status().as_u16();
            match (e.err().code(), status) {
                (Some("NoSuchKey" | "NotFound"), _) | (_, 404) => FileStoreError::NotFound,
                (
                    Some(
                        "AccessDenied"
                        | "InvalidAccessKeyId"
                        | "SignatureDoesNotMatch"
                        | "InvalidToken"
                        | "ExpiredToken"
                        | "AllAccessDisabled",
                    ),
                    _,
                )
                | (_, 401 | 403) => FileStoreError::Denied,
                _ => FileStoreError::Unavailable,
            }
        }
        // Identity resolution failures (missing/denied credentials) surface here.
        SdkError::ConstructionFailure(_) => FileStoreError::Denied,
        _ => FileStoreError::Unavailable,
    }
}

/// The same SDK client the backend uses, without the encryption layer, for
/// integration tests that create buckets or tamper with stored ciphertext.
#[cfg(feature = "integration-tests")]
pub async fn raw_client_for_tests(settings: &S3Settings) -> anyhow::Result<Client> {
    Ok(S3Backend::new(settings).await?.client)
}

impl S3Backend {
    pub(crate) async fn new(settings: &S3Settings) -> anyhow::Result<Self> {
        let credentials = match &settings.auth {
            S3Auth::Static {
                access_key_id,
                secret_access_key,
            } => SharedCredentialsProvider::new(Credentials::new(
                access_key_id.as_str(),
                secret_access_key.as_str(),
                None,
                None,
                "gateway-file-store-static",
            )),
            S3Auth::Aws(auth) => {
                // STS/credential discovery needs an AWS region even for "auto".
                let region = if aws_auth::valid_region(&settings.region) {
                    settings.region.as_str()
                } else {
                    "us-east-1"
                };
                aws_auth::credentials(auth, region).await
            }
        };
        let http = transport::client(settings.ca_pem.as_deref())?;
        let mut builder = aws_sdk_s3::Config::builder();
        builder.set_endpoint_url(settings.endpoint.clone());
        let config = builder
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(settings.region.clone()))
            .credentials_provider(credentials)
            .http_client(http)
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .read_timeout(Duration::from_secs(60))
                    .operation_attempt_timeout(Duration::from_secs(300))
                    .operation_timeout(Duration::from_secs(300))
                    .build(),
            )
            .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .force_path_style(settings.path_style)
            .disable_s3_express_session_auth(true)
            .disable_multi_region_access_points(true)
            .use_arn_region(false)
            .build();
        Ok(Self {
            client: Client::from_conf(config),
            bucket: settings.bucket.clone(),
            prefix: settings.prefix.clone(),
        })
    }

    fn object(&self, key: &ObjectKey) -> String {
        match &self.prefix {
            Some(p) => format!("{p}/{}", key.as_str()),
            None => key.as_str().to_owned(),
        }
    }

    async fn upload_parts(
        &self,
        object: &str,
        upload_id: &str,
        mut buf: BytesMut,
        body: &mut ByteStream,
    ) -> Result<(u64, Vec<CompletedPart>), FileStoreError> {
        let mut parts = Vec::new();
        let mut total = 0u64;
        let mut ended = false;
        let mut number = 1i32;
        loop {
            while !ended && buf.len() <= PART {
                match body.next().await {
                    Some(piece) => buf.extend_from_slice(&piece?),
                    None => ended = true,
                }
            }
            let take = if ended { buf.len() } else { PART };
            if take == 0 {
                break;
            }
            if number > MAX_PARTS {
                return Err(FileStoreError::TooLarge);
            }
            let part: Bytes = buf.split_to(take).freeze();
            total += part.len() as u64;
            let output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(object)
                .upload_id(upload_id)
                .part_number(number)
                .content_length(part.len() as i64)
                .body(SdkStream::from(part))
                .send()
                .await
                .map_err(|e| sdk_error(&e))?;
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(output.e_tag().map(str::to_owned))
                    .build(),
            );
            number += 1;
            if ended && buf.is_empty() {
                break;
            }
        }
        Ok((total, parts))
    }
}

/// Aborts an unfinished multipart upload if the put future is dropped
/// (cancellation). Explicit failures abort inline instead.
struct AbortOnDrop {
    client: Client,
    bucket: String,
    object: String,
    upload_id: Option<String>,
}

impl AbortOnDrop {
    async fn abort_now(&mut self) {
        if let Some(id) = self.upload_id.take() {
            let _ = tokio::time::timeout(
                Duration::from_secs(30),
                self.client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(&self.object)
                    .upload_id(id)
                    .send(),
            )
            .await;
        }
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(id) = self.upload_id.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let request = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.object)
                .upload_id(id);
            handle.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(30), request.send()).await;
            });
        }
    }
}

#[async_trait::async_trait]
impl Backend for S3Backend {
    fn kind(&self) -> BackendKind {
        BackendKind::S3
    }

    async fn put(&self, key: &ObjectKey, mut body: ByteStream) -> Result<u64, FileStoreError> {
        let object = self.object(key);
        let mut buf = BytesMut::new();
        let mut ended = false;
        while !ended && buf.len() <= PART {
            match body.next().await {
                Some(piece) => buf.extend_from_slice(&piece?),
                None => ended = true,
            }
        }
        if ended {
            let len = buf.len() as u64;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&object)
                .content_type("application/octet-stream")
                .content_length(len as i64)
                .body(SdkStream::from(buf.freeze()))
                .send()
                .await
                .map_err(|e| sdk_error(&e))?;
            return Ok(len);
        }
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&object)
            .content_type("application/octet-stream")
            .send()
            .await
            .map_err(|e| sdk_error(&e))?;
        let upload_id = created
            .upload_id()
            .ok_or(FileStoreError::Unavailable)?
            .to_owned();
        let mut guard = AbortOnDrop {
            client: self.client.clone(),
            bucket: self.bucket.clone(),
            object: object.clone(),
            upload_id: Some(upload_id.clone()),
        };
        let result = async {
            let (total, parts) = self
                .upload_parts(&object, &upload_id, buf, &mut body)
                .await?;
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&object)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .map_err(|e| sdk_error(&e))?;
            Ok(total)
        }
        .await;
        match result {
            Ok(total) => {
                guard.upload_id = None;
                Ok(total)
            }
            Err(e) => {
                guard.abort_now().await;
                Err(e)
            }
        }
    }

    async fn get(&self, key: &ObjectKey) -> Result<ByteStream, FileStoreError> {
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.object(key))
            .send()
            .await
            .map_err(|e| sdk_error(&e))?;
        let mut body = output.body;
        let stream = async_stream::stream! {
            loop {
                match body.next().await {
                    None => return,
                    Some(Ok(bytes)) => yield Ok(bytes),
                    Some(Err(_)) => { yield Err(FileStoreError::Unavailable); return; }
                }
            }
        };
        Ok(stream.boxed())
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), FileStoreError> {
        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.object(key))
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => match sdk_error(&e) {
                FileStoreError::NotFound if e.code() != Some("NoSuchBucket") => Ok(()),
                other => Err(other),
            },
        }
    }

    async fn head(&self, key: &ObjectKey) -> Result<Option<u64>, FileStoreError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.object(key))
            .send()
            .await
        {
            Ok(output) => output
                .content_length()
                .and_then(|n| u64::try_from(n).ok())
                .map(Some)
                .ok_or(FileStoreError::Unavailable),
            Err(e) => match sdk_error(&e) {
                FileStoreError::NotFound => Ok(None),
                other => Err(other),
            },
        }
    }
}

#[cfg(test)]
mod tests;

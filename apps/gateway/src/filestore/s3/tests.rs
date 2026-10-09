//! S3 configuration validation and wire behaviour against a loopback mock:
//! path-style URLs, no flexible checksums or aws-chunked bodies, one attempt,
//! no redirects. Real MinIO/RustFS compatibility is in tests/filestore_s3.rs.
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::StreamExt;

use super::*;
use crate::filestore::{Encrypted, FileStore, KeyRing, Purpose, PutMeta, Scope};

type Lookup = Box<dyn Fn(&str) -> Option<String>>;

fn lookup(pairs: &[(&str, &str)]) -> Lookup {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Box::new(move |name| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
}

fn error(pairs: &[(&str, &str)]) -> String {
    S3Settings::from_lookup(&*lookup(pairs))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn validates_bucket_prefix_region_and_endpoint_allowlist() {
    for ok in ["abc", "my-bucket.logs", "a1-b2"] {
        assert!(valid_bucket(ok), "{ok}");
    }
    for bad in [
        "ab",
        "Upper",
        "-x-",
        "a..b",
        "a.-b",
        "192.168.1.1",
        &"a".repeat(64),
        "a_b",
    ] {
        assert!(!valid_bucket(bad), "{bad}");
    }
    assert_eq!(
        normalize_prefix("/gateway/files/").as_deref(),
        Some("gateway/files")
    );
    for bad in ["", "/", "a//b", "a/../b", "./a", "a b", "ä"] {
        assert_eq!(normalize_prefix(bad), None, "{bad}");
    }
    assert_eq!(
        canonical_origin("https://s3.example.com/").as_deref(),
        Some("https://s3.example.com")
    );
    assert_eq!(
        canonical_origin("http://minio:9000").as_deref(),
        Some("http://minio:9000")
    );
    for bad in [
        "https://user:pw@s3.example.com",
        "https://s3.example.com/path",
        "https://s3.example.com?x=1",
        "ftp://s3.example.com",
        "https://s3.example.com:443",
        "s3.example.com",
    ] {
        assert_eq!(canonical_origin(bad), None, "{bad}");
    }
    let base = [("GATEWAY_S3_BUCKET", "files")];
    // AWS defaults: aws:default, us-east-1, virtual-hosted style.
    let s = S3Settings::from_lookup(&*lookup(&base)).unwrap();
    assert_eq!(
        (s.region(), s.path_style(), s.auth_mode()),
        ("us-east-1", false, "aws_default")
    );
    assert!(s.endpoint_host().is_none() && s.endpoint_tls());
    assert!(
        error(&[
            ("GATEWAY_S3_BUCKET", "files"),
            ("GATEWAY_S3_REGION", "auto")
        ])
        .contains("region")
    );
    // An endpoint must be allowlisted exactly; http is approved only by that entry.
    let minio = [
        ("GATEWAY_S3_BUCKET", "files"),
        ("GATEWAY_S3_ENDPOINT", "http://minio.internal:9000"),
    ];
    assert!(error(&minio).contains("GATEWAY_S3_ENDPOINT_ALLOWLIST"));
    let mut allowed = minio.to_vec();
    allowed.push((
        "GATEWAY_S3_ENDPOINT_ALLOWLIST",
        "https://other.example, http://minio.internal:9000",
    ));
    let s = S3Settings::from_lookup(&*lookup(&allowed)).unwrap();
    assert!(s.path_style() && !s.endpoint_tls());
    assert_eq!(s.endpoint_host().as_deref(), Some("minio.internal"));
    let mut https_only = minio.to_vec();
    https_only.push((
        "GATEWAY_S3_ENDPOINT_ALLOWLIST",
        "https://minio.internal:9000",
    ));
    assert!(error(&https_only).contains("ALLOWLIST"));
    let mut broken = minio.to_vec();
    broken.push(("GATEWAY_S3_ENDPOINT_ALLOWLIST", "minio.internal"));
    assert!(error(&broken).contains("exact"));
    // R2 uses region "auto" with an endpoint; path style can be turned off explicitly.
    let r2 = [
        ("GATEWAY_S3_BUCKET", "files"),
        (
            "GATEWAY_S3_ENDPOINT",
            "https://acct.r2.cloudflarestorage.com",
        ),
        (
            "GATEWAY_S3_ENDPOINT_ALLOWLIST",
            "https://acct.r2.cloudflarestorage.com",
        ),
        ("GATEWAY_S3_REGION", "auto"),
        ("GATEWAY_S3_FORCE_PATH_STYLE", "false"),
    ];
    let s = S3Settings::from_lookup(&*lookup(&r2)).unwrap();
    assert!(!s.path_style() && s.endpoint_tls());
    let mut bad_style = r2.to_vec();
    bad_style[4] = ("GATEWAY_S3_FORCE_PATH_STYLE", "yes");
    assert!(error(&bad_style).contains("true or false"));
}

#[test]
fn auth_references_follow_bedrock_rules_and_static_keys_are_references() {
    let with = |extra: &[(&'static str, &'static str)]| {
        let mut pairs = vec![("GATEWAY_S3_BUCKET", "files")];
        pairs.extend_from_slice(extra);
        pairs
    };
    let role = with(&[(
        "GATEWAY_S3_AUTH",
        "aws:role:arn:aws:iam::123456789012:role/gateway-files",
    )]);
    assert_eq!(
        S3Settings::from_lookup(&*lookup(&role))
            .unwrap()
            .auth_mode(),
        "aws_role"
    );
    let profile = with(&[("GATEWAY_S3_AUTH", "aws:profile:files")]);
    assert!(error(&profile).contains("GATEWAY_AWS_PROFILE_ALLOWLIST"));
    let allowed = with(&[
        ("GATEWAY_S3_AUTH", "aws:profile:files"),
        ("GATEWAY_AWS_PROFILE_ALLOWLIST", "bedrock,files"),
    ]);
    assert_eq!(
        S3Settings::from_lookup(&*lookup(&allowed))
            .unwrap()
            .auth_mode(),
        "aws_profile"
    );
    assert!(
        error(&with(&[("GATEWAY_S3_AUTH", "aws:role:not-an-arn")])).contains("GATEWAY_S3_AUTH")
    );
    assert!(error(&with(&[("GATEWAY_S3_AUTH", "AKIA123")])).contains("GATEWAY_S3_AUTH"));
    // Static: names of variables, never values; missing pieces fail clearly.
    let stat = with(&[
        ("GATEWAY_S3_AUTH", "static"),
        ("GATEWAY_S3_ACCESS_KEY_ID_ENV", "FILES_KEY_ID"),
        ("GATEWAY_S3_SECRET_ACCESS_KEY_ENV", "FILES_SECRET"),
        ("FILES_KEY_ID", "minioadmin"),
        ("FILES_SECRET", "very-secret-value"),
    ]);
    assert_eq!(
        S3Settings::from_lookup(&*lookup(&stat))
            .unwrap()
            .auth_mode(),
        "static"
    );
    let unset = with(&[
        ("GATEWAY_S3_AUTH", "static"),
        ("GATEWAY_S3_ACCESS_KEY_ID_ENV", "FILES_KEY_ID"),
        ("GATEWAY_S3_SECRET_ACCESS_KEY_ENV", "FILES_SECRET"),
        ("FILES_KEY_ID", "minioadmin"),
    ]);
    assert!(error(&unset).contains("GATEWAY_S3_SECRET_ACCESS_KEY_ENV"));
    let literal = with(&[
        ("GATEWAY_S3_AUTH", "static"),
        ("GATEWAY_S3_ACCESS_KEY_ID_ENV", "minioadmin"),
        ("GATEWAY_S3_SECRET_ACCESS_KEY_ENV", "FILES_SECRET"),
    ]);
    let message = error(&literal);
    assert!(
        message.contains("must name an environment variable"),
        "{message}"
    );
    let mixed = with(&[("GATEWAY_S3_ACCESS_KEY_ID_ENV", "FILES_KEY_ID")]);
    assert!(error(&mixed).contains("only used with GATEWAY_S3_AUTH=static"));
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body_len: usize,
}

/// Loopback S3 stand-in: records requests; `mode` picks the response.
async fn mock(mode: &'static str) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let record = seen.clone();
    let app = Router::new().fallback(move |request: Request| {
        let record = record.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, 64 * 1024 * 1024).await.unwrap();
            record.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
                    .collect(),
                body_len: bytes.len(),
            });
            let response: Response = match mode {
                "unavailable" => (StatusCode::SERVICE_UNAVAILABLE, "<Error><Code>SlowDown</Code></Error>").into_response(),
                "redirect" => Response::builder()
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("location", "http://127.0.0.1:9/elsewhere")
                    .body(Body::empty())
                    .unwrap(),
                "denied" => (StatusCode::FORBIDDEN, "<Error><Code>AccessDenied</Code><Message>no</Message></Error>").into_response(),
                _ if parts.method == "POST" && parts.uri.query() == Some("uploads") => (
                    StatusCode::OK,
                    "<InitiateMultipartUploadResult><Bucket>files</Bucket><Key>k</Key><UploadId>up-1</UploadId></InitiateMultipartUploadResult>",
                )
                    .into_response(),
                _ if parts.method == "POST" => (
                    StatusCode::OK,
                    "<CompleteMultipartUploadResult><ETag>\"e\"</ETag></CompleteMultipartUploadResult>",
                )
                    .into_response(),
                _ => Response::builder()
                    .status(StatusCode::OK)
                    .header("etag", "\"etag\"")
                    .body(Body::empty())
                    .unwrap(),
            };
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (origin, seen)
}

async fn store_for(origin: &str) -> Encrypted<S3Backend> {
    let settings = S3Settings::from_lookup(&*lookup(&[
        ("GATEWAY_S3_BUCKET", "files"),
        ("GATEWAY_S3_PREFIX", "gw/test"),
        ("GATEWAY_S3_ENDPOINT", origin),
        ("GATEWAY_S3_ENDPOINT_ALLOWLIST", origin),
        ("GATEWAY_S3_AUTH", "static"),
        ("GATEWAY_S3_ACCESS_KEY_ID_ENV", "K"),
        ("GATEWAY_S3_SECRET_ACCESS_KEY_ENV", "S"),
        ("K", "test-access-key"),
        ("S", "test-secret-key"),
    ]))
    .unwrap();
    Encrypted::new(
        S3Backend::new(&settings).await.unwrap(),
        Arc::new(KeyRing::random("mock")),
    )
}

fn checksum_headers(seen: &Seen) -> Vec<&(String, String)> {
    seen.headers
        .iter()
        .filter(|(k, v)| {
            k.starts_with("x-amz-checksum")
                || k == "x-amz-sdk-checksum-algorithm"
                || k == "x-amz-trailer"
                || k == "content-md5"
                || (k == "content-encoding" && v.contains("aws-chunked"))
                || (k == "x-amz-content-sha256" && v.starts_with("STREAMING-"))
        })
        .collect()
}

fn body_of(len: usize) -> crate::filestore::ByteStream {
    futures::stream::iter(
        (0..len)
            .step_by(65_536)
            .map(move |at| Ok(bytes::Bytes::from(vec![7u8; (len - at).min(65_536)]))),
    )
    .boxed()
}

#[tokio::test]
async fn requests_are_path_style_without_flexible_checksums() {
    let (origin, seen) = mock("ok").await;
    let s = store_for(&origin).await;
    let key = ObjectKey::new(Purpose::BatchInput, Scope::Workspace(uuid::Uuid::new_v4())).unwrap();
    s.put(&key, body_of(1000), PutMeta::default())
        .await
        .unwrap();
    // Larger than one part: multipart create, two parts, complete.
    let big = ObjectKey::new(Purpose::BatchOutput, Scope::Workspace(uuid::Uuid::new_v4())).unwrap();
    s.put(&big, body_of(PART + 1000), PutMeta::default())
        .await
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["PUT", "POST", "PUT", "PUT", "POST"], "{seen:#?}");
    assert!(
        seen[0].uri.starts_with(&format!("/files/gw/test/{key}")),
        "{}",
        seen[0].uri
    );
    assert!(seen[1].uri.ends_with("?uploads"));
    assert!(seen[2].uri.contains("partNumber=1") && seen[2].uri.contains("uploadId=up-1"));
    assert_eq!(seen[2].body_len, PART);
    for request in &seen {
        assert!(checksum_headers(request).is_empty(), "{request:#?}");
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k == "authorization" && v.starts_with("AWS4-HMAC-SHA256"))
        );
        // Host header carries the endpoint, not a virtual-hosted bucket name.
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k == "host" && v.starts_with("127.0.0.1"))
        );
    }
}

#[tokio::test]
async fn one_attempt_no_redirects_and_safe_errors() {
    let (origin, seen) = mock("unavailable").await;
    let s = store_for(&origin).await;
    let key = ObjectKey::new(Purpose::Export, Scope::Installation).unwrap();
    assert_eq!(
        s.put(&key, body_of(10), PutMeta::default())
            .await
            .unwrap_err(),
        FileStoreError::Unavailable
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "no implicit retry");
    let (origin, seen) = mock("redirect").await;
    let s = store_for(&origin).await;
    assert!(s.get(&key).await.is_err());
    assert_eq!(seen.lock().unwrap().len(), 1, "redirect not followed");
    let (origin, _) = mock("denied").await;
    let s = store_for(&origin).await;
    assert_eq!(s.head(&key).await.unwrap_err(), FileStoreError::Denied);
    assert_eq!(s.get(&key).await.err(), Some(FileStoreError::Denied));
}

#[tokio::test]
async fn failed_multipart_uploads_are_aborted() {
    let (origin, seen) = mock("ok").await;
    let s = store_for(&origin).await;
    let key = ObjectKey::new(Purpose::VideoOutput, Scope::Workspace(uuid::Uuid::new_v4())).unwrap();
    let failing = body_of(PART + 70_000)
        .chain(futures::stream::iter([Err(FileStoreError::Source)]))
        .boxed();
    assert_eq!(
        s.put(&key, failing, PutMeta::default()).await.unwrap_err(),
        FileStoreError::Source
    );
    let seen = seen.lock().unwrap().clone();
    let last = seen.last().unwrap();
    assert_eq!(last.method, "DELETE", "{seen:#?}");
    assert!(last.uri.contains("uploadId=up-1"));
}

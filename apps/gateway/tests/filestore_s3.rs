#![cfg(feature = "integration-tests")]
//! Real S3-compatible stores: MinIO and RustFS in throwaway containers on random
//! loopback ports (docs/file-storage.md#compatibility). Images are pinned by digest.
//!
//! Skips with a message only when Docker is unavailable; CI sets
//! `FILESTORE_REQUIRE_DOCKER=1`, which turns a missing Docker into a failure.
//! `FILESTORE_MINIO_IMAGE` / `FILESTORE_RUSTFS_IMAGE` override the images (for
//! example a local MinIO source build). The optional real-AWS test runs only
//! with `FILESTORE_AWS_TEST_BUCKET` set (never in CI).
use std::{process::Command, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::StreamExt;
use open_model_gateway::filestore::{
    ByteStream, FileStore, FileStoreConfig, FileStoreError, FileStoreRuntime, ObjectKey, Purpose,
    PutMeta, Scope, s3::S3Settings,
};
use sha2::Digest;
use uuid::Uuid;

/// Upstream MinIO stopped publishing community images in October 2025; this is
/// the pgsty community build of the upstream AGPL source. pgsty publishes only
/// to Docker Hub, so it is pulled through Google's Docker Hub mirror (same
/// digest; no Docker Hub rate limit). The mirror serves only images it has
/// cached, so [`pull`] retries with backoff and names the override on failure.
const MINIO_IMAGE: &str = "mirror.gcr.io/pgsty/minio:RELEASE.2026-08-04T00-00-00Z@sha256:b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372";
/// RustFS's own registry (the same digest as its Docker Hub image).
const RUSTFS_IMAGE: &str = "ghcr.io/rustfs/rustfs:1.0.1@sha256:1803faef57627e2d9c2e7d89d655d712ddded5389040054987163043fecb6a3c";
/// Bounded pull retries: 4 attempts, waiting 5, 10 and 20 seconds between them.
const PULL_ATTEMPTS: u32 = 4;
const CHUNK: usize = 65_536;
const PART: usize = 8 * 1024 * 1024;
const KEYS: &str = "it2026:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn docker_available() -> bool {
    let ok = Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var("FILESTORE_REQUIRE_DOCKER").as_deref() != Ok("1"),
            "FILESTORE_REQUIRE_DOCKER=1 but Docker is unavailable"
        );
        eprintln!(
            "SKIPPED: Docker is unavailable, so the MinIO/RustFS file store tests did not run"
        );
    }
    ok
}

/// Removes the container even if the test panics.
struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .output();
    }
}

/// Pulls `image` unless it is already present, retrying transient registry
/// failures with exponential backoff (bounded by [`PULL_ATTEMPTS`]).
fn pull(image: &str, override_var: &str) {
    let present = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
        .is_ok_and(|o| o.status.success());
    if present {
        return;
    }
    let mut last = String::new();
    for attempt in 1..=PULL_ATTEMPTS {
        let out = Command::new("docker")
            .args(["pull", "--quiet", image])
            .output()
            .expect("docker pull");
        if out.status.success() {
            return;
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if attempt < PULL_ATTEMPTS {
            let wait = Duration::from_secs(5 << (attempt - 1));
            eprintln!(
                "docker pull {image} failed (attempt {attempt}/{PULL_ATTEMPTS}), retrying in {wait:?}: {last}"
            );
            std::thread::sleep(wait);
        }
    }
    panic!(
        "docker pull {image} failed after {PULL_ATTEMPTS} attempts ({last}); set {override_var} to another reference for the same image"
    );
}

fn start(image: &str, env: &[(&str, &str)], args: &[&str]) -> (Container, u16) {
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "-d",
        "-p",
        "127.0.0.1::9000",
        "--label",
        "omg-filestore-test=1",
    ]);
    for (k, v) in env {
        cmd.args(["-e", &format!("{k}={v}")]);
    }
    cmd.arg(image).args(args);
    let out = cmd.output().expect("docker run");
    assert!(
        out.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let container = Container(String::from_utf8(out.stdout).unwrap().trim().to_owned());
    let port = Command::new("docker")
        .args(["port", &container.0, "9000/tcp"])
        .output()
        .expect("docker port");
    let mapping = String::from_utf8(port.stdout).unwrap();
    let port: u16 = mapping
        .lines()
        .find_map(|l| l.strip_prefix("127.0.0.1:"))
        .and_then(|p| p.trim().parse().ok())
        .expect("published port");
    (container, port)
}

struct Target {
    _container: Container,
    name: &'static str,
    origin: String,
    secret: String,
}

fn secret() -> String {
    format!("s{}", Uuid::new_v4().simple())
}

fn minio() -> Target {
    let secret = secret();
    let image = std::env::var("FILESTORE_MINIO_IMAGE").unwrap_or_else(|_| MINIO_IMAGE.into());
    pull(&image, "FILESTORE_MINIO_IMAGE");
    let (container, port) = start(
        &image,
        &[
            ("MINIO_ROOT_USER", "omgtest"),
            ("MINIO_ROOT_PASSWORD", &secret),
        ],
        &["server", "/data"],
    );
    Target {
        _container: container,
        name: "minio",
        origin: format!("http://127.0.0.1:{port}"),
        secret,
    }
}

fn rustfs() -> Target {
    let secret = secret();
    let image = std::env::var("FILESTORE_RUSTFS_IMAGE").unwrap_or_else(|_| RUSTFS_IMAGE.into());
    pull(&image, "FILESTORE_RUSTFS_IMAGE");
    let (container, port) = start(
        &image,
        &[
            ("RUSTFS_ACCESS_KEY", "omgtest"),
            ("RUSTFS_SECRET_KEY", &secret),
        ],
        &[],
    );
    Target {
        _container: container,
        name: "rustfs",
        origin: format!("http://127.0.0.1:{port}"),
        secret,
    }
}

fn lookup(target: &Target, bucket: &str, secret: &str) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(&'static str, String)> = vec![
        ("GATEWAY_FILE_STORE", "s3".into()),
        ("GATEWAY_S3_BUCKET", bucket.into()),
        ("GATEWAY_S3_PREFIX", "omg/it".into()),
        ("GATEWAY_S3_ENDPOINT", target.origin.clone()),
        ("GATEWAY_S3_ENDPOINT_ALLOWLIST", target.origin.clone()),
        ("GATEWAY_S3_AUTH", "static".into()),
        ("GATEWAY_S3_ACCESS_KEY_ID_ENV", "IT_S3_KEY".into()),
        ("GATEWAY_S3_SECRET_ACCESS_KEY_ENV", "IT_S3_SECRET".into()),
        ("IT_S3_KEY", "omgtest".into()),
        ("IT_S3_SECRET", secret.into()),
        ("GATEWAY_FILE_ENCRYPTION_KEYS_ENV", "IT_FILE_KEYS".into()),
        ("IT_FILE_KEYS", KEYS.into()),
    ];
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    }
}

async fn raw(target: &Target, bucket: &str) -> aws_sdk_s3::Client {
    let settings = S3Settings::from_lookup(&lookup(target, bucket, &target.secret)).unwrap();
    open_model_gateway::filestore::s3::raw_client_for_tests(&settings)
        .await
        .unwrap()
}

/// Waits until the server answers S3 requests, then creates the bucket.
async fn ready(target: &Target, bucket: &str) -> aws_sdk_s3::Client {
    let client = raw(target, bucket).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match client.create_bucket().bucket(bucket).send().await {
            Ok(_) => return client,
            Err(e) if tokio::time::Instant::now() > deadline => {
                panic!("{} did not become ready: {e:?}", target.name)
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn body(data: &[u8], piece: usize) -> ByteStream {
    let parts: Vec<Result<Bytes, FileStoreError>> = data
        .chunks(piece.max(1))
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    futures::stream::iter(parts).boxed()
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 131) as u8) ^ seed).collect()
}

async fn read(store: &Arc<dyn FileStore>, key: &ObjectKey) -> Result<Vec<u8>, FileStoreError> {
    let mut stream = store.get(key).await?;
    let mut out = Vec::new();
    while let Some(piece) = stream.next().await {
        out.extend_from_slice(&piece?);
    }
    Ok(out)
}

async fn exercise(target: Target) {
    let bucket = format!("omg-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let raw = ready(&target, &bucket).await;
    let runtime = FileStoreRuntime::build(
        FileStoreConfig::from_lookup(&lookup(&target, &bucket, &target.secret)).unwrap(),
    )
    .await
    .unwrap();
    let store = runtime.store().unwrap();
    let name = target.name;

    let health = store
        .health()
        .await
        .unwrap_or_else(|e| panic!("{name} health: {e}"));
    assert_eq!(health.key_id, "it2026");

    let ws = Uuid::new_v4();
    for (len, piece) in [
        (0, 1),
        (1, 1),
        (3 * CHUNK + 5, 4093),
        (2 * PART + 12_345, 1 << 20),
    ] {
        let key = ObjectKey::new(Purpose::BatchOutput, Scope::Workspace(ws)).unwrap();
        let plain = data(len, 0x5a);
        let stored = store
            .put(&key, body(&plain, piece), PutMeta::default())
            .await
            .unwrap_or_else(|e| panic!("{name} put {len}: {e}"));
        assert_eq!(stored.size, len as u64);
        assert_eq!(
            stored.sha256,
            <[u8; 32]>::from(sha2::Sha256::digest(&plain))
        );
        assert_eq!(read(&store, &key).await.unwrap(), plain, "{name} {len}");
        let info = store.head(&key).await.unwrap().unwrap();
        assert_eq!(
            (info.size, info.stored_size),
            (len as u64, stored.stored_size)
        );
        // Stored below the prefix, as ciphertext only.
        let object = format!("omg/it/{key}");
        let ciphertext = raw
            .get_object()
            .bucket(&bucket)
            .key(&object)
            .send()
            .await
            .unwrap();
        let ciphertext = ciphertext.body.collect().await.unwrap().into_bytes();
        assert_eq!(ciphertext.len() as u64, stored.stored_size);
        assert_eq!(&ciphertext[..4], b"OMGF");
        if len >= 64 {
            assert!(
                !ciphertext.windows(64).any(|w| w == &plain[..64]),
                "{name}: plaintext stored"
            );
        }
    }

    // Tampering with the stored object is detected.
    let key = ObjectKey::new(Purpose::UserFile, Scope::Workspace(ws)).unwrap();
    store
        .put(&key, body(&data(CHUNK * 2, 1), CHUNK), PutMeta::default())
        .await
        .unwrap();
    let object = format!("omg/it/{key}");
    let mut bytes = raw
        .get_object()
        .bucket(&bucket)
        .key(&object)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes()
        .to_vec();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    raw.put_object()
        .bucket(&bucket)
        .key(&object)
        .body(bytes.into())
        .send()
        .await
        .unwrap();
    assert_eq!(
        read(&store, &key).await,
        Err(FileStoreError::Integrity),
        "{name}"
    );

    // Idempotent delete; missing objects are absent, not errors.
    store.delete(&key).await.unwrap();
    store.delete(&key).await.unwrap();
    assert_eq!(store.head(&key).await.unwrap(), None);
    assert_eq!(
        store.get(&key).await.err(),
        Some(FileStoreError::NotFound),
        "{name}"
    );

    // A failing multipart upload is aborted: no object, no pending upload.
    let key = ObjectKey::new(Purpose::VideoOutput, Scope::Workspace(ws)).unwrap();
    let failing = body(&data(PART + 300_000, 2), CHUNK)
        .chain(futures::stream::iter([Err(FileStoreError::Source)]))
        .boxed();
    assert_eq!(
        store
            .put(&key, failing, PutMeta::default())
            .await
            .unwrap_err(),
        FileStoreError::Source
    );
    assert_eq!(store.head(&key).await.unwrap(), None);
    let pending = raw
        .list_multipart_uploads()
        .bucket(&bucket)
        .prefix(format!("omg/it/{key}"))
        .send()
        .await
        .unwrap();
    assert!(
        pending.uploads().is_empty(),
        "{name}: multipart upload not aborted"
    );
    // The size limit aborts too.
    assert_eq!(
        store
            .put(
                &key,
                body(&data(PART * 2, 3), CHUNK),
                PutMeta {
                    max_bytes: Some(PART as u64)
                }
            )
            .await
            .unwrap_err(),
        FileStoreError::TooLarge
    );
    assert_eq!(store.head(&key).await.unwrap(), None);

    // Wrong credentials are reported as a denial, never as content.
    let denied = FileStoreRuntime::build(
        FileStoreConfig::from_lookup(&lookup(&target, &bucket, "wrong-secret-value")).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        denied.store().unwrap().health().await.unwrap_err(),
        FileStoreError::Denied,
        "{name}"
    );

    // Nothing but the objects above remains under the prefix (health probe cleaned up).
    let listed = raw
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("omg/it/health/")
        .send()
        .await
        .unwrap();
    assert_eq!(
        listed.key_count().unwrap_or_default(),
        0,
        "{name}: health probe left an object"
    );
}

#[tokio::test]
async fn minio_compatibility() {
    if docker_available() {
        exercise(minio()).await;
    }
}

#[tokio::test]
async fn rustfs_compatibility() {
    if docker_available() {
        exercise(rustfs()).await;
    }
}

/// Opt-in real AWS S3 (never in CI): set FILESTORE_AWS_TEST_BUCKET (and region /
/// GATEWAY_S3_AUTH as for the gateway); uses the default AWS credential chain.
#[tokio::test]
async fn aws_s3_when_configured() {
    let Ok(bucket) = std::env::var("FILESTORE_AWS_TEST_BUCKET") else {
        eprintln!("SKIPPED: FILESTORE_AWS_TEST_BUCKET not set; real AWS S3 test not run");
        return;
    };
    let region = std::env::var("FILESTORE_AWS_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
    let auth = std::env::var("FILESTORE_AWS_TEST_AUTH").unwrap_or_else(|_| "aws:default".into());
    let lookup = move |name: &str| match name {
        "GATEWAY_FILE_STORE" => Some("s3".to_owned()),
        "GATEWAY_S3_BUCKET" => Some(bucket.clone()),
        "GATEWAY_S3_REGION" => Some(region.clone()),
        "GATEWAY_S3_PREFIX" => Some("omg-integration-test".to_owned()),
        "GATEWAY_S3_AUTH" => Some(auth.clone()),
        "GATEWAY_AWS_PROFILE_ALLOWLIST" => std::env::var("GATEWAY_AWS_PROFILE_ALLOWLIST").ok(),
        "GATEWAY_FILE_ENCRYPTION_KEYS_ENV" => Some("IT_FILE_KEYS".to_owned()),
        "IT_FILE_KEYS" => Some(KEYS.to_owned()),
        _ => None,
    };
    let runtime = FileStoreRuntime::build(FileStoreConfig::from_lookup(&lookup).unwrap())
        .await
        .unwrap();
    runtime.store().unwrap().health().await.unwrap();
}

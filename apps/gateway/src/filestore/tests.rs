//! Encryption format, key validation, object keys, path confinement, and the
//! local + memory backends (no network).
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use futures::StreamExt;
use proptest::prelude::*;
use uuid::Uuid;

use super::{
    crypto::{self, CHUNK, CT_CHUNK, HEADER_LEN},
    memory::MemoryBackend,
    *,
};

fn key_b64(byte: u8) -> String {
    let mut k = [byte; 32];
    k[0] ^= 0x5a;
    base64::engine::general_purpose::STANDARD.encode(k)
}

fn ring(spec: &str) -> Arc<KeyRing> {
    Arc::new(KeyRing::parse(spec).unwrap())
}

fn body(parts: Vec<Vec<u8>>) -> ByteStream {
    futures::stream::iter(parts.into_iter().map(|p| Ok(Bytes::from(p)))).boxed()
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

async fn collect(mut s: ByteStream) -> Result<Vec<u8>, FileStoreError> {
    let mut out = Vec::new();
    while let Some(piece) = s.next().await {
        out.extend_from_slice(&piece?);
    }
    Ok(out)
}

fn workspace_key(purpose: Purpose) -> ObjectKey {
    ObjectKey::new(purpose, Scope::Workspace(Uuid::new_v4())).unwrap()
}

fn store(keys: Arc<KeyRing>) -> (Encrypted<MemoryBackend>, MemoryBackend) {
    let backend = MemoryBackend::default();
    (Encrypted::new(backend.clone(), keys), backend)
}

// ---------- keys ----------

#[test]
fn key_ring_parses_rotation_lists_and_rejects_bad_material() {
    let keys = KeyRing::parse(&format!("k2026:{} , k2025:{}", key_b64(1), key_b64(2))).unwrap();
    assert_eq!(keys.active_id(), "k2026");
    assert_eq!(keys.ids().collect::<Vec<_>>(), ["k2026", "k2025"]);
    let short = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
    let constant = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    for bad in [
        String::new(),
        "k1".into(),
        format!("bad id:{}", key_b64(1)),
        format!(":{}", key_b64(1)),
        format!("{}:{}", "x".repeat(33), key_b64(1)),
        format!("-k:{}", key_b64(1)),
        "k1:not base64!".into(),
        format!("k1:{short}"),
        format!("k1:{constant}"),
        format!("k1:{},k1:{}", key_b64(1), key_b64(2)),
    ] {
        let error = KeyRing::parse(&bad).err().expect(&bad).to_string();
        // Errors never echo key material.
        assert!(
            !error.contains(&key_b64(1)) && !error.contains(&short),
            "{error}"
        );
    }
}

#[test]
fn key_ring_is_read_through_an_environment_reference() {
    let value = format!("main:{}", key_b64(3));
    let lookup = |name: &str| match name {
        crypto::KEYS_ENV => Some("FILE_KEYS".to_owned()),
        "FILE_KEYS" => Some(value.clone()),
        _ => None,
    };
    assert_eq!(KeyRing::from_lookup(&lookup).unwrap().active_id(), "main");
    assert!(KeyRing::from_lookup(&|_| None).is_err());
    let lower = |name: &str| (name == crypto::KEYS_ENV).then(|| "file_keys".to_owned());
    assert!(KeyRing::from_lookup(&lower).is_err());
    let unset = |name: &str| (name == crypto::KEYS_ENV).then(|| "FILE_KEYS".to_owned());
    assert!(KeyRing::from_lookup(&unset).is_err());
}

// ---------- object keys ----------

#[test]
fn object_keys_are_canonical_and_scoped() {
    let ws = Uuid::new_v4();
    let key = ObjectKey::new(Purpose::BatchInput, Scope::Workspace(ws)).unwrap();
    assert_eq!(
        key.as_str(),
        format!("batch_input/{ws}/{}", key.id().hyphenated())
    );
    assert_eq!(ObjectKey::parse(key.as_str()).unwrap(), key);
    let logo = ObjectKey::new(Purpose::Branding, Scope::Installation).unwrap();
    assert!(logo.as_str().starts_with("branding/installation/"));
    assert!(ObjectKey::new(Purpose::Branding, Scope::Workspace(ws)).is_err());
    assert!(ObjectKey::new(Purpose::BatchOutput, Scope::Installation).is_err());
    assert!(ObjectKey::new(Purpose::Export, Scope::Installation).is_ok());
    let id = Uuid::new_v4();
    for bad in [
        String::new(),
        format!("batch_input/{ws}"),
        format!("batch_input/{ws}/{id}/x"),
        format!("unknown/{ws}/{id}"),
        format!("health/installation/{id}"),
        format!("batch_input/{}/{id}", ws.simple()),
        format!("batch_input/{}/{id}", ws.to_string().to_uppercase()),
        format!("batch_input/../{id}"),
        format!("batch_input/{ws}/../../etc"),
        format!("/batch_input/{ws}/{id}"),
        format!("branding/{ws}/{id}"),
        format!("batch_input/installation/{id}"),
    ] {
        assert_eq!(
            ObjectKey::parse(&bad),
            Err(FileStoreError::InvalidKey),
            "{bad}"
        );
    }
}

#[test]
fn purposes_map_openai_values_and_groups() {
    assert_eq!(Purpose::from_openai("batch"), Some(Purpose::BatchInput));
    for p in ["user_data", "vision", "assistants", "evals"] {
        assert_eq!(Purpose::from_openai(p), Some(Purpose::UserFile));
    }
    assert_eq!(Purpose::from_openai("fine-tune"), None);
    for p in Purpose::ALL {
        assert_eq!(Purpose::parse(p.as_str()), Some(p));
        assert!(p.group().purposes().contains(&p));
    }
    assert!(Purpose::VideoOutput.holds_customer_content());
    assert!(!Purpose::Branding.holds_customer_content());
    assert_eq!(PurposeGroup::Branding.default_retention_days(), None);
    assert_eq!(PurposeGroup::Export.default_retention_days(), Some(1));
    assert_eq!(PurposeGroup::Batch.default_retention_days(), Some(7));
}

// ---------- encryption ----------

#[test]
fn sizes_are_derivable_from_ciphertext_length() {
    for plain in [
        0u64,
        1,
        100,
        CHUNK as u64 - 1,
        CHUNK as u64,
        CHUNK as u64 + 1,
        5 * CHUNK as u64,
    ] {
        let stored = crypto::stored_len(plain).unwrap();
        assert_eq!(crypto::plaintext_len(stored), Some(plain), "{plain}");
    }
    assert_eq!(crypto::stored_len(0), Some((HEADER_LEN + 16) as u64));
    assert_eq!(crypto::plaintext_len(HEADER_LEN as u64), None);
    assert_eq!(
        crypto::plaintext_len((HEADER_LEN + 16 + CT_CHUNK) as u64),
        None
    );
}

#[tokio::test]
async fn round_trips_every_chunk_boundary_and_hides_plaintext() {
    let (s, backend) = store(ring(&format!("k1:{}", key_b64(4))));
    for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 3 * CHUNK + 17] {
        let key = workspace_key(Purpose::BatchOutput);
        let plain = data(len, 3);
        // Odd framing from the caller.
        let parts: Vec<Vec<u8>> = plain.chunks(4093).map(<[u8]>::to_vec).collect();
        let stored = s.put(&key, body(parts), PutMeta::default()).await.unwrap();
        assert_eq!(stored.size, len as u64);
        assert_eq!(stored.key_id, "k1");
        assert_eq!(
            stored.sha256,
            <[u8; 32]>::from(sha2::Sha256::digest(&plain))
        );
        let raw = backend.raw(&key).unwrap();
        assert_eq!(raw.len() as u64, stored.stored_size);
        assert_eq!(crypto::header_key_id(&raw).as_deref(), Some("k1"));
        if len >= 32 {
            assert!(
                !raw.windows(32).any(|w| w == &plain[..32]),
                "plaintext leaked"
            );
        }
        assert_eq!(collect(s.get(&key).await.unwrap()).await.unwrap(), plain);
        assert_eq!(
            s.head(&key).await.unwrap(),
            Some(ObjectInfo {
                size: len as u64,
                stored_size: stored.stored_size
            })
        );
    }
}

use sha2::Digest as _;

#[tokio::test]
async fn tampering_truncation_reordering_and_swaps_fail() {
    let (s, backend) = store(ring(&format!("k1:{}", key_b64(5))));
    let key = workspace_key(Purpose::UserFile);
    let plain = data(3 * CHUNK + 10, 1);
    s.put(&key, body(vec![plain.clone()]), PutMeta::default())
        .await
        .unwrap();
    let raw = backend.raw(&key).unwrap().to_vec();
    let expect_integrity = |label: &'static str, bytes: Vec<u8>| {
        let (s, backend, key) = (&s, &backend, &key);
        async move {
            backend.set_raw(key, Bytes::from(bytes));
            let result = match s.get(key).await {
                Ok(stream) => collect(stream).await,
                Err(e) => Err(e),
            };
            assert_eq!(result, Err(FileStoreError::Integrity), "{label}");
        }
    };
    // Flip one bit in the header, in each chunk and in the tag.
    for at in [
        0,
        5,
        6,
        40,
        HEADER_LEN - 1,
        HEADER_LEN,
        HEADER_LEN + CT_CHUNK + 3,
        raw.len() - 1,
    ] {
        let mut t = raw.clone();
        t[at] ^= 1;
        expect_integrity("bit flip", t).await;
    }
    // Truncate at a chunk boundary (drop the real last chunk) and mid-chunk.
    expect_integrity("chunk boundary", raw[..HEADER_LEN + 3 * CT_CHUNK].to_vec()).await;
    expect_integrity("mid chunk", raw[..raw.len() - 5].to_vec()).await;
    expect_integrity("header only", raw[..HEADER_LEN].to_vec()).await;
    expect_integrity("short header", raw[..10].to_vec()).await;
    // Swap two full chunks.
    let mut swapped = raw[..HEADER_LEN].to_vec();
    swapped.extend_from_slice(&raw[HEADER_LEN + CT_CHUNK..HEADER_LEN + 2 * CT_CHUNK]);
    swapped.extend_from_slice(&raw[HEADER_LEN..HEADER_LEN + CT_CHUNK]);
    swapped.extend_from_slice(&raw[HEADER_LEN + 2 * CT_CHUNK..]);
    expect_integrity("reordered", swapped).await;
    // Drop a middle chunk.
    let mut dropped = raw[..HEADER_LEN + CT_CHUNK].to_vec();
    dropped.extend_from_slice(&raw[HEADER_LEN + 2 * CT_CHUNK..]);
    expect_integrity("dropped", dropped).await;
    // Appended garbage.
    let mut extended = raw.clone();
    extended.extend_from_slice(&[0u8; 16]);
    expect_integrity("appended", extended).await;
    // A whole object moved to another key (another workspace) does not decrypt.
    let other = workspace_key(Purpose::UserFile);
    backend.set_raw(&other, Bytes::from(raw.clone()));
    assert_eq!(
        collect(s.get(&other).await.unwrap()).await,
        Err(FileStoreError::Integrity)
    );
    // The untouched original still decrypts.
    backend.set_raw(&key, Bytes::from(raw));
    assert_eq!(collect(s.get(&key).await.unwrap()).await.unwrap(), plain);
}

#[tokio::test]
async fn rotation_keeps_old_objects_readable_and_unknown_keys_are_reported() {
    let old = ring(&format!("old:{}", key_b64(6)));
    let (s_old, backend) = store(old);
    let key = workspace_key(Purpose::BatchInput);
    s_old
        .put(&key, body(vec![b"hello".to_vec()]), PutMeta::default())
        .await
        .unwrap();
    // New active key first; old one decrypt-only.
    let rotated = Encrypted::new(
        backend.clone(),
        ring(&format!("new:{},old:{}", key_b64(7), key_b64(6))),
    );
    assert_eq!(
        collect(rotated.get(&key).await.unwrap()).await.unwrap(),
        b"hello"
    );
    let fresh = workspace_key(Purpose::BatchInput);
    assert_eq!(
        rotated
            .put(&fresh, body(vec![b"x".to_vec()]), PutMeta::default())
            .await
            .unwrap()
            .key_id,
        "new"
    );
    // Old key removed: objects under it are reported, not misread.
    let dropped = Encrypted::new(backend.clone(), ring(&format!("new:{}", key_b64(7))));
    assert_eq!(
        collect(dropped.get(&key).await.unwrap()).await,
        Err(FileStoreError::KeyUnavailable)
    );
    // Same id, different key material: integrity failure.
    let wrong = Encrypted::new(backend, ring(&format!("old:{}", key_b64(8))));
    assert_eq!(
        collect(wrong.get(&key).await.unwrap()).await,
        Err(FileStoreError::Integrity)
    );
}

#[tokio::test]
async fn size_limits_and_source_errors_leave_nothing_behind() {
    let (s, backend) = store(ring(&format!("k:{}", key_b64(9))));
    let key = workspace_key(Purpose::BatchInput);
    let result = s
        .put(
            &key,
            body(vec![data(100, 0), data(100, 0)]),
            PutMeta {
                max_bytes: Some(150),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileStoreError::TooLarge);
    let failing: ByteStream = futures::stream::iter([
        Ok(Bytes::from(data(CHUNK * 2, 0))),
        Err(FileStoreError::Source),
    ])
    .boxed();
    assert_eq!(
        s.put(&key, failing, PutMeta::default()).await.unwrap_err(),
        FileStoreError::Source
    );
    assert!(backend.raw(&key).is_none());
    assert_eq!(s.head(&key).await.unwrap(), None);
    assert_eq!(s.get(&key).await.err(), Some(FileStoreError::NotFound));
    s.delete(&key).await.unwrap();
}

#[tokio::test]
async fn health_probe_round_trips_and_cleans_up() {
    let (s, backend) = store(ring(&format!("probe:{}", key_b64(10))));
    let health = s.health().await.unwrap();
    assert_eq!(health.key_id, "probe");
    assert_eq!(health.backend, BackendKind::Memory);
    let _ = backend;
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn any_plaintext_round_trips_and_any_single_bit_flip_fails(
        plain in proptest::collection::vec(any::<u8>(), 0..(3 * CHUNK)),
        split in 1usize..10_000,
        flip in any::<prop::sample::Index>(),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let (s, backend) = store(ring(&format!("p:{}", key_b64(11))));
            let key = workspace_key(Purpose::Export);
            let parts: Vec<Vec<u8>> = plain.chunks(split).map(<[u8]>::to_vec).collect();
            let stored = s.put(&key, body(parts), PutMeta::default()).await.unwrap();
            prop_assert_eq!(stored.size as usize, plain.len());
            prop_assert_eq!(collect(s.get(&key).await.unwrap()).await.unwrap(), plain.clone());
            let mut raw = backend.raw(&key).unwrap().to_vec();
            let at = flip.index(raw.len());
            raw[at] ^= 0x80;
            backend.set_raw(&key, Bytes::from(raw));
            let result = match s.get(&key).await { Ok(st) => collect(st).await, Err(e) => Err(e) };
            prop_assert!(matches!(result, Err(FileStoreError::Integrity | FileStoreError::KeyUnavailable)));
            Ok(())
        })?;
    }

    #[test]
    fn any_truncation_fails(len in 1usize..(2 * CHUNK + 50), cut in any::<prop::sample::Index>()) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let (s, backend) = store(ring(&format!("p:{}", key_b64(12))));
            let key = workspace_key(Purpose::Export);
            s.put(&key, body(vec![data(len, 2)]), PutMeta::default()).await.unwrap();
            let raw = backend.raw(&key).unwrap();
            let keep = cut.index(raw.len());
            backend.set_raw(&key, raw.slice(..keep));
            let result = match s.get(&key).await { Ok(st) => collect(st).await, Err(e) => Err(e) };
            prop_assert_eq!(result, Err(FileStoreError::Integrity));
            Ok(())
        })?;
    }
}

// ---------- local backend ----------

#[cfg(unix)]
mod local_backend {
    use super::*;
    use crate::filestore::local::LocalBackend;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn local(dir: &std::path::Path) -> Encrypted<LocalBackend> {
        Encrypted::new(
            LocalBackend::open(dir).unwrap(),
            ring(&format!("disk:{}", key_b64(13))),
        )
    }

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[tokio::test]
    async fn stores_private_files_atomically_and_deletes_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let s = local(&root);
        assert_eq!(mode(&root), 0o700);
        let ws = Uuid::new_v4();
        let key = ObjectKey::new(Purpose::VideoOutput, Scope::Workspace(ws)).unwrap();
        let plain = data(2 * CHUNK + 3, 9);
        let stored = s
            .put(&key, body(vec![plain.clone()]), PutMeta::default())
            .await
            .unwrap();
        let root = s.backend().root().to_path_buf();
        let path = root
            .join("video_output")
            .join(ws.to_string())
            .join(key.id().to_string());
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.join("video_output")), 0o700);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), stored.stored_size);
        assert!(
            !std::fs::read(&path)
                .unwrap()
                .windows(16)
                .any(|w| w == &plain[..16])
        );
        // No temp files remain.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(collect(s.get(&key).await.unwrap()).await.unwrap(), plain);
        s.delete(&key).await.unwrap();
        s.delete(&key).await.unwrap();
        assert_eq!(s.head(&key).await.unwrap(), None);
        // Deleting under a never-created workspace directory is fine too.
        s.delete(&workspace_key(Purpose::BatchInput)).await.unwrap();
        s.health().await.unwrap();
    }

    #[tokio::test]
    async fn failed_writes_remove_their_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let s = local(&tmp.path().join("store"));
        let ws = Uuid::new_v4();
        let key = ObjectKey::new(Purpose::BatchInput, Scope::Workspace(ws)).unwrap();
        let failing: ByteStream =
            futures::stream::iter([Ok(Bytes::from(data(10, 0))), Err(FileStoreError::Source)])
                .boxed();
        assert_eq!(
            s.put(&key, failing, PutMeta::default()).await.unwrap_err(),
            FileStoreError::Source
        );
        let dir = s.backend().root().join("batch_input").join(ws.to_string());
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn symlinks_are_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let s = local(&tmp.path().join("store"));
        let root = s.backend().root().to_path_buf();
        let ws = Uuid::new_v4();
        // A symlinked purpose directory pointing outside the root.
        symlink(&outside, root.join("batch_input")).unwrap();
        let key = ObjectKey::new(Purpose::BatchInput, Scope::Workspace(ws)).unwrap();
        assert_eq!(
            s.put(&key, body(vec![b"x".to_vec()]), PutMeta::default())
                .await
                .unwrap_err(),
            FileStoreError::UnsafePath
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(s.get(&key).await.err(), Some(FileStoreError::UnsafePath));
        assert_eq!(s.head(&key).await.unwrap_err(), FileStoreError::UnsafePath);
        // A symlinked object file is not read, and deleting removes only the link.
        let real = ObjectKey::new(Purpose::UserFile, Scope::Workspace(ws)).unwrap();
        s.put(&real, body(vec![b"secret".to_vec()]), PutMeta::default())
            .await
            .unwrap();
        let target = outside.join("victim");
        std::fs::write(&target, b"do not touch").unwrap();
        let link_key = ObjectKey::new(Purpose::UserFile, Scope::Workspace(ws)).unwrap();
        let link = root
            .join("user_file")
            .join(ws.to_string())
            .join(link_key.id().to_string());
        symlink(&target, &link).unwrap();
        assert_eq!(
            s.get(&link_key).await.err(),
            Some(FileStoreError::UnsafePath)
        );
        assert_eq!(
            s.head(&link_key).await.unwrap_err(),
            FileStoreError::UnsafePath
        );
        // A write must not replace a planted symlink either.
        let planted =
            ObjectKey::with_id(Purpose::UserFile, Scope::Workspace(ws), link_key.id()).unwrap();
        assert_eq!(
            s.put(&planted, body(vec![b"x".to_vec()]), PutMeta::default())
                .await
                .unwrap_err(),
            FileStoreError::UnsafePath
        );
        s.delete(&link_key).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"do not touch");
    }

    #[test]
    fn store_directory_must_be_private_absolute_and_real() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(LocalBackend::open(std::path::Path::new("relative/store")).is_err());
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(LocalBackend::open(&open).is_err());
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(LocalBackend::open(&open).is_ok());
        let link = tmp.path().join("link");
        symlink(&open, &link).unwrap();
        assert!(LocalBackend::open(&link).is_err());
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(LocalBackend::open(&file).is_err());
    }
}

// ---------- configuration ----------

#[test]
fn configuration_defaults_off_and_fails_clearly() {
    let keys = format!("main:{}", key_b64(14));
    let env = |pairs: Vec<(&'static str, String)>| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    };
    assert!(FileStoreConfig::from_lookup(&env(vec![])).is_ok());
    assert!(FileStoreConfig::from_lookup(&env(vec![("GATEWAY_FILE_STORE", "off".into())])).is_ok());
    for (pairs, needle) in [
        (
            vec![("GATEWAY_FILE_STORE", "disk".into())],
            "off, local or s3",
        ),
        (
            vec![("GATEWAY_FILE_STORE", "local".into())],
            "GATEWAY_FILE_STORE_DIR",
        ),
        (
            vec![
                ("GATEWAY_FILE_STORE", "local".into()),
                ("GATEWAY_FILE_STORE_DIR", "/tmp/x".into()),
            ],
            "GATEWAY_FILE_ENCRYPTION_KEYS_ENV",
        ),
        (
            vec![("GATEWAY_FILE_STORE", "s3".into())],
            "GATEWAY_S3_BUCKET",
        ),
    ] {
        let error = FileStoreConfig::from_lookup(&env(pairs))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(needle), "{error}");
    }
    assert!(
        FileStoreConfig::from_lookup(&env(vec![
            ("GATEWAY_FILE_STORE", "local".into()),
            ("GATEWAY_FILE_STORE_DIR", "/tmp/x".into()),
            ("GATEWAY_FILE_ENCRYPTION_KEYS_ENV", "KEYS".into()),
            ("KEYS", keys),
        ]))
        .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_builds_a_local_store_and_describes_it_without_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("files").to_string_lossy().into_owned();
    let keys = format!("main:{},old:{}", key_b64(15), key_b64(16));
    let lookup = move |name: &str| match name {
        "GATEWAY_FILE_STORE" => Some("local".to_owned()),
        "GATEWAY_FILE_STORE_DIR" => Some(dir.clone()),
        "GATEWAY_FILE_ENCRYPTION_KEYS_ENV" => Some("KEYS".to_owned()),
        "KEYS" => Some(keys.clone()),
        _ => None,
    };
    let rt = FileStoreRuntime::build(FileStoreConfig::from_lookup(&lookup).unwrap())
        .await
        .unwrap();
    assert_eq!(rt.backend_name(), "local");
    assert_eq!(rt.active_key_id(), Some("main"));
    assert_eq!(rt.decrypt_only_keys(), 1);
    assert!(rt.knows_key("old") && !rt.knows_key("other"));
    let location = rt.location().unwrap().to_string();
    assert!(
        !location.contains(tmp.path().to_str().unwrap()),
        "{location}"
    );
    rt.store().unwrap().health().await.unwrap();
    assert_eq!(FileStoreRuntime::off().backend_name(), "off");
}

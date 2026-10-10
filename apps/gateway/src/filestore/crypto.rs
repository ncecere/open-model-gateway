//! Streaming, chunked AES-256-GCM for stored objects (STREAM construction).
//!
//! Each object gets a fresh random 256-bit data key, wrapped (AES-256-GCM) by the
//! active master key. Layout, all integers big-endian:
//!
//! ```text
//! header (106 bytes):
//!   "OMGF" | version=1 | chunk_log2=16 | kid_len | kid (32, zero-padded)
//!   | nonce_prefix (7) | wrap_nonce (12) | wrapped data key (32 + 16 tag)
//! chunks: AES-256-GCM(data key, nonce = nonce_prefix | counter u32 | last u8,
//!                     aad = SHA-256("omg-file-chunk-v1" | version | chunk_log2
//!                                    | nonce_prefix | object key))
//!   64 KiB plaintext per chunk (+16-byte tag); the final chunk has last=1 and
//!   1..=64 KiB (0 only for an empty object).
//! ```
//!
//! The wrap AAD binds version, chunk size, key id, nonce prefix and the object
//! key, so a header cannot be moved to another object or key id. The chunk AAD
//! binds the object key, so ciphertext cannot be swapped between objects; the
//! counter prevents reordering/dropping chunks and the last flag prevents
//! truncation at a chunk boundary. The key id and wrapped data key are excluded
//! from the chunk AAD so a future re-wrap can replace the header only.
use std::sync::{Arc, Mutex};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadInPlace, Payload},
};
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{BackendKind, ByteStream, FileStoreError, ObjectKey};

pub const KEYS_ENV: &str = "GATEWAY_FILE_ENCRYPTION_KEYS_ENV";
const MAGIC: &[u8; 4] = b"OMGF";
const VERSION: u8 = 1;
const CHUNK_LOG2: u8 = 16;
pub(crate) const CHUNK: usize = 1 << CHUNK_LOG2;
const TAG: usize = 16;
pub(crate) const CT_CHUNK: usize = CHUNK + TAG;
const KID_MAX: usize = 32;
const PREFIX: usize = 7;
pub(crate) const HEADER_LEN: usize = 4 + 1 + 1 + 1 + KID_MAX + PREFIX + 12 + 32 + TAG;
const MAX_KEYS: usize = 16;

/// One master key. No Debug: never printable.
struct MasterKey {
    id: String,
    cipher: Aes256Gcm,
}

/// Master keys from `GATEWAY_FILE_ENCRYPTION_KEYS_ENV`: the first is active for
/// new objects, the others decrypt only (rotation). Not Debug or Clone-able into logs.
pub struct KeyRing {
    keys: Vec<MasterKey>,
}

/// Key id: 1-32 of `[A-Za-z0-9._-]`, starting with a letter or digit.
pub fn valid_key_id(id: &str) -> bool {
    (1..=KID_MAX).contains(&id.len())
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Environment variable names used as references: `[A-Z_][A-Z0-9_]{0,127}`.
pub(crate) fn valid_env_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

impl KeyRing {
    /// Parses `kid:base64key[,kid:base64key…]`. Errors never include key material.
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let mut keys: Vec<MasterKey> = Vec::new();
        for (index, entry) in value.split(',').enumerate() {
            let n = index + 1;
            let entry = entry.trim();
            let Some((id, encoded)) = entry.split_once(':') else {
                anyhow::bail!("encryption key entry {n} must be kid:base64key");
            };
            anyhow::ensure!(
                valid_key_id(id),
                "encryption key entry {n}: key id must be 1-32 characters of A-Z a-z 0-9 . _ -"
            );
            anyhow::ensure!(
                keys.iter().all(|k| k.id != id),
                "encryption key entry {n}: duplicate key id"
            );
            let bytes = Zeroizing::new(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .map_err(|_| {
                        anyhow::anyhow!("encryption key entry {n}: key is not valid base64")
                    })?,
            );
            anyhow::ensure!(
                bytes.len() == 32,
                "encryption key entry {n}: key must decode to exactly 32 bytes"
            );
            anyhow::ensure!(
                bytes.iter().any(|b| *b != bytes[0]),
                "encryption key entry {n}: key is not random"
            );
            keys.push(MasterKey {
                id: id.to_owned(),
                cipher: Aes256Gcm::new_from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("encryption key entry {n}: invalid key"))?,
            });
        }
        anyhow::ensure!(
            keys.len() <= MAX_KEYS,
            "at most {MAX_KEYS} encryption keys may be configured"
        );
        Ok(Self { keys })
    }

    /// Reads the variable named by `GATEWAY_FILE_ENCRYPTION_KEYS_ENV`.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let name = lookup(KEYS_ENV)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("{KEYS_ENV} is required when the file store is enabled")
            })?;
        anyhow::ensure!(
            valid_env_name(&name),
            "{KEYS_ENV} must name an environment variable (A-Z, 0-9, _)"
        );
        let value = lookup(&name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("the variable named by {KEYS_ENV} is not set"))?;
        Self::parse(&value)
    }

    /// A random single-key ring for tests and ephemeral stores (fails closed
    /// when the OS random number generator fails).
    pub fn random(id: &str) -> Result<Self, crate::entropy::EntropyUnavailable> {
        assert!(valid_key_id(id));
        let mut key = Zeroizing::new([0u8; 32]);
        crate::entropy::fill(key.as_mut())?;
        Ok(Self {
            keys: vec![MasterKey {
                id: id.to_owned(),
                cipher: Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key"),
            }],
        })
    }

    pub fn active_id(&self) -> &str {
        &self.keys[0].id
    }
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|k| k.id.as_str())
    }
    pub fn contains(&self, id: &str) -> bool {
        self.keys.iter().any(|k| k.id == id)
    }
    fn get(&self, id: &str) -> Option<&MasterKey> {
        self.keys.iter().find(|k| k.id == id)
    }
}

fn wrap_aad(kid: &[u8; KID_MAX], kid_len: u8, prefix: &[u8; PREFIX], key: &ObjectKey) -> Vec<u8> {
    let mut aad = Vec::with_capacity(64 + key.as_str().len());
    aad.extend_from_slice(b"omg-file-wrap-v1");
    aad.extend_from_slice(&[VERSION, CHUNK_LOG2, kid_len]);
    aad.extend_from_slice(kid);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(key.as_str().as_bytes());
    aad
}

fn chunk_aad(prefix: &[u8; PREFIX], key: &ObjectKey) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"omg-file-chunk-v1");
    h.update([VERSION, CHUNK_LOG2]);
    h.update(prefix);
    h.update(key.as_str().as_bytes());
    h.finalize().into()
}

fn nonce(prefix: &[u8; PREFIX], counter: u32, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..PREFIX].copy_from_slice(prefix);
    n[PREFIX..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = u8::from(last);
    n
}

/// Ciphertext size for a plaintext size.
pub(crate) fn stored_len(plain: u64) -> Option<u64> {
    let chunk = CHUNK as u64;
    let chunks = if plain == 0 { 1 } else { plain.div_ceil(chunk) };
    if chunks > u64::from(u32::MAX) + 1 {
        return None;
    }
    plain
        .checked_add(chunks * TAG as u64)?
        .checked_add(HEADER_LEN as u64)
}

/// Plaintext size for a canonical ciphertext size; `None` if no plaintext maps to it.
pub(crate) fn plaintext_len(stored: u64) -> Option<u64> {
    let body = stored.checked_sub(HEADER_LEN as u64)?;
    if body < TAG as u64 {
        return None;
    }
    let chunks = body.div_ceil(CT_CHUNK as u64);
    let plain = body - chunks * TAG as u64;
    (stored_len(plain) == Some(stored)).then_some(plain)
}

/// Outcome of a finished encryption, filled once the last chunk is sealed.
#[derive(Clone, Copy)]
pub(crate) struct Summary {
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Clone, Default)]
pub(crate) struct SummaryHandle(Arc<Mutex<Option<Summary>>>);
impl SummaryHandle {
    pub(crate) fn take(&self) -> Option<Summary> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    fn set(&self, s: Summary) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }
}

struct Sealer {
    cipher: Aes256Gcm,
    prefix: [u8; PREFIX],
    aad: [u8; 32],
    counter: u32,
}

impl Sealer {
    fn seal(&mut self, plain: &[u8], last: bool) -> Result<Bytes, FileStoreError> {
        let n = nonce(&self.prefix, self.counter, last);
        let mut out = Vec::with_capacity(plain.len() + TAG);
        out.extend_from_slice(plain);
        self.cipher
            .encrypt_in_place(Nonce::from_slice(&n), &self.aad, &mut out)
            .map_err(|_| FileStoreError::Integrity)?;
        if !last {
            self.counter = self
                .counter
                .checked_add(1)
                .ok_or(FileStoreError::TooLarge)?;
        }
        Ok(Bytes::from(out))
    }
}

/// Header plus a sealer for a new object under the active key.
fn begin(keys: &KeyRing, key: &ObjectKey) -> Result<(Bytes, Sealer), FileStoreError> {
    let master = &keys.keys[0];
    // Data key, nonce prefix and wrap nonce from the OS CSPRNG; a failure
    // writes nothing (fail closed: `unavailable`), never a weaker nonce.
    let entropy = |_| FileStoreError::Unavailable;
    let mut dek = Zeroizing::new([0u8; 32]);
    crate::entropy::fill(dek.as_mut()).map_err(entropy)?;
    let mut prefix = [0u8; PREFIX];
    crate::entropy::fill(&mut prefix).map_err(entropy)?;
    let mut wrap_nonce = [0u8; 12];
    crate::entropy::fill(&mut wrap_nonce).map_err(entropy)?;
    let mut kid = [0u8; KID_MAX];
    kid[..master.id.len()].copy_from_slice(master.id.as_bytes());
    let kid_len = master.id.len() as u8;
    let wrapped = master
        .cipher
        .encrypt(
            Nonce::from_slice(&wrap_nonce),
            Payload {
                msg: dek.as_ref(),
                aad: &wrap_aad(&kid, kid_len, &prefix, key),
            },
        )
        .map_err(|_| FileStoreError::Integrity)?;
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&[VERSION, CHUNK_LOG2, kid_len]);
    header.extend_from_slice(&kid);
    header.extend_from_slice(&prefix);
    header.extend_from_slice(&wrap_nonce);
    header.extend_from_slice(&wrapped);
    debug_assert_eq!(header.len(), HEADER_LEN);
    let cipher = Aes256Gcm::new_from_slice(dek.as_ref()).map_err(|_| FileStoreError::Integrity)?;
    Ok((
        Bytes::from(header),
        Sealer {
            cipher,
            aad: chunk_aad(&prefix, key),
            prefix,
            counter: 0,
        },
    ))
}

/// Parses and unwraps a header. Unknown key ids are `KeyUnavailable`; anything
/// else that does not authenticate is `Integrity`.
fn open_header(keys: &KeyRing, key: &ObjectKey, header: &[u8]) -> Result<Sealer, FileStoreError> {
    if header.len() != HEADER_LEN || &header[..4] != MAGIC {
        return Err(FileStoreError::Integrity);
    }
    if header[4] != VERSION || header[5] != CHUNK_LOG2 {
        return Err(FileStoreError::Integrity);
    }
    let kid_len = header[6];
    let mut kid = [0u8; KID_MAX];
    kid.copy_from_slice(&header[7..7 + KID_MAX]);
    let len = usize::from(kid_len);
    if !(1..=KID_MAX).contains(&len) || kid[len..].iter().any(|b| *b != 0) {
        return Err(FileStoreError::Integrity);
    }
    let id = std::str::from_utf8(&kid[..len]).map_err(|_| FileStoreError::Integrity)?;
    if !valid_key_id(id) {
        return Err(FileStoreError::Integrity);
    }
    let master = keys.get(id).ok_or(FileStoreError::KeyUnavailable)?;
    let mut at = 7 + KID_MAX;
    let mut prefix = [0u8; PREFIX];
    prefix.copy_from_slice(&header[at..at + PREFIX]);
    at += PREFIX;
    let wrap_nonce = &header[at..at + 12];
    at += 12;
    let dek = Zeroizing::new(
        master
            .cipher
            .decrypt(
                Nonce::from_slice(wrap_nonce),
                Payload {
                    msg: &header[at..],
                    aad: &wrap_aad(&kid, kid_len, &prefix, key),
                },
            )
            .map_err(|_| FileStoreError::Integrity)?,
    );
    let cipher = Aes256Gcm::new_from_slice(&dek).map_err(|_| FileStoreError::Integrity)?;
    Ok(Sealer {
        cipher,
        aad: chunk_aad(&prefix, key),
        prefix,
        counter: 0,
    })
}

impl Sealer {
    fn open(&mut self, chunk: &[u8], last: bool) -> Result<Bytes, FileStoreError> {
        let n = nonce(&self.prefix, self.counter, last);
        let mut buf = chunk.to_vec();
        self.cipher
            .decrypt_in_place(Nonce::from_slice(&n), &self.aad, &mut buf)
            .map_err(|_| FileStoreError::Integrity)?;
        if !last {
            self.counter = self
                .counter
                .checked_add(1)
                .ok_or(FileStoreError::Integrity)?;
        }
        Ok(Bytes::from(buf))
    }
}

/// Encrypts `body` for `key`. The returned handle is filled with the plaintext
/// size and SHA-256 only after the final chunk has been produced.
pub(crate) fn encrypt_stream(
    keys: &KeyRing,
    key: &ObjectKey,
    mut body: ByteStream,
    max_bytes: Option<u64>,
) -> (ByteStream, SummaryHandle) {
    let summary = SummaryHandle::default();
    let started = begin(keys, key);
    let done = summary.clone();
    let stream = async_stream::stream! {
        let (header, mut sealer) = match started {
            Ok(v) => v,
            Err(e) => { yield Err(e); return; }
        };
        yield Ok(header);
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = BytesMut::new();
        while let Some(item) = body.next().await {
            let piece = match item {
                Ok(piece) => piece,
                Err(e) => { yield Err(e); return; }
            };
            size = size.saturating_add(piece.len() as u64);
            if max_bytes.is_some_and(|max| size > max) || stored_len(size).is_none() {
                yield Err(FileStoreError::TooLarge);
                return;
            }
            hasher.update(&piece);
            buf.extend_from_slice(&piece);
            // Strictly greater: a trailing full chunk waits to learn whether it is last.
            while buf.len() > CHUNK {
                let plain = buf.split_to(CHUNK);
                match sealer.seal(&plain, false) {
                    Ok(c) => yield Ok(c),
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        match sealer.seal(&buf, true) {
            Ok(c) => {
                done.set(Summary { size, sha256: hasher.finalize().into() });
                yield Ok(c);
            }
            Err(e) => yield Err(e),
        }
    };
    (stream.boxed(), summary)
}

/// Decrypts and authenticates a ciphertext stream. The stream ends with an
/// error (never a clean EOF) on any tampering, truncation or reordering.
pub(crate) fn decrypt_stream(
    keys: Arc<KeyRing>,
    key: ObjectKey,
    mut ciphertext: ByteStream,
    kind: BackendKind,
) -> ByteStream {
    let stream = async_stream::stream! {
        let mut buf = BytesMut::new();
        let mut sealer: Option<Sealer> = None;
        let mut total = 0u64;
        let fail = |e: FileStoreError| {
            crate::metrics::METRICS.observe_file_store(kind.as_str(), "read", Some(e.code()));
            e
        };
        loop {
            let item = ciphertext.next().await;
            let ended = item.is_none();
            if let Some(item) = item {
                match item {
                    Ok(piece) => buf.extend_from_slice(&piece),
                    Err(e) => { yield Err(fail(e)); return; }
                }
            }
            if sealer.is_none() {
                if buf.len() < HEADER_LEN {
                    if ended { yield Err(fail(FileStoreError::Integrity)); return; }
                    continue;
                }
                let header = buf.split_to(HEADER_LEN);
                match open_header(&keys, &key, &header) {
                    Ok(s) => sealer = Some(s),
                    Err(e) => { yield Err(fail(e)); return; }
                }
            }
            let Some(s) = sealer.as_mut() else { continue };
            while buf.len() > CT_CHUNK {
                let chunk = buf.split_to(CT_CHUNK);
                match s.open(&chunk, false) {
                    Ok(p) => { total += p.len() as u64; yield Ok(p) }
                    Err(e) => { yield Err(fail(e)); return; }
                }
            }
            if ended {
                // Canonical final chunk: 1..=CHUNK bytes, or empty only for an empty object.
                if buf.len() < TAG || (buf.len() == TAG && s.counter > 0) {
                    yield Err(fail(FileStoreError::Integrity));
                    return;
                }
                match s.open(&buf, true) {
                    Ok(p) => {
                        total += p.len() as u64;
                        crate::metrics::METRICS.observe_file_store_bytes(kind.as_str(), "get", total);
                        if !p.is_empty() { yield Ok(p); }
                    }
                    Err(e) => yield Err(fail(e)),
                }
                return;
            }
        }
    };
    stream.boxed()
}

#[cfg(test)]
pub(crate) fn header_key_id(header: &[u8]) -> Option<String> {
    let len = usize::from(*header.get(6)?);
    std::str::from_utf8(header.get(7..7 + len)?)
        .ok()
        .map(str::to_owned)
}

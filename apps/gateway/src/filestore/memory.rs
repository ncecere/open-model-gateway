//! In-memory backend for tests. Still encrypted: tests exercise the real format.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use super::{
    Backend, BackendKind, ByteStream, Encrypted, FileStore, FileStoreError, KeyRing, ObjectKey,
};

#[derive(Clone, Default)]
pub(crate) struct MemoryBackend {
    objects: Arc<Mutex<HashMap<String, Bytes>>>,
}

impl MemoryBackend {
    fn objects(&self) -> std::sync::MutexGuard<'_, HashMap<String, Bytes>> {
        self.objects.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Raw ciphertext (tests).
    #[cfg(test)]
    pub(crate) fn raw(&self, key: &ObjectKey) -> Option<Bytes> {
        self.objects().get(key.as_str()).cloned()
    }
    /// Replace raw ciphertext (tamper tests).
    #[cfg(test)]
    pub(crate) fn set_raw(&self, key: &ObjectKey, value: Bytes) {
        self.objects().insert(key.as_str().to_owned(), value);
    }
}

/// An encrypted in-memory [`FileStore`] with a random key, for tests of
/// consumers (for example the batch engine). Contents vanish with the process.
/// Test builds only; the key ring comes from the fallible OS CSPRNG.
pub fn memory_store() -> Arc<dyn FileStore> {
    Arc::new(Encrypted::new(
        MemoryBackend::default(),
        Arc::new(KeyRing::random("memory").expect("OS random number generator (test fixture)")),
    ))
}

#[async_trait::async_trait]
impl Backend for MemoryBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Memory
    }
    async fn put(&self, key: &ObjectKey, mut body: ByteStream) -> Result<u64, FileStoreError> {
        let mut all = BytesMut::new();
        while let Some(piece) = body.next().await {
            all.extend_from_slice(&piece?);
        }
        let len = all.len() as u64;
        self.objects().insert(key.as_str().to_owned(), all.freeze());
        Ok(len)
    }
    async fn get(&self, key: &ObjectKey) -> Result<ByteStream, FileStoreError> {
        let value = self
            .objects()
            .get(key.as_str())
            .cloned()
            .ok_or(FileStoreError::NotFound)?;
        // Re-chunk unevenly so readers cannot rely on backend framing.
        let pieces: Vec<Result<Bytes, FileStoreError>> = value
            .chunks(7919)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Ok(futures::stream::iter(pieces).boxed())
    }
    async fn delete(&self, key: &ObjectKey) -> Result<(), FileStoreError> {
        self.objects().remove(key.as_str());
        Ok(())
    }
    async fn head(&self, key: &ObjectKey) -> Result<Option<u64>, FileStoreError> {
        Ok(self.objects().get(key.as_str()).map(|v| v.len() as u64))
    }
}

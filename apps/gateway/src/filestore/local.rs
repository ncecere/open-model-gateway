//! Local-disk backend: `<root>/<purpose>/<scope>/<uuid>`.
//!
//! Directories are created 0700 and files 0600. Writes go to a unique temp file
//! (`O_CREAT|O_EXCL|O_NOFOLLOW`), are fsynced, atomically renamed into place and
//! the directory is fsynced. No symlink is followed below the root: every
//! directory component is checked with `lstat`, and files are opened with
//! `O_NOFOLLOW`. Key components come from [`ObjectKey`]'s safe charset, so no
//! path can leave the root.
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use bytes::BytesMut;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Backend, BackendKind, ByteStream, FileStoreError, ObjectKey};

pub(crate) struct LocalBackend {
    root: PathBuf,
}

fn io_error(e: &std::io::Error) -> FileStoreError {
    if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
        return FileStoreError::UnsafePath;
    }
    match e.kind() {
        ErrorKind::NotFound => FileStoreError::NotFound,
        ErrorKind::PermissionDenied => FileStoreError::Denied,
        ErrorKind::TimedOut => FileStoreError::Timeout,
        _ => FileStoreError::Unavailable,
    }
}

const NOFOLLOW: i32 = libc::O_NOFOLLOW | libc::O_CLOEXEC;

impl LocalBackend {
    /// Opens (creating 0700 if missing) the store directory. It must be an absolute
    /// path to a real directory (not a symlink) owned by this user and not
    /// accessible to group or others.
    pub(crate) fn open(dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        anyhow::ensure!(
            dir.is_absolute(),
            "GATEWAY_FILE_STORE_DIR must be an absolute path"
        );
        match std::fs::symlink_metadata(dir) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|_| anyhow::anyhow!("GATEWAY_FILE_STORE_DIR could not be created"))?;
            }
            Err(_) => anyhow::bail!("GATEWAY_FILE_STORE_DIR is not accessible"),
            Ok(_) => {}
        }
        let meta = std::fs::symlink_metadata(dir)
            .map_err(|_| anyhow::anyhow!("GATEWAY_FILE_STORE_DIR is not accessible"))?;
        anyhow::ensure!(
            meta.file_type().is_dir(),
            "GATEWAY_FILE_STORE_DIR must be a directory, not a file or symlink"
        );
        anyhow::ensure!(
            meta.uid() == unsafe { libc::geteuid() },
            "GATEWAY_FILE_STORE_DIR must be owned by the gateway user"
        );
        anyhow::ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "GATEWAY_FILE_STORE_DIR must not be accessible by group or others (chmod 700)"
        );
        let root = std::fs::canonicalize(dir)
            .map_err(|_| anyhow::anyhow!("GATEWAY_FILE_STORE_DIR is not accessible"))?;
        Ok(Self { root })
    }

    /// Walks `components` below the root, refusing symlinks or non-directories;
    /// creates missing directories (0700) when `create`.
    async fn directory(
        &self,
        components: &[&str],
        create: bool,
    ) -> Result<PathBuf, FileStoreError> {
        let mut path = self.root.clone();
        for component in components {
            if component.is_empty()
                || *component == "."
                || *component == ".."
                || component.contains('/')
            {
                return Err(FileStoreError::UnsafePath);
            }
            path.push(component);
            match tokio::fs::symlink_metadata(&path).await {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => return Err(FileStoreError::UnsafePath),
                Err(e) if e.kind() == ErrorKind::NotFound && create => {
                    let mut builder = tokio::fs::DirBuilder::new();
                    builder.mode(0o700);
                    match builder.create(&path).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                            let m = tokio::fs::symlink_metadata(&path)
                                .await
                                .map_err(|e| io_error(&e))?;
                            if !m.file_type().is_dir() {
                                return Err(FileStoreError::UnsafePath);
                            }
                        }
                        Err(e) => return Err(io_error(&e)),
                    }
                }
                Err(e) => return Err(io_error(&e)),
            }
        }
        if !path.starts_with(&self.root) {
            return Err(FileStoreError::UnsafePath);
        }
        Ok(path)
    }

    fn split(key: &ObjectKey) -> ([&str; 2], &str) {
        let [a, b, c] = key.components();
        ([a, b], c)
    }
}

/// Removes an unfinished temp file when a write fails or is cancelled.
struct TempGuard(Option<PathBuf>);
impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[async_trait::async_trait]
impl Backend for LocalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }

    async fn put(&self, key: &ObjectKey, mut body: ByteStream) -> Result<u64, FileStoreError> {
        let (dirs, name) = Self::split(key);
        let dir = self.directory(&dirs, true).await?;
        let mut suffix = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
        let temp = dir.join(format!(".tmp-{name}-{}", hex::encode(suffix)));
        let target = dir.join(name);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(&temp)
            .await
            .map_err(|e| io_error(&e))?;
        let mut guard = TempGuard(Some(temp.clone()));
        let mut written = 0u64;
        while let Some(piece) = body.next().await {
            let piece = piece?;
            file.write_all(&piece).await.map_err(|e| io_error(&e))?;
            written += piece.len() as u64;
        }
        file.flush().await.map_err(|e| io_error(&e))?;
        file.sync_all().await.map_err(|e| io_error(&e))?;
        drop(file);
        // Keys are unique; never replace an existing object (or a symlink planted there).
        if tokio::fs::symlink_metadata(&target).await.is_ok() {
            return Err(FileStoreError::UnsafePath);
        }
        tokio::fs::rename(&temp, &target)
            .await
            .map_err(|e| io_error(&e))?;
        guard.0 = None;
        let dir_handle = tokio::fs::File::open(&dir)
            .await
            .map_err(|e| io_error(&e))?;
        dir_handle.sync_all().await.map_err(|e| io_error(&e))?;
        Ok(written)
    }

    async fn get(&self, key: &ObjectKey) -> Result<ByteStream, FileStoreError> {
        let (dirs, name) = Self::split(key);
        let dir = self.directory(&dirs, false).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW)
            .open(dir.join(name))
            .await
            .map_err(|e| io_error(&e))?;
        let meta = file.metadata().await.map_err(|e| io_error(&e))?;
        if !meta.is_file() {
            return Err(FileStoreError::UnsafePath);
        }
        let stream = async_stream::stream! {
            loop {
                let mut buf = BytesMut::with_capacity(64 * 1024);
                match file.read_buf(&mut buf).await {
                    Ok(0) => return,
                    Ok(_) => yield Ok(buf.freeze()),
                    Err(e) => { yield Err(io_error(&e)); return; }
                }
            }
        };
        Ok(stream.boxed())
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), FileStoreError> {
        let (dirs, name) = Self::split(key);
        let dir = match self.directory(&dirs, false).await {
            Ok(dir) => dir,
            Err(FileStoreError::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        // unlink never follows a symlink; it removes the link itself.
        match tokio::fs::remove_file(dir.join(name)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&e)),
        }
    }

    async fn head(&self, key: &ObjectKey) -> Result<Option<u64>, FileStoreError> {
        let (dirs, name) = Self::split(key);
        let dir = match self.directory(&dirs, false).await {
            Ok(dir) => dir,
            Err(FileStoreError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        match tokio::fs::symlink_metadata(dir.join(name)).await {
            Ok(m) if m.file_type().is_file() => Ok(Some(m.len())),
            Ok(_) => Err(FileStoreError::UnsafePath),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(&e)),
        }
    }
}

#[cfg(test)]
impl LocalBackend {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

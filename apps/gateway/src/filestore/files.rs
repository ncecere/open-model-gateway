//! Metadata-tracked files: the API consumers (batch engine, Files API, exports,
//! branding, video outputs) use. Each file is one `stored_files` row plus one
//! encrypted object.
//!
//! Write path: insert a pending row (identity, owner, purpose, key id) → stream the
//! object → record size and SHA-256 once (`committed_at`). A failed upload marks the
//! row deleted; a crash leaves a pending row that the sweeper removes after a day.
//! A file is readable only while committed, not deleted and not expired (explicit
//! `expires_at` or the purpose group's current retention, whichever is first).
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{
    ByteStream, FileStoreError, FileStoreRuntime, ObjectKey, Purpose, PurposeGroup, PutMeta, Scope,
};
use crate::store::Store;

/// Per-purpose retention (days) from `installation_settings`, as SQL over
/// `stored_files f` and `installation_settings s`. NULL means no expiry.
pub(crate) const RETENTION_SQL: &str = "CASE f.purpose WHEN 'batch_input' THEN s.file_batch_retention_days WHEN 'batch_output' THEN s.file_batch_retention_days WHEN 'video_output' THEN s.file_video_retention_days WHEN 'user_file' THEN s.file_user_files_retention_days WHEN 'export' THEN s.file_export_retention_days END";

fn expiry_sql() -> String {
    format!("least(f.expires_at, f.created_at + make_interval(days => {RETENTION_SQL}))")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileError {
    /// No store configured, or the purpose is turned off in Admin › Settings › Storage.
    Disabled,
    NotFound,
    /// Invalid metadata (scope, expiry, owner).
    Invalid,
    Store(FileStoreError),
    Database,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("file storage is not enabled for this purpose"),
            Self::NotFound => f.write_str("file not found"),
            Self::Invalid => f.write_str("invalid file metadata"),
            Self::Store(e) => e.fmt(f),
            Self::Database => f.write_str("file metadata storage unavailable"),
        }
    }
}
impl std::error::Error for FileError {}
impl From<sqlx::Error> for FileError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}
impl From<FileStoreError> for FileError {
    fn from(e: FileStoreError) -> Self {
        match e {
            FileStoreError::NotFound => Self::NotFound,
            FileStoreError::Disabled => Self::Disabled,
            other => Self::Store(other),
        }
    }
}

/// Installation settings for files (migration 0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoragePolicy {
    pub batch_enabled: bool,
    pub batch_retention_days: i32,
    pub video_enabled: bool,
    pub video_retention_days: i32,
    pub user_files_enabled: bool,
    pub user_files_retention_days: i32,
    pub export_retention_days: i32,
}

pub(crate) const POLICY_SQL: &str = "SELECT file_batch_enabled,file_batch_retention_days,file_video_enabled,file_video_retention_days,file_user_files_enabled,file_user_files_retention_days,file_export_retention_days FROM installation_settings WHERE singleton";

impl StoragePolicy {
    pub(crate) async fn load<'e>(executor: impl sqlx::PgExecutor<'e>) -> Result<Self, sqlx::Error> {
        let row: (bool, i32, bool, i32, bool, i32, i32) =
            sqlx::query_as(POLICY_SQL).fetch_one(executor).await?;
        Ok(Self {
            batch_enabled: row.0,
            batch_retention_days: row.1,
            video_enabled: row.2,
            video_retention_days: row.3,
            user_files_enabled: row.4,
            user_files_retention_days: row.5,
            export_retention_days: row.6,
        })
    }
    /// The admin toggle; groups without customer content are always on (they
    /// still need a configured backend).
    pub fn enabled(&self, group: PurposeGroup) -> bool {
        match group {
            PurposeGroup::Batch => self.batch_enabled,
            PurposeGroup::Video => self.video_enabled,
            PurposeGroup::UserFiles => self.user_files_enabled,
            PurposeGroup::Export | PurposeGroup::Branding => true,
        }
    }
    pub fn retention_days(&self, group: PurposeGroup) -> Option<i32> {
        match group {
            PurposeGroup::Batch => Some(self.batch_retention_days),
            PurposeGroup::Video => Some(self.video_retention_days),
            PurposeGroup::UserFiles => Some(self.user_files_retention_days),
            PurposeGroup::Export => Some(self.export_retention_days),
            PurposeGroup::Branding => None,
        }
    }
}

/// A new file. `workspace_id = None` is installation scope (branding, exports).
#[derive(Clone, Debug)]
pub struct NewFile {
    pub purpose: Purpose,
    pub workspace_id: Option<Uuid>,
    pub created_by_user_id: Option<Uuid>,
    /// Must belong to `workspace_id`.
    pub created_by_api_key_id: Option<Uuid>,
    /// Client-supplied name; sanitized (see [`sanitize_filename`]) and bounded.
    pub filename: Option<String>,
    /// Client-supplied type; kept only if it is a plain `type/subtype`.
    pub content_type: Option<String>,
    /// Optional explicit expiry (for example a client-requested `expires_after`);
    /// the purpose retention still applies if it is sooner. Must be in the future.
    pub expires_at: Option<DateTime<Utc>>,
    /// Reject uploads above this many plaintext bytes.
    pub max_bytes: Option<u64>,
}

impl NewFile {
    pub fn new(purpose: Purpose, workspace_id: Option<Uuid>) -> Self {
        Self {
            purpose,
            workspace_id,
            created_by_user_id: None,
            created_by_api_key_id: None,
            filename: None,
            content_type: None,
            expires_at: None,
            max_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    pub id: Uuid,
    pub key: ObjectKey,
    pub purpose: Purpose,
    pub workspace_id: Option<Uuid>,
    pub created_by_user_id: Option<Uuid>,
    pub created_by_api_key_id: Option<Uuid>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub sha256: [u8; 32],
    pub encryption_key_id: String,
    pub created_at: DateTime<Utc>,
    /// Effective expiry; `None` never expires (branding).
    pub expires_at: Option<DateTime<Utc>>,
}

/// Keeps the last path segment, drops control and bidirectional-override
/// characters, trims, and bounds to 255 characters. `None` if nothing usable remains.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let bounded: String = trimmed.chars().take(255).collect();
    Some(bounded.trim_end().to_owned()).filter(|s| !s.is_empty())
}

/// Lowercased `type/subtype` without parameters, or `None` when not a plain media type.
pub fn normalize_content_type(value: &str) -> Option<String> {
    let essence = value.split(';').next()?.trim().to_ascii_lowercase();
    let (kind, sub) = essence.split_once('/')?;
    let part = |s: &str| {
        (1..=64).contains(&s.len())
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$&^_.+-".contains(&b))
    };
    (part(kind) && part(sub)).then_some(essence)
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    object_key: String,
    workspace_id: Option<Uuid>,
    created_by_user_id: Option<Uuid>,
    created_by_api_key_id: Option<Uuid>,
    filename: Option<String>,
    content_type: Option<String>,
    size_bytes: Option<i64>,
    sha256: Option<Vec<u8>>,
    encryption_key_id: String,
    created_at: DateTime<Utc>,
    effective_expires_at: Option<DateTime<Utc>>,
}

impl Row {
    fn into_file(self) -> Result<StoredFile, FileError> {
        let key = ObjectKey::parse(&self.object_key).map_err(|_| FileError::Database)?;
        let purpose = key.purpose().ok_or(FileError::Database)?;
        let sha256: [u8; 32] = self
            .sha256
            .as_deref()
            .and_then(|s| s.try_into().ok())
            .ok_or(FileError::Database)?;
        Ok(StoredFile {
            id: self.id,
            key,
            purpose,
            workspace_id: self.workspace_id,
            created_by_user_id: self.created_by_user_id,
            created_by_api_key_id: self.created_by_api_key_id,
            filename: self.filename,
            content_type: self.content_type,
            size_bytes: self
                .size_bytes
                .and_then(|n| u64::try_from(n).ok())
                .ok_or(FileError::Database)?,
            sha256,
            encryption_key_id: self.encryption_key_id,
            created_at: self.created_at,
            expires_at: self.effective_expires_at,
        })
    }
}

/// Database metadata + the configured store.
#[derive(Clone)]
pub struct FileStorage {
    db: Store,
    runtime: FileStoreRuntime,
}

impl FileStorage {
    pub fn new(db: Store, runtime: FileStoreRuntime) -> Self {
        Self { db, runtime }
    }

    pub fn runtime(&self) -> &FileStoreRuntime {
        &self.runtime
    }

    pub async fn policy(&self) -> Result<StoragePolicy, FileError> {
        Ok(StoragePolicy::load(&self.db.pool).await?)
    }

    /// Whether new files of `purpose` may be stored right now.
    pub async fn accepts(&self, purpose: Purpose) -> Result<bool, FileError> {
        Ok(self.runtime.store().is_some() && self.policy().await?.enabled(purpose.group()))
    }

    /// Stores a new file. See the module docs for the write path.
    pub async fn create(&self, new: NewFile, body: ByteStream) -> Result<StoredFile, FileError> {
        let store = self.runtime.store().ok_or(FileError::Disabled)?;
        let key_id = self
            .runtime
            .active_key_id()
            .ok_or(FileError::Disabled)?
            .to_owned();
        let purpose = new.purpose;
        let scope = new
            .workspace_id
            .map_or(Scope::Installation, Scope::Workspace);
        let key = ObjectKey::new(purpose, scope).map_err(|_| FileError::Invalid)?;
        if new.created_by_api_key_id.is_some() && new.workspace_id.is_none() {
            return Err(FileError::Invalid);
        }
        if new.expires_at.is_some_and(|at| at <= Utc::now()) {
            return Err(FileError::Invalid);
        }
        if !self.policy().await?.enabled(purpose.group()) {
            return Err(FileError::Disabled);
        }
        let filename = new.filename.as_deref().and_then(sanitize_filename);
        let content_type = new.content_type.as_deref().and_then(normalize_content_type);
        sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_user_id,created_by_api_key_id,filename,content_type,backend,encryption_key_id,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(key.id())
            .bind(key.as_str())
            .bind(purpose.as_str())
            .bind(new.workspace_id)
            .bind(new.created_by_user_id)
            .bind(new.created_by_api_key_id)
            .bind(&filename)
            .bind(&content_type)
            .bind(self.runtime.backend_name())
            .bind(&key_id)
            .bind(new.expires_at)
            .execute(&self.db.pool)
            .await
            .map_err(|e| {
                if e.as_database_error().is_some_and(|d| d.code().as_deref() == Some("23503")) {
                    FileError::Invalid
                } else {
                    FileError::Database
                }
            })?;
        let stored = match store
            .put(
                &key,
                body,
                PutMeta {
                    max_bytes: new.max_bytes,
                },
            )
            .await
        {
            Ok(stored) => stored,
            Err(e) => {
                self.abandon(key.id()).await;
                return Err(e.into());
            }
        };
        let committed = sqlx::query("UPDATE stored_files SET size_bytes=$2,sha256=$3,committed_at=clock_timestamp() WHERE id=$1 AND committed_at IS NULL AND deleted_at IS NULL")
            .bind(key.id())
            .bind(i64::try_from(stored.size).map_err(|_| FileError::Invalid)?)
            .bind(stored.sha256.as_slice())
            .execute(&self.db.pool)
            .await;
        match committed {
            Ok(r) if r.rows_affected() == 1 => {}
            _ => {
                // Swept or unreachable meanwhile: never leave an untracked object.
                let _ = store.delete(&key).await;
                self.abandon(key.id()).await;
                return Err(FileError::Database);
            }
        }
        self.get(key.id(), new.workspace_id)
            .await?
            .ok_or(FileError::Database)
    }

    /// Marks a never-committed row deleted (best effort; the sweeper retries).
    async fn abandon(&self, id: Uuid) {
        let _ = sqlx::query("UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=$1 AND deleted_at IS NULL AND committed_at IS NULL")
            .bind(id)
            .execute(&self.db.pool)
            .await;
    }

    /// A live (committed, unexpired, undeleted) file in exactly this scope.
    pub async fn get(
        &self,
        id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<Option<StoredFile>, FileError> {
        let sql = format!(
            "SELECT f.id,f.object_key,f.workspace_id,f.created_by_user_id,f.created_by_api_key_id,f.filename,f.content_type,f.size_bytes,f.sha256,f.encryption_key_id,f.created_at,{expiry} AS effective_expires_at FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.id=$1 AND f.workspace_id IS NOT DISTINCT FROM $2 AND f.deleted_at IS NULL AND f.committed_at IS NOT NULL AND coalesce({expiry},'infinity')>now()",
            expiry = expiry_sql()
        );
        let row: Option<Row> = sqlx::query_as(&sql)
            .bind(id)
            .bind(workspace_id)
            .fetch_optional(&self.db.pool)
            .await?;
        row.map(Row::into_file).transpose()
    }

    /// Metadata and decrypted contents of a live file in this scope.
    pub async fn open(
        &self,
        id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<(StoredFile, ByteStream), FileError> {
        let store = self.runtime.store().ok_or(FileError::Disabled)?;
        let file = self
            .get(id, workspace_id)
            .await?
            .ok_or(FileError::NotFound)?;
        let stream = store.get(&file.key).await?;
        Ok((file, stream))
    }

    /// Deletes a file in this scope. The row is marked deleted once the object
    /// is gone; if the store fails, the file becomes unreadable at once and the
    /// sweeper retries the object. Returns false if there was no such file.
    pub async fn delete(&self, id: Uuid, workspace_id: Option<Uuid>) -> Result<bool, FileError> {
        let row: Option<(String, String)> = sqlx::query_as("SELECT object_key,backend FROM stored_files WHERE id=$1 AND workspace_id IS NOT DISTINCT FROM $2 AND deleted_at IS NULL")
            .bind(id)
            .bind(workspace_id)
            .fetch_optional(&self.db.pool)
            .await?;
        let Some((object_key, backend)) = row else {
            return Ok(false);
        };
        let key = ObjectKey::parse(&object_key).map_err(|_| FileError::Database)?;
        let deleted = match self.runtime.store() {
            Some(store) if backend == self.runtime.backend_name() => store.delete(&key).await,
            Some(_) => Err(FileStoreError::Unavailable),
            None => Err(FileStoreError::Disabled),
        };
        match deleted {
            Ok(()) => {
                sqlx::query("UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=$1 AND deleted_at IS NULL")
                    .bind(id)
                    .execute(&self.db.pool)
                    .await?;
            }
            Err(e) => {
                sqlx::query("UPDATE stored_files SET expires_at=least(expires_at,clock_timestamp()),delete_attempts=delete_attempts+1,last_delete_attempt_at=clock_timestamp(),last_delete_error=$2 WHERE id=$1 AND deleted_at IS NULL")
                    .bind(id)
                    .bind(e.code())
                    .execute(&self.db.pool)
                    .await?;
            }
        }
        Ok(true)
    }

    /// Bytes held by a workspace's undeleted files (uses the partial index).
    pub async fn workspace_stored_bytes(&self, workspace_id: Uuid) -> Result<u64, FileError> {
        let total: i64 = sqlx::query_scalar("SELECT coalesce(sum(size_bytes),0)::bigint FROM stored_files WHERE workspace_id=$1 AND deleted_at IS NULL")
            .bind(workspace_id)
            .fetch_one(&self.db.pool)
            .await?;
        Ok(u64::try_from(total).unwrap_or_default())
    }
}

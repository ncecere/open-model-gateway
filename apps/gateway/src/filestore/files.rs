//! Metadata-tracked files: the API consumers (batch engine, Files API, exports,
//! branding, video outputs) use. Each file is one `stored_files` row plus one
//! encrypted object.
//!
//! Write path: insert a pending row (identity, owner, purpose, key id) → stream the
//! object → record size and SHA-256 once (`committed_at`). A failed upload marks the
//! row deleted; a crash leaves a pending row that the sweeper removes after a day.
//! A file is readable only while committed, not deleted and not expired (explicit
//! `expires_at` or the purpose group's current retention, whichever is first).
//!
//! Storage quota (0020): a workspace file's upload reserves bytes as it streams
//! (see [`reserve`]); the reservation counts against the workspace's stacked
//! `storage_bytes` limit until the file commits (then its size counts) or is
//! abandoned. Concurrent uploads therefore never jointly exceed the quota.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use uuid::Uuid;

use super::{
    ByteStream, FileStore, FileStoreError, FileStoreRuntime, ObjectKey, Purpose, PurposeGroup,
    PutMeta, Scope,
};
use crate::store::Store;

/// Client-facing file id prefix (`file-<32 lowercase hex>`).
pub const FILE_ID_PREFIX: &str = "file-";

/// The gateway file id clients see for a stored file.
pub fn public_id(id: Uuid) -> String {
    format!("{FILE_ID_PREFIX}{}", id.simple())
}

/// Strictly parses [`public_id`]: the prefix plus exactly 32 lowercase hex digits.
pub fn parse_public_id(value: &str) -> Option<Uuid> {
    let hex = value.strip_prefix(FILE_ID_PREFIX)?;
    if hex.len() != 32
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Uuid::parse_str(hex).ok()
}

/// OpenAI Files API purposes a file can carry (`stored_files.api_purpose`).
pub const API_PURPOSES: [&str; 6] = [
    "batch",
    "batch_output",
    "user_data",
    "vision",
    "assistants",
    "evals",
];

/// The client-facing purpose stored with a new file: explicit for user files
/// (validated), derived for batch files, none for other purposes.
fn api_purpose(
    purpose: Purpose,
    requested: Option<&str>,
) -> Result<Option<&'static str>, FileError> {
    let derived = match purpose {
        Purpose::BatchInput => Some("batch"),
        Purpose::BatchOutput => Some("batch_output"),
        Purpose::UserFile => Some("user_data"),
        _ => None,
    };
    match requested {
        None => Ok(derived),
        Some(value) => {
            let known = API_PURPOSES
                .into_iter()
                .find(|p| *p == value)
                .ok_or(FileError::Invalid)?;
            let fits = match purpose {
                Purpose::BatchInput => known == "batch",
                Purpose::BatchOutput => known == "batch_output",
                Purpose::UserFile => Purpose::from_openai(known) == Some(Purpose::UserFile),
                _ => false,
            };
            if fits {
                Ok(Some(known))
            } else {
                Err(FileError::Invalid)
            }
        }
    }
}

/// Effective storage quota of workspace `$1`: the platform layer (override if
/// present, else the type default) and the tighten-only local layer; NULL = no cap.
pub(crate) const QUOTA_SQL: &str = "SELECT least(CASE WHEN o.workspace_id IS NOT NULL THEN o.storage_bytes ELSE t.storage_bytes END,l.storage_bytes) FROM workspaces w LEFT JOIN workspace_platform_policy_overrides o ON o.workspace_id=w.id LEFT JOIN workspace_type_policies t ON t.kind=w.kind LEFT JOIN workspace_local_policies l ON l.workspace_id=w.id WHERE w.id=$1";

/// Bytes counted against workspace `$1`'s quota, excluding file `$2`: live
/// (committed, unexpired, undeleted) sizes plus reservations of uploads in
/// progress (pending rows younger than the sweeper's one-day cutoff).
fn used_sql() -> String {
    format!(
        "SELECT coalesce(sum(CASE WHEN f.committed_at IS NOT NULL THEN f.size_bytes ELSE f.reserved_bytes END),0)::bigint FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.workspace_id=$1 AND f.deleted_at IS NULL AND f.id IS DISTINCT FROM $2 AND CASE WHEN f.committed_at IS NULL THEN f.created_at > now()-interval '1 day' ELSE coalesce({},'infinity') > now() END",
        expiry_sql()
    )
}

/// [`used_sql`] for every workspace: `(workspace_id, bytes)`.
pub(crate) fn used_by_workspace_sql() -> String {
    format!(
        "SELECT f.workspace_id,sum(CASE WHEN f.committed_at IS NOT NULL THEN f.size_bytes ELSE f.reserved_bytes END)::bigint AS bytes FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.workspace_id IS NOT NULL AND f.deleted_at IS NULL AND CASE WHEN f.committed_at IS NULL THEN f.created_at > now()-interval '1 day' ELSE coalesce({},'infinity') > now() END GROUP BY f.workspace_id",
        expiry_sql()
    )
}

/// Reservation granularity: an upload reserves ahead in steps of this size
/// (bounded by the remaining quota and the file's maximum).
const RESERVE_STEP: u64 = 8 * 1024 * 1024;

/// Whether a workspace file's upload may fail on the storage quota.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuotaMode {
    /// Abort and delete the partial object once the quota would be exceeded.
    #[default]
    Enforce,
    /// Reserve and count the bytes, but never fail on the quota (for example
    /// results the workspace already paid for).
    CountOnly,
}

/// A workspace's stored bytes against its effective quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageUse {
    /// Live file sizes plus reservations of uploads in progress.
    pub used_bytes: u64,
    /// `None`: no storage cap applies.
    pub quota_bytes: Option<u64>,
}

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
    /// Invalid metadata (scope, expiry, owner, purpose).
    Invalid,
    /// Larger than the caller's `max_bytes`.
    TooLarge,
    /// The workspace's storage quota would be exceeded.
    QuotaExceeded,
    Store(FileStoreError),
    Database,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("file storage is not enabled for this purpose"),
            Self::NotFound => f.write_str("file not found"),
            Self::Invalid => f.write_str("invalid file metadata"),
            Self::TooLarge => f.write_str("file too large"),
            Self::QuotaExceeded => f.write_str("storage quota exceeded"),
            Self::Store(e) => e.fmt(f),
            Self::Database => f.write_str("file metadata storage unavailable"),
        }
    }
}
impl std::error::Error for FileError {}
impl From<sqlx::Error> for FileError {
    fn from(e: sqlx::Error) -> Self {
        // Callers map this to a generic 503; keep the sanitized cause.
        crate::management::log_storage_error(&e);
        Self::Database
    }
}
impl From<FileStoreError> for FileError {
    fn from(e: FileStoreError) -> Self {
        match e {
            FileStoreError::NotFound => Self::NotFound,
            FileStoreError::Disabled => Self::Disabled,
            FileStoreError::TooLarge => Self::TooLarge,
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
    /// Optional expiry relative to the row's `created_at` (OpenAI
    /// `expires_after` with anchor `created_at`); combines with `expires_at`
    /// by taking the sooner. Must be positive.
    pub expires_after: Option<chrono::TimeDelta>,
    /// Reject uploads above this many plaintext bytes.
    pub max_bytes: Option<u64>,
    /// OpenAI purpose shown by the Files API. `None` derives it (`batch`,
    /// `batch_output`, `user_data`); user files may name `vision`,
    /// `assistants` or `evals`. `Some("")` stores none: the file is internal
    /// and never listed or served by the Files API (see [`NewFile::internal`]);
    /// a staged upload gets its purpose at [`FileStorage::commit`].
    pub api_purpose: Option<String>,
    /// Workspace files only; installation files never count.
    pub quota: QuotaMode,
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
            expires_after: None,
            max_bytes: None,
            api_purpose: None,
            quota: QuotaMode::Enforce,
        }
    }

    /// A workspace file the Files API never shows (for example a batch's
    /// private working copy). It still counts toward the storage quota.
    pub fn internal(purpose: Purpose, workspace_id: Option<Uuid>) -> Self {
        Self {
            api_purpose: Some(String::new()),
            ..Self::new(purpose, workspace_id)
        }
    }
}

/// Files API listing (one workspace, newest first unless `ascending`).
#[derive(Clone, Debug, Default)]
pub struct FileList {
    /// An OpenAI purpose (`batch`, `batch_output`, `user_data`, …).
    pub purpose: Option<String>,
    /// Cursor: list files after this one in the chosen order.
    pub after: Option<Uuid>,
    /// 1..=10000.
    pub limit: i64,
    pub ascending: bool,
    /// Only files created by this user (members without workspace-wide visibility).
    pub created_by_user_id: Option<Uuid>,
    /// Case-insensitive filename substring.
    pub search: Option<String>,
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
    /// OpenAI purpose for files the Files API shows (batch input/output and
    /// user files); `None` for exports, branding and video outputs.
    pub api_purpose: Option<String>,
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
    api_purpose: Option<String>,
}

/// Columns of [`Row`] over `stored_files f` / `installation_settings s`.
fn row_columns() -> String {
    format!(
        "f.id,f.object_key,f.workspace_id,f.created_by_user_id,f.created_by_api_key_id,f.filename,f.content_type,f.size_bytes,f.sha256,f.encryption_key_id,f.created_at,{expiry} AS effective_expires_at,{API_PURPOSE_SQL} AS api_purpose",
        expiry = expiry_sql()
    )
}

/// The Files API purpose of `stored_files f`: NULL for files the API never
/// shows (exports, branding, video outputs, internal batch copies, uploads
/// not committed yet).
const API_PURPOSE_SQL: &str = "f.api_purpose";

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
            api_purpose: self.api_purpose,
        })
    }
}

/// Grows the reservation of pending upload `id` in workspace `ws` to cover
/// `needed` bytes (reserving ahead by [`RESERVE_STEP`], bounded by the remaining
/// quota and `cap`). Returns the new reservation.
///
/// Locking: the catalog advisory lock (shared), then a per-workspace
/// transaction advisory lock, the same global order as admission (catalog
/// first). The transaction does no I/O besides these statements, so concurrent
/// uploads to one workspace serialize only for a few milliseconds per step and
/// can never jointly exceed the quota.
pub(crate) async fn reserve(
    db: &Store,
    ws: Uuid,
    id: Uuid,
    needed: u64,
    cap: Option<u64>,
    mode: QuotaMode,
) -> Result<u64, FileError> {
    let mut tx = crate::db::begin(&db.pool).await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('storage_quota:'||$1::text,0))")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    let quota: Option<Option<i64>> = sqlx::query_scalar(QUOTA_SQL)
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?;
    let quota = quota.ok_or(FileError::Invalid)?;
    let used: i64 = sqlx::query_scalar(&used_sql())
        .bind(ws)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let used = u64::try_from(used).unwrap_or_default();
    let mut ahead = needed.saturating_add(RESERVE_STEP);
    if let Some(cap) = cap {
        ahead = ahead.min(cap.max(needed));
    }
    let reserved = match (mode, quota.and_then(|q| u64::try_from(q).ok())) {
        (QuotaMode::Enforce, Some(quota)) => {
            let available = quota.saturating_sub(used);
            if needed > available {
                return Err(FileError::QuotaExceeded);
            }
            ahead.min(available)
        }
        _ => ahead,
    };
    let updated = sqlx::query("UPDATE stored_files SET reserved_bytes=greatest(reserved_bytes,$2) WHERE id=$1 AND workspace_id=$3 AND committed_at IS NULL AND deleted_at IS NULL")
        .bind(id)
        .bind(i64::try_from(reserved).map_err(|_| FileError::TooLarge)?)
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(FileError::Database);
    }
    tx.commit().await?;
    Ok(reserved)
}

/// Quota metering of one workspace upload.
struct Meter {
    db: Store,
    ws: Uuid,
    id: Uuid,
    cap: Option<u64>,
    mode: QuotaMode,
    /// Set when the quota refused the upload.
    exceeded: Arc<AtomicBool>,
    /// Set when the reservation could not be recorded.
    failed: Arc<AtomicBool>,
}

impl Meter {
    /// Grows the reservation before each chunk that would exceed it. On a
    /// refusal the stream fails (the store then discards the partial object).
    fn wrap(self, body: ByteStream) -> ByteStream {
        async_stream::stream! {
            let mut body = body;
            let (mut total, mut reserved) = (0u64, 0u64);
            while let Some(item) = body.next().await {
                match item {
                    Ok(chunk) => {
                        total = total.saturating_add(chunk.len() as u64);
                        if total > reserved {
                            match reserve(&self.db, self.ws, self.id, total, self.cap, self.mode).await {
                                Ok(n) => reserved = n,
                                Err(e) => {
                                    let flag = if e == FileError::QuotaExceeded { &self.exceeded } else { &self.failed };
                                    flag.store(true, Ordering::SeqCst);
                                    yield Err(FileStoreError::Source);
                                    return;
                                }
                            }
                        }
                        yield Ok(chunk);
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
        }
        .boxed()
    }
}

/// Cleans up an upload whose future was dropped (client disconnect, deadline):
/// deletes any object (idempotent) and marks the pending row deleted, which
/// also releases its quota reservation.
struct PendingUpload {
    db: Store,
    store: Arc<dyn FileStore>,
    key: ObjectKey,
    armed: bool,
}
impl Drop for PendingUpload {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (db, store, key) = (self.db.clone(), self.store.clone(), self.key.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = store.delete(&key).await;
                abandon_row(&db, key.id()).await;
            });
        }
    }
}

/// Marks a never-committed row deleted (best effort; the sweeper retries).
async fn abandon_row(db: &Store, id: Uuid) {
    let _ = sqlx::query("UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=$1 AND deleted_at IS NULL AND committed_at IS NULL")
        .bind(id)
        .execute(&db.pool)
        .await;
}

/// A written, uncommitted file ([`FileStorage::stage`]). Dropping it unfinished
/// deletes the object and marks the row deleted.
pub(crate) struct StagedFile {
    key: ObjectKey,
    workspace_id: Option<Uuid>,
    created_by_user_id: Option<Uuid>,
    created_by_api_key_id: Option<Uuid>,
    filename: Option<String>,
    content_type: Option<String>,
    size: u64,
    sha256: [u8; 32],
    guard: PendingUpload,
}
impl StagedFile {
    pub(crate) fn purpose(&self) -> Purpose {
        self.key.purpose().unwrap_or(Purpose::UserFile)
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

    /// Stores a new file. See the module docs for the write path. Workspace
    /// files reserve storage quota while streaming ([`QuotaMode`]); a refusal
    /// aborts the upload, deletes the partial object and marks the row deleted.
    pub async fn create(&self, new: NewFile, body: ByteStream) -> Result<StoredFile, FileError> {
        self.runtime.store().ok_or(FileError::Disabled)?;
        if !self.policy().await?.enabled(new.purpose.group()) {
            return Err(FileError::Disabled);
        }
        let staged = self.stage(new, body).await?;
        self.commit(staged, None, None).await
    }

    /// Writes a file's object under a pending row without committing it (the
    /// first two steps of the write path). The purpose's Settings toggle is
    /// **not** checked: callers check [`Self::accepts`] before [`Self::commit`].
    /// Dropping the result unfinished deletes the object and the reservation.
    pub(crate) async fn stage(
        &self,
        new: NewFile,
        body: ByteStream,
    ) -> Result<StagedFile, FileError> {
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
        if new.expires_at.is_some_and(|at| at <= Utc::now())
            || new
                .expires_after
                .is_some_and(|d| d <= chrono::TimeDelta::zero())
        {
            return Err(FileError::Invalid);
        }
        let expires_after_ms = new.expires_after.map(|d| d.num_milliseconds());
        // Staged uploads whose purpose is still unknown carry no API purpose yet.
        let api_purpose = match new.api_purpose.as_deref() {
            Some("") => None,
            requested => api_purpose(purpose, requested)?,
        };
        let filename = new.filename.as_deref().and_then(sanitize_filename);
        let content_type = new.content_type.as_deref().and_then(normalize_content_type);
        sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_user_id,created_by_api_key_id,filename,content_type,backend,encryption_key_id,expires_at,api_purpose) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,least($11::timestamptz,now()+make_interval(secs => $13::bigint/1000.0)),$12)")
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
            .bind(api_purpose)
            .bind(expires_after_ms)
            .execute(&self.db.pool)
            .await
            .map_err(|e| {
                if e.as_database_error().is_some_and(|d| d.code().as_deref() == Some("23503")) {
                    FileError::Invalid
                } else {
                    FileError::Database
                }
            })?;
        let mut guard = PendingUpload {
            db: self.db.clone(),
            store: store.clone(),
            key: key.clone(),
            armed: true,
        };
        let exceeded = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let body = match new.workspace_id {
            Some(ws) => Meter {
                db: self.db.clone(),
                ws,
                id: key.id(),
                cap: new.max_bytes,
                mode: new.quota,
                exceeded: exceeded.clone(),
                failed: failed.clone(),
            }
            .wrap(body),
            None => body,
        };
        let put = store
            .put(
                &key,
                body,
                PutMeta {
                    max_bytes: new.max_bytes,
                },
            )
            .await;
        match put {
            Ok(stored) => Ok(StagedFile {
                key,
                workspace_id: new.workspace_id,
                created_by_user_id: new.created_by_user_id,
                created_by_api_key_id: new.created_by_api_key_id,
                filename,
                content_type,
                size: stored.size,
                sha256: stored.sha256,
                guard,
            }),
            Err(e) => {
                guard.armed = false;
                self.abandon(key.id()).await;
                Err(if exceeded.load(Ordering::SeqCst) {
                    FileError::QuotaExceeded
                } else if failed.load(Ordering::SeqCst) {
                    FileError::Database
                } else {
                    e.into()
                })
            }
        }
    }

    /// Records a staged file's size and SHA-256 once (`committed_at`). A staged
    /// row without an API purpose gets `api_purpose` now (validated against its
    /// store purpose); `expires_after` (relative to `created_at`) can only
    /// shorten the expiry.
    pub(crate) async fn commit(
        &self,
        mut staged: StagedFile,
        api_purpose_now: Option<&str>,
        expires_after: Option<chrono::TimeDelta>,
    ) -> Result<StoredFile, FileError> {
        let purpose = staged.purpose();
        let api = api_purpose_now
            .map(|p| api_purpose(purpose, Some(p)))
            .transpose()?
            .flatten();
        let committed = sqlx::query("UPDATE stored_files SET size_bytes=$2,sha256=$3,committed_at=clock_timestamp(),api_purpose=coalesce(api_purpose,$4),expires_at=least(expires_at,created_at+make_interval(secs => $5::bigint/1000.0)) WHERE id=$1 AND committed_at IS NULL AND deleted_at IS NULL")
            .bind(staged.key.id())
            .bind(i64::try_from(staged.size).map_err(|_| FileError::Invalid)?)
            .bind(staged.sha256.as_slice())
            .bind(api)
            .bind(expires_after.map(|d| d.num_milliseconds()))
            .execute(&self.db.pool)
            .await;
        staged.guard.armed = false;
        match committed {
            Ok(r) if r.rows_affected() == 1 => {}
            _ => {
                // Swept or unreachable meanwhile: never leave an untracked object.
                if let Some(store) = self.runtime.store() {
                    let _ = store.delete(&staged.key).await;
                }
                self.abandon(staged.key.id()).await;
                return Err(FileError::Database);
            }
        }
        self.get(staged.key.id(), staged.workspace_id)
            .await?
            .ok_or(FileError::Database)
    }

    /// Deletes a staged file's object and marks its row deleted (releasing its
    /// quota reservation).
    pub(crate) async fn discard(&self, mut staged: StagedFile) {
        staged.guard.armed = false;
        if let Some(store) = self.runtime.store() {
            let _ = store.delete(&staged.key).await;
        }
        self.abandon(staged.key.id()).await;
    }

    /// Moves a staged file to another store purpose: streams the decrypted
    /// object into a new staged row (counted, never refused, by the quota:
    /// the bytes were already admitted) and discards the original.
    pub(crate) async fn restage(
        &self,
        staged: StagedFile,
        purpose: Purpose,
        max_bytes: Option<u64>,
    ) -> Result<StagedFile, FileError> {
        let store = self.runtime.store().ok_or(FileError::Disabled)?;
        let body = match store.get(&staged.key).await {
            Ok(body) => body,
            Err(e) => {
                self.discard(staged).await;
                return Err(e.into());
            }
        };
        let new = NewFile {
            created_by_user_id: staged.created_by_user_id,
            created_by_api_key_id: staged.created_by_api_key_id,
            filename: staged.filename.clone(),
            content_type: staged.content_type.clone(),
            max_bytes,
            api_purpose: Some(String::new()),
            quota: QuotaMode::CountOnly,
            ..NewFile::new(purpose, staged.workspace_id)
        };
        let moved = self.stage(new, body).await;
        self.discard(staged).await;
        moved
    }

    /// Marks a never-committed row deleted (best effort; the sweeper retries).
    async fn abandon(&self, id: Uuid) {
        abandon_row(&self.db, id).await;
    }

    /// A live (committed, unexpired, undeleted) file in exactly this scope.
    pub async fn get(
        &self,
        id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<Option<StoredFile>, FileError> {
        let sql = format!(
            "SELECT {cols} FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.id=$1 AND f.workspace_id IS NOT DISTINCT FROM $2 AND f.deleted_at IS NULL AND f.committed_at IS NOT NULL AND coalesce({expiry},'infinity')>now()",
            cols = row_columns(),
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

    /// Quota usage of a workspace: what [`reserve`] counts, against its effective quota.
    pub async fn workspace_storage(&self, workspace_id: Uuid) -> Result<StorageUse, FileError> {
        workspace_storage(&self.db.pool, workspace_id).await
    }

    /// Files API listing of one workspace. Returns the page and whether more follow.
    pub async fn list(
        &self,
        workspace_id: Uuid,
        q: &FileList,
    ) -> Result<(Vec<StoredFile>, bool), FileError> {
        if !(1..=10_000).contains(&q.limit) {
            return Err(FileError::Invalid);
        }
        let (cmp, dir) = if q.ascending {
            (">", "ASC")
        } else {
            ("<", "DESC")
        };
        let sql = format!(
            "SELECT {cols} FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.workspace_id=$1 AND f.api_purpose IS NOT NULL AND f.deleted_at IS NULL AND f.committed_at IS NOT NULL AND coalesce({expiry},'infinity')>now() AND ($2::text IS NULL OR {API_PURPOSE_SQL}=$2) AND ($3::uuid IS NULL OR f.created_by_user_id=$3) AND ($4::text IS NULL OR strpos(lower(f.filename),lower($4))>0) AND ($5::uuid IS NULL OR (f.created_at,f.id) {cmp} (SELECT a.created_at,a.id FROM stored_files a WHERE a.id=$5 AND a.workspace_id=$1)) ORDER BY f.created_at {dir},f.id {dir} LIMIT $6",
            cols = row_columns(),
            expiry = expiry_sql()
        );
        let rows: Vec<Row> = sqlx::query_as(&sql)
            .bind(workspace_id)
            .bind(q.purpose.as_deref())
            .bind(q.created_by_user_id)
            .bind(q.search.as_deref().filter(|s| !s.is_empty()))
            .bind(q.after)
            .bind(q.limit + 1)
            .fetch_all(&self.db.pool)
            .await?;
        let more = rows.len() as i64 > q.limit;
        let files = rows
            .into_iter()
            .take(q.limit as usize)
            .map(Row::into_file)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((files, more))
    }
}

/// Quota usage of a workspace (see [`FileStorage::workspace_storage`]).
pub(crate) async fn workspace_storage<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace_id: Uuid,
) -> Result<StorageUse, FileError> {
    let (quota, used): (Option<i64>, i64) =
        sqlx::query_as(&format!("SELECT ({QUOTA_SQL}),({})", used_sql()))
            .bind(workspace_id)
            .bind(None::<Uuid>)
            .fetch_one(executor)
            .await?;
    Ok(StorageUse {
        used_bytes: u64::try_from(used).unwrap_or_default(),
        quota_bytes: quota.and_then(|q| u64::try_from(q).ok()),
    })
}

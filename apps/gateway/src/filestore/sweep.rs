//! Retention sweeper and metadata/store verification.
//!
//! The sweeper removes objects whose file expired (explicit `expires_at` or the
//! purpose group's current retention), whose deletion is pending, or whose upload
//! never committed within a day. Replica-safe without holding row locks across
//! network I/O: a short transaction claims rows (`last_delete_attempt_at` acts as
//! a 5-minute lease), the objects are deleted (idempotent), then each row is
//! marked deleted or its failure recorded (`delete_attempts`, `last_delete_error`).
use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use super::{FileStoreRuntime, ObjectKey, files::RETENTION_SQL};
use crate::store::Store;

/// One sweep's outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub claimed: u64,
    pub deleted: u64,
    pub failed: u64,
}

/// Claims and deletes up to `limit` (1..=1000) due objects.
pub async fn sweep_once(
    db: &Store,
    runtime: &FileStoreRuntime,
    limit: i64,
) -> Result<SweepReport, sqlx::Error> {
    let limit = limit.clamp(1, 1000);
    let sql = format!(
        "UPDATE stored_files f SET last_delete_attempt_at=clock_timestamp() FROM (SELECT f.id FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.deleted_at IS NULL AND (f.last_delete_attempt_at IS NULL OR f.last_delete_attempt_at < now()-interval '5 minutes') AND ((f.committed_at IS NULL AND f.created_at < now()-interval '1 day') OR f.expires_at <= now() OR f.created_at + make_interval(days => {RETENTION_SQL}) <= now()) ORDER BY f.created_at,f.id LIMIT $1 FOR UPDATE OF f SKIP LOCKED) due WHERE f.id=due.id RETURNING f.id,f.object_key,f.backend"
    );
    let due: Vec<(Uuid, String, String)> =
        sqlx::query_as(&sql).bind(limit).fetch_all(&db.pool).await?;
    let mut report = SweepReport {
        claimed: due.len() as u64,
        ..SweepReport::default()
    };
    for (id, object_key, backend) in due {
        let outcome = match (runtime.store(), ObjectKey::parse(&object_key)) {
            (None, _) => Err("store_off"),
            (Some(_), _) if backend != runtime.backend_name() => Err("backend_mismatch"),
            (Some(_), Err(_)) => Err("invalid_key"),
            (Some(store), Ok(key)) => store.delete(&key).await.map_err(|e| e.code()),
        };
        match outcome {
            Ok(()) => {
                sqlx::query("UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=$1 AND deleted_at IS NULL")
                    .bind(id)
                    .execute(&db.pool)
                    .await?;
                report.deleted += 1;
            }
            Err(code) => {
                sqlx::query("UPDATE stored_files SET delete_attempts=delete_attempts+1,last_delete_error=$2 WHERE id=$1 AND deleted_at IS NULL")
                    .bind(id)
                    .bind(code)
                    .execute(&db.pool)
                    .await?;
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

/// Background sweeper: every minute, while a store is configured.
pub fn start(db: Store, runtime: FileStoreRuntime) -> Option<tokio::task::JoinHandle<()>> {
    runtime.store()?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match tokio::time::timeout(Duration::from_secs(50), sweep_once(&db, &runtime, 200))
                .await
            {
                Ok(Ok(r)) if r.claimed > 0 => tracing::info!(
                    deleted = r.deleted,
                    failed = r.failed,
                    "expired stored files swept"
                ),
                Ok(Ok(_)) => {}
                _ => tracing::warn!("stored file sweep incomplete; retrying next interval"),
            }
        }
    }))
}

/// Read-only comparison of metadata with the store.
#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub backend: &'static str,
    pub checked: u64,
    pub ok: u64,
    /// Committed, undeleted files whose object is missing.
    pub missing: Vec<String>,
    /// Objects whose size does not match the recorded plaintext size.
    pub size_mismatch: Vec<String>,
    /// Files recorded under another backend than the configured one.
    pub backend_mismatch: u64,
    /// Files encrypted under a key id that is no longer configured.
    pub unknown_key_ids: Vec<String>,
    /// Uploads that never committed and are older than a day (sweeper will remove).
    pub stale_pending: u64,
    /// Undeleted rows with failed delete attempts.
    pub delete_failures: u64,
    /// Store errors while checking (key → error code).
    pub errors: Vec<(String, &'static str)>,
    /// Undeleted files per encryption key id (for rotation).
    pub by_key_id: std::collections::BTreeMap<String, u64>,
}

impl VerifyReport {
    pub fn consistent(&self) -> bool {
        self.missing.is_empty()
            && self.size_mismatch.is_empty()
            && self.backend_mismatch == 0
            && self.unknown_key_ids.is_empty()
            && self.errors.is_empty()
    }
}

/// Checks up to `limit` committed, undeleted files against the store.
pub async fn verify(
    db: &Store,
    runtime: &FileStoreRuntime,
    limit: i64,
) -> anyhow::Result<VerifyReport> {
    let store = runtime
        .store()
        .ok_or_else(|| anyhow::anyhow!("no file store is configured (GATEWAY_FILE_STORE=off)"))?;
    let mut report = VerifyReport {
        backend: runtime.backend_name(),
        ..VerifyReport::default()
    };
    let (stale, failures): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE committed_at IS NULL AND created_at < now()-interval '1 day'),count(*) FILTER(WHERE delete_attempts>0) FROM stored_files WHERE deleted_at IS NULL")
        .fetch_one(&db.pool)
        .await?;
    report.stale_pending = stale as u64;
    report.delete_failures = failures as u64;
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as("SELECT object_key,backend,encryption_key_id,size_bytes FROM stored_files WHERE deleted_at IS NULL AND committed_at IS NOT NULL ORDER BY created_at,id LIMIT $1")
        .bind(limit.clamp(1, 1_000_000))
        .fetch_all(&db.pool)
        .await?;
    for (object_key, backend, key_id, size) in rows {
        report.checked += 1;
        *report.by_key_id.entry(key_id.clone()).or_default() += 1;
        if backend != runtime.backend_name() {
            report.backend_mismatch += 1;
            continue;
        }
        if !runtime.knows_key(&key_id) && !report.unknown_key_ids.contains(&key_id) {
            report.unknown_key_ids.push(key_id);
        }
        let Ok(key) = ObjectKey::parse(&object_key) else {
            report.errors.push((object_key, "invalid_key"));
            continue;
        };
        match store.head(&key).await {
            Ok(None) => report.missing.push(object_key),
            Ok(Some(info)) if i64::try_from(info.size).ok() != Some(size) => {
                report.size_mismatch.push(object_key)
            }
            Ok(Some(_)) => report.ok += 1,
            Err(e) => report.errors.push((object_key, e.code())),
        }
    }
    Ok(report)
}

//! `files rekey` (#35): move stored files from one master key to another so
//! the old key can be retired. Each object's data key is re-wrapped
//! ([`super::FileStore::rekey`]); contents are never decrypted or re-encrypted.
//!
//! One file at a time, in id order, each under its row lock (`FOR UPDATE SKIP
//! LOCKED`) for the whole rewrite, in a transaction with no idle timeout (a
//! server's `idle_in_transaction_session_timeout` would otherwise end it, and
//! release the lock, during a long copy). Deleting a file claims the row first
//! (`last_delete_attempt_at`, waiting for that lock) and deletes the object
//! after, so a rewrite can't land after a delete. Files whose delete claim is
//! within the sweeper's 5-minute lease are left to it. If recording a rewrite
//! fails (a lost connection), the row is read again and the object deleted
//! once more if it was claimed meanwhile. Resumable: an object already on the
//! new key (an interrupted run) is only recorded, and reads always follow the
//! key id in the object's own header.
use serde::Serialize;
use uuid::Uuid;

use super::{FileStoreRuntime, ObjectKey, Rekeyed, crypto::valid_key_id};
use crate::store::Store;

#[derive(Debug, Default, Serialize)]
pub struct RekeyReport {
    pub from: String,
    pub to: String,
    pub dry_run: bool,
    /// Files whose data key was re-wrapped under `to`.
    pub rewrapped: u64,
    /// Objects already on `to` (an interrupted run): only the record changed.
    pub recorded: u64,
    /// Files being deleted right now (left for the sweeper; rerun after).
    pub skipped_deleting: u64,
    /// Files recorded under another backend than the configured one.
    pub other_backend: u64,
    /// Files that failed (object key → error code); rerun to retry.
    pub failed: Vec<(String, &'static str)>,
    /// Committed, undeleted files still recorded under `from` afterwards.
    pub remaining: u64,
    /// True when no file needs `from` any more.
    pub complete: bool,
}

const LIVE: &str = "encryption_key_id=$1 AND committed_at IS NOT NULL AND deleted_at IS NULL";
/// Not being deleted: unclaimed, or a claim past the sweeper's 5-minute lease
/// (a failed delete waiting for its retry, or a hold placed after the claim).
const NOT_DELETING: &str =
    "(last_delete_attempt_at IS NULL OR last_delete_attempt_at < now()-interval '5 minutes')";

pub async fn rekey(
    db: &Store,
    runtime: &FileStoreRuntime,
    from: &str,
    to: &str,
    dry_run: bool,
    limit: i64,
) -> anyhow::Result<RekeyReport> {
    let store = runtime
        .store()
        .ok_or_else(|| anyhow::anyhow!("no file store is configured (GATEWAY_FILE_STORE=off)"))?;
    anyhow::ensure!(valid_key_id(from) && valid_key_id(to), "invalid key id");
    anyhow::ensure!(from != to, "--from and --to must differ");
    anyhow::ensure!(
        runtime.knows_key(to) && runtime.knows_key(from),
        "both keys must be configured in {} (the new key first, the old one decrypt-only)",
        super::crypto::KEYS_ENV
    );
    let mut report = RekeyReport {
        from: from.into(),
        to: to.into(),
        dry_run,
        ..RekeyReport::default()
    };
    let backend = runtime.backend_name();
    let mut after = Uuid::nil();
    let mut done = 0;
    while !dry_run && done < limit.max(0) {
        let mut tx = crate::db::begin_with_setup(
            &db.pool,
            "SET LOCAL idle_in_transaction_session_timeout=0; SET LOCAL lock_timeout='5s'",
        )
        .await?;
        let row: Option<(Uuid, String)> = sqlx::query_as(&format!(
            "SELECT id,object_key FROM stored_files WHERE {LIVE} AND backend=$2 AND {NOT_DELETING} AND id>$3 ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED"
        ))
        .bind(from)
        .bind(backend)
        .bind(after)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, object_key)) = row else { break };
        after = id;
        done += 1;
        if done % 1000 == 0 {
            eprintln!("files rekey: {done} files processed");
        }
        let Ok(key) = ObjectKey::parse(&object_key) else {
            report.failed.push((object_key, "invalid_key"));
            continue;
        };
        let outcome = match store.rekey(&key, to).await {
            Ok(outcome) => outcome,
            Err(e) => {
                report.failed.push((object_key, e.code()));
                continue;
            }
        };
        let recorded = async {
            sqlx::query("UPDATE stored_files SET encryption_key_id=$2 WHERE id=$1")
                .bind(id)
                .bind(to)
                .execute(&mut *tx)
                .await?;
            tx.commit().await
        }
        .await;
        if recorded.is_err() {
            // The lock may have gone with the connection: a delete that
            // claimed the row meanwhile must still win.
            let claimed: Option<bool> = sqlx::query_scalar("SELECT deleted_at IS NOT NULL OR last_delete_attempt_at IS NOT NULL FROM stored_files WHERE id=$1")
                .bind(id)
                .fetch_optional(&db.pool)
                .await?;
            if claimed != Some(false) {
                store.delete(&key).await.map_err(|e| {
                    anyhow::anyhow!(
                        "re-wrapped {object_key} while it was being deleted and could not delete it again ({}); delete that object by hand",
                        e.code()
                    )
                })?;
            }
            report.failed.push((object_key, "record_failed"));
            continue;
        }
        match outcome {
            Rekeyed::Rewrapped => report.rewrapped += 1,
            Rekeyed::AlreadyOn => report.recorded += 1,
        }
    }
    let (remaining, deleting, other): (i64, i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*),count(*) FILTER(WHERE backend=$2 AND NOT {NOT_DELETING}),count(*) FILTER(WHERE backend<>$2) FROM stored_files WHERE {LIVE}"
    ))
    .bind(from)
    .bind(backend)
    .fetch_one(&db.pool)
    .await?;
    report.remaining = remaining as u64;
    report.skipped_deleting = deleting as u64;
    report.other_backend = other as u64;
    report.complete = remaining == 0;
    Ok(report)
}

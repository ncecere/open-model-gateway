//! Storage-quota metering of a workspace upload: the reservation (see
//! [`reserve`]) and the stream wrapper that grows it chunk by chunk.
use super::*;

/// Grows the reservation of pending upload `id` in workspace `ws` to cover
/// `needed` bytes (reserving ahead by [`RESERVE_STEP`], bounded by the remaining
/// quota and `cap`). Returns the new reservation.
///
/// Locking: the catalog advisory lock (shared), then a per-workspace
/// transaction advisory lock, the same global order as admission (catalog
/// first). The transaction does no I/O besides these statements, so concurrent
/// uploads to one workspace serialize only for a few milliseconds per step and
/// can never jointly exceed the quota.
async fn reserve(
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
pub(super) struct Meter {
    pub(super) db: Store,
    pub(super) ws: Uuid,
    pub(super) id: Uuid,
    pub(super) cap: Option<u64>,
    pub(super) mode: QuotaMode,
    /// Set when the quota refused the upload.
    pub(super) exceeded: Arc<AtomicBool>,
    /// Set when the reservation could not be recorded.
    pub(super) failed: Arc<AtomicBool>,
}

impl Meter {
    /// Grows the reservation before each chunk that would exceed it. On a
    /// refusal the stream fails (the store then discards the partial object).
    pub(super) fn wrap(self, body: ByteStream) -> ByteStream {
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

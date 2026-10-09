//! Storage usage tracking (0020): append-only hourly byte-seconds per
//! workspace and purpose, derived from `stored_files` timestamps (a file
//! occupies storage from `committed_at` until `deleted_at`).
//!
//! Storage is **not charged**: rows hold quantities only. An optional future
//! installation storage price (micro-USD per GB-day, effective-dated and
//! append-only) would apply to hours starting at or after its effective time;
//! earlier hours stay "Not charged" and are never rewritten.
use chrono::{DateTime, Utc};

use crate::store::Store;

/// Byte-seconds in one GB-day (1 GB = 2^30 bytes, for 86,400 seconds).
pub const GB_DAY_BYTE_SECONDS: i64 = 1_073_741_824 * 86_400;
/// Hours are recorded once they ended at least this long ago, so commits and
/// deletions stamped inside an hour are visible before it is recorded.
const SETTLE_MINUTES: i32 = 5;

/// Records every complete hour after the progress mark, at most `max_hours`
/// (1..=720) per call. Replica-safe: the progress row is claimed with
/// `FOR UPDATE SKIP LOCKED`; a concurrent run returns 0. Returns the number
/// of hours recorded.
pub async fn record_hours(db: &Store, max_hours: i32) -> Result<i64, sqlx::Error> {
    let max_hours = max_hours.clamp(1, 720);
    let mut tx = db.pool.begin().await?;
    let Some(through): Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT recorded_through FROM storage_usage_progress WHERE singleton FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(0);
    };
    let end: DateTime<Utc> = sqlx::query_scalar("SELECT least($1 + make_interval(hours => $2), date_trunc('hour', now() - make_interval(mins => $3), 'UTC'))")
        .bind(through)
        .bind(max_hours)
        .bind(SETTLE_MINUTES)
        .fetch_one(&mut *tx)
        .await?;
    if end <= through {
        tx.commit().await?;
        return Ok(0);
    }
    sqlx::query("WITH f AS (SELECT workspace_id,purpose,size_bytes,committed_at,deleted_at FROM stored_files WHERE workspace_id IS NOT NULL AND committed_at IS NOT NULL AND committed_at < $2 AND (deleted_at IS NULL OR deleted_at > $1) AND size_bytes > 0), h AS (SELECT generate_series($1::timestamptz, $2::timestamptz - interval '1 hour', interval '1 hour') AS start), o AS (SELECT f.workspace_id,f.purpose,h.start,f.size_bytes * extract(epoch FROM least(coalesce(f.deleted_at,'infinity'::timestamptz), h.start + interval '1 hour') - greatest(f.committed_at, h.start)) AS bs FROM f JOIN h ON f.committed_at < h.start + interval '1 hour' AND (f.deleted_at IS NULL OR f.deleted_at > h.start)) INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) SELECT workspace_id,purpose,start,floor(sum(bs)),count(*) FROM o GROUP BY workspace_id,purpose,start HAVING floor(sum(bs)) > 0 ON CONFLICT DO NOTHING")
        .bind(through)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE storage_usage_progress SET recorded_through=$1 WHERE singleton")
        .bind(end)
        .execute(&mut *tx)
        .await?;
    let hours = (end - through).num_hours();
    tx.commit().await?;
    Ok(hours)
}

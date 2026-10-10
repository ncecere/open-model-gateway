//! Scope-lock ceiling visibility (migration 0036; scale follow-up to P3).
//!
//! Scoped admission serializes one workspace's (and one key lineage's)
//! admissions and settlements on that scope's totals and counter rows, so a
//! single scope tops out at a few hundred admissions per second. Two signals
//! make that ceiling visible without identifiers in metrics:
//!
//! - Prometheus (per replica, no scope ids): `gateway_scope_lock_wait_seconds`
//!   by lock (`authority`, `rows`) and path (`admission`, `settlement`), and
//!   `gateway_scope_lock_waiting` (transactions waiting right now).
//! - Per scope, for the `admission_ceiling` alert rule only: each replica
//!   counts, per UTC minute and scope, the interactive admissions whose scope
//!   locks took at least 10 ms ([`WAIT_BUCKETS_MS`]), in a bounded in-memory
//!   map, and adds them to `admission_lock_waits` every few seconds
//!   ([`flush`], off the admission path). Fast admissions are not recorded;
//!   the admission count of a minute comes from `rate_minute_counters`.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::Duration,
};

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use uuid::Uuid;

/// Lower bounds (ms) of the recorded lock waits; `waits[i]` counts the
/// admissions that waited at least `WAIT_BUCKETS_MS[i]`. The alert's
/// lock-wait threshold is one of these.
pub const WAIT_BUCKETS_MS: [i32; 8] = [10, 25, 50, 100, 250, 500, 1000, 2500];
/// At most this many (minute, scope) entries wait for the next flush; more
/// are dropped (and counted as a collection error), never blocking admission.
const MAX_ENTRIES: usize = 20_000;
/// Rows kept for diagnosis; the alert reads at most the last 10 minutes.
pub const RETAINED_HOURS: i64 = 1;

type Key = (DateTime<Utc>, &'static str, Uuid);
static WAITS: LazyLock<Mutex<HashMap<Key, [i64; 8]>>> = LazyLock::new(Mutex::default);

/// Bucket counts of one wait: 1 for every bound it reached.
fn buckets(wait: Duration) -> Option<[i64; 8]> {
    let ms = wait.as_millis();
    if ms < WAIT_BUCKETS_MS[0] as u128 {
        return None;
    }
    Some(WAIT_BUCKETS_MS.map(|b| i64::from(ms >= b as u128)))
}

/// Record the scope-lock wait of one interactive admission of `workspace`
/// and key `lineage`, admitted at `at`. Cheap and non-blocking for fast
/// admissions (nothing is recorded below 10 ms).
pub fn record(at: DateTime<Utc>, workspace: Uuid, lineage: Uuid, wait: Duration) {
    let Some(add) = buckets(wait) else {
        return;
    };
    let minute = at.duration_trunc(TimeDelta::minutes(1)).unwrap_or(at);
    let mut map = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    for key in [(minute, "workspace", workspace), (minute, "key", lineage)] {
        if !map.contains_key(&key) && map.len() >= MAX_ENTRIES {
            crate::metrics::METRICS.observe_collection_error("scope_lock_waits");
            continue;
        }
        let entry = map.entry(key).or_insert([0; 8]);
        for (slot, n) in entry.iter_mut().zip(add) {
            *slot += n;
        }
    }
}

fn take() -> HashMap<Key, [i64; 8]> {
    std::mem::take(&mut *WAITS.lock().unwrap_or_else(|e| e.into_inner()))
}

fn restore(rows: HashMap<Key, [i64; 8]>) {
    let mut map = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    for (key, add) in rows {
        if !map.contains_key(&key) && map.len() >= MAX_ENTRIES {
            continue;
        }
        let entry = map.entry(key).or_insert([0; 8]);
        for (slot, n) in entry.iter_mut().zip(add) {
            *slot += n;
        }
    }
}

/// Add this replica's recorded waits to `admission_lock_waits` (one additive
/// upsert, rows in primary-key order). On failure the counts are kept for the
/// next attempt (bounded by the entry cap). Returns the rows written.
pub async fn flush(store: &crate::store::Store) -> Result<usize, sqlx::Error> {
    let rows = take();
    if rows.is_empty() {
        return Ok(0);
    }
    let mut sorted: Vec<_> = rows.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let payload: Vec<serde_json::Value> = sorted
        .iter()
        .map(|((minute, kind, id), waits)| {
            serde_json::json!({"minute_start": minute, "scope_kind": kind, "scope_id": id, "waits": waits})
        })
        .collect();
    let written = sqlx::query("INSERT INTO admission_lock_waits AS t(minute_start,scope_kind,scope_id,waits) SELECT x.minute_start,x.scope_kind,x.scope_id,x.waits FROM jsonb_to_recordset($1) AS x(minute_start timestamptz,scope_kind text,scope_id uuid,waits bigint[]) ORDER BY 1,2,3 ON CONFLICT(minute_start,scope_kind,scope_id) DO UPDATE SET waits=ARRAY(SELECT a+b FROM unnest(t.waits,EXCLUDED.waits) WITH ORDINALITY u(a,b,i) ORDER BY i)")
        .bind(serde_json::Value::Array(payload))
        .execute(&store.pool)
        .await;
    match written {
        Ok(done) => Ok(done.rows_affected() as usize),
        Err(error) => {
            restore(rows);
            Err(error)
        }
    }
}

/// Delete rows older than [`RETAINED_HOURS`] (bounded batch), fenced to a
/// `maintenance` lease term.
pub async fn prune_fenced(
    store: &crate::store::Store,
    limit: i64,
    fence: Option<&crate::leases::Fence>,
) -> Result<u64, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let pruned = sqlx::query("DELETE FROM admission_lock_waits WHERE (minute_start,scope_kind,scope_id) IN (SELECT minute_start,scope_kind,scope_id FROM admission_lock_waits WHERE minute_start<clock_timestamp()-make_interval(hours=>$1::int) ORDER BY 1,2,3 LIMIT $2 FOR UPDATE SKIP LOCKED)")
        .bind(RETAINED_HOURS as i32)
        .bind(limit)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(pruned)
}

/// Forget unflushed counts (tests).
#[cfg(any(test, feature = "integration-tests"))]
pub fn reset() {
    take();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_fill_every_bound_they_reached() {
        assert_eq!(buckets(Duration::from_millis(9)), None);
        assert_eq!(
            buckets(Duration::from_millis(10)),
            Some([1, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            buckets(Duration::from_millis(120)),
            Some([1, 1, 1, 1, 0, 0, 0, 0])
        );
        assert_eq!(buckets(Duration::from_secs(9)), Some([1; 8]));
    }
}

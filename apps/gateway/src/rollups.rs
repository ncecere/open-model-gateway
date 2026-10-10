//! Hourly usage rollups (migration 0032, scale plan P6).
//!
//! `usage_rollups_hourly` keeps exact integer sums per UTC hour and usage
//! dimension. The leased `rollups` job ([`run_once`], every minute in
//! `serve`) recomputes an hour from raw history only after the hour ended
//! before every running gateway transaction began (and at least an hour
//! ago); any later write to a row of an ended hour appends a
//! `usage_rollup_dirty` marker in its own transaction (0032 triggers), and
//! readers (`omg_usage_rows`) use an hour's rollup only when it is rolled
//! and no marker is visible in their snapshot, aggregating raw rows
//! otherwise. So a rollup-backed answer equals the raw answer in the same
//! snapshot. Readers: usage explore over long ranges (not request counts,
//! which are not additive) and the spend-spike alert baseline.
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Hours recomputed per run at most.
pub const BATCH_HOURS: i64 = 168;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RollupReport {
    /// Hours rolled for the first time.
    pub new_hours: u64,
    /// Rolled hours recomputed because of change markers.
    pub changed_hours: u64,
    /// Rollup groups written.
    pub groups: u64,
    /// Hours still due after this run (time budget or batch limit).
    pub remaining: i64,
    /// Hours ending at or before this instant were eligible.
    pub bound: Option<DateTime<Utc>>,
}

/// Latest hour end the job may roll: an hour ago, and no later than the
/// start of any running transaction (read BEFORE any hour's snapshot is
/// taken, so every transaction that began before the bound has finished).
const BOUND: &str = "SELECT least(date_trunc('hour',clock_timestamp(),'UTC')-interval '1 hour',
 coalesce((SELECT date_trunc('hour',min(xact_start),'UTC') FROM pg_stat_activity
  WHERE xact_start IS NOT NULL AND pid<>pg_backend_pid() AND datname=current_database()
   AND backend_type='client backend'),'infinity'))";

/// Hours with change markers, then never-rolled hours, oldest first.
const DUE: &str = "SELECT h FROM (
  SELECT DISTINCT d.hour_start h FROM usage_rollup_dirty d WHERE d.hour_start+interval '1 hour'<=$1
  UNION
  SELECT g FROM usage_rollup_progress p,
   generate_series(p.rolled_through,$1-interval '1 hour',interval '1 hour') g) x
 ORDER BY h";

/// Roll due hours until `budget` is spent (each hour in its own REPEATABLE
/// READ transaction fenced to the lease term).
pub async fn run_once(
    store: &crate::store::Store,
    fence: Option<&crate::leases::Fence>,
    budget: Duration,
) -> Result<RollupReport, sqlx::Error> {
    let started = Instant::now();
    let pool = &store.pool;
    let bound: DateTime<Utc> = sqlx::query_scalar(BOUND).fetch_one(pool).await?;
    let due: Vec<DateTime<Utc>> = sqlx::query_scalar(&format!("{DUE} LIMIT $2"))
        .bind(bound)
        .bind(BATCH_HOURS + 1)
        .fetch_all(pool)
        .await?;
    let mut report = RollupReport {
        bound: Some(bound),
        ..Default::default()
    };
    let mut done = 0i64;
    for hour in due.iter().take(BATCH_HOURS as usize) {
        if started.elapsed() >= budget {
            break;
        }
        let (groups, changed) = roll_hour(store, *hour, fence).await?;
        report.groups += groups;
        if changed {
            report.changed_hours += 1;
        } else {
            report.new_hours += 1;
        }
        done += 1;
    }
    report.remaining = if due.len() as i64 > BATCH_HOURS {
        // More than one batch was due: count them all.
        sqlx::query_scalar(&format!("SELECT count(*) FROM ({DUE}) d"))
            .bind(bound)
            .fetch_one(pool)
            .await?
    } else {
        due.len() as i64
    } - done;
    crate::metrics::METRICS.observe_rollup_hours("new", report.new_hours);
    crate::metrics::METRICS.observe_rollup_hours("changed", report.changed_hours);
    Ok(report)
}

/// Recompute one hour from raw history. Returns (groups, whether the hour
/// had been rolled before).
pub async fn roll_hour(
    store: &crate::store::Store,
    hour: DateTime<Utc>,
    fence: Option<&crate::leases::Fence>,
) -> Result<(u64, bool), sqlx::Error> {
    let mut tx = crate::db::begin_with_setup(
        &store.pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    )
    .await?;
    crate::leases::fence(&mut tx, fence).await?;
    let existed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM usage_rollup_hours WHERE hour_start=$1)")
            .bind(hour)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM usage_rollups_hourly WHERE hour_start=$1")
        .bind(hour)
        .execute(&mut *tx)
        .await?;
    let groups = sqlx::query("INSERT INTO usage_rollups_hourly SELECT * FROM omg_usage_aggregate($1,$1+interval '1 hour')")
        .bind(hour)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("INSERT INTO usage_rollup_hours(hour_start,rolled_at,groups,lease_epoch) VALUES($1,clock_timestamp(),$2,$3) ON CONFLICT(hour_start) DO UPDATE SET rolled_at=excluded.rolled_at,groups=excluded.groups,lease_epoch=excluded.lease_epoch")
        .bind(hour)
        .bind(groups as i64)
        .bind(fence.map(|f| f.epoch))
        .execute(&mut *tx)
        .await?;
    // Only the markers this snapshot saw: later ones keep the hour raw.
    sqlx::query("DELETE FROM usage_rollup_dirty WHERE hour_start=$1")
        .bind(hour)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE usage_rollup_progress SET rolled_through=$1+interval '1 hour' WHERE rolled_through=$1")
        .bind(hour)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((groups, existed))
}

/// Usage reads choose rollups for ranges of at least this many days.
pub const MIN_DAYS: i64 = 7;

/// How a usage read may use rollups (`GATEWAY_USAGE_ROLLUPS`: `auto`, the
/// default, or `off`). Both give identical answers; `off` always scans raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Off,
    /// Tests: always (also for short ranges).
    Always,
}

#[cfg(any(test, feature = "integration-tests"))]
tokio::task_local! {
    /// Tests: force a mode for the reads of one task (parity checks).
    pub static OVERRIDE: Mode;
}

pub fn mode() -> Mode {
    #[cfg(any(test, feature = "integration-tests"))]
    if let Ok(m) = OVERRIDE.try_with(|m| *m) {
        return m;
    }
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GATEWAY_USAGE_ROLLUPS").as_deref() {
        Ok("off") => Mode::Off,
        _ => Mode::Auto,
    })
}

/// Whether a read over `days` days uses rollups.
pub fn use_rollups(mode: Mode, days: i64) -> bool {
    match mode {
        Mode::Off => false,
        Mode::Always => true,
        Mode::Auto => days >= MIN_DAYS,
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "rollups/tests.rs"]
mod tests;

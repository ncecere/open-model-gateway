//! Hourly usage rollups (0032) against raw history: parity, late changes
//! (change markers), the running-transaction bound, and the spend-spike
//! alert baseline.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::governance::tests::db::fixture;
use crate::partitions::tests::seed_history;
use chrono::TimeDelta;
use sqlx::PgPool;
use uuid::Uuid;

/// Rows of `omg_usage_rows` and of the raw aggregation over `[lo, hi)` that
/// are not in the other (multiset difference, both ways).
async fn differences(pool: &PgPool, lo: DateTime<Utc>, hi: DateTime<Utc>) -> i64 {
    sqlx::query_scalar("SELECT (SELECT count(*) FROM (SELECT * FROM omg_usage_rows($1,$2) EXCEPT ALL SELECT * FROM omg_usage_aggregate($1,$2)) a)+(SELECT count(*) FROM (SELECT * FROM omg_usage_aggregate($1,$2) EXCEPT ALL SELECT * FROM omg_usage_rows($1,$2)) b)")
        .bind(lo)
        .bind(hi)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn hour(pool: &PgPool, back: i64) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT date_trunc('hour',now(),'UTC')-make_interval(hours=>$1::int)")
        .bind(back as i32)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rollups_equal_raw_history_and_follow_late_changes(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let lo = hour(&pool, 72).await;
    let hi = hour(&pool, 2).await;
    seed_history(&pool, &f, "r1-", 4000, lo, (hi - lo).num_seconds(), true).await;
    // Seeded rows of ended hours left change markers (a late write).
    let marks: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT hour_start) FROM usage_rollup_dirty")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(marks >= 60, "{marks}");
    assert_eq!(differences(&pool, lo, hi).await, 0, "raw before rolling");
    let report = run_once(&f.store, None, Duration::from_secs(120))
        .await
        .unwrap();
    assert!(report.new_hours + report.changed_hours >= 70, "{report:?}");
    assert_eq!(report.remaining, 0, "{report:?}");
    let rolled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_rollup_hours WHERE hour_start>=$1 AND hour_start<$2",
    )
    .bind(lo)
    .bind(hi)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rolled, (hi - lo).num_hours());
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_rollup_dirty WHERE hour_start<$1")
            .bind(hi)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(open, 0);
    // Whole and partial ranges equal the raw aggregation exactly.
    for (a, b) in [
        (lo, hi),
        (lo + TimeDelta::minutes(17), hi - TimeDelta::minutes(41)),
        (lo + TimeDelta::hours(5), lo + TimeDelta::hours(6)),
    ] {
        assert_eq!(differences(&pool, a, b).await, 0, "{a}..{b}");
    }
    // Rolled hours really are served from rollups (not raw): the rollup
    // rows of one hour equal its raw aggregation.
    let h = lo + TimeDelta::hours(10);
    let groups: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_rollups_hourly WHERE hour_start=$1")
            .bind(h)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(groups > 0);
    // A late change to a rolled hour (an unknown cost resolved) marks it;
    // readers use raw for it until the job recomputes it.
    let id: Uuid = sqlx::query_scalar("SELECT execution_id FROM governance_reservations WHERE state='unknown' AND admitted_at>=$1 AND admitted_at<$2 ORDER BY 1 LIMIT 1")
        .bind(lo).bind(hi).fetch_one(&pool).await.unwrap();
    let changed_hour: DateTime<Utc> = sqlx::query_scalar("UPDATE governance_reservations SET state='settled',actual_microusd=4242,input_tokens=1,output_tokens=1 WHERE execution_id=$1 RETURNING date_trunc('hour',admitted_at,'UTC')")
        .bind(id).fetch_one(&pool).await.unwrap();
    let marked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_rollup_dirty WHERE hour_start=$1")
            .bind(changed_hour)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marked, 1);
    assert_eq!(
        differences(&pool, lo, hi).await,
        0,
        "a marked hour is read raw"
    );
    let again = run_once(&f.store, None, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!((again.changed_hours, again.new_hours), (1, 0), "{again:?}");
    assert_eq!(differences(&pool, lo, hi).await, 0, "after recomputing");
    // Exact integer sums: the resolved cost is in the rollup.
    let cost: String = sqlx::query_scalar("SELECT sum(known_cost_microusd)::text FROM usage_rollups_hourly WHERE hour_start=$1 AND accounting_state='settled'")
        .bind(changed_hour).fetch_one(&pool).await.unwrap();
    let raw: String = sqlx::query_scalar("SELECT sum(actual_microusd)::text FROM governance_reservations WHERE admitted_at>=$1 AND admitted_at<$1+interval '1 hour' AND state='settled'")
        .bind(changed_hour).fetch_one(&pool).await.unwrap();
    assert_eq!(cost, raw);
    // Unknown is never folded in: unknown attempts keep their own groups.
    let unknown: i64 = sqlx::query_scalar("SELECT coalesce(sum(attempts),0)::bigint FROM omg_usage_rows($1,$2) WHERE accounting_state='unknown' AND known_cost_microusd IS NULL")
        .bind(lo).bind(hi).fetch_one(&pool).await.unwrap();
    let raw_unknown: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE state='unknown' AND admitted_at>=$1 AND admitted_at<$2")
        .bind(lo).bind(hi).fetch_one(&pool).await.unwrap();
    assert_eq!(unknown, raw_unknown);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_bound_waits_for_running_transactions(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    // A transaction that began (here: 0 hours ago) bounds what may be rolled.
    let mut open = pool.begin().await.unwrap();
    let started: DateTime<Utc> =
        sqlx::query_scalar("SELECT date_trunc('hour',transaction_timestamp(),'UTC')")
            .fetch_one(&mut *open)
            .await
            .unwrap();
    let report = run_once(&f.store, None, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(report.bound.unwrap() <= started, "{report:?}");
    open.rollback().await.unwrap();
    // Never the current or the previous hour.
    let report = run_once(&f.store, None, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(report.bound.unwrap() <= hour(&pool, 1).await, "{report:?}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn spike_baseline_from_rollups_equals_the_raw_scan(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let lo = hour(&pool, 200).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    seed_history(
        &pool,
        &f,
        "s1-",
        3000,
        lo,
        (now - lo).num_seconds() - 5,
        true,
    )
    .await;
    run_once(&f.store, None, Duration::from_secs(120))
        .await
        .unwrap();
    for at in [
        now,
        now - TimeDelta::minutes(97),
        now - TimeDelta::hours(30),
    ] {
        let hour_ago = at - TimeDelta::hours(1);
        let start = hour_ago - TimeDelta::hours(crate::alerts::BASELINE_HOURS as i64);
        for ws in [
            None,
            Some(f.principal.workspace_id),
            Some(f.team.workspace_id),
        ] {
            let rolled: (String, String, i64) = sqlx::query_as(crate::alerts::SPIKE)
                .bind(start)
                .bind(hour_ago)
                .bind(at)
                .bind(ws)
                .fetch_one(&pool)
                .await
                .unwrap();
            let raw: (String, String, i64) = sqlx::query_as(crate::alerts::SPIKE_RAW)
                .bind(start)
                .bind(hour_ago)
                .bind(at)
                .bind(ws)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(rolled, raw, "at {at} workspace {ws:?}");
            assert_ne!(raw.1, "0");
        }
    }
}

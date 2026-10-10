//! Operator archival (0033): refusals (retention, pending/unknown cost,
//! unreserved or running executions, incomplete rollups), the checksummed
//! export, detach to omg_archive or drop, `budget verify` with archived
//! contributions, and usage history kept by rollups.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::governance::tests::db::fixture;
use crate::partitions::tests::seed_history;
use sqlx::PgPool;

/// Budget totals against the full scan plus archived contributions. (The
/// test's history lies in future months, whose per-minute rate counters the
/// retained-window rate check would compare with an empty scan.)
async fn consistent(pool: &PgPool) -> bool {
    let mut tx = crate::db::begin(pool).await.unwrap();
    let report = crate::governance::totals::verify_in(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    if report.mismatch_count != 0 {
        eprintln!("{report:#?}");
    }
    report.buckets > 0 && report.mismatch_count == 0
}

fn refusal(e: anyhow::Error) -> Refusal {
    match e.downcast::<Refusal>() {
        Ok(r) => r,
        Err(e) => panic!("not a refusal: {e:#}"),
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn closed_months_archive_only_when_settled_rolled_and_past_retention(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let upper: DateTime<Utc> = sqlx::query_scalar(
        "SELECT legacy_upper FROM history_partitions WHERE parent='inference_executions'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    crate::partitions::ensure_at(&pool, 6, upper).await.unwrap();
    let month = upper + Months::new(1);
    let next = upper + Months::new(2);
    let label = month.format("%Y-%m").to_string();
    seed_history(
        &pool,
        &f,
        "a1-",
        1500,
        month,
        (next - month).num_seconds() - 1,
        true,
    )
    .await;
    // Other history (legacy) must stay untouched.
    seed_history(
        &pool,
        &f,
        "a0-",
        300,
        upper - chrono::TimeDelta::days(20),
        86_400 * 10,
        true,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let req = |drop| Request {
        group: Group::History,
        month: Month::parse(&label).unwrap(),
        to: dir.path().to_path_buf(),
        drop,
        retention_months: 1,
    };
    let later = next + Months::new(1);
    // Within retention (the month after it is still hot).
    let e = archive_as_of(&pool, &req(false), next + chrono::TimeDelta::days(3))
        .await
        .unwrap_err();
    assert_eq!(refusal(e), Refusal::WithinRetention(label.clone(), 1));
    // Pending and unknown reservations keep their holds: refused.
    let e = archive_as_of(&pool, &req(false), later).await.unwrap_err();
    assert!(matches!(refusal(e), Refusal::OpenReservations(n) if n > 0));
    sqlx::raw_sql(&format!("UPDATE inference_executions SET state='succeeded' WHERE state='started' AND started_at>='{month}' AND started_at<'{next}';
      UPDATE governance_reservations SET state='settled',actual_microusd=5,input_tokens=0,output_tokens=0 WHERE state IN ('pending','unknown') AND admitted_at>='{month}' AND admitted_at<'{next}'"))
        .execute(&pool).await.unwrap();
    // Executions without a reservation are unknown cost: refused.
    let e = archive_as_of(&pool, &req(false), later).await.unwrap_err();
    assert!(matches!(refusal(e), Refusal::OpenExecutions(n) if n > 0));
    sqlx::raw_sql(&format!("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,input_tokens,output_tokens)
      SELECT e.id,e.workspace_id,e.api_key_id,e.deployment_id,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,'settled',3,0,0
      FROM inference_executions e WHERE e.started_at>='{month}' AND e.started_at<'{next}' AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)"))
        .execute(&pool).await.unwrap();
    // Hours without a clean rollup: refused (usage history would be lost).
    let e = archive_as_of(&pool, &req(false), later).await.unwrap_err();
    assert!(matches!(refusal(e), Refusal::RollupsIncomplete(n) if n > 0));
    let hours: Vec<DateTime<Utc>> = sqlx::query_scalar(&format!("SELECT DISTINCT date_trunc('hour',started_at,'UTC') FROM inference_executions WHERE started_at>='{month}' AND started_at<'{next}' ORDER BY 1"))
        .fetch_all(&pool).await.unwrap();
    for h in &hours {
        crate::rollups::roll_hour(&f.store, *h, None).await.unwrap();
    }
    assert!(consistent(&pool).await);
    let usage_before: Vec<String> =
        sqlx::query_scalar("SELECT u::text FROM omg_usage_rows($1,$2) u ORDER BY 1")
            .bind(month)
            .bind(next)
            .fetch_all(&pool)
            .await
            .unwrap();
    let others_before: (i64, i64, String) = sqlx::query_as(&format!("SELECT (SELECT count(*) FROM inference_executions WHERE started_at<'{month}'),(SELECT count(*) FROM monetary_ledger WHERE admitted_at<'{month}'),(SELECT coalesce(sum(amount_microusd),0)::text FROM monetary_ledger WHERE admitted_at<'{month}')"))
        .fetch_one(&pool).await.unwrap();
    let (reservations, settled): (i64, String) = sqlx::query_as(&format!("SELECT count(*),sum(actual_microusd)::text FROM governance_reservations WHERE admitted_at>='{month}' AND admitted_at<'{next}'"))
        .fetch_one(&pool).await.unwrap();
    // Archive: exported, recorded, detached into omg_archive.
    let report = archive_as_of(&pool, &req(false), later).await.unwrap();
    assert_eq!(report.files.len(), 3);
    assert_eq!(report.disposition, "detached");
    let manifest = verify_directory(&report.directory).unwrap();
    assert_eq!(manifest["sums"]["reservations"], reservations);
    assert_eq!(manifest["sums"]["settled_microusd"], settled);
    let suffix = month.format("%Y_%m").to_string();
    for table in [
        "inference_executions",
        "governance_reservations",
        "monetary_ledger",
    ] {
        let (public, archived): (bool, bool) = sqlx::query_as(&format!("SELECT to_regclass('public.{table}_p{suffix}') IS NOT NULL,to_regclass('omg_archive.{table}_p{suffix}') IS NOT NULL"))
            .fetch_one(&pool).await.unwrap();
        assert_eq!((public, archived), (false, true), "{table}");
        let rows: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM omg_archive.{table}_p{suffix}"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        let file = report.files.iter().find(|x| x.parent == table).unwrap();
        assert_eq!(rows, file.rows, "{table}");
    }
    let store = crate::store::Store::new(pool.clone());
    assert!(
        store.is_ready().await,
        "detached months live outside public"
    );
    // Maintained totals still reconcile: archived contributions are counted.
    assert!(consistent(&pool).await);
    // Usage of the archived month is still answered (from its rollups).
    let usage_after: Vec<String> =
        sqlx::query_scalar("SELECT u::text FROM omg_usage_rows($1,$2) u ORDER BY 1")
            .bind(month)
            .bind(next)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(usage_before, usage_after);
    // Nothing else changed.
    let others_after: (i64, i64, String) = sqlx::query_as(&format!("SELECT (SELECT count(*) FROM inference_executions WHERE started_at<'{month}'),(SELECT count(*) FROM monetary_ledger WHERE admitted_at<'{month}'),(SELECT coalesce(sum(amount_microusd),0)::text FROM monetary_ledger WHERE admitted_at<'{month}')"))
        .fetch_one(&pool).await.unwrap();
    assert_eq!(others_before, others_after);
    // The archive record is immutable; the month cannot be archived twice.
    assert!(
        sqlx::query("DELETE FROM archived_partitions")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(archive_as_of(&pool, &req(false), later).await.is_err());
    // A tampered export no longer verifies.
    let file = report.directory.join(&report.files[0].file);
    std::fs::write(&file, b"tampered").unwrap();
    assert!(verify_directory(&report.directory).is_err());
    // Audit months archive on their own; --drop removes the detached table.
    let audit = Request {
        group: Group::Audit,
        month: Month::parse(&label).unwrap(),
        to: dir.path().to_path_buf(),
        drop: true,
        retention_months: 1,
    };
    let dropped = archive_as_of(&pool, &audit, later).await.unwrap();
    assert_eq!(dropped.disposition, "dropped");
    let gone: bool = sqlx::query_scalar(&format!("SELECT to_regclass('public.audit_events_p{suffix}') IS NULL AND to_regclass('omg_archive.audit_events_p{suffix}') IS NULL"))
        .fetch_one(&pool).await.unwrap();
    assert!(gone);
    assert!(consistent(&pool).await);
}

#[test]
fn month_and_group_arguments_are_strict() {
    assert_eq!(Month::parse("legacy"), Some(Month::Legacy));
    assert!(Month::parse("2026-10").is_some());
    for bad in ["2026-13", "2026-1", "26-10", "2026_10", "", "legacy2"] {
        assert!(Month::parse(bad).is_none(), "{bad}");
    }
    assert_eq!(Group::parse("history"), Some(Group::History));
    assert!(Group::parse("ledger").is_none());
}

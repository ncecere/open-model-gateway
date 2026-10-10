//! History partitions (0030/0031): month creation ahead of time and at the
//! boundary, fail-closed writes outside every partition, the
//! partitions_missing alert, preflight, partition-key immutability, and the
//! safe conversion of a seeded 0029 database (timed, with the locks held).
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::governance::tests::db::{Fixture, done, fixture, request};
use crate::store::Store;
use chrono::TimeDelta;
use sqlx::{PgPool, migrate::Migrator};

/// Seed `n` attempts of history at `lo + (i * 7919 mod span)` seconds into
/// `[lo, lo + span)`, with a deterministic mix: about 2% running (pending),
/// 5% failed (unknown cost, hold kept), 2% without a reservation, cache
/// usage on every 4th, unreported tokens on some; the rest settled. Works on
/// the 0029 schema (`ledger_admitted`: false) and on partitioned history.
pub(crate) async fn seed_history(
    pool: &PgPool,
    f: &Fixture,
    tag: &str,
    n: i64,
    lo: DateTime<Utc>,
    span_seconds: i64,
    ledger_admitted: bool,
) {
    let (la, lv) = if ledger_admitted {
        (",admitted_at", ",r.admitted_at")
    } else {
        ("", "")
    };
    let sql = format!(
        r#"INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,attempt_number,started_at,completed_at,input_tokens,output_tokens,billing_usage)
SELECT md5($1||i)::uuid,CASE WHEN i%3=0 THEN $3 ELSE $2 END,CASE WHEN i%3=0 THEN $5 ELSE $4 END,$6,
 CASE WHEN i%5=0 THEN 'alias/b' ELSE 'company/smart' END,CASE WHEN i%7=0 THEN 'anthropic' ELSE 'openai' END,false,
 CASE WHEN i%50=1 THEN 'started' WHEN i%20=3 THEN 'failed' ELSE 'succeeded' END,CASE WHEN i%20=3 THEN 'upstream_unavailable' END,
 md5($1||i)::uuid,1,$7::timestamptz+make_interval(secs=>(i*7919)%$8),
 CASE WHEN i%50<>1 THEN $7::timestamptz+make_interval(secs=>(i*7919)%$8)+interval '50 milliseconds' END,
 CASE WHEN i%11=0 OR i%20=3 THEN NULL ELSE 10+i%13 END,CASE WHEN i%13=0 OR i%20=3 THEN NULL ELSE 5+i%7 END,
 CASE WHEN i%4=0 AND i%11<>0 AND i%20<>3 THEN jsonb_build_object('total_input_tokens',(10+i%13)::text,'uncached_input_tokens',(10+i%13-i%3)::text,'cache_read_input_tokens',(i%3)::text,'cache_write_input_tokens','0','cache_write_default_input_tokens',NULL,'cache_write_5m_input_tokens',NULL,'cache_write_1h_input_tokens',NULL) END
FROM generate_series(1,$9::bigint) i;
INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,actual_microusd,input_tokens,output_tokens)
SELECT e.id,e.workspace_id,e.api_key_id,e.deployment_id,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),
 CASE WHEN e.state='started' THEN greatest(e.started_at,now())+interval '1 day' ELSE e.started_at+interval '2 minutes' END,
 CASE e.state WHEN 'started' THEN 'pending' WHEN 'failed' THEN 'unknown' ELSE 'settled' END,100,100,
 CASE WHEN e.state='succeeded' THEN 1+(e.attempt_number*0+abs(hashtext(e.id::text))%97) END,
 CASE WHEN e.state='succeeded' THEN coalesce(e.input_tokens,0) END,CASE WHEN e.state='succeeded' THEN coalesce(e.output_tokens,0) END
FROM inference_executions e WHERE e.root_request_id IN (SELECT md5($1||i)::uuid FROM generate_series(1,$9::bigint) i WHERE i%50<>2);
INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,created_at{la})
SELECT md5('h'||$1||r.execution_id)::uuid,r.execution_id,'hold',100,r.admitted_at{lv} FROM governance_reservations r
 WHERE r.execution_id IN (SELECT md5($1||i)::uuid FROM generate_series(1,$9::bigint) i);
INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens,created_at{la})
SELECT md5('s'||$1||r.execution_id)::uuid,r.execution_id,CASE r.state WHEN 'settled' THEN 'settlement' ELSE 'unknown' END,
 coalesce(r.actual_microusd,100),r.input_tokens,r.output_tokens,r.admitted_at+interval '60 milliseconds'{lv} FROM governance_reservations r
 WHERE r.state<>'pending' AND r.execution_id IN (SELECT md5($1||i)::uuid FROM generate_series(1,$9::bigint) i);"#
    );
    for statement in sql.split(";\n") {
        let statement = statement.trim().trim_end_matches(';');
        if statement.is_empty() {
            continue;
        }
        sqlx::query(statement)
            .bind(tag)
            .bind(f.principal.workspace_id)
            .bind(f.team.workspace_id)
            .bind(f.principal.key_id)
            .bind(f.team.key_id)
            .bind(f.deployment)
            .bind(lo)
            .bind(span_seconds.max(1))
            .bind(n)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn legacy_upper(pool: &PgPool) -> DateTime<Utc> {
    sqlx::query_scalar(
        "SELECT legacy_upper FROM history_partitions WHERE parent='inference_executions'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn partition_of(pool: &PgPool, table: &str, key: &str, id: Uuid) -> String {
    sqlx::query_scalar(&format!(
        "SELECT tableoid::regclass::text FROM {table} WHERE {key}=$1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn assert_consistent(pool: &PgPool, context: &str) {
    let mut tx = crate::db::begin(pool).await.unwrap();
    let report = crate::governance::totals::verify_in(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(report.consistent(), "{context}: {report:#?}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn months_are_created_ahead_and_rows_route_at_the_boundary(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let upper = legacy_upper(&pool).await;
    let month = |n: u32| upper + chrono::Months::new(n);
    let name = |m: DateTime<Utc>| format!("inference_executions_p{}", m.format("%Y_%m"));
    // The migration covered the next three months already.
    let now = coverage(&pool, None).await.unwrap();
    assert!(now.iter().all(|c| c.months_ahead >= 3), "{now:?}");
    assert_eq!(now.len(), 5);
    // One second before the month after the legacy partition: three ahead.
    let at = month(2) - TimeDelta::seconds(1);
    let report = ensure_at(&pool, 4, at).await.unwrap();
    assert_eq!(
        report
            .created
            .iter()
            .filter(|(p, _)| p == "inference_executions")
            .map(|(_, n)| n.clone())
            .collect::<Vec<_>>(),
        vec![name(month(3)), name(month(4)), name(month(5))]
    );
    assert_eq!(report.created.len(), 15, "{report:?}");
    assert!(report.short.is_empty());
    // Ensuring again creates nothing (idempotent).
    assert!(ensure_at(&pool, 4, at).await.unwrap().created.is_empty());
    // Rows route by their own time at the boundary: legacy up to the instant
    // before, the month partition from the instant itself.
    for (at, expected) in [
        (
            upper - TimeDelta::microseconds(1),
            "inference_executions_p_legacy".to_owned(),
        ),
        (upper, name(upper)),
        (month(5) + TimeDelta::days(3), name(month(5))),
    ] {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1,$5)")
            .bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(at)
            .execute(&pool).await.unwrap();
        assert_eq!(
            partition_of(&pool, "inference_executions", "id", id).await,
            expected
        );
    }
    // Beyond every partition a write fails closed (no default partition).
    let id = Uuid::now_v7();
    let err = sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1,$5)")
        .bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(month(6))
        .execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("no partition"), "{err}");
    // Admission, settlement and the ledger at the first instant of a new
    // month land in that month's partitions; totals stay exact.
    f.price(1_000_000).await;
    let pinned = Store::new(pool.clone());
    pinned.pin_admission_clock(upper).unwrap();
    let a = f.start();
    crate::governance::admit(&pinned, &a, &request(), 3600)
        .await
        .unwrap();
    crate::governance::finish(&pinned, &done(a.id, Some(7), Some(3)))
        .await
        .unwrap();
    assert_eq!(
        partition_of(&pool, "inference_executions", "id", a.id).await,
        name(upper)
    );
    assert_eq!(
        partition_of(&pool, "governance_reservations", "execution_id", a.id).await,
        format!("governance_reservations_p{}", upper.format("%Y_%m"))
    );
    let ledger: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT tableoid::regclass::text FROM monetary_ledger WHERE execution_id=$1",
    )
    .bind(a.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        ledger,
        [format!("monetary_ledger_p{}", upper.format("%Y_%m"))]
    );
    assert_consistent(&pool, "month boundary").await;
    // Every new partition has its TRUNCATE guard; history stays immutable.
    assert!(
        sqlx::query(&format!("TRUNCATE {}", name(month(5))))
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE monetary_ledger SET amount_microusd=0")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn missing_future_months_raise_and_clear_the_builtin_alert(pool: PgPool) {
    let upper = legacy_upper(&pool).await;
    // Seen from eight months later, nothing ahead is covered.
    let later = upper + chrono::Months::new(8);
    let coverage_later = coverage(&pool, Some(later)).await.unwrap();
    assert!(
        coverage_later.iter().all(|c| c.months_ahead == -1),
        "{coverage_later:?}"
    );
    let report = EnsureReport {
        short: coverage_later.iter().map(|c| c.parent.clone()).collect(),
        coverage: coverage_later,
        ..Default::default()
    };
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert_eq!(
        reconcile_alert(&mut tx, &report).await.unwrap(),
        (true, false)
    );
    // While open it fires once.
    assert_eq!(
        reconcile_alert(&mut tx, &report).await.unwrap(),
        (false, false)
    );
    tx.commit().await.unwrap();
    let (severity, deliveries): (String, i64) = sqlx::query_as("SELECT e.severity,(SELECT count(*) FROM alert_deliveries d WHERE d.event_id=e.id) FROM alert_events e WHERE e.builtin='partitions_missing' AND e.resolved_at IS NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((severity.as_str(), deliveries), ("critical", 1));
    // Ensuring from then on covers the months and clears the incident.
    let fixed = ensure_at(&pool, 3, later).await.unwrap();
    assert!(fixed.short.is_empty() && fixed.error.is_none(), "{fixed:?}");
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert_eq!(
        reconcile_alert(&mut tx, &fixed).await.unwrap(),
        (false, true)
    );
    tx.commit().await.unwrap();
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_events WHERE builtin='partitions_missing' AND resolved_at IS NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(open, 0);
    // The job itself (no lease outside serve) ensures and reconciles.
    let store = Store::new(pool.clone());
    let job = run_job(&store, 3, None).await.unwrap();
    assert!(job.short.is_empty() && job.error.is_none(), "{job:?}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preflight_accepts_partitions_of_known_parents_only(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(store.is_ready().await);
    ensure(&pool, 6).await.unwrap();
    assert!(
        store.is_ready().await,
        "new month partitions are enterprise relations"
    );
    sqlx::raw_sql("CREATE TABLE stray(x int) PARTITION BY RANGE(x); CREATE TABLE stray_p1 PARTITION OF stray FOR VALUES FROM (1) TO (2)")
        .execute(&pool).await.unwrap();
    assert!(
        !store.is_ready().await,
        "a partition of an unknown parent is not"
    );
    sqlx::raw_sql("DROP TABLE stray")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.is_ready().await);
    // A detached month left in public is refused; moved to omg_archive it is not inspected.
    let last: String = sqlx::query_scalar("SELECT partition FROM omg_partition_bounds('audit_events') ORDER BY upper_bound DESC LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    sqlx::raw_sql(&format!("ALTER TABLE audit_events DETACH PARTITION {last}"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.is_ready().await);
    sqlx::raw_sql(&format!("ALTER TABLE {last} SET SCHEMA omg_archive"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.is_ready().await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn history_never_moves_between_month_partitions(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let upper = legacy_upper(&pool).await;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1,$5)")
        .bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(upper - TimeDelta::days(3))
        .execute(&pool).await.unwrap();
    // Within the partition the owner may correct a time (triggers see the UPDATE).
    sqlx::query(
        "UPDATE inference_executions SET started_at=started_at-interval '1 day' WHERE id=$1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // Across a month boundary it would be a DELETE + INSERT: refused.
    let err = sqlx::query("UPDATE inference_executions SET started_at=$2 WHERE id=$1")
        .bind(id)
        .bind(upper + TimeDelta::days(1))
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("never move"), "{err}");
    assert_consistent(&pool, "moves").await;
}

/// Lock modes on history tables observed during the upgrade: per mode, the
/// longest continuously observed span (ms) and the sampled total.
#[derive(Debug, Default, serde::Serialize)]
struct LockSpans {
    longest_ms: std::collections::BTreeMap<String, u128>,
}

/// The conversion of a seeded 0029 database (`OMG_P6_MIGRATION_ROWS`
/// attempts, default 3000; the P6 measurement used 2,000,000): counts, sums
/// and a hash of the ledger survive, totals verify, and the exclusive
/// locks are metadata-only (reported as JSON on stderr).
#[sqlx::test(migrations = false)]
async fn seeded_history_converts_without_exclusive_rewrites(test_pool: PgPool) {
    // The disposable test cluster's container has a 64 MB /dev/shm: no
    // parallel workers (serial index builds and scans; a conservative timing).
    let options = (*test_pool.connect_options()).clone().options([
        ("max_parallel_workers_per_gather", "0"),
        ("max_parallel_maintenance_workers", "0"),
    ]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .unwrap();
    let rows: i64 = std::env::var("OMG_P6_MIGRATION_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version <= 29)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    let f = fixture(pool.clone()).await;
    let seeded = std::time::Instant::now();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let chunk = 200_000i64;
    let mut done_rows = 0;
    while done_rows < rows {
        let n = chunk.min(rows - done_rows);
        seed_history(
            &pool,
            &f,
            &format!("m{done_rows}-"),
            n,
            now - TimeDelta::days(120),
            120 * 86_400 - 600,
            false,
        )
        .await;
        done_rows += n;
    }
    let seed_seconds = seeded.elapsed().as_secs_f64();
    sqlx::raw_sql("INSERT INTO audit_events(id,action,resource_type,created_at) SELECT gen_random_uuid(),'probe.seeded','installation',now()-make_interval(secs=>i) FROM generate_series(1,1000) i; ANALYZE")
        .execute(&pool).await.unwrap();
    const STATS: &str = "SELECT concat_ws(',',(SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),
      (SELECT count(*) FROM monetary_ledger),(SELECT coalesce(sum(amount_microusd),0) FROM monetary_ledger),
      (SELECT coalesce(sum(actual_microusd),0) FROM governance_reservations),(SELECT count(*) FROM audit_events),
      (SELECT sum(('x'||substr(md5(ROW(l.id,l.execution_id,l.kind,l.amount_microusd,l.created_at)::text),1,15))::bit(60)::bigint::numeric) FROM monetary_ledger l))";
    let stats_before: String = sqlx::query_scalar(STATS).fetch_one(&pool).await.unwrap();
    // Sample locks on history tables and probe reads while migrating.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (pool, stop) = (pool.clone(), stop.clone());
        tokio::spawn(async move {
            let mut spans = LockSpans::default();
            let mut open: std::collections::BTreeMap<String, std::time::Instant> =
                Default::default();
            let mut read_max_ms = 0u128;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let held: Vec<String> = sqlx::query_scalar("SELECT DISTINCT c.relname||':'||l.mode FROM pg_locks l JOIN pg_class c ON c.oid=l.relation WHERE l.granted AND l.pid<>pg_backend_pid() AND l.database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND c.relname ~ '^(inference_executions|governance_reservations|monetary_ledger|audit_events|storage_usage_hours)' AND l.mode IN ('AccessExclusiveLock','ExclusiveLock','ShareLock','ShareRowExclusiveLock')")
                    .fetch_all(&pool).await.unwrap_or_default();
                let at = std::time::Instant::now();
                for h in &held {
                    open.entry(h.clone()).or_insert(at);
                }
                open.retain(|k, since| {
                    let keep = held.contains(k);
                    let mode = k.split(':').nth(1).unwrap_or_default().to_owned();
                    let span = since.elapsed().as_millis();
                    let e = spans.longest_ms.entry(mode).or_insert(0);
                    *e = (*e).max(span);
                    keep
                });
                // A reader of recent history (what a report would do).
                let t = std::time::Instant::now();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(120), sqlx::query("SELECT count(*) FROM governance_reservations WHERE admitted_at>now()-interval '1 hour'").execute(&pool)).await;
                read_max_ms = read_max_ms.max(t.elapsed().as_millis());
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            (spans, read_max_ms)
        })
    };
    let store = Store::new(pool.clone());
    let started = std::time::Instant::now();
    store.migrate_enterprise().await.unwrap();
    let migrate_seconds = started.elapsed().as_secs_f64();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (spans, read_max_ms) = sampler.await.unwrap();
    assert!(store.is_ready().await);
    let steps: Vec<(i64, i64)> = sqlx::query_as("SELECT version,execution_time/1000000 FROM _sqlx_migrations WHERE version>=30 ORDER BY version")
        .fetch_all(&pool).await.unwrap();
    let stats_after: String = sqlx::query_scalar(STATS).fetch_one(&pool).await.unwrap();
    assert_eq!(
        stats_before, stats_after,
        "history changed during conversion"
    );
    let kinds: Vec<(String, String)> = sqlx::query_as("SELECT relname,relkind::text FROM pg_class WHERE relname IN ('inference_executions','governance_reservations','monetary_ledger','audit_events','storage_usage_hours') ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert!(kinds.iter().all(|(_, k)| k == "p"), "{kinds:?}");
    let verified = std::time::Instant::now();
    assert_consistent(&pool, "after conversion").await;
    let verify_seconds = verified.elapsed().as_secs_f64();
    // Triggers, totals and counters keep working on the partitioned tables.
    f.price(1_000_000).await;
    let a = f.start();
    crate::governance::admit(&f.store, &a, &request(), 60)
        .await
        .unwrap();
    crate::governance::finish(&f.store, &done(a.id, Some(10), Some(5)))
        .await
        .unwrap();
    assert_consistent(&pool, "admission after conversion").await;
    eprintln!(
        "P6 migration measurement: {}",
        serde_json::json!({
            "attempts": rows, "seed_seconds": seed_seconds, "migrate_seconds": migrate_seconds,
            "migration_ms": steps, "longest_lock_ms_by_mode": spans.longest_ms,
            "max_reader_ms": read_max_ms, "budget_verify_seconds": verify_seconds,
        })
    );
    f.store.pool.close().await;
    pool.close().await;
}

/// `partitions prepare` builds the 0030/0031 keys online on a 0029 database
/// (CREATE INDEX CONCURRENTLY, an interrupted invalid build is replaced); the
/// upgrade then adopts them instead of building.
#[sqlx::test(migrations = false)]
async fn prepared_keys_are_adopted_by_the_upgrade(pool: PgPool) {
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::store::MIGRATOR
                .iter()
                .filter(|m| m.version <= 29)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    let f = fixture(pool.clone()).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    seed_history(
        &pool,
        &f,
        "p-",
        500,
        now - TimeDelta::days(40),
        40 * 86_400 - 600,
        false,
    )
    .await;
    // An invalid leftover of an interrupted concurrent build.
    sqlx::raw_sql("CREATE UNIQUE INDEX audit_events_p_legacy_pkey ON audit_events(id,created_at); UPDATE pg_index SET indisvalid=false WHERE indexrelid='audit_events_p_legacy_pkey'::regclass")
        .execute(&pool).await.unwrap();
    let store = Store::new(pool.clone());
    store.preflight_upgrade().await.unwrap();
    let built = prepare(&pool).await.unwrap();
    assert_eq!(built.len(), PREPARED_INDEXES.len(), "{built:?}");
    let invalid: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname LIKE '%p\\_legacy%' AND NOT i.indisvalid")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(invalid, 0);
    assert!(prepare(&pool).await.unwrap().is_empty(), "idempotent");
    let pkey: String = sqlx::query_scalar(
        "SELECT relfilenode::text FROM pg_class WHERE relname='inference_executions_p_legacy_pkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    store.migrate_enterprise().await.unwrap();
    assert!(store.is_ready().await);
    let after: String = sqlx::query_scalar(
        "SELECT relfilenode::text FROM pg_class WHERE relname='inference_executions_p_legacy_pkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, pkey, "the prebuilt key was adopted, not rebuilt");
    assert_consistent(&pool, "after prepared upgrade").await;
    // Nothing to prepare once partitioned.
    assert!(prepare(&pool).await.unwrap().is_empty());
}

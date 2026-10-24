//! Real PostgreSQL count regressions and the opt-in retained-history plan gate.
use super::*;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

const TERMINAL: [&str; 4] = ["sent", "failed", "exhausted", "ineligible"];

async fn destination(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('metrics','https://example.test/hook','env:HOOK',true) RETURNING id")
        .fetch_one(pool).await.unwrap()
}

/// Owner-only fixtures use real guards and canonical envelopes, never disable
/// triggers or rewrite immutable history. Test deliveries avoid unrelated events.
async fn terminal_rows(pool: &PgPool, dest: Uuid, n: i32, state: &str, age: &str) {
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload,state,created_at,completed_at) SELECT i,d.id,d.enable_generation,true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8'),$3,clock_timestamp()-interval '48 hours',clock_timestamp()-$4::interval FROM (SELECT gen_random_uuid() i FROM generate_series(1,$2)) ids CROSS JOIN alert_webhook_destinations d WHERE d.id=$1")
        .bind(dest).bind(n).bind(state).bind(age).execute(pool).await.unwrap();
}

async fn small_fixture(pool: &PgPool) -> Uuid {
    let dest = destination(pool).await;
    // A real late enqueue: management cannot see these two rows when disabling
    // and re-enabling. Waiting must be excluded, but an already leased row must
    // still be counted even though its generation no longer matches.
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload,created_at) SELECT i,$1,0,true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8'),clock_timestamp()-interval '48 hours' FROM (SELECT gen_random_uuid() i FROM generate_series(1,2)) ids")
        .bind(dest).execute(&mut *enqueue).await.unwrap();
    sqlx::query("UPDATE alert_webhook_outbox SET state='leased',attempt_count=1,lease_owner=gen_random_uuid(),lease_token=gen_random_uuid(),lease_expires_at=clock_timestamp()+interval '90 seconds' WHERE id=(SELECT id FROM alert_webhook_outbox ORDER BY id LIMIT 1)")
        .execute(&mut *enqueue).await.unwrap();
    for enabled in [false, true] {
        sqlx::query("UPDATE alert_webhook_destinations SET enabled=$2 WHERE id=$1")
            .bind(dest)
            .bind(enabled)
            .execute(pool)
            .await
            .unwrap();
    }
    enqueue.commit().await.unwrap();
    // Active counts are not restricted to the last 24h. The stale 48h row
    // must not contribute either a count or the oldest-waiting age.
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload,created_at) SELECT i,d.id,d.enable_generation,true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8'),clock_timestamp()-age FROM (SELECT gen_random_uuid() i,age FROM unnest(ARRAY[interval '2 hours',interval '25 hours']) age) ids CROSS JOIN alert_webhook_destinations d WHERE d.id=$1")
        .bind(dest).execute(pool).await.unwrap();
    for (i, state) in TERMINAL.iter().enumerate() {
        // Recent completion of old work counts, not its enqueue time.
        terminal_rows(pool, dest, (i + 1) as i32, state, "1 hour").await;
        terminal_rows(pool, dest, 1, state, "25 hours").await;
    }
    dest
}

async fn assert_counts(pool: &PgPool) {
    let mut rows: Vec<(String, i64, i64)> = sqlx::query_as(ALERT_WEBHOOK_COUNTS)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.sort();
    let counts: Vec<(&str, i64)> = rows
        .iter()
        .map(|(state, n, _)| (state.as_str(), *n))
        .collect();
    assert_eq!(
        counts,
        [
            ("exhausted", 3),
            ("failed", 2),
            ("ineligible", 4),
            ("leased", 1),
            ("sent", 1),
            ("waiting", 2)
        ]
    );
    for (state, _, age) in rows {
        if state == "waiting" {
            assert!(
                (90000..172800).contains(&age),
                "stale generation must not set oldest age: {age}"
            );
        } else {
            assert_eq!(age, 0, "{state}");
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_metrics_exact_counts_generations_and_lease_visibility(pool: PgPool) {
    let store = Store::new(pool.clone());
    let m = Metrics::new();
    let empty = m.render(Some(&store)).await;
    for state in [
        "waiting",
        "leased",
        "sent",
        "failed",
        "exhausted",
        "ineligible",
    ] {
        assert!(
            empty.contains(&format!(
                "gateway_alert_webhook_deliveries{{state=\"{state}\"}} 0"
            )),
            "{empty}"
        );
    }
    assert!(empty.contains("gateway_alert_webhook_oldest_waiting_seconds{} 0"));
    let dest = small_fixture(&pool).await;
    assert_counts(&pool).await;
    // A new collector bypasses only the normal 15s refresh cache, not the SQL.
    let m = Metrics::new();
    let scrape = m.render(Some(&store)).await;
    for (state, count) in [
        ("waiting", 2),
        ("leased", 1),
        ("sent", 1),
        ("failed", 2),
        ("exhausted", 3),
        ("ineligible", 4),
    ] {
        assert!(
            scrape.contains(&format!(
                "gateway_alert_webhook_deliveries{{state=\"{state}\"}} {count}"
            )),
            "{scrape}"
        );
    }
    assert!(
        !scrape.contains("gateway_metrics_collection_errors_total{collector=\"alert_webhook\"}")
    );
    assert!(!scrape.contains(&dest.to_string()));
    assert!(!scrape.contains("example.test"));
    assert!(!scrape.contains("env:HOOK"));
    let unleased = store
        .clone()
        .with_leases(std::sync::Arc::new(crate::leases::Leases::new()));
    let scrape = m.render(Some(&unleased)).await;
    assert!(
        !scrape
            .lines()
            .any(|line| line.starts_with("gateway_alert_webhook_deliveries{"))
    );
    assert!(
        !scrape
            .lines()
            .any(|line| line.starts_with("gateway_alert_webhook_oldest_waiting_seconds{"))
    );
    // Losing the lease also invalidates the refresh cache.
    let scrape = m.render(Some(&store)).await;
    assert!(scrape.contains("gateway_alert_webhook_deliveries{state=\"waiting\"} 2"));
}

/// Sum plan-node tuples (including discarded tuples), weighted by loops. This
/// deliberately overcounts parent/child output: it is a conservative work bound,
/// not the final aggregate's six result rows. Buffers below are root-only since
/// PostgreSQL already includes descendant buffers in the parent totals.
fn tuples_visited(node: &Value) -> f64 {
    let n = |key: &str| node[key].as_f64().unwrap_or(0.0);
    (n("Actual Rows") + n("Rows Removed by Filter") + n("Rows Removed by Index Recheck"))
        * n("Actual Loops")
        + node["Plans"]
            .as_array()
            .map_or(0.0, |children| children.iter().map(tuples_visited).sum())
}

async fn explain(pool: &PgPool, history: i32) -> (u64, f64) {
    // Do not force a planner strategy or VACUUM into an index-only best case.
    // Fresh append-only rows need a bounded plan even before visibility-map work.
    sqlx::raw_sql("ANALYZE alert_webhook_outbox; ANALYZE alert_webhook_destinations")
        .execute(pool)
        .await
        .unwrap();
    let plan: Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {ALERT_WEBHOOK_COUNTS}"
    ))
    .fetch_one(pool)
    .await
    .unwrap();
    let root = &plan[0]["Plan"];
    let buffers =
        root["Shared Hit Blocks"].as_u64().unwrap() + root["Shared Read Blocks"].as_u64().unwrap();
    let tuples = tuples_visited(root);
    println!(
        "retained_old_terminal_rows={history}; shared_hit_plus_read={buffers}; plan_tuples_visited={tuples}; execution_ms={}; SQL:\n{ALERT_WEBHOOK_COUNTS}\nEXPLAIN (ANALYZE, BUFFERS, FORMAT JSON):\n{}",
        plan[0]["Execution Time"],
        serde_json::to_string_pretty(&plan).unwrap()
    );
    (buffers, tuples)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "seeds 1M retained terminal rows; PostgreSQL 17 plan gate, run with --ignored --nocapture"]
async fn webhook_metrics_retained_history_work_is_bounded(pool: PgPool) {
    let server: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("PostgreSQL {server}; exact collector plan, default planner settings");
    let dest = small_fixture(&pool).await;
    let mut measurements = Vec::new();
    for (total, additional) in [(200_000, 200_000), (1_000_000, 800_000)] {
        for state in TERMINAL {
            terminal_rows(&pool, dest, additional / 4, state, "30 days").await;
        }
        // Print BOTH plans before asserting the bound, including on the old SQL.
        measurements.push(explain(&pool, total).await);
        assert_counts(&pool).await;
    }
    for (buffers, tuples) in &measurements {
        assert!(
            *buffers <= 1024,
            "collector read {buffers} buffers with only 3 active and 10 recent terminal rows"
        );
        assert!(
            *tuples <= 1024.0,
            "collector visited {tuples} plan tuples with fixed visible data"
        );
    }
    assert!(
        measurements[1].0 <= measurements[0].0 + 128,
        "5x old history increased buffer work: {measurements:?}"
    );
    assert!(
        measurements[1].1 <= measurements[0].1 + 128.0,
        "5x old history increased tuple work: {measurements:?}"
    );
    // Also exercise the actual 2-second collector path: timeout/failure must not
    // look like zero or a successful scrape with missing webhook series.
    let scrape = Metrics::new().render(Some(&Store::new(pool))).await;
    assert!(
        scrape.contains("gateway_alert_webhook_deliveries{state=\"waiting\"} 2"),
        "{scrape}"
    );
    assert!(
        !scrape.contains("gateway_metrics_collection_errors_total{collector=\"alert_webhook\"}"),
        "{scrape}"
    );
}

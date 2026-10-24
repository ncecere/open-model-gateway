//! Fair dispatch on the real recipient outbox and local SMTP path.
use super::*;

// One external recipient per delivery, independent of workspace memberships.
// Intentionally uses the pre-0055 INSERT shape (no bucket_id).
async fn backlog(s: &Seed, workspace: Option<Uuid>, n: i32, age_seconds: i32) {
    let scope = workspace.map_or("'installation'".to_owned(), |ws| {
        format!("'workspace','{ws}'")
    });
    let columns = if workspace.is_some() {
        "scope,workspace_id,kind,budget_layers,thresholds,notify_emails"
    } else {
        "scope,kind,budget_layers,thresholds,notify_emails"
    };
    let r = rule(
        s,
        columns,
        &format!("{scope},'budget_threshold',ARRAY['local'],ARRAY[100],ARRAY['fair@example.test']"),
    )
    .await;
    sqlx::query("WITH events AS (INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) SELECT gen_random_uuid(),$1,'budget_threshold',i::text,100,'warning',$2,'Fair dispatch' FROM generate_series(1,$3) i RETURNING id) INSERT INTO alert_deliveries(id,event_id,transition,created_at) SELECT gen_random_uuid(),id,'fired',clock_timestamp()-make_interval(secs=>$4) FROM events")
        .bind(r).bind(workspace).bind(n).bind(age_seconds).execute(&s.pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn fair_dispatch_upgrade_backfills_only_pending_and_recovers_index(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= 54).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let s = seed(pool).await;
    backlog(&s, Some(s.team), 2, 60).await;
    sqlx::query("UPDATE alert_deliveries SET status='not_configured',completed_at=clock_timestamp() WHERE id=(SELECT id FROM alert_deliveries ORDER BY id LIMIT 1)")
        .execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    let buckets: Vec<(String, Uuid)> =
        sqlx::query_as("SELECT status,bucket_id FROM alert_deliveries ORDER BY status")
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        buckets,
        vec![
            ("not_configured".into(), Uuid::nil()),
            ("pending".into(), s.team)
        ]
    );
    // Simulate an interrupted concurrent build (0056 not recorded, invalid index).
    sqlx::raw_sql("DELETE FROM _sqlx_migrations WHERE version=56; UPDATE pg_index SET indisvalid=false WHERE indexrelid='public.alert_deliveries_pending_bucket'::regclass")
        .execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    let valid: bool = sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid='public.alert_deliveries_pending_bucket'::regclass")
        .fetch_one(&s.pool).await.unwrap();
    assert!(valid);
    assert!(s.store.is_ready().await);
}

#[sqlx::test(migrations = false)]
async fn fair_dispatch_bucket_migration_resets_lock_timeout_before_concurrent_index(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    let mut connection = pool.acquire().await.unwrap();
    let default: String = sqlx::query_scalar("SHOW lock_timeout")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= 55).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&mut *connection)
    .await
    .unwrap();
    let actual: String = sqlx::query_scalar("SHOW lock_timeout")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(
        actual, default,
        "0056 must not inherit the ALTER lock timeout"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_fresh_bucket_survives_many_backlogged_buckets_across_ticks(pool: PgPool) {
    let s = seed(pool).await;
    for _ in 0..30 {
        backlog(&s, Some(workspace(&s).await), 200, 3600).await;
    }
    let fresh = workspace(&s).await;
    backlog(&s, Some(fresh), 1, 60).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 25);
    assert_eq!(deliver_pending(&s.store, 25).await, 25);
    let done: bool = sqlx::query_scalar(
        "SELECT status='not_configured' FROM alert_deliveries WHERE bucket_id=$1",
    )
    .bind(fresh)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert!(
        done,
        "fresh work must get a slot by the second tick, not after old backlogs drain"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_short_ticks_rotate_backlogged_buckets(pool: PgPool) {
    let s = seed(pool).await;
    let mut buckets = Vec::new();
    for i in 0..6 {
        let ws = workspace(&s).await;
        buckets.push(ws);
        backlog(&s, Some(ws), 20, 3600 - i * 60).await;
    }
    for slots in [1, 2, 1, 2] {
        assert_eq!(deliver_pending(&s.store, slots).await, slots as usize);
    }
    let touched: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT bucket_id FROM alert_deliveries WHERE status='not_configured' ORDER BY bucket_id")
        .fetch_all(&s.pool).await.unwrap();
    buckets.sort();
    assert_eq!(
        touched, buckets,
        "1–2 claim ticks must rotate instead of restarting at the oldest bucket"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_send_budget_rotates_buckets_after_slow_sends(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx, gate) = mock::serve_gated().await;
    relay(&s, port).await;
    for i in 0..3 {
        backlog(&s, Some(workspace(&s).await), 20, 3600 - i * 60).await;
    }
    for _ in 0..3 {
        let delivery = outbox::deliver_pending_within(
            &s.store,
            25,
            outbox::SEND_DEADLINE + Duration::from_secs(1),
        );
        let release = async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            // Exhaust the extra budget after DATA, not via timing the database.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            gate.add_permits(1);
        };
        let (touched, ()) = tokio::join!(delivery, release);
        assert_eq!(
            touched, 1,
            "the budget must stop each tick after its slow send"
        );
    }
    assert_eq!(
        count(
            &s,
            "SELECT count(DISTINCT bucket_id) FROM alert_deliveries WHERE status='sent'"
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_expired_backoff_prefix_has_bounded_plan_work(pool: PgPool) {
    let s = seed(pool).await;
    backlog(&s, Some(s.team), 30000, 90000).await;
    // Expired work may remain after bounded cleanup; recipients can also be
    // backing off. Include a small live backoff prefix before the due head.
    backlog(&s, Some(s.team), 16, 3600).await;
    sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint,next_attempt_at) SELECT id,sha256(uuid_send(id)),clock_timestamp()+interval '1 hour' FROM alert_deliveries")
        .execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE alert_deliveries SET initialized=true")
        .execute(&s.pool)
        .await
        .unwrap();
    backlog(&s, Some(s.team), 1, 60).await;
    sqlx::raw_sql("ANALYZE alert_deliveries; ANALYZE alert_recipient_outbox")
        .execute(&s.pool)
        .await
        .unwrap();
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, FORMAT JSON) {}",
        outbox::bucket_candidates_sql()
    ))
    .bind(Vec::<Uuid>::new())
    .bind(Vec::<i32>::new())
    .bind(Vec::<Uuid>::new())
    .bind(Vec::<i64>::new())
    .bind(expiry_now(&s).await)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    fn work(node: &serde_json::Value) -> f64 {
        let loops = node["Actual Loops"].as_f64().unwrap_or(0.0);
        let rows = [
            "Actual Rows",
            "Rows Removed by Filter",
            "Rows Removed by Index Recheck",
        ]
        .iter()
        .map(|key| node[key].as_f64().unwrap_or(0.0))
        .sum::<f64>();
        loops * rows
            + node["Plans"]
                .as_array()
                .map_or(0.0, |children| children.iter().map(work).sum())
    }
    assert!(
        work(&plan[0]["Plan"]) < 500.0,
        "claim must skip the expired range, not visit 30k old recipients: {plan}"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_cancelled_claim_ends_tick_after_cleanup_but_other_errors_fail(pool: PgPool) {
    use std::io::Write;
    use tracing::instrument::WithSubscriber;
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let s = seed(pool).await;
    backlog(&s, Some(s.team), 1, 90000).await;
    backlog(&s, Some(s.team), 2, 60).await;
    // A real database cancellation, confined to this transaction's backend.
    sqlx::raw_sql("CREATE FUNCTION cancel_alert_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='not_configured' THEN PERFORM pg_cancel_backend(pg_backend_pid()); PERFORM pg_sleep(0.01); END IF; RETURN NEW; END $$; CREATE TRIGGER cancel_alert_claim BEFORE UPDATE ON alert_deliveries FOR EACH ROW EXECUTE FUNCTION cancel_alert_claim()")
        .execute(&s.pool).await.unwrap();
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    assert_eq!(
        deliver_pending(&s.store, 25)
            .with_subscriber(subscriber)
            .await,
        0
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE error='interrupted'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='pending'"
        )
        .await,
        2
    );
    let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("alert outbox claim timed out; ending dispatch tick"),
        "{output}"
    );
    assert!(!output.contains("alert outbox unavailable"), "{output}");
    // An unrelated SQL failure must retain the existing failure path, not be
    // classified as an empty queue or a statement timeout.
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION cancel_alert_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test claim failure'; END $$")
        .execute(&s.pool).await.unwrap();
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    assert_eq!(
        deliver_pending(&s.store, 25)
            .with_subscriber(subscriber)
            .await,
        0
    );
    let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("alert outbox unavailable"), "{output}");
    assert!(!output.contains("alert outbox claim timed out"), "{output}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_history_is_capped_and_evicts_oldest_not_refreshed_bucket(pool: PgPool) {
    let s = seed(pool).await;
    let old = std::time::Instant::now() - Duration::from_secs(10);
    let evicted = Uuid::new_v4();
    {
        let mut history = s.store.alert_dispatch.lock().unwrap();
        history.insert(s.team, old - Duration::from_secs(1));
        history.insert(evicted, old);
        for _ in 2..outbox::RECENT_BUCKETS {
            history.insert(Uuid::new_v4(), old + Duration::from_secs(1));
        }
    }
    backlog(&s, Some(s.team), 1, 60).await;
    assert_eq!(deliver_pending(&s.store, 1).await, 1);
    assert_eq!(
        s.store.alert_dispatch.lock().unwrap().len(),
        outbox::RECENT_BUCKETS
    );
    let fresh = workspace(&s).await;
    backlog(&s, Some(fresh), 1, 60).await;
    assert_eq!(deliver_pending(&s.store, 1).await, 1);
    let history = s.store.alert_dispatch.lock().unwrap();
    assert_eq!(history.len(), outbox::RECENT_BUCKETS);
    assert!(!history.contains_key(&evicted));
    assert!(history.contains_key(&s.team));
    assert!(history.contains_key(&fresh));
}

async fn workspace(s: &Seed) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Fair dispatch','team')")
        .bind(id)
        .execute(&s.pool)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_quiet_workspaces_get_slots_in_first_tick(pool: PgPool) {
    let s = seed(pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    let b = workspace(&s).await;
    let c = workspace(&s).await;
    backlog(&s, Some(s.team), 200, 3600).await;
    backlog(&s, Some(b), 1, 1800).await;
    backlog(&s, Some(c), 1, 900).await;
    assert_eq!(deliver_pending(&s.store, 100).await, 25);
    let sent: Vec<(Uuid, i64)> = sqlx::query_as("SELECT e.workspace_id,count(*) FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE d.status='sent' GROUP BY e.workspace_id ORDER BY e.workspace_id")
        .fetch_all(&s.pool).await.unwrap();
    let mut expected = vec![(s.team, 23), (b, 1), (c, 1)];
    expected.sort();
    assert_eq!(
        sent, expected,
        "a noisy workspace must not consume the quiet buckets' slots"
    );
    let mut received = 0;
    while rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, 25);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_forty_buckets_drain_oldest_first_in_two_ticks(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    let mut ordered = Vec::new();
    for i in 0..40 {
        let ws = workspace(&s).await;
        ordered.push(ws);
        backlog(&s, Some(ws), 1, 4000 - i * 60).await;
    }
    assert_eq!(deliver_pending(&s.store, 25).await, 25);
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT e.workspace_id FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE d.status='pending' ORDER BY d.created_at,d.id")
        .fetch_all(&s.pool).await.unwrap();
    assert_eq!(pending, ordered[25..]);
    assert_eq!(deliver_pending(&s.store, 25).await, 15);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='sent'"
        )
        .await,
        40
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_installation_bucket_and_legacy_insert(pool: PgPool) {
    let s = seed(pool).await;
    let (port, _rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    backlog(&s, Some(s.team), 1, 60).await;
    backlog(&s, None, 1, 30).await;
    let buckets: Vec<(Option<Uuid>, Uuid)> = sqlx::query_as("SELECT e.workspace_id,d.bucket_id FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id ORDER BY e.workspace_id NULLS LAST")
        .fetch_all(&s.pool).await.unwrap();
    assert_eq!(buckets, vec![(Some(s.team), s.team), (None, Uuid::nil())]);
    assert_eq!(deliver_pending(&s.store, 25).await, 2);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='sent'"
        )
        .await,
        2
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_not_configured_steps_also_share_slots(pool: PgPool) {
    let s = seed(pool).await;
    let quiet = workspace(&s).await;
    backlog(&s, Some(s.team), 200, 3600).await;
    backlog(&s, Some(quiet), 1, 60).await;
    assert_eq!(deliver_pending(&s.store, 25).await, 25);
    let quiet_done: bool = sqlx::query_scalar(
        "SELECT status='not_configured' FROM alert_deliveries WHERE bucket_id=$1",
    )
    .bind(quiet)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert!(quiet_done);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_locked_bucket_does_not_block_other_candidates(pool: PgPool) {
    let s = seed(pool).await;
    relay(&s, 25).await;
    backlog(&s, Some(s.team), 1, 3600).await;
    let quiet = workspace(&s).await;
    backlog(&s, Some(quiet), 1, 60).await;
    let mut locked = s.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM alert_deliveries WHERE bucket_id=$1 FOR UPDATE")
        .bind(s.team)
        .execute(&mut *locked)
        .await
        .unwrap();
    let (_, claim, bucket) = tokio::time::timeout(
        Duration::from_secs(3),
        outbox::claim(&s.store, Uuid::new_v4(), &Default::default()),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(bucket, quiet);
    assert!(claim.is_some());
    locked.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_bucket_index_is_usable_for_large_backlog(pool: PgPool) {
    let s = seed(pool).await;
    backlog(&s, Some(s.team), 30000, 3600).await;
    backlog(&s, Some(workspace(&s).await), 1, 60).await;
    sqlx::query("ANALYZE alert_deliveries")
        .execute(&s.pool)
        .await
        .unwrap();
    let mut tx = s.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: Vec<String> =
        sqlx::query_scalar(&format!("EXPLAIN {}", outbox::bucket_candidates_sql()))
            .bind(Vec::<Uuid>::new())
            .bind(Vec::<i32>::new())
            .bind(Vec::<Uuid>::new())
            .bind(Vec::<i64>::new())
            .bind(expiry_now(&s).await)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let plan = plan.join("\n");
    assert!(plan.contains("alert_deliveries_pending_bucket"), "{plan}");
    assert!(!plan.contains("Seq Scan on alert_deliveries"), "{plan}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_dispatch_concurrent_claimers_never_duplicate_lease(pool: PgPool) {
    let s = seed(pool).await;
    relay(&s, 25).await;
    for _ in 0..8 {
        backlog(&s, Some(workspace(&s).await), 1, 60).await;
    }
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let store = s.store.clone();
        workers.spawn(async move {
            let mut claimed = Vec::new();
            while let Some((id, claim, _)) =
                outbox::claim(&store, Uuid::new_v4(), &Default::default())
                    .await
                    .unwrap()
            {
                assert!(claim.is_some());
                claimed.push(id);
            }
            claimed
        });
    }
    let mut ids = Vec::new();
    while let Some(result) = workers.join_next().await {
        ids.extend(result.unwrap());
    }
    assert_eq!(ids.len(), 8);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_recipient_outbox WHERE state='leased' AND attempt_count=1"
        )
        .await,
        8
    );
}

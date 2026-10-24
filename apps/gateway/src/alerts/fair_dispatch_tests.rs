//! Fair dispatch on the real recipient outbox and local SMTP path.
use super::*;

// One external recipient per delivery, independent of workspace memberships.
// Intentionally uses the pre-0056 INSERT shape (no bucket_id).
async fn backlog(s: &Seed, workspace: Option<Uuid>, n: i32, age_seconds: i32) {
    let scope = workspace.map_or("'installation'".to_owned(), |ws| {
        format!("'workspace','{ws}'")
    });
    let columns = if workspace.is_some() {
        "scope,workspace_id,kind,budget_layers,thresholds,notify_emails"
    } else {
        "scope,kind,budget_layers,thresholds,notify_emails"
    };
    let r = rule(s, columns, &format!("{scope},'budget_threshold',ARRAY['local'],ARRAY[100],ARRAY['fair@example.test']")).await;
    sqlx::query("WITH events AS (INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) SELECT gen_random_uuid(),$1,'budget_threshold',i::text,100,'warning',$2,'Fair dispatch' FROM generate_series(1,$3) i RETURNING id) INSERT INTO alert_deliveries(id,event_id,transition,created_at) SELECT gen_random_uuid(),id,'fired',clock_timestamp()-make_interval(secs=>$4) FROM events")
        .bind(r).bind(workspace).bind(n).bind(age_seconds).execute(&s.pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn fair_dispatch_upgrade_backfills_only_pending_and_recovers_index(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(all.iter().filter(|m| m.version <= 54).cloned().collect()),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }.run(&pool).await.unwrap();
    let s = seed(pool).await;
    backlog(&s, Some(s.team), 2, 60).await;
    sqlx::query("UPDATE alert_deliveries SET status='not_configured',completed_at=clock_timestamp() WHERE id=(SELECT id FROM alert_deliveries ORDER BY id LIMIT 1)")
        .execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    let buckets: Vec<(String,Uuid)> = sqlx::query_as("SELECT status,bucket_id FROM alert_deliveries ORDER BY status")
        .fetch_all(&s.pool).await.unwrap();
    assert_eq!(buckets,vec![("not_configured".into(),Uuid::nil()),("pending".into(),s.team)]);
    // Simulate an interrupted concurrent build (0057 not recorded, invalid index).
    sqlx::raw_sql("DELETE FROM _sqlx_migrations WHERE version=57; UPDATE pg_index SET indisvalid=false WHERE indexrelid='public.alert_deliveries_pending_bucket'::regclass")
        .execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    let valid: bool = sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid='public.alert_deliveries_pending_bucket'::regclass")
        .fetch_one(&s.pool).await.unwrap();
    assert!(valid);
    assert!(s.store.is_ready().await);
}

async fn workspace(s: &Seed) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Fair dispatch','team')")
        .bind(id).execute(&s.pool).await.unwrap();
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
    assert_eq!(sent, expected, "a noisy workspace must not consume the quiet buckets' slots");
    let mut received = 0;
    while rx.try_recv().is_ok() { received += 1; }
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
        backlog(&s, Some(ws), 1, 4000-i*60).await;
    }
    assert_eq!(deliver_pending(&s.store, 25).await, 25);
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT e.workspace_id FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE d.status='pending' ORDER BY d.created_at,d.id")
        .fetch_all(&s.pool).await.unwrap();
    assert_eq!(pending, ordered[25..]);
    assert_eq!(deliver_pending(&s.store, 25).await, 15);
    assert_eq!(count(&s,"SELECT count(*) FROM alert_deliveries WHERE status='sent'").await,40);
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
    assert_eq!(buckets, vec![(Some(s.team),s.team),(None,Uuid::nil())]);
    assert_eq!(deliver_pending(&s.store, 25).await, 2);
    assert_eq!(count(&s,"SELECT count(*) FROM alert_deliveries WHERE status='sent'").await,2);
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
            while let Some((id, claim)) = outbox::claim(&store, Uuid::new_v4()).await.unwrap() {
                assert!(claim.is_some());
                claimed.push(id);
            }
            claimed
        });
    }
    let mut ids = Vec::new();
    while let Some(result) = workers.join_next().await { ids.extend(result.unwrap()); }
    assert_eq!(ids.len(), 8);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8);
    assert_eq!(count(&s,"SELECT count(*) FROM alert_recipient_outbox WHERE state='leased' AND attempt_count=1").await,8);
}

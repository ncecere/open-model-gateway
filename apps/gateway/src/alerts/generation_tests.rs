//! #277: real PostgreSQL admission, concurrency and rolling-upgrade oracles.
use super::*;

async fn generation_rule(s: &Seed, workspace: Option<Uuid>) -> Uuid {
    match workspace {
        Some(ws) => {
            rule(
                s,
                "scope,workspace_id,kind,budget_layers,thresholds",
                &format!("'workspace','{ws}','budget_threshold',ARRAY['local'],ARRAY[100]"),
            )
            .await
        }
        None => {
            rule(
                s,
                "scope,kind,budget_layers,thresholds,notify_platform_admins",
                "'installation','budget_threshold',ARRAY['local'],ARRAY[100],true",
            )
            .await
        }
    }
}

async fn generation_events(s: &Seed, workspace: Option<Uuid>, n: i32) -> Vec<Uuid> {
    let r = generation_rule(s, workspace).await;
    sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) SELECT gen_random_uuid(),$1,'budget_threshold',i::text,100,'critical',$2,'Generation test' FROM generate_series(1,$3) i RETURNING id")
        .bind(r).bind(workspace).bind(n).fetch_all(&s.pool).await.unwrap()
}
const GENERATE: &str = "INSERT INTO alert_deliveries(id,event_id,transition) SELECT gen_random_uuid(),id,'fired' FROM unnest($1::uuid[]) id ON CONFLICT(event_id,transition) DO NOTHING";

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn alert_generation_across_rules_transitions_buckets_and_webhooks(pool: PgPool) {
    let s = seed(pool).await;
    sqlx::query("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('test','https://receiver.invalid/','env:HOOK')").execute(&s.pool).await.unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true")
        .execute(&s.pool)
        .await
        .unwrap();
    // Installation rules may describe shared workspaces: webhooks remain independent.
    let r = generation_rule(&s, None).await;
    let ids: Vec<Uuid> = sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) SELECT gen_random_uuid(),$1,'budget_threshold',i::text,100,'critical',$2,'Generation test' FROM generate_series(1,99) i RETURNING id")
        .bind(r).bind(s.team).fetch_all(&s.pool).await.unwrap();
    sqlx::query(GENERATE)
        .bind(&ids)
        .execute(&s.pool)
        .await
        .unwrap();
    let last = generation_events(&s, Some(s.team), 2).await;
    sqlx::query(GENERATE)
        .bind(&last)
        .execute(&s.pool)
        .await
        .unwrap();
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE quota_charged),count(*) FILTER(WHERE status='suppressed' AND suppression_reason='workspace_generation_limit' AND completed_at IS NOT NULL AND NOT initialized AND recipients=0 AND attempt_count=0) FROM alert_deliveries")
        .fetch_one(&s.pool).await.unwrap();
    assert_eq!(counts, (101, 100, 1));
    assert_eq!(count(&s, "SELECT count(*) FROM alert_events").await, 101);
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_webhook_outbox").await,
        99
    );
    // Both clearing transitions are suppressed separately, but enqueue webhooks.
    sqlx::query("UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='cleared' WHERE id=ANY($1)").bind(&ids[..2]).execute(&s.pool).await.unwrap();
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) SELECT gen_random_uuid(),id,'resolved' FROM unnest($1::uuid[]) id").bind(&ids[..2]).execute(&s.pool).await.unwrap();
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='suppressed'"
        )
        .await,
        3
    );
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_webhook_outbox").await,
        101
    );
    for ws in [Some(s.personal), None] {
        let quiet = generation_events(&s, ws, 1).await;
        sqlx::query(GENERATE)
            .bind(&quiet)
            .execute(&s.pool)
            .await
            .unwrap();
        let charged: bool =
            sqlx::query_scalar("SELECT quota_charged FROM alert_deliveries WHERE event_id=$1")
                .bind(quiet[0])
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert!(charged);
    }
    assert_eq!(
        count(&s, "SELECT count(*) FROM alert_recipient_outbox").await,
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn alert_generation_replica_race_at_99_and_rollback_duplicate_multirow(pool: PgPool) {
    let s = seed(pool).await;
    let ids = generation_events(&s, Some(s.team), 103).await;
    // A rolled-back reservation consumes nothing.
    let mut tx = s.pool.begin().await.unwrap();
    sqlx::query(GENERATE)
        .bind(&ids[..1])
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    sqlx::query(GENERATE)
        .bind(&ids[..99])
        .execute(&s.pool)
        .await
        .unwrap();
    // Duplicates at capacity must not consume extra slots.
    sqlx::query(GENERATE)
        .bind(&ids[..99])
        .execute(&s.pool)
        .await
        .unwrap();
    let a = async {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query(GENERATE)
            .bind(&ids[99..101])
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    };
    let b = async {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query(GENERATE)
            .bind(&ids[101..])
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(a, b);
    })
    .await
    .unwrap();
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE quota_charged"
        )
        .await,
        100
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='suppressed'"
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn alert_generation_canonical_locks_opposite_bucket_orders(pool: PgPool) {
    let s = seed(pool).await;
    let a = generation_events(&s, Some(s.team), 2).await;
    let b = generation_events(&s, Some(s.personal), 2).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let run = |reverse: bool| {
        let barrier = barrier.clone();
        let pool = s.pool.clone();
        let buckets = if reverse {
            vec![s.personal, s.team]
        } else {
            vec![s.team, s.personal]
        };
        let events = if reverse {
            vec![b[1], a[1]]
        } else {
            vec![a[0], b[0]]
        };
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL statement_timeout='5s'")
                .execute(&mut *tx)
                .await
                .unwrap();
            barrier.wait().await;
            // Every multi-bucket producer prelocks its full union before writes.
            sqlx::query("SELECT alert_delivery_lock_buckets($1)")
                .bind(buckets)
                .execute(&mut *tx)
                .await
                .unwrap();
            for id in events {
                sqlx::query(GENERATE)
                    .bind(vec![id])
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run(false), run(true));
    })
    .await
    .unwrap();
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE quota_charged"
        )
        .await,
        4
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn alert_generation_clock_boundary_forgery_and_isolation(pool: PgPool) {
    let s = seed(pool).await;
    let ids = generation_events(&s, Some(s.team), 3).await;
    let before = expiry_now(&s).await;
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition,bucket_id,created_at,quota_charged,suppression_reason) VALUES(gen_random_uuid(),$1,'fired',$2,'2000-01-01',false,'workspace_generation_limit')")
        .bind(ids[0]).bind(s.personal).execute(&s.pool).await.unwrap();
    let (bucket, at, charged, reason): (Uuid, DateTime<Utc>, bool, Option<String>) =
        sqlx::query_as(
            "SELECT bucket_id,created_at,quota_charged,suppression_reason FROM alert_deliveries",
        )
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(bucket, s.team);
    assert!(at >= before);
    assert!(charged);
    assert!(reason.is_none());
    // Exact open lower / closed upper bound, including terminal charged rows.
    let inside: i64 = sqlx::query_scalar(
        "SELECT alert_delivery_recent_count($1,$2+interval '24 hours'-interval '1 microsecond')",
    )
    .bind(s.team)
    .bind(at)
    .fetch_one(&s.pool)
    .await
    .unwrap();
    let boundary: i64 =
        sqlx::query_scalar("SELECT alert_delivery_recent_count($1,$2+interval '24 hours')")
            .bind(s.team)
            .bind(at)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    let future: i64 =
        sqlx::query_scalar("SELECT alert_delivery_recent_count($1,$2-interval '1 microsecond')")
            .bind(s.team)
            .bind(at)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((inside, boundary, future), (1, 0, 0));
    for level in ["REPEATABLE READ", "SERIALIZABLE"] {
        let mut tx = s.pool.begin().await.unwrap();
        sqlx::query(&format!("SET TRANSACTION ISOLATION LEVEL {level}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let error = sqlx::query(GENERATE)
            .bind(&ids[1..2])
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("25001")
        );
        tx.rollback().await.unwrap();
    }
    for col in [
        "quota_charged",
        "suppression_reason",
        "bucket_id",
        "created_at",
    ] {
        assert!(
            sqlx::query(&format!("UPDATE alert_deliveries SET {col}=DEFAULT"))
                .execute(&s.pool)
                .await
                .is_err()
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn alert_generation_recipient_50_51_and_suppressed_terminal(pool: PgPool) {
    let s = seed(pool).await;
    let ids = generation_events(&s, Some(s.team), 101).await;
    sqlx::query(GENERATE)
        .bind(&ids)
        .execute(&s.pool)
        .await
        .unwrap();
    let admitted: Uuid =
        sqlx::query_scalar("SELECT id FROM alert_deliveries WHERE quota_charged LIMIT 1")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) SELECT $1,sha256(i::text::bytea) FROM generate_series(1,50) i").bind(admitted).execute(&s.pool).await.unwrap();
    assert!(sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) VALUES($1,sha256('51'::bytea))").bind(admitted).execute(&s.pool).await.is_err());
    assert!(sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) SELECT id,sha256('suppressed'::bytea) FROM alert_deliveries WHERE status='suppressed'").execute(&s.pool).await.is_err());
    assert!(sqlx::query("UPDATE alert_deliveries SET status='pending',completed_at=NULL WHERE status='suppressed'").execute(&s.pool).await.is_err());
    assert!(
        sqlx::query(
            "UPDATE alert_deliveries SET suppression_reason=NULL WHERE status='suppressed'"
        )
        .execute(&s.pool)
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = false)]
async fn alert_generation_upgrade_0057_old_sql_and_concurrent_index_recovery(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= 57).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let s = seed(pool).await;
    let ids = generation_events(&s, Some(s.team), 3).await;
    sqlx::query(GENERATE)
        .bind(&ids[..2])
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_deliveries SET status='not_configured',completed_at=clock_timestamp() WHERE event_id=$1").bind(ids[0]).execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    // No old rows charged, reopened or replayed by the upgrade.
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE quota_charged"
        )
        .await,
        0
    );
    sqlx::query(GENERATE)
        .bind(&ids[2..])
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE quota_charged"
        )
        .await,
        1
    );
    assert_eq!(deliver_pending(&s.store, 25).await, 2);
    // Pre-0059 worker final UPDATE still works; suppressed never matches pending.
    sqlx::query("UPDATE alert_deliveries SET status='not_configured',completed_at=clock_timestamp() WHERE status='pending'").execute(&s.pool).await.unwrap();
    sqlx::raw_sql("DELETE FROM _sqlx_migrations WHERE version=60; UPDATE pg_index SET indisvalid=false WHERE indexrelid='public.alert_deliveries_generation_window'::regclass").execute(&s.pool).await.unwrap();
    s.store.migrate_enterprise().await.unwrap();
    assert!(s.store.is_ready().await);
    let valid:bool=sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid='public.alert_deliveries_generation_window'::regclass").fetch_one(&s.pool).await.unwrap();
    assert!(valid);
}

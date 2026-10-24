#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_cap_survives_concurrent_and_stale_snapshots(pool: PgPool) {
    for _ in 0..9 {
        destination(&pool).await;
    }
    let insert = "INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('race','https://example.test/hook','env:HOOK') RETURNING id";
    let mut stale = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *stale)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_destinations")
            .fetch_one(&mut *stale)
            .await
            .unwrap(),
        9
    );
    let (one, two) = tokio::join!(
        sqlx::query_scalar::<_, Uuid>(insert).fetch_one(&pool),
        sqlx::query_scalar::<_, Uuid>(insert).fetch_one(&pool)
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert!(
        sqlx::query(insert).execute(&mut *stale).await.is_err(),
        "stale snapshot cannot allocate an eleventh live slot"
    );
    stale.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_webhook_destinations WHERE deleted_at IS NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        10
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_payload_attempt_and_terminal_bounds(pool: PgPool) {
    let dest = destination(&pool).await;
    let insert = "INSERT INTO alert_webhook_outbox(destination_id,test,payload) VALUES($1,true,$2) RETURNING id";
    assert!(
        sqlx::query(insert)
            .bind(dest)
            .bind(vec![255_u8])
            .execute(&pool)
            .await
            .is_err(),
        "payload must be UTF-8"
    );
    let id: Uuid = sqlx::query_scalar(insert)
        .bind(dest)
        .bind(vec![b' '; 65536])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind(dest)
            .bind(vec![b' '; 65537])
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(insert)
            .bind(dest)
            .bind(Vec::<u8>::new())
            .execute(&pool)
            .await
            .is_err()
    );
    refuses(&pool, "UPDATE alert_webhook_outbox SET attempt_count=2").await;
    sqlx::query("UPDATE alert_webhook_outbox SET state='leased',attempt_count=1,lease_owner=gen_random_uuid(),lease_token=gen_random_uuid(),lease_expires_at=clock_timestamp()+interval '90 seconds' WHERE id=$1").bind(id).execute(&pool).await.unwrap();
    refuses(&pool, "UPDATE alert_webhook_outbox SET attempt_count=0").await;
    for state in ["sent", "failed", "exhausted", "ineligible"] {
        let id: Uuid = sqlx::query_scalar(insert)
            .bind(dest)
            .bind(b"{}".as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE alert_webhook_outbox SET state=$2,completed_at=clock_timestamp() WHERE id=$1",
        )
        .bind(id)
        .bind(state)
        .execute(&pool)
        .await
        .unwrap();
        refuses(
            &pool,
            &format!(
                "UPDATE alert_webhook_outbox SET state='waiting',completed_at=NULL WHERE id='{id}'"
            ),
        )
        .await;
        refuses(
            &pool,
            &format!("UPDATE alert_webhook_outbox SET last_error='connection' WHERE id='{id}'"),
        )
        .await;
    }
    // Duplicate event/transition/destination snapshots cannot be inserted manually either.
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','dedup',1,'critical','safe')").bind(event).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO alert_webhook_outbox(event_id,transition,destination_id,payload) VALUES($1,'fired',$2,'{}'::bytea)").bind(event).bind(dest).execute(&pool).await.unwrap();
    assert!(sqlx::query("INSERT INTO alert_webhook_outbox(event_id,transition,destination_id,payload) VALUES($1,'fired',$2,'{}'::bytea)").bind(event).bind(dest).execute(&pool).await.is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_enqueue_serializes_with_disablement(pool: PgPool) {
    let dest = destination(&pool).await;
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','disable-race',1,'critical','safe')").bind(event).execute(&pool).await.unwrap();
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&mut *enqueue)
    .await
    .unwrap();
    let mut disable = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *disable)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
            .bind(dest)
            .execute(&mut *disable)
            .await
            .unwrap();
        disable.commit().await.unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if sqlx::query_scalar::<_, bool>("SELECT cardinality(pg_blocking_pids($1))>0")
                .bind(pid)
                .fetch_one(&pool)
                .await
                .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disable must wait for the transactional destination snapshot");
    enqueue.commit().await.unwrap();
    task.await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM alert_webhook_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "ineligible"
    );
}

#[sqlx::test(migrations = false)]
async fn webhook_explicit_upgrade_has_no_backfill_and_preserves_old_evaluator_path(pool: PgPool) {
    use open_model_gateway::store::{MIGRATOR, Store};
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version <= 54)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
    .run(&pool)
    .await
    .unwrap();
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','historical',1,'critical','safe')").bind(event).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    let store = Store::new(pool.clone());
    assert!(
        store.preflight_enterprise().await.is_err(),
        "old schema does not satisfy new readiness"
    );
    store.migrate_enterprise().await.unwrap();
    store.preflight_enterprise().await.unwrap();
    let dest = destination(&pool).await;
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query(
        "UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='cleared' WHERE id=$1",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    // The same INSERT issued by pre-0055 evaluators, without webhook awareness.
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'resolved')").bind(event).execute(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

async fn destination(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('receiver','https://example.test/hook','env:HOOK') RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn refuses(pool: &PgPool, sql: &str) {
    assert!(
        sqlx::query(sql).execute(pool).await.is_err(),
        "accepted {sql}"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_destination_cap_and_immutable_identity(pool: PgPool) {
    let id = destination(&pool).await;
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM alert_webhook_destinations WHERE id=$1"
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    for _ in 1..10 {
        destination(&pool).await;
    }
    refuses(&pool,"INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('extra','https://example.test/extra','env:HOOK')").await;
    refuses(
        &pool,
        "UPDATE alert_webhook_destinations SET url='https://example.test/moved'",
    )
    .await;
    refuses(
        &pool,
        "UPDATE alert_webhook_destinations SET id=gen_random_uuid()",
    )
    .await;
    refuses(&pool, "DELETE FROM alert_webhook_destinations").await;
    refuses(&pool, "TRUNCATE alert_webhook_destinations CASCADE").await;
    refuses(&pool,"UPDATE alert_webhook_destinations SET previous_secret_ref='env:OLD',previous_secret_until=now()+interval '25 hours'").await;
    refuses(
        &pool,
        "UPDATE alert_webhook_destinations SET current_secret_ref='file:/secret'",
    )
    .await;
    sqlx::query("UPDATE alert_webhook_destinations SET deleted_at=clock_timestamp(),enabled=false WHERE id=$1").bind(id).execute(&pool).await.unwrap();
    destination(&pool).await;
    refuses(
        &pool,
        &format!("UPDATE alert_webhook_destinations SET deleted_at=NULL WHERE id='{id}'"),
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_transactional_projection_and_guards(pool: PgPool) {
    let dest = destination(&pool).await;
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,details) VALUES($1,'scim_last_admin','scim_last_admin','PERSONAL-CANARY',1,'critical','PERSONAL-CANARY','{\"owner\":\"PERSONAL-CANARY\"}')").bind(event).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&mut *tx)
    .await
    .unwrap();
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT payload FROM alert_webhook_outbox")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!String::from_utf8(bytes.clone()).unwrap().contains("CANARY"));
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["schema"], "omg.alert.v1");
    assert_eq!(body["event"]["id"], event.to_string());
    assert_eq!(body["event"].as_object().unwrap().len(), 6);
    tx.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','scim',1,'critical','safe')").bind(event).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    for update in [
        "payload='x'::bytea",
        "event_id=gen_random_uuid()",
        "destination_id=gen_random_uuid()",
        "transition='resolved'",
        "attempt_count=6",
        "state='leased'",
        "created_at=now()+interval '1 hour'",
    ] {
        refuses(&pool, &format!("UPDATE alert_webhook_outbox SET {update}")).await;
    }
    refuses(&pool, "DELETE FROM alert_webhook_outbox").await;
    refuses(&pool, "TRUNCATE alert_webhook_outbox").await;
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "ineligible"
    );
    refuses(&pool, "UPDATE alert_webhook_outbox SET state='waiting'").await;
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM alert_webhook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "ineligible"
    );
}

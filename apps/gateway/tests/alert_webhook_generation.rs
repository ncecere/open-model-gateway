#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
//! Enable generations close the disable -> re-enable window: a snapshot taken
//! under one generation can never be delivered under a later one.
use sqlx::PgPool;
use uuid::Uuid;

const DELIVERY: &str =
    "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')";

async fn receiver(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver','https://example.test/hook','env:HOOK',true) RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn event(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',gen_random_uuid()::text,1,'critical','safe') RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn set_enabled(pool: &PgPool, dest: Uuid, enabled: bool) {
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=$2 WHERE id=$1")
        .bind(dest)
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
}
async fn generation(pool: &PgPool, dest: Uuid) -> i64 {
    sqlx::query_scalar("SELECT enable_generation FROM alert_webhook_destinations WHERE id=$1")
        .bind(dest)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn rows(pool: &PgPool, event: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1")
        .bind(event)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Exactly the predicate slice 2's claim must use while holding the
/// destination lock: enabled, not deleted and the same generation.
async fn claimable(pool: &PgPool, event: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM alert_webhook_outbox o JOIN alert_webhook_destinations d ON d.id=o.destination_id WHERE o.event_id=$1 AND o.state='waiting' AND d.enabled AND d.deleted_at IS NULL AND o.destination_generation=d.enable_generation")
        .bind(event).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn enqueue_that_read_an_older_generation_is_refused_after_disable_and_reenable(pool: PgPool) {
    let dest = receiver(&pool).await;
    // The enqueue has read generation N (its destination SELECT) and waits
    // here, before its outbox INSERT, on a lock this test holds.
    sqlx::raw_sql("CREATE FUNCTION gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(424242); RETURN NEW; END $$; CREATE TRIGGER aaa_gate BEFORE INSERT ON alert_webhook_outbox FOR EACH ROW EXECUTE FUNCTION gate();").execute(&pool).await.unwrap();
    let event = event(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(424242)")
        .execute(&mut *holder)
        .await
        .unwrap();
    let queued = {
        let pool = pool.clone();
        tokio::spawn(async move { sqlx::query(DELIVERY).bind(event).execute(&pool).await })
    };
    let mut waiting = 0_i64;
    for _ in 0..100 {
        waiting = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND NOT granted",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(waiting > 0, "the enqueue never reached its insert");
    set_enabled(&pool, dest, false).await;
    set_enabled(&pool, dest, true).await;
    holder.commit().await.unwrap();
    queued
        .await
        .unwrap()
        .expect("a stale snapshot never aborts the delivery");
    assert_eq!(
        rows(&pool, event).await,
        0,
        "a snapshot taken before the disable and re-enable was queued"
    );
    assert_eq!(generation(&pool, dest).await, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn enqueue_committed_after_disable_and_reenable_is_never_claimable(pool: PgPool) {
    let dest = receiver(&pool).await;
    let event = event(&pool).await;
    // The row is inserted under generation N and stays uncommitted, so
    // neither the disable nor the re-enable trigger can see or end it.
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&mut *enqueue)
        .await
        .unwrap();
    set_enabled(&pool, dest, false).await;
    set_enabled(&pool, dest, true).await;
    enqueue.commit().await.unwrap();
    assert_eq!(rows(&pool, event).await, 1);
    assert_eq!(
        claimable(&pool, event).await,
        0,
        "the late-committed row is claimable after a re-enable"
    );
    let (row, current): (i64, i64) = sqlx::query_as("SELECT o.destination_generation,d.enable_generation FROM alert_webhook_outbox o JOIN alert_webhook_destinations d ON d.id=o.destination_id WHERE o.event_id=$1").bind(event).fetch_one(&pool).await.unwrap();
    assert_eq!((row, current), (0, 2));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn normal_enqueue_without_a_change_is_claimable_and_stamped(pool: PgPool) {
    let dest = receiver(&pool).await;
    set_enabled(&pool, dest, false).await;
    set_enabled(&pool, dest, true).await;
    let event = event(&pool).await;
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(rows(&pool, event).await, 1);
    assert_eq!(claimable(&pool, event).await, 1);
    let stamped: i64 = sqlx::query_scalar(
        "SELECT destination_generation FROM alert_webhook_outbox WHERE event_id=$1",
    )
    .bind(event)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamped, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reenabling_ends_waiting_rows_of_older_generations_only(pool: PgPool) {
    let dest = receiver(&pool).await;
    let old = event(&pool).await;
    // Committed while disabled (its enqueue raced the disable), still waiting.
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query(DELIVERY)
        .bind(old)
        .execute(&mut *enqueue)
        .await
        .unwrap();
    set_enabled(&pool, dest, false).await;
    enqueue.commit().await.unwrap();
    set_enabled(&pool, dest, true).await;
    let state = |event: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM alert_webhook_outbox WHERE event_id=$1",
            )
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(state(old).await, "ineligible");
    let fresh = event(&pool).await;
    sqlx::query(DELIVERY)
        .bind(fresh)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(state(fresh).await, "waiting");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn label_only_update_keeps_waiting_rows_and_the_generation(pool: PgPool) {
    let dest = receiver(&pool).await;
    let event = event(&pool).await;
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET label='renamed' WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM alert_webhook_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "waiting"
    );
    assert_eq!(generation(&pool, dest).await, 0);
    assert_eq!(claimable(&pool, event).await, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn generation_counts_every_enabled_change_and_is_trigger_owned(pool: PgPool) {
    let dest = receiver(&pool).await;
    assert_eq!(generation(&pool, dest).await, 0);
    sqlx::query("UPDATE alert_webhook_destinations SET label='x' WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(generation(&pool, dest).await, 0);
    set_enabled(&pool, dest, false).await;
    assert_eq!(generation(&pool, dest).await, 1);
    set_enabled(&pool, dest, true).await;
    assert_eq!(generation(&pool, dest).await, 2);
    // No caller can set, rewind or skip it.
    sqlx::query("UPDATE alert_webhook_destinations SET enable_generation=99 WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(generation(&pool, dest).await, 2);
    sqlx::query(
        "UPDATE alert_webhook_destinations SET enabled=false,enable_generation=0 WHERE id=$1",
    )
    .bind(dest)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(generation(&pool, dest).await, 3);
    sqlx::query("UPDATE alert_webhook_destinations SET deleted_at=clock_timestamp() WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(generation(&pool, dest).await, 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn direct_insert_must_carry_the_current_generation_and_it_is_immutable(pool: PgPool) {
    let dest = receiver(&pool).await;
    set_enabled(&pool, dest, false).await;
    set_enabled(&pool, dest, true).await;
    let event = event(&pool).await;
    let insert = "INSERT INTO alert_webhook_outbox(id,destination_id,event_id,transition,destination_generation,payload) SELECT i,$1,$2,'fired',$3,convert_to(alert_webhook_payload(i,$2,'fired'),'UTF8') FROM (SELECT gen_random_uuid() AS i) x";
    for stale in [0_i64, 1, 3, -1] {
        let error = sqlx::query(insert)
            .bind(dest)
            .bind(event)
            .bind(stale)
            .execute(&pool)
            .await
            .expect_err(&format!("generation {stale} was accepted"));
        assert_eq!(
            error.as_database_error().unwrap().message(),
            "webhook destination is not eligible"
        );
    }
    sqlx::query(insert)
        .bind(dest)
        .bind(event)
        .bind(2_i64)
        .execute(&pool)
        .await
        .expect("the current generation is accepted");
    let error = sqlx::query("UPDATE alert_webhook_outbox SET destination_generation=3")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().message(),
        "immutable webhook payload, identity or terminal outcome"
    );
}

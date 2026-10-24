#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reenabling_never_replays_a_waiting_row_that_survived_a_disable(pool: PgPool) {
    let dest: Uuid = sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver','https://example.test/hook','env:HOOK',true) RETURNING id").fetch_one(&pool).await.unwrap();
    let event: Uuid = sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin','stale',1,'critical','safe') RETURNING id").fetch_one(&pool).await.unwrap();
    // An evaluator's enqueue is still uncommitted when the destination is
    // disabled, so the disable trigger cannot see (or end) its row.
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&mut *enqueue)
    .await
    .unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    enqueue.commit().await.unwrap();
    let state = || async {
        sqlx::query_as::<_, (String, bool)>(
            "SELECT state,completed_at IS NOT NULL FROM alert_webhook_outbox WHERE event_id=$1",
        )
        .bind(event)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        state().await,
        ("waiting".into(), false),
        "the stale row survives the disable (the claim-time recheck owns this window)"
    );
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(dest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        state().await,
        ("ineligible".into(), true),
        "re-enabling must not replay a row queued before the enable"
    );
}

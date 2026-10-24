//! Transaction-local counts, published by the evaluator only after commit.
use super::*;

async fn measured(s: &Seed, now: DateTime<Utc>) -> super::super::super::aged_hold::AgedHoldCounts {
    let mut tx = s.pool.begin().await.unwrap();
    let (_, counts) = aged_hold_tick(&mut tx, now).await.unwrap();
    tx.commit().await.unwrap();
    counts
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_metrics_distinguish_initial_critical_escalation_clear_and_suppression(
    pool: PgPool,
) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    hold_since(&s, s.team, s.key, now - TimeDelta::hours(25)).await;
    hold_since(&s, s.personal, s.personal_key, now - TimeDelta::hours(169)).await;
    let events = generation_events(&s, None, 100).await;
    sqlx::query(GENERATE)
        .bind(events)
        .execute(&s.pool)
        .await
        .unwrap();
    let counts = measured(&s, now).await;
    assert_eq!(
        (
            counts.fired,
            counts.escalated,
            counts.cleared,
            counts.suppressed,
            counts.deferred
        ),
        (2, 0, 0, 1, 0)
    );
    let counts = measured(&s, now).await;
    assert_eq!(
        (
            counts.fired,
            counts.escalated,
            counts.cleared,
            counts.suppressed,
            counts.deferred
        ),
        (0, 0, 0, 0, 0)
    );
    let counts = measured(&s, now + TimeDelta::hours(144)).await;
    assert_eq!((counts.fired, counts.escalated, counts.cleared), (0, 1, 0));
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=1,input_tokens=1,output_tokens=1 WHERE state='unknown'")
        .execute(&s.pool).await.unwrap();
    let counts = measured(&s, now + TimeDelta::hours(145)).await;
    assert_eq!((counts.fired, counts.escalated, counts.cleared), (0, 0, 2));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_metrics_deferred_is_a_bounded_backlog_signal_including_exhausted_day(
    pool: PgPool,
) {
    let s = seed(pool).await;
    let now = expiry_now(&s).await;
    for _ in 0..101 {
        let (ws, key) = generation_project(&s).await;
        hold_since(&s, ws, key, now - TimeDelta::hours(25)).await;
    }
    let counts = measured(&s, now).await;
    assert_eq!((counts.fired, counts.deferred), (100, 1));
    let counts = measured(&s, now).await;
    assert_eq!((counts.fired, counts.deferred), (1, 0));
    // Fill the installation's daily incident allowance using valid, resolved
    // historical subjects. The remaining candidate must still signal backlog.
    sqlx::query("INSERT INTO alert_events(id,kind,subject_key,workspace_id,level,severity,summary,details,fired_at,resolved_at,resolution) SELECT gen_random_uuid(),'aged_unknown_hold','workspace:'||w.id,w.id,1,'warning','Holds with unknown cost are over 24 hours old','{\"threshold_hours\":24}',$1,$1,'cleared' FROM workspaces w CROSS JOIN generate_series(1,399) WHERE w.id=$2")
        .bind(now).bind(s.team).execute(&s.pool).await.unwrap();
    hold_since(&s, s.personal, s.personal_key, now - TimeDelta::hours(25)).await;
    let counts = measured(&s, now).await;
    assert_eq!((counts.fired, counts.deferred), (0, 1));
}

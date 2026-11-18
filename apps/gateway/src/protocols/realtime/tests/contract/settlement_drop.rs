//! Abort the session owner while PostgreSQL is blocking its write.
use super::*;
use crate::inference::{
    realtime::{RealtimeUsage, ResponseStatus},
    repository::Outcome,
};

async fn interrupted(pool: PgPool, during_response: bool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    let principal = Store::new(g.pool.clone())
        .authenticate(g.key())
        .await
        .unwrap()
        .unwrap();
    let id = Uuid::now_v7();
    let mut session = g
        .engine
        .open_realtime(principal, "company/smart", id)
        .await
        .unwrap();
    session.reserve_window(100).await.unwrap();
    session.open_response("one".into()).await.unwrap();
    let usage = Some(RealtimeUsage {
        input_text_tokens: 3,
        output_text_tokens: 1,
        ..Default::default()
    });
    let status = Some(ResponseStatus::Completed);
    if !during_response {
        session.settle_response("one", status, usage).await.unwrap();
    }

    let mut blocker = g.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM governance_reservations WHERE execution_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let owner = tokio::spawn(async move {
        if during_response {
            session.settle_response("one", status, usage).await.unwrap();
        } else {
            session
                .finish(Outcome::Succeeded, None, None)
                .await
                .unwrap();
        }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(pid).fetch_one(&g.pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("the settlement reached the locked reservation");
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    blocker.rollback().await.unwrap();

    let row = tokio::time::timeout(Duration::from_secs(5), g.finished())
        .await
        .expect("session settlement completed");
    assert_eq!(
        row["state"],
        if during_response {
            "cancelled"
        } else {
            "succeeded"
        }
    );
    assert_eq!(row["reservation"], "settled");
    assert_eq!(row["actual"], 28);
    assert_eq!(row["input_tokens"], 3);
    assert_eq!(row["output_tokens"], 1);
    let responses = g.responses(&id.to_string()).await;
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["status"], "completed");
    assert_eq!(responses[0]["actual"], 28);
    let ledger = g.ledger(&id.to_string()).await;
    assert_eq!(
        ledger
            .iter()
            .filter(|(kind, _)| kind == "settlement" || kind == "unknown")
            .collect::<Vec<_>>(),
        vec![&("settlement".into(), Some(28))]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn drop_during_final_write(pool: PgPool) {
    interrupted(pool, false).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn drop_during_response_write(pool: PgPool) {
    interrupted(pool, true).await;
}

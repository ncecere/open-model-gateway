//! A client that disconnects while the settlement write of a finished
//! attempt is in flight must not lose that write. Real `Store` and `Engine`
//! on PostgreSQL; the settlement is held up by a row lock the test owns, and
//! the request is dropped while the database reports the settlement waiting.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::{sync::Mutex, time::timeout};
use uuid::Uuid;

use crate::{
    governance::tests::db::{Fixture, fixture, request},
    inference::{
        Engine, EngineLimits,
        error::InferenceError,
        types::{
            Capabilities, ChatEvent, ChatRequest, ChatResponse, Deployment, FinishReason,
            ProviderOutput, Usage,
        },
    },
    providers::{ProviderAdapter, ProviderRegistry},
};

#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    /// A complete response with usage.
    Complete,
    /// A stream that ends with its final usage and `Done`.
    Stream,
    /// A stream that delivers one delta and then stalls.
    Stalled,
}

/// The upstream. With `hold` set it takes a row lock on the attempt's
/// reservation after admission, so the settlement that follows must wait.
struct Upstream {
    pool: PgPool,
    behaviour: Behaviour,
    hold: bool,
    blocker: Arc<Mutex<Option<PgConnection>>>,
}
#[async_trait]
impl ProviderAdapter for Upstream {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    async fn execute(
        &self,
        target: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        if self.hold {
            // A separate connection, so the engine's pool is untouched.
            let mut connection = PgConnection::connect_with(&self.pool.connect_options())
                .await
                .unwrap();
            sqlx::query("BEGIN").execute(&mut connection).await.unwrap();
            let locked: Vec<Uuid> = sqlx::query_scalar(
                "SELECT execution_id FROM governance_reservations WHERE deployment_id=$1 AND state='pending' FOR UPDATE",
            )
            .bind(target.id)
            .fetch_all(&mut connection)
            .await
            .unwrap();
            assert_eq!(locked.len(), 1, "only this attempt is pending");
            *self.blocker.lock().await = Some(connection);
        }
        let usage = Usage {
            input_tokens: Some(3),
            output_tokens: Some(1),
            ..Default::default()
        };
        Ok(match self.behaviour {
            Behaviour::Complete => ProviderOutput::Complete(ChatResponse {
                messages_content: None,
                content: Some("ok".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            }),
            Behaviour::Stream => ProviderOutput::Stream(Box::pin(futures_util::stream::iter([
                Ok(ChatEvent::Delta {
                    text: Some("ok".into()),
                    tool_calls: vec![],
                }),
                Ok(ChatEvent::Finish(FinishReason::Stop)),
                Ok(ChatEvent::Usage(usage)),
                Ok(ChatEvent::Done),
            ]))),
            Behaviour::Stalled => ProviderOutput::Stream(Box::pin(
                futures_util::stream::iter([Ok(ChatEvent::Delta {
                    text: Some("ok".into()),
                    tool_calls: vec![],
                })])
                .chain(futures_util::stream::pending()),
            )),
        })
    }
}

struct World {
    fixture: Fixture,
    engine: Engine,
    blocker: Arc<Mutex<Option<PgConnection>>>,
}
async fn world(pool: PgPool, behaviour: Behaviour, hold: bool) -> World {
    let fixture = fixture(pool).await;
    fixture.price(1_000_000).await;
    let blocker = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Upstream {
            pool: fixture.store.pool.clone(),
            behaviour,
            hold,
            blocker: blocker.clone(),
        }))
        .unwrap();
    let engine = Engine::new(
        Arc::new(fixture.store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap();
    World {
        fixture,
        engine,
        blocker,
    }
}

/// Waits until PostgreSQL itself reports the settlement's own statement
/// (the reservation lock) waiting for a lock, and returns when it saw that.
/// The settlement deadline (3 s) runs from the moment the write was built,
/// so this polls tightly and the caller releases the lock at once.
async fn settlement_is_waiting(pool: &PgPool) -> Instant {
    timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM governance_reservations r%FOR UPDATE OF r%'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                return Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the settlement waits on the held reservation row")
}

/// Releases the lock the test owns. `waiting` is when the settlement was
/// seen waiting: the write must still be inside its 3 s deadline, or the
/// test measures the harness, not the product.
async fn release(world: &World, waiting: Instant) {
    assert!(
        waiting.elapsed() < Duration::from_secs(1),
        "the harness was too slow to release the lock inside the settlement deadline"
    );
    let mut connection = world.blocker.lock().await.take().unwrap();
    sqlx::query("ROLLBACK")
        .execute(&mut connection)
        .await
        .unwrap();
}

struct Rows {
    reservation: String,
    held: Option<i64>,
    actual: Option<i64>,
    execution: String,
    input: Option<i64>,
    output: Option<i64>,
    settlements: i64,
    unknowns: i64,
}
async fn rows(world: &World, id: Uuid) -> Rows {
    let pool = &world.fixture.store.pool;
    let (reservation, held, actual): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT state,held_microusd,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let (execution, input, output): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT state,input_tokens,output_tokens FROM inference_executions WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let (settlements, unknowns): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE kind='settlement'),count(*) FILTER (WHERE kind='unknown') FROM monetary_ledger WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    Rows {
        reservation,
        held,
        actual,
        execution,
        input,
        output,
        settlements,
        unknowns,
    }
}
/// Waits for the reservation to leave `pending`, which only a settlement
/// or a reconciliation does.
async fn resolved(world: &World, id: Uuid) -> Rows {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = rows(world, id).await;
        if found.reservation != "pending" || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_settled_with_reported_usage(found: &Rows) {
    assert_eq!(found.reservation, "settled");
    assert_eq!(found.execution, "succeeded");
    // 3 input and 1 output token at 1 micro-USD per token.
    assert_eq!(found.actual, Some(4));
    assert_eq!((found.input, found.output), (Some(3), Some(1)));
    assert_eq!((found.settlements, found.unknowns), (1, 0));
}

/// The client disconnects while a non-streamed success is being settled.
#[dbtest::test]
async fn a_dropped_request_still_settles_a_complete_response(pool: PgPool) {
    let world = Arc::new(world(pool, Behaviour::Complete, true).await);
    let id = Uuid::now_v7();
    let request_task = tokio::spawn({
        let world = world.clone();
        async move {
            world
                .engine
                .execute(world.fixture.principal, request(), id)
                .await
        }
    });
    let waiting = settlement_is_waiting(&world.fixture.store.pool).await;
    request_task.abort();
    assert!(matches!(request_task.await, Err(error) if error.is_cancelled()));
    release(&world, waiting).await;
    assert_settled_with_reported_usage(&resolved(&world, id).await);
}

/// The client disconnects after the stream delivered its final usage, while
/// the settlement at `Done` is in flight.
#[dbtest::test]
async fn a_dropped_stream_still_settles_after_its_final_usage(pool: PgPool) {
    let world = Arc::new(world(pool, Behaviour::Stream, true).await);
    let id = Uuid::now_v7();
    let mut streamed = request();
    streamed.stream = true;
    let ProviderOutput::Stream(mut output) = world
        .engine
        .execute(world.fixture.principal, streamed, id)
        .await
        .unwrap()
    else {
        panic!("expected a stream")
    };
    let consumer = tokio::spawn(async move { while output.next().await.is_some() {} });
    let waiting = settlement_is_waiting(&world.fixture.store.pool).await;
    consumer.abort();
    assert!(matches!(consumer.await, Err(error) if error.is_cancelled()));
    release(&world, waiting).await;
    assert_settled_with_reported_usage(&resolved(&world, id).await);
}

/// A drop before the upstream outcome is complete is unchanged: the
/// cancellation settlement keeps the hold as unknown cost and records no
/// usage.
#[dbtest::test]
async fn a_stream_dropped_before_its_final_usage_stays_unknown_with_its_hold(pool: PgPool) {
    let world = world(pool, Behaviour::Stalled, false).await;
    let id = Uuid::now_v7();
    let mut streamed = request();
    streamed.stream = true;
    let ProviderOutput::Stream(mut output) = world
        .engine
        .execute(world.fixture.principal, streamed, id)
        .await
        .unwrap()
    else {
        panic!("expected a stream")
    };
    assert!(matches!(
        output.next().await,
        Some(Ok(ChatEvent::Delta { .. }))
    ));
    drop(output);
    let found = resolved(&world, id).await;
    assert_eq!(found.reservation, "unknown");
    assert_eq!(found.execution, "cancelled");
    assert!(found.held.is_some_and(|held| held > 0), "the hold is kept");
    assert_eq!(found.actual, None);
    assert_eq!((found.input, found.output), (None, None));
    assert_eq!((found.settlements, found.unknowns), (0, 1));
}

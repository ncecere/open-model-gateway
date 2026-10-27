//! #370: request-owned capture only; no database, exporter, or process globals.
use super::*;
use std::future::{Future, pending};
use std::sync::{Arc, mpsc};
use std::task::Poll;
use std::time::Duration;

#[tokio::test]
async fn concurrent_request_scopes_keep_events_isolated() {
    let first = Handle::new();
    let second = Handle::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_handle = first.clone();
    let first_barrier = barrier.clone();
    let first_task = tokio::spawn(async move {
        scope(first_handle, async move {
            first_barrier.wait().await;
            sql(Stage::GovernanceStorage, async { Ok::<_, sqlx::Error>(7) })
                .await
                .unwrap()
        })
        .await
    });
    let second_handle = second.clone();
    let second_barrier = barrier.clone();
    let second_task = tokio::spawn(async move {
        scope(second_handle, async move {
            second_barrier.wait().await;
            sql(Stage::ExplicitSettlement, async {
                Err::<(), _>(sqlx::Error::PoolClosed)
            })
            .await
            .unwrap_err()
        })
        .await
    });
    // Both scopes are active before either operation can finish. The parent
    // has no request context, and no scheduler delay is used as an oracle.
    barrier.wait().await;
    assert!(current().is_none());
    assert_eq!(first_task.await.unwrap(), 7);
    assert!(matches!(
        second_task.await.unwrap(),
        sqlx::Error::PoolClosed
    ));
    let first_snapshot = first.snapshot();
    let second_snapshot = second.snapshot();
    assert_eq!(first_snapshot.events.len(), 1);
    assert_eq!(second_snapshot.events.len(), 1);
    assert_eq!(first_snapshot.events[0].stage, Stage::GovernanceStorage);
    assert_eq!(first_snapshot.events[0].outcome, Outcome::Success);
    assert_eq!(second_snapshot.events[0].stage, Stage::ExplicitSettlement);
    assert_eq!(second_snapshot.events[0].outcome, Outcome::Error);
    for snapshot in [first_snapshot, second_snapshot] {
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.dropped, 0);
        assert!(!snapshot.unavailable);
    }
}

#[tokio::test]
async fn unrelated_tokio_spawn_does_not_inherit_request_capture() {
    let handle = Handle::new();
    scope(handle.clone(), async {
        assert!(current().is_some());
        tokio::spawn(async {
            assert!(current().is_none());
            sql(Stage::Setup, async { Ok::<_, sqlx::Error>(()) })
                .await
                .unwrap();
        })
        .await
        .unwrap();
        sql(Stage::GovernanceStorage, async { Ok::<_, sqlx::Error>(()) })
            .await
            .unwrap();
    })
    .await;
    assert!(current().is_none());
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].stage, Stage::GovernanceStorage);
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.dropped, 0);
    assert!(!snapshot.unavailable);
}

#[tokio::test]
async fn inherit_registers_before_poll_and_unpolled_drop_clears_pending() {
    let handle = Handle::new();
    let mut inherited = None;
    scope(handle.clone(), async {
        inherited = Some(inherit(pending::<()>()));
        assert_eq!(handle.snapshot().pending, 1);
    })
    .await;
    assert!(current().is_none());
    assert_eq!(handle.snapshot().pending, 1);
    drop(inherited.take().unwrap());
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.pending, 0);
    assert!(
        snapshot.events.is_empty(),
        "an unpolled task has no SQL stage"
    );
}

#[tokio::test]
async fn inherit_captures_the_construction_scope_before_spawn() {
    let handle = Handle::new();
    let mut inherited = None;
    scope(handle.clone(), async {
        inherited = Some(inherit(async {
            assert!(current().is_some());
            sql(Stage::Setup, async { Ok::<_, sqlx::Error>(19) }).await
        }));
    })
    .await;
    assert!(current().is_none());
    assert_eq!(handle.snapshot().pending, 1);
    assert_eq!(
        tokio::spawn(inherited.take().unwrap())
            .await
            .unwrap()
            .unwrap(),
        19
    );
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].stage, Stage::Setup);
    assert_eq!(snapshot.events[0].outcome, Outcome::Success);
}

#[tokio::test]
async fn inherit_without_an_owner_masks_a_later_unrelated_scope() {
    assert!(current().is_none());
    let inherited = inherit(async {
        assert!(
            current().is_none(),
            "unobserved work must not adopt its poller's recorder"
        );
        observe_sqlx(Stage::Setup, &sqlx::Error::PoolClosed);
    });
    let unrelated = Handle::new();
    scope(unrelated.clone(), inherited).await;
    assert!(unrelated.snapshot().events.is_empty());
}

#[tokio::test]
async fn settlement_without_an_owner_masks_a_later_unrelated_scope() {
    let future = settlement(None, Stage::ExplicitSettlement, async {
        assert!(
            current().is_none(),
            "an unobserved guard must not adopt another request"
        );
        observe_sqlx(Stage::GovernanceStorage, &sqlx::Error::PoolClosed);
        Ok::<_, tokio::time::error::Elapsed>(Ok::<(), crate::inference::error::InferenceError>(()))
    });
    let unrelated = Handle::new();
    scope(unrelated.clone(), future).await.unwrap().unwrap();
    assert!(unrelated.snapshot().events.is_empty());
}

#[test]
fn snapshot_marks_completion_between_copy_and_counter_capture_incomplete() {
    let handle = Handle::new();
    let operation = Operation::new(handle.clone(), Stage::Setup);
    let go = Arc::new(std::sync::Barrier::new(2));
    let worker_go = go.clone();
    let (send, receive) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        worker_go.wait();
        operation.finish(Outcome::Success, None);
        send.send(()).unwrap();
    });
    let raced = handle.snapshot_after_copy(|| {
        go.wait();
        receive
            .recv_timeout(Duration::from_secs(10))
            .expect("completion did not reach the forced snapshot gap");
    });
    worker.join().unwrap();
    assert!(
        raced.events.is_empty(),
        "completion happens after this snapshot's copy"
    );
    assert!(
        raced.unavailable || raced.pending > 0 || raced.dropped > 0,
        "omitted completion must never look like complete, available evidence"
    );
    let after = handle.snapshot();
    assert!(!after.unavailable);
    assert_eq!(
        (after.pending, after.dropped, after.events.len()),
        (0, 0, 1)
    );
}

#[tokio::test]
async fn disabled_capture_has_no_current_observer_and_preserves_results() {
    // No Handle is constructed. Do not use global allocator instrumentation:
    // this checks the observable disabled path, not unrelated runtime allocation.
    assert!(current().is_none());
    let inherited = inherit(async {
        assert!(current().is_none());
        sql(Stage::Setup, async { Ok::<_, sqlx::Error>(23) }).await
    });
    assert_eq!(inherited.await.unwrap(), 23);
    observe_sqlx(Stage::GovernanceStorage, &sqlx::Error::PoolClosed);
    assert!(current().is_none());
}

#[tokio::test]
async fn sixty_fifth_event_is_dropped_at_the_fixed_capacity() {
    assert_eq!(CAPACITY, 64);
    let handle = Handle::new();
    scope(handle.clone(), async {
        for _ in 0..65 {
            observe_sqlx(Stage::GovernanceStorage, &sqlx::Error::PoolClosed);
        }
    })
    .await;
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.events.len(), 64);
    assert_eq!(snapshot.dropped, 1);
    assert_eq!(snapshot.pending, 0);
    assert!(!snapshot.unavailable);
}

#[test]
fn contended_recording_drops_explicitly_and_snapshot_does_not_block() {
    let handle = Handle::new();
    let guard = handle.0.events.lock().unwrap();
    let worker_handle = handle.clone();
    let (send, receive) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(scope(worker_handle.clone(), async {
            observe_sqlx(Stage::GovernanceStorage, &sqlx::Error::PoolClosed);
        }));
        send.send(worker_handle.snapshot()).unwrap();
    });
    // Release before asserting/joining even when the implementation blocks;
    // this makes a blocking regression fail rather than strand the worker.
    let observed = receive.recv_timeout(Duration::from_secs(10));
    drop(guard);
    worker.join().unwrap();
    let snapshot = observed.expect("recording or snapshot blocked on the event mutex");
    assert!(snapshot.unavailable);
    assert_eq!(snapshot.dropped, 1);
    assert_eq!(snapshot.pending, 0);
    assert!(snapshot.events.is_empty());
    assert_eq!(handle.snapshot().dropped, 1);
}

#[tokio::test]
async fn poisoned_event_buffer_is_reported_unavailable_without_panicking() {
    let handle = Handle::new();
    let poison_handle = handle.clone();
    let poisoned = std::thread::spawn(move || {
        let _guard = poison_handle.0.events.lock().unwrap();
        panic!("intentional test-only event buffer poison");
    });
    assert!(poisoned.join().is_err());
    assert!(handle.snapshot().unavailable);
    scope(handle.clone(), async {
        observe_sqlx(Stage::Setup, &sqlx::Error::PoolClosed);
    })
    .await;
    let snapshot = handle.snapshot();
    assert!(snapshot.unavailable);
    assert!(snapshot.dropped >= 1);
    assert_eq!(snapshot.pending, 0);
}

#[tokio::test]
async fn pool_closed_is_a_fixed_category_without_sqlstate() {
    let handle = Handle::new();
    let result = scope(
        handle.clone(),
        sql(Stage::PoolAcquireAndBegin, async {
            Err::<(), _>(sqlx::Error::PoolClosed)
        }),
    )
    .await;
    assert!(matches!(result, Err(sqlx::Error::PoolClosed)));
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.events.len(), 1);
    let event = &snapshot.events[0];
    assert_eq!(event.stage, Stage::PoolAcquireAndBegin);
    assert_eq!(event.outcome, Outcome::Error);
    assert_eq!(event.sqlx_error, Some(SqlxKind::PoolClosed));
    assert_eq!(event.sqlstate, None);
    assert_eq!(snapshot.pending, 0);
}

#[tokio::test]
async fn protocol_error_message_is_returned_but_never_serialized() {
    const CANARY: &str = "secret-sql-id-and-message-canary-370";
    let handle = Handle::new();
    let error = scope(
        handle.clone(),
        sql(Stage::GovernanceStorage, async {
            Err::<(), _>(sqlx::Error::Protocol(CANARY.to_owned()))
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, sqlx::Error::Protocol(ref message) if message == CANARY));
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.events[0].sqlx_error, Some(SqlxKind::Protocol));
    assert_eq!(snapshot.events[0].sqlstate, None);
    let json = serde_json::to_value(&snapshot).unwrap();
    assert!(!json.to_string().contains(CANARY));
    let event = json["events"][0].as_object().unwrap();
    assert_eq!(
        event.len(),
        5,
        "only the bounded diagnostic event fields belong here"
    );
    for field in ["stage", "outcome", "duration_ns", "sqlx_error", "sqlstate"] {
        assert!(event.contains_key(field), "missing event field {field}");
    }
    assert_eq!(event["stage"], "governance_storage");
    assert_eq!(event["outcome"], "error");
    assert_eq!(event["sqlx_error"], "protocol");
    assert!(event["duration_ns"].as_u64().is_some());
}

#[tokio::test]
async fn dropping_a_polled_sql_future_records_cancellation_not_success() {
    let handle = Handle::new();
    scope(handle.clone(), async {
        let mut operation = Box::pin(sql(
            Stage::CancellationSettlement,
            pending::<Result<(), sqlx::Error>>(),
        ));
        std::future::poll_fn(|cx| {
            assert!(matches!(operation.as_mut().poll(cx), Poll::Pending));
            Poll::Ready(())
        })
        .await;
        let running = handle.snapshot();
        assert!(running.pending > 0);
        assert!(running.events.is_empty());
        drop(operation);
    })
    .await;
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.events.len(), 1);
    let event = &snapshot.events[0];
    assert_eq!(event.stage, Stage::CancellationSettlement);
    assert_eq!(event.outcome, Outcome::Cancelled);
    assert_eq!(event.sqlx_error, None);
    assert_eq!(event.sqlstate, None);
}

// Only the driver boundary is synthetic here: malformed codes cannot normally
// be produced by PostgreSQL, but must not bypass the capture's validation.
#[derive(Debug)]
struct SyntheticDatabaseError(&'static str);

impl std::fmt::Display for SyntheticDatabaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("database-message-canary-370")
    }
}

impl std::error::Error for SyntheticDatabaseError {}

impl sqlx::error::DatabaseError for SyntheticDatabaseError {
    fn message(&self) -> &str {
        "database-message-canary-370"
    }

    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed(self.0))
    }

    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::Other
    }
}

#[tokio::test]
async fn database_sqlstate_requires_exactly_five_uppercase_ascii_letters_or_digits() {
    for (code, expected) in [
        ("22012", Some("22012")),
        ("42P01", Some("42P01")),
        ("", None),
        ("2201", None),
        ("220120", None),
        ("22p01", None),
        ("22 01", None),
        ("22é2", None), // five bytes, but not five ASCII SQLSTATE characters
    ] {
        let handle = Handle::new();
        let error = sqlx::Error::Database(Box::new(SyntheticDatabaseError(code)));
        scope(handle.clone(), async {
            observe_sqlx(Stage::GovernanceStorage, &error);
        })
        .await;
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].sqlx_error, Some(SqlxKind::Database));
        assert_eq!(
            snapshot.events[0].sqlstate.as_deref(),
            expected,
            "code {code:?}"
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("database-message-canary-370")
        );
    }
}

#[test]
fn diagnostic_enums_serialize_as_fixed_snake_case_labels() {
    let stages = [
        Stage::PoolAcquireAndBegin,
        Stage::Setup,
        Stage::ScopeAudit,
        Stage::GovernanceStorage,
        Stage::ExplicitSettlement,
        Stage::CancellationSettlement,
    ];
    assert_eq!(
        serde_json::to_value(stages).unwrap(),
        serde_json::json!([
            "pool_acquire_and_begin",
            "setup",
            "scope_audit",
            "governance_storage",
            "explicit_settlement",
            "cancellation_settlement"
        ])
    );
    assert_eq!(
        serde_json::to_value([
            Outcome::Success,
            Outcome::Error,
            Outcome::Cancelled,
            Outcome::Timeout
        ])
        .unwrap(),
        serde_json::json!(["success", "error", "cancelled", "timeout"])
    );
    assert_eq!(
        serde_json::to_value([
            SqlxKind::Database,
            SqlxKind::PoolClosed,
            SqlxKind::PoolTimedOut,
            SqlxKind::Io,
            SqlxKind::Tls,
            SqlxKind::Protocol,
            SqlxKind::WorkerCrashed,
            SqlxKind::Other
        ])
        .unwrap(),
        serde_json::json!([
            "database",
            "pool_closed",
            "pool_timed_out",
            "io",
            "tls",
            "protocol",
            "worker_crashed",
            "other"
        ])
    );
}

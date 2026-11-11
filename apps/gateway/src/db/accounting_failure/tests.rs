//! #389: the production accounting-failure signal. No database: the driver
//! boundary is synthetic here; `inference::steering_tests` drives the real faults.
use super::*;
use crate::db::diagnostics::{self, Handle};
use std::collections::BTreeSet;

const CANARY: &str = "accounting-secret-canary-389";

#[derive(Debug)]
struct SyntheticDatabaseError(Option<&'static str>);
impl std::fmt::Display for SyntheticDatabaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(CANARY)
    }
}
impl std::error::Error for SyntheticDatabaseError {}
impl sqlx::error::DatabaseError for SyntheticDatabaseError {
    fn message(&self) -> &str {
        CANARY
    }
    fn code(&self) -> Option<Cow<'_, str>> {
        self.0.map(Cow::Borrowed)
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
fn database(code: Option<&'static str>) -> sqlx::Error {
    sqlx::Error::Database(Box::new(SyntheticDatabaseError(code)))
}

const DEADLINES: [Cause; 3] = [
    Cause::DeadlineInQueue,
    Cause::DeadlineAcquiringConnection,
    Cause::DeadlineInDatabase,
];
const STAGES: [&str; 5] = [
    "pool_acquire_and_begin",
    "governance_storage",
    "route_lookup",
    "explicit_settlement",
    "cancellation_settlement",
];
const CAUSES: [&str; 17] = [
    "pool_timeout",
    "pool_closed",
    "lock_timeout",
    "statement_timeout",
    "backend_terminated",
    "deadlock",
    "read_only_transaction",
    "too_many_connections",
    "other_database",
    "io",
    "tls",
    "protocol",
    "worker_crashed",
    "other",
    "deadline_in_queue",
    "deadline_acquiring_connection",
    "deadline_in_database",
];

/// The accounting-failure log lines in `text`, as (stage, cause). Every such
/// line, whoever wrote it, must carry exactly the two closed fields.
pub(crate) fn signal_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|line| line["target"] == "open_model_gateway::db::accounting_failure")
        .map(|line| {
            assert_eq!(line["level"], "WARN");
            let fields = line["fields"].as_object().expect("structured fields");
            let keys: BTreeSet<_> = fields.keys().map(String::as_str).collect();
            assert_eq!(keys, BTreeSet::from(["cause", "message", "stage"]));
            assert_eq!(fields["message"], "accounting unavailable");
            let stage = fields["stage"].as_str().unwrap().to_owned();
            let cause = fields["cause"].as_str().unwrap().to_owned();
            assert!(
                STAGES.contains(&stage.as_str()),
                "stage outside the closed set"
            );
            assert!(
                CAUSES.contains(&cause.as_str()),
                "cause outside the closed set"
            );
            (stage, cause)
        })
        .collect()
}

/// The (stage, cause) label pairs of every exported series of the family.
/// Fails on any other label key or any value outside the closed sets.
pub(crate) fn exported_series(exposition: &str) -> Vec<(String, String, u64)> {
    exposition
        .lines()
        .filter_map(|line| line.strip_prefix("gateway_accounting_failures_total{"))
        .map(|line| {
            let (labels, value) = line.split_once("} ").expect("labels then value");
            let labels: Vec<_> = labels
                .split(',')
                .map(|label| {
                    let (key, value) = label.split_once('=').expect("key=value");
                    (key, value.trim_matches('"'))
                })
                .collect();
            assert_eq!(labels.len(), 2, "exactly two labels");
            assert_eq!((labels[0].0, labels[1].0), ("stage", "cause"));
            assert!(
                STAGES.contains(&labels[0].1),
                "stage outside the closed set"
            );
            assert!(
                CAUSES.contains(&labels[1].1),
                "cause outside the closed set"
            );
            (
                labels[0].1.to_owned(),
                labels[1].1.to_owned(),
                value.parse().expect("counter value"),
            )
        })
        .collect()
}

#[test]
fn label_values_are_exactly_the_closed_sets() {
    assert_eq!(Stage::ALL.map(Stage::as_str), STAGES);
    assert_eq!(Cause::ALL.map(Cause::as_str), CAUSES);
    for value in STAGES.iter().chain(&CAUSES) {
        assert!(
            !value.is_empty()
                && value.len() <= 32
                && value.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "a label value is lowercase ASCII words"
        );
    }
    assert_eq!(BTreeSet::from(STAGES).len(), STAGES.len());
    assert_eq!(BTreeSet::from(CAUSES).len(), CAUSES.len());
}

#[test]
fn classification_names_the_known_failures_and_buckets_everything_else() {
    for (error, expected) in [
        (database(Some("55P03")), Cause::LockTimeout),
        (database(Some("57014")), Cause::StatementTimeout),
        (database(Some("57P01")), Cause::BackendTerminated),
        (database(Some("40P01")), Cause::Deadlock),
        (database(Some("25006")), Cause::ReadOnlyTransaction),
        (database(Some("53300")), Cause::TooManyConnections),
        // The neighbours of the named codes stay in the fixed bucket.
        (database(Some("25001")), Cause::OtherDatabase),
        (database(Some("53400")), Cause::OtherDatabase),
        // Unrecognised, malformed or missing codes: one fixed bucket, never the code.
        (database(Some("23505")), Cause::OtherDatabase),
        (database(Some("XX000")), Cause::OtherDatabase),
        (database(Some("55p03")), Cause::OtherDatabase),
        (database(Some("55P03 ")), Cause::OtherDatabase),
        (database(Some("57014\n")), Cause::OtherDatabase),
        (database(Some("")), Cause::OtherDatabase),
        (database(Some(CANARY)), Cause::OtherDatabase),
        (database(None), Cause::OtherDatabase),
        (sqlx::Error::PoolTimedOut, Cause::PoolTimeout),
        (sqlx::Error::PoolClosed, Cause::PoolClosed),
        (sqlx::Error::Io(std::io::Error::other(CANARY)), Cause::Io),
        (sqlx::Error::Tls(CANARY.into()), Cause::Tls),
        (sqlx::Error::Protocol(CANARY.into()), Cause::Protocol),
        (sqlx::Error::WorkerCrashed, Cause::WorkerCrashed),
        (sqlx::Error::RowNotFound, Cause::Other),
        (sqlx::Error::ColumnNotFound(CANARY.into()), Cause::Other),
    ] {
        let cause = classify(&error);
        assert_eq!(cause, expected);
        assert!(Cause::ALL.contains(&cause));
        assert!(
            !DEADLINES.contains(&cause),
            "a driver error is never a settlement deadline"
        );
    }
}

#[tokio::test]
async fn a_failure_is_counted_and_logged_with_closed_labels_and_no_error_text() {
    use crate::metrics::METRICS;
    let capture = crate::log_capture_test::Capture::start();
    let before = [
        METRICS.accounting_failure_count(Stage::GovernanceStorage, Cause::OtherDatabase),
        METRICS.accounting_failure_count(Stage::GovernanceStorage, Cause::Protocol),
        METRICS.accounting_failure_count(Stage::PoolAcquireAndBegin, Cause::PoolTimeout),
        METRICS.accounting_failure_count(Stage::GovernanceStorage, Cause::Deadlock),
    ];
    // No other test of this binary produces these two pairs (a TLS failure of
    // a route lookup, a crashed driver worker at a transaction start), so
    // their counts are exact while the capture is exclusive.
    let only_here = [
        (Stage::RouteLookup, Cause::Tls),
        (Stage::PoolAcquireAndBegin, Cause::WorkerCrashed),
    ];
    let only_here_before = only_here.map(|(s, c)| METRICS.accounting_failure_count(s, c));
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        storage_error(&database(Some("99ZZ9")));
        storage_error(&sqlx::Error::Protocol(CANARY.into()));
        begin_error(&sqlx::Error::PoolTimedOut);
        deadlock_exhausted();
        route_lookup_error(&sqlx::Error::Tls(CANARY.into()));
        begin_error(&sqlx::Error::WorkerCrashed);
    })
    .await;
    let expected = [
        (Stage::GovernanceStorage, Cause::OtherDatabase),
        (Stage::GovernanceStorage, Cause::Protocol),
        (Stage::PoolAcquireAndBegin, Cause::PoolTimeout),
        (Stage::GovernanceStorage, Cause::Deadlock),
        (Stage::RouteLookup, Cause::Tls),
        (Stage::PoolAcquireAndBegin, Cause::WorkerCrashed),
    ];
    assert_eq!(handle.signals(), expected);
    for ((stage, cause), before) in only_here.into_iter().zip(only_here_before) {
        assert_eq!(
            METRICS.accounting_failure_count(stage, cause),
            before + 1,
            "one failure is one increment"
        );
        let pair = (stage.as_str().to_owned(), cause.as_str().to_owned());
        assert_eq!(
            signal_lines(&capture.text())
                .iter()
                .filter(|line| **line == pair)
                .count(),
            1,
            "one failure is one WARN line"
        );
    }
    for ((stage, cause), before) in expected.into_iter().zip(before) {
        assert!(
            METRICS.accounting_failure_count(stage, cause) > before,
            "the production counter moved"
        );
    }
    let exposition = METRICS.render(None).await;
    let series = exported_series(&exposition);
    let logged = signal_lines(&capture.text());
    for (stage, cause) in expected {
        let pair = (stage.as_str().to_owned(), cause.as_str().to_owned());
        assert!(logged.contains(&pair), "one WARN line per failure");
        assert!(
            series
                .iter()
                .any(|(s, c, n)| (s, c) == (&pair.0, &pair.1) && *n >= 1),
            "the series is exported"
        );
    }
    for rendered in [exposition, capture.text()] {
        assert!(!rendered.contains(CANARY), "no driver or database message");
        assert!(!rendered.contains("99ZZ9"), "no raw SQLSTATE");
    }
}

/// The recorder itself, on a private metrics instance: one increment and one
/// line for one call, exactly.
#[tokio::test]
async fn record_counts_once_and_logs_one_line() {
    let capture = crate::log_capture_test::Capture::start();
    let metrics = crate::metrics::Metrics::new();
    // A pair no code path produces (a settlement deadline is never a TLS
    // error), so no concurrent test writes the same line.
    let (stage, cause) = (Stage::CancellationSettlement, Cause::Tls);
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        record_to(&metrics, stage, cause);
    })
    .await;
    assert_eq!(metrics.accounting_failure_count(stage, cause), 1);
    assert_eq!(
        exported_series(&metrics.render(None).await),
        [(stage.as_str().to_owned(), cause.as_str().to_owned(), 1)]
    );
    assert_eq!(handle.signals(), [(stage, cause)]);
    let pair = (stage.as_str().to_owned(), cause.as_str().to_owned());
    assert_eq!(
        signal_lines(&capture.text())
            .iter()
            .filter(|line| **line == pair)
            .count(),
        1,
        "one WARN line for one failure"
    );
}

/// A statement run straight on the pool: no connection is a failed start,
/// anything else a failed statement.
#[tokio::test]
async fn a_pool_statement_failure_without_a_connection_is_a_failed_start() {
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        pool_statement_error(&sqlx::Error::PoolTimedOut);
        pool_statement_error(&sqlx::Error::PoolClosed);
        pool_statement_error(&database(Some("57014")));
        pool_statement_error(&sqlx::Error::Io(std::io::Error::other(CANARY)));
    })
    .await;
    assert_eq!(
        handle.signals(),
        [
            (Stage::PoolAcquireAndBegin, Cause::PoolTimeout),
            (Stage::PoolAcquireAndBegin, Cause::PoolClosed),
            (Stage::GovernanceStorage, Cause::StatementTimeout),
            (Stage::GovernanceStorage, Cause::Io),
        ]
    );
}

/// An elapsed settlement deadline names what the settlement was waiting on,
/// as the settlement itself last marked it.
#[tokio::test]
async fn an_elapsed_settlement_deadline_names_what_the_settlement_waited_on() {
    type Settled = Result<(), crate::inference::error::InferenceError>;
    // Outside a settlement there is nothing to mark, and nothing breaks.
    settlement_waiting(Waiting::Database);
    let hung = |marks: &'static [Waiting]| {
        tokio::time::timeout(std::time::Duration::from_millis(1), async move {
            for mark in marks {
                settlement_waiting(*mark);
            }
            std::future::pending::<Settled>().await
        })
    };
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        for marks in [
            &[][..],
            &[Waiting::Queue],
            &[Waiting::Queue, Waiting::Connection],
            &[Waiting::Queue, Waiting::Connection, Waiting::Database],
            // A deadlock re-run queues again.
            &[
                Waiting::Queue,
                Waiting::Connection,
                Waiting::Database,
                Waiting::Queue,
            ],
        ] {
            assert!(explicit_settlement(hung(marks)).await.is_err());
        }
        assert!(
            cancellation_settlement(hung(&[Waiting::Connection]))
                .await
                .is_err()
        );
        // Each settlement starts again in the queue: no mark leaks from the last.
        assert!(cancellation_settlement(hung(&[])).await.is_err());
    })
    .await;
    assert_eq!(
        handle.signals(),
        [
            (Stage::ExplicitSettlement, Cause::DeadlineInQueue),
            (Stage::ExplicitSettlement, Cause::DeadlineInQueue),
            (
                Stage::ExplicitSettlement,
                Cause::DeadlineAcquiringConnection
            ),
            (Stage::ExplicitSettlement, Cause::DeadlineInDatabase),
            (Stage::ExplicitSettlement, Cause::DeadlineInQueue),
            (
                Stage::CancellationSettlement,
                Cause::DeadlineAcquiringConnection
            ),
            (Stage::CancellationSettlement, Cause::DeadlineInQueue),
        ]
    );
}

#[tokio::test]
async fn a_settlement_deadline_is_recorded_only_when_it_elapses() {
    type Settled = Result<(), crate::inference::error::InferenceError>;
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        let done =
            tokio::time::timeout(std::time::Duration::from_secs(3), async { Settled::Ok(()) });
        assert!(matches!(explicit_settlement(done).await, Ok(Ok(()))));
        // A settlement that failed in time was already recorded where it failed.
        let failed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            Settled::Err(crate::inference::error::InferenceError::Storage)
        });
        assert!(matches!(cancellation_settlement(failed).await, Ok(Err(_))));
    })
    .await;
    assert!(
        handle.signals().is_empty(),
        "nothing recorded without a timeout"
    );
    diagnostics::scope(handle.clone(), async {
        let hung = || {
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                std::future::pending::<Settled>(),
            )
        };
        assert!(explicit_settlement(hung()).await.is_err());
        assert!(cancellation_settlement(hung()).await.is_err());
    })
    .await;
    assert_eq!(
        handle.signals(),
        [
            (Stage::ExplicitSettlement, Cause::DeadlineInQueue),
            (Stage::CancellationSettlement, Cause::DeadlineInQueue),
        ]
    );
}

/// A store whose pool is closed: every statement fails without a database.
async fn closed_store() -> crate::store::Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    pool.close().await;
    crate::store::Store::new(pool)
}

/// The reads before admission (the first database work of a request) name
/// their stage; before, `503 accounting_unavailable` from here had no signal.
#[tokio::test]
async fn a_failed_route_lookup_is_recorded_at_its_own_stage() {
    use crate::inference::{error::InferenceError, repository::InferenceRepository};
    let store = closed_store().await;
    let principal = crate::auth::Principal {
        key_id: uuid::Uuid::new_v4(),
        workspace_id: uuid::Uuid::new_v4(),
        user_id: None,
    };
    let candidate = crate::inference::types::Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: "upstream".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
        supported_protocols: Vec::new(),
    };
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        assert!(matches!(
            store.deployments(&principal, "company/smart").await,
            Err(InferenceError::Storage)
        ));
        assert!(matches!(
            store
                .route_plan(
                    &principal,
                    "company/smart",
                    &[candidate],
                    uuid::Uuid::new_v4()
                )
                .await,
            Err(InferenceError::Storage)
        ));
    })
    .await;
    assert_eq!(
        handle.signals(),
        [
            (Stage::RouteLookup, Cause::PoolClosed),
            (Stage::RouteLookup, Cause::PoolClosed),
        ]
    );
}

/// Accounting statements run straight on the pool: without a connection the
/// failure is a failed start, not a later statement.
#[tokio::test]
async fn a_pool_statement_without_a_connection_is_recorded_as_a_failed_start() {
    use crate::inference::{error::InferenceError, repository::InferenceRepository};
    let store = closed_store().await;
    let id = uuid::Uuid::new_v4();
    let start = crate::inference::repository::ExecutionStart {
        id,
        root_request_id: id,
        attempt_number: 1,
        principal: crate::auth::Principal {
            key_id: uuid::Uuid::new_v4(),
            workspace_id: uuid::Uuid::new_v4(),
            user_id: None,
        },
        deployment_id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        model: "company/smart".into(),
        streamed: false,
        upstream_model: None,
        client: Default::default(),
    };
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        assert!(matches!(
            store.realtime_window_output(id, 1).await,
            Err(InferenceError::Storage)
        ));
        assert!(matches!(
            crate::governance::batch::batch_cost(&store, id).await,
            Err(InferenceError::Storage)
        ));
        assert!(matches!(
            store.start(&start).await,
            Err(InferenceError::Storage)
        ));
    })
    .await;
    assert_eq!(
        handle.signals(),
        [
            (Stage::PoolAcquireAndBegin, Cause::PoolClosed),
            (Stage::PoolAcquireAndBegin, Cause::PoolClosed),
            (Stage::PoolAcquireAndBegin, Cause::PoolClosed),
        ]
    );
}

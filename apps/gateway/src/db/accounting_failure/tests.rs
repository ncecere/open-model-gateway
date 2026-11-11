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

const STAGES: [&str; 4] = [
    "pool_acquire_and_begin",
    "governance_storage",
    "explicit_settlement",
    "cancellation_settlement",
];
const CAUSES: [&str; 13] = [
    "pool_timeout",
    "pool_closed",
    "lock_timeout",
    "statement_timeout",
    "backend_terminated",
    "deadlock",
    "other_database",
    "io",
    "tls",
    "protocol",
    "worker_crashed",
    "other",
    "deadline_exceeded",
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
        assert_ne!(cause, Cause::DeadlineExceeded);
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
    let handle = Handle::new();
    diagnostics::scope(handle.clone(), async {
        storage_error(&database(Some("99ZZ9")));
        storage_error(&sqlx::Error::Protocol(CANARY.into()));
        begin_error(&sqlx::Error::PoolTimedOut);
        deadlock_exhausted();
    })
    .await;
    let expected = [
        (Stage::GovernanceStorage, Cause::OtherDatabase),
        (Stage::GovernanceStorage, Cause::Protocol),
        (Stage::PoolAcquireAndBegin, Cause::PoolTimeout),
        (Stage::GovernanceStorage, Cause::Deadlock),
    ];
    assert_eq!(handle.signals(), expected);
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
            (Stage::ExplicitSettlement, Cause::DeadlineExceeded),
            (Stage::CancellationSettlement, Cause::DeadlineExceeded),
        ]
    );
}

#[tokio::test]
async fn diagnostic_storage_mapping_preserves_error_and_never_projects_its_message() {
    use crate::db::diagnostics::{self, Handle, Outcome, SqlxKind, Stage};
    let capture = Handle::new();
    let mapped = diagnostics::scope(capture.clone(), async {
        super::storage(sqlx::Error::Protocol("native-secret-canary-370".into()))
    })
    .await;
    assert!(matches!(mapped, super::InferenceError::Storage));
    let snapshot = capture.snapshot();
    assert!(!snapshot.unavailable);
    assert_eq!((snapshot.pending, snapshot.dropped), (0, 0));
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].stage, Stage::GovernanceStorage);
    assert_eq!(snapshot.events[0].outcome, Outcome::Error);
    assert_eq!(snapshot.events[0].sqlx_error, Some(SqlxKind::Protocol));
    assert!(snapshot.events[0].sqlstate.is_none());
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("native-secret-canary-370")
    );
}

/// #389: the same mapping produces the production signal, with closed labels
/// only; a successful mapping-free path produces none.
#[tokio::test]
async fn storage_mapping_records_the_production_accounting_failure_signal() {
    use crate::db::accounting_failure::{Cause, Stage};
    use crate::db::diagnostics::{self, Handle};
    let capture = Handle::new();
    diagnostics::scope(capture.clone(), async {
        assert!(capture.signals().is_empty());
        super::storage(sqlx::Error::Protocol("native-secret-canary-389".into()))
    })
    .await;
    assert_eq!(
        capture.signals(),
        [(Stage::GovernanceStorage, Cause::Protocol)]
    );
}

#[derive(Debug)]
struct Deadlock;
impl std::fmt::Display for Deadlock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deadlock detected")
    }
}
impl std::error::Error for Deadlock {}
impl sqlx::error::DatabaseError for Deadlock {
    fn message(&self) -> &str {
        "deadlock detected"
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some("40P01".into())
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

/// #389: a deadlock victim that is re-run and then succeeds is not an
/// accounting failure; one that is still a victim on its last run is, once.
#[tokio::test]
async fn deadlock_reruns_signal_only_when_exhausted() {
    use crate::db::accounting_failure::{Cause, Stage};
    use crate::db::diagnostics::{self, Handle};
    let deadlock = || super::storage(sqlx::Error::Database(Box::new(Deadlock)));
    let capture = Handle::new();
    let mut runs = 0;
    let recovered: Result<(), super::InferenceError> = diagnostics::scope(capture.clone(), async {
        super::locks::retry_deadlocks!("reconciliation", {
            runs += 1;
            if runs == 1 { Err(deadlock()) } else { Ok(()) }
        })
    })
    .await;
    assert!(recovered.is_ok());
    assert_eq!(runs, 2);
    assert!(
        capture.signals().is_empty(),
        "a successful re-run is not an accounting failure"
    );

    let capture = Handle::new();
    let mut runs = 0;
    let exhausted: Result<(), super::InferenceError> = diagnostics::scope(capture.clone(), async {
        super::locks::retry_deadlocks!("reconciliation", {
            runs += 1;
            Err(deadlock())
        })
    })
    .await;
    assert!(matches!(exhausted, Err(super::InferenceError::Storage)));
    assert_eq!(runs, super::locks::DEADLOCK_ATTEMPTS);
    assert_eq!(
        capture.signals(),
        [(Stage::GovernanceStorage, Cause::Deadlock)]
    );

    // Outside a re-run scope there is no later run: recorded where it failed.
    let capture = Handle::new();
    diagnostics::scope(capture.clone(), async { deadlock() }).await;
    assert_eq!(
        capture.signals(),
        [(Stage::GovernanceStorage, Cause::Deadlock)]
    );
}

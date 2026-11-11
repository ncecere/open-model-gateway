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

/// #389: every accounting transaction start in the governance sources that
/// maps its error to `InferenceError` maps it through `begin_storage`, so a
/// starved or closed pool is recorded as `pool_acquire_and_begin`. The other
/// starts (maintenance) return the driver error unchanged with `.await?`.
/// A start is a call of `db::begin` or `db::begin_with_setup`, however it is
/// named: `crate::db::begin(`, `db::begin(` or an imported `begin(`.
#[test]
fn every_governance_transaction_start_records_the_begin_stage() {
    use std::path::{Path, PathBuf};
    fn sources(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, files);
            } else {
                files.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("governance.rs")];
    sources(&src.join("governance"), &mut files);
    let (mut mapped, mut unmapped) = (0, 0);
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let mut calls: Vec<usize> = ["begin(", "begin_with_setup("]
            .iter()
            .flat_map(|call| source.match_indices(call).map(|(at, _)| at))
            .collect();
        calls.sort_unstable();
        for at in calls {
            let before = &source[..at];
            let line = before.rsplit('\n').next().unwrap();
            // Not a call of the function: part of a longer name, a method of
            // something else, a definition or a comment.
            if before.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.')
                || before.ends_with("fn ")
                || line.trim_start().starts_with("//")
            {
                continue;
            }
            // The whole statement, whatever its formatting.
            let statement: String = source[at..]
                .split(';')
                .next()
                .unwrap()
                .split_whitespace()
                .collect();
            if statement.ends_with(".await?") && !statement.contains("map_err") {
                unmapped += 1;
                continue;
            }
            assert!(
                statement.ends_with(".await.map_err(begin_storage)?"),
                "{name}: a transaction start that is not mapped through begin_storage"
            );
            mapped += 1;
        }
    }
    assert!(mapped >= 16, "found {mapped} starts; did the scan break?");
    assert!(unmapped <= 3, "found {unmapped} unmapped starts");
}

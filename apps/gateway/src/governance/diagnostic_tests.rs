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

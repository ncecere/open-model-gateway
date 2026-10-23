//! Real-PostgreSQL proof of the eligibility decision itself (#98 PR 2):
//! `admit_generation`'s real phase boundaries and the live-revalidated
//! personal/shared distinction, independent of the engine-level mock-upstream
//! tests in `inference::telemetry_tests`.

use super::tests::db::{Fixture, fixture, request};
use super::*;
use crate::{
    inference::repository::{ExecutionStart, InferenceRepository},
    telemetry,
};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

/// Runs one `admit_generation` under the ambient phase bridge, exactly as
/// `inference::mod::execute_protocol` wraps it, and returns the sub-phase
/// spans governance.rs recorded plus any `SharedAdmission` eligibility a
/// commit resolved.
async fn admit_traced(
    f: &Fixture,
    start: &ExecutionStart,
) -> (
    Result<(), InferenceError>,
    Vec<telemetry::Span>,
    Option<telemetry::SharedAdmission>,
) {
    let deployment = f
        .store
        .deployments(&start.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let trace = telemetry::trace_id(start.root_request_id);
    let parent = telemetry::reserve_span_id().unwrap();
    let start_ns = telemetry::now_unix_nanos();
    let buffer = Arc::new(Mutex::new(telemetry::PhaseBuffer::new(
        trace, parent, start_ns,
    )));
    let result = telemetry::with_phases(
        buffer.clone(),
        admit_generation(&f.store, start, &request(), 30, &deployment, u32::MAX),
    )
    .await
    .map(|_| ());
    let Ok(mutex) = Arc::try_unwrap(buffer) else {
        panic!("buffer still shared")
    };
    let (spans, eligibility) = mutex.into_inner().expect("buffer not poisoned").take();
    (result, spans, eligibility)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_personal_admission_commits_without_shared_eligibility(pool: PgPool) {
    let f = fixture(pool).await;
    let start = ExecutionStart {
        principal: f.principal, // the personal workspace key
        ..f.start()
    };
    let (result, spans, eligibility) = admit_traced(&f, &start).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        eligibility.is_none(),
        "a personal-workspace admission must never resolve trace-export eligibility"
    );
    // The real governance.rs phase boundaries still fired: this is a
    // committed admission, just one whose owning request can never export.
    let names: Vec<&str> = spans.iter().map(span_name).collect();
    for phase in [
        "admission.queue",
        "admission.connect",
        "admission.read",
        "admission.totals_locks",
        "admission.limits",
        "admission.write",
        "admission.commit",
    ] {
        assert!(names.contains(&phase), "{phase} missing from {names:?}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_shared_workspace_admission_commits_eligibility_with_the_admitted_alias(pool: PgPool) {
    let f = fixture(pool).await;
    let start = ExecutionStart {
        principal: f.team, // the team (shared) workspace key
        ..f.start()
    };
    let (result, spans, eligibility) = admit_traced(&f, &start).await;
    assert!(result.is_ok(), "{result:?}");
    let eligibility = eligibility.expect("a committed team-workspace admission is eligible");
    assert!(!spans.is_empty());

    // The capability is bound to this exact request and carries the alias
    // admission itself resolved (`company/smart`): Context::admitted_alias
    // is the only way to read it back, and it takes no string argument.
    let (recorder, _rx) = telemetry::Recorder::channel(1.0);
    let context = recorder.begin(start.root_request_id).unwrap();
    let alias = context
        .admitted_alias(&eligibility)
        .expect("eligibility matches this request");
    assert_eq!(alias.as_str(), "company/smart");

    // Bound to the request: another request's admission can never use it.
    let other = recorder.begin(Uuid::new_v4()).unwrap();
    assert!(other.admitted_alias(&eligibility).is_none());
}

fn span_name(span: &telemetry::Span) -> &'static str {
    // Spans only expose their shape via Serialize; a round trip through
    // serde_json is the only way to read the name back out in a test.
    let value = serde_json::to_value(span).unwrap();
    match value["name"].as_str().unwrap() {
        "admission.queue" => "admission.queue",
        "admission.connect" => "admission.connect",
        "admission.catalog_locks" => "admission.catalog_locks",
        "admission.authority_locks" => "admission.authority_locks",
        "admission.read" => "admission.read",
        "admission.totals_locks" => "admission.totals_locks",
        "admission.limits" => "admission.limits",
        "admission.write" => "admission.write",
        "admission.commit" => "admission.commit",
        other => panic!("unexpected span name {other}"),
    }
}

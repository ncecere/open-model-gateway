//! A batch's envelope reservation and its job row are created in one
//! transaction (#640): there is no moment at which the job exists with an
//! envelope that a short admission lease will turn `unknown`, or at which an
//! envelope holds budget for a job that was never created.
use super::*;
use crate::governance::reconcile_expired;

/// Refuse every later *extension* of a reservation's lease, as a process that
/// died (or a database that failed) between creating a batch and a second
/// step that extended its lease would leave it. A lease can still move
/// earlier: the tests use that as their clock.
async fn refuse_lease_extensions(w: &World) {
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_lease_extension_640() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN
                  IF NEW.lease_expires_at > OLD.lease_expires_at THEN
                    RAISE EXCEPTION 'lease extension refused';
                  END IF;
                  RETURN NEW;
                END $$;
           CREATE TRIGGER refuse_lease_extension_640 BEFORE UPDATE ON governance_reservations
             FOR EACH ROW EXECUTE FUNCTION refuse_lease_extension_640();"#,
    )
    .execute(&w.f.store.pool)
    .await
    .unwrap();
}

/// Refuse the job row, as a failing database does at the last step of a create.
async fn refuse_job_rows(w: &World) {
    sqlx::raw_sql(
        r#"CREATE FUNCTION refuse_job_row_640() RETURNS trigger LANGUAGE plpgsql AS
             $$ BEGIN RAISE EXCEPTION 'job row refused'; END $$;
           CREATE TRIGGER refuse_job_row_640 BEFORE INSERT ON async_jobs
             FOR EACH ROW EXECUTE FUNCTION refuse_job_row_640();"#,
    )
    .execute(&w.f.store.pool)
    .await
    .unwrap();
}

/// Ten minutes and a bit pass: every lease is that much closer to its end.
async fn pass_the_admission_lease(w: &World) {
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=lease_expires_at-interval '11 minutes'")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

async fn created(pool: PgPool, native: bool, metadata: Option<Value>) -> (World, JobRow) {
    let w = world(pool, native, false, 1).await;
    refuse_lease_extensions(&w).await;
    let file = w
        .upload(
            &w.f.principal,
            &[
                chat_line("a", "company/smart", "one"),
                chat_line("b", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(
            w.f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            metadata,
        )
        .await
        .unwrap();
    (w, job)
}

/// The envelope outlives the job's poll deadline from the moment the job exists.
async fn lease_covers_the_job(w: &World, job: &JobRow) {
    let (lease, deadline): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as("SELECT r.lease_expires_at,j.poll_deadline_at FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE j.id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert!(
        lease >= deadline + LEASE_GRACE,
        "lease {lease} must cover the poll deadline {deadline} and its grace"
    );
}

/// The crash between admission and the lease extension, for a gateway-run batch:
/// ten minutes later the reconciler leaves the envelope alone and the runner
/// runs every line.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_gateway_batch_whose_lease_was_never_extended_still_runs(pool: PgPool) {
    let (w, job) = created(pool, false, None).await;
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    lease_covers_the_job(&w, &job).await;
    pass_the_admission_lease(&w).await;
    assert_eq!(reconcile_expired(&w.f.store, 100).await.unwrap(), 0);
    assert_eq!(envelope(&w, &job).await, "pending");
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(0))
    );
    assert_eq!(w.openai.executed().len(), 2);
    assert_eq!(
        reservation(&w.f, job.execution_id).await.0,
        "settled",
        "the envelope closes"
    );
    consistent(&w.f).await;
}

/// The same for a native batch: its hold is not turned into unknown cost
/// while the provider still has the batch.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_native_batch_whose_lease_was_never_extended_keeps_a_pending_hold(pool: PgPool) {
    let (w, job) = created(pool, true, None).await;
    assert_eq!(job.mode(), Some(BatchMode::Native));
    lease_covers_the_job(&w, &job).await;
    pass_the_admission_lease(&w).await;
    assert_eq!(reconcile_expired(&w.f.store, 100).await.unwrap(), 0);
    assert_eq!(envelope(&w, &job).await, "pending");
    consistent(&w.f).await;
}

/// A longer completion window lengthens the lease with it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_longer_window_lengthens_the_envelope_lease(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .jobs
        .create_batch(
            w.f.principal,
            Uuid::new_v4(),
            CreateBatch {
                input_file_id: file,
                endpoint: BatchEndpoint::ChatCompletions,
                metadata: None,
                completion_window_hours: Some(168),
            },
        )
        .await
        .unwrap();
    lease_covers_the_job(&w, &job).await;
    let hours: f64 = sqlx::query_scalar("SELECT (extract(epoch FROM (poll_deadline_at-created_at))/3600)::float8 FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert!((169.9..170.1).contains(&hours), "{hours}");
}

async fn envelope(w: &World, job: &JobRow) -> String {
    reservation(&w.f, job.execution_id).await.0
}

/// The job row fails after the envelope was written: the create fails
/// cleanly and nothing is left (no envelope holding budget, no execution, no
/// ledger entry, no private copy of the input).
async fn a_job_row_that_cannot_be_written(pool: PgPool, native: bool, metadata: Option<Value>) {
    let w = world(pool, native, false, 1).await;
    refuse_job_rows(&w).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let files_before = live_files(&w).await;
    let failed = w
        .create(
            w.f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            metadata,
        )
        .await;
    assert!(
        matches!(
            failed,
            Err(CreateError::Job(JobError::Inference(
                InferenceError::Storage
            )))
        ),
        "{failed:?}"
    );
    assert_eq!(admission_rowcounts(&w.f).await, (0, 0, 0, 0));
    assert_eq!(
        live_files(&w).await,
        files_before,
        "the private copy is removed"
    );
    assert!(w.openai.calls().is_empty());
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_native_job_row_that_cannot_be_written_leaves_no_envelope(pool: PgPool) {
    a_job_row_that_cannot_be_written(pool, true, None).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_gateway_job_row_that_cannot_be_written_leaves_no_envelope(pool: PgPool) {
    a_job_row_that_cannot_be_written(pool, false, Some(json!({"omg_mode":"gateway"}))).await;
}

async fn live_files(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE workspace_id=$1 AND deleted_at IS NULL",
    )
    .bind(w.f.principal.workspace_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap()
}

/// A gateway-run batch an earlier release left with an `unknown` envelope
/// (its short lease ran out while the job stayed queued): the runner attempts
/// no line, the batch ends failed with its own reason and error-file entries,
/// and the envelope keeps its hold for reconciliation.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_batch_left_with_an_unknown_envelope_fails_with_its_own_reason(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(
            &w.f.principal,
            &[
                chat_line("a", "company/smart", "one"),
                chat_line("b", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let held = reservation(&w.f, job.execution_id).await.1;
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(job.execution_id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(reconcile_expired(&w.f.store, 100).await.unwrap(), 1);
    assert_eq!(envelope(&w, &job).await, "unknown");

    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "failed");
    assert_eq!(job.error_code.as_deref(), Some("hold_unavailable"));
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(0), Some(0)),
        "lines that did not run are listed in the error file, not counted"
    );
    assert!(job.settled_at.is_some());
    assert!(w.openai.executed().is_empty());
    assert!(w.openai.calls().is_empty());
    let errors = w.read(w.f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e["error"]["code"] == "batch_hold_unavailable")
    );
    assert!(
        w.read(w.f.principal.workspace_id, job.output_file_id)
            .await
            .is_empty()
    );
    let (state, still_held, actual, _) = reservation(&w.f, job.execution_id).await;
    assert_eq!(
        (state.as_str(), still_held, actual),
        ("unknown", held, None),
        "the unknown hold is neither released nor valued"
    );
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM batch_lines WHERE job_id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(lines, 0, "no line was claimed");
    consistent(&w.f).await;
}

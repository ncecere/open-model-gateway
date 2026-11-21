//! A batch's envelope reservation and its job row are created in one
//! transaction (#640): there is no moment at which the job exists with an
//! envelope that a short admission lease will turn `unknown`, or at which an
//! envelope holds budget for a job that was never created.
use super::*;
use crate::governance::reconcile_expired;

// Refuses only later UPDATEs. Initial admission must INSERT the final lease;
// these tests prove it needs no extension after committing its job row.
use crate::jobs::db_tests::refuse_lease_extensions;

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
    refuse_lease_extensions(&w.f.store.pool).await;
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
    let (lease, deadline): (gateway_datetime_compat::UtcInstant, gateway_datetime_compat::UtcInstant) = sqlx::query_as("SELECT r.lease_expires_at,j.poll_deadline_at FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE j.id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert!(
        lease >= deadline + LEASE_GRACE,
        "lease {lease} must cover the poll deadline {deadline} and its grace"
    );
}

/// Initial admission INSERTs the final lease, without a later UPDATE. After
/// the original ten-minute admission interval, reconciliation leaves the
/// envelope pending and the gateway runner executes every line.
#[dbtest::test]
async fn a_gateway_batch_inserts_its_final_lease_and_needs_no_extension(pool: PgPool) {
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

/// A native batch also INSERTs its final lease; refusing later extensions
/// cannot shorten the hold protecting its submission and polling.
#[dbtest::test]
async fn a_native_batch_inserts_its_final_lease_and_needs_no_extension(pool: PgPool) {
    let (w, job) = created(pool, true, None).await;
    assert_eq!(job.mode(), Some(BatchMode::Native));
    lease_covers_the_job(&w, &job).await;
    pass_the_admission_lease(&w).await;
    assert_eq!(reconcile_expired(&w.f.store, 100).await.unwrap(), 0);
    assert_eq!(envelope(&w, &job).await, "pending");
    consistent(&w.f).await;
}

/// A longer completion window lengthens the lease with it.
#[dbtest::test]
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
    let handle = crate::db::diagnostics::Handle::new();
    let failed = crate::db::diagnostics::scope(
        handle.clone(),
        w.create(
            w.f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            metadata,
        ),
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
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("governance_storage", "other_database")]
    );
    assert_eq!(admission_rowcounts(&w.f).await, (0, 0, 0, 0));
    let counters: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inflight_counters WHERE requests<>0 OR jobs<>0),(SELECT count(*) FROM rate_minute_counters WHERE requests<>0 OR unreserved<>0 OR tokens<>0)")
        .fetch_one(&w.f.store.pool).await.unwrap();
    assert_eq!(
        counters,
        (0, 0),
        "rolled-back admission consumes no slots or rate allowance"
    );
    assert_eq!(
        live_files(&w).await,
        files_before,
        "the private copy is removed"
    );
    assert!(w.openai.calls().is_empty());
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_native_job_row_that_cannot_be_written_leaves_no_envelope(pool: PgPool) {
    a_job_row_that_cannot_be_written(pool, true, None).await;
}

#[dbtest::test]
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
#[dbtest::test]
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

/// Pause admission just before its job insert, while it holds the user advisory
/// lock, then let SCIM request that lock while holding its user row lock.
#[dbtest::test]
async fn scim_user_lock_and_batch_creator_fk_do_not_deadlock(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.team, &[chat_line("a", "company/smart", "one")])
        .await;
    let mut barrier = crate::db::begin(&w.f.store.pool).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(640)")
        .execute(&mut *barrier)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION pause_job_640() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(640); RETURN NEW; END $$; CREATE TRIGGER pause_job_640 BEFORE INSERT ON async_jobs FOR EACH ROW EXECUTE FUNCTION pause_job_640()")
        .execute(&w.f.store.pool).await.unwrap();
    let mut scim = crate::db::begin(&w.f.store.pool).await.unwrap();
    let handle = crate::db::diagnostics::Handle::new();
    let create = crate::db::diagnostics::scope(
        handle.clone(),
        w.create(w.f.team, &file, BatchEndpoint::ChatCompletions, None),
    );
    let management = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND objid=640 AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                    .fetch_one(&w.f.store.pool).await.unwrap();
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("batch reached its job insert");
        crate::scim::lock_user_for_test(&mut scim, w.f.team.user_id.unwrap()).await;
        barrier.rollback().await.unwrap();
        let locked = crate::lifecycle::lock_users(&mut scim, &[w.f.team.user_id.unwrap()]).await;
        // Release the row even when PostgreSQL chose this transaction as victim.
        scim.rollback().await.unwrap();
        locked.expect("SCIM user scope must not deadlock with the creator FK");
    };
    let (created, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(create, management)
    })
    .await
    .unwrap();
    created.unwrap();
    let snapshot = handle.snapshot();
    assert!(!snapshot.unavailable);
    assert_eq!((snapshot.pending, snapshot.dropped), (0, 0));
    assert!(
        snapshot
            .events
            .iter()
            .all(|e| e.sqlstate.as_deref() != Some("40P01")),
        "admission must not hide a creator FK deadlock with retries"
    );
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_lost_batch_commit_ack_keeps_the_committed_jobs_work_file(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let result = crate::governance::batch::LOSE_COMMIT_ACK
        .scope(
            true,
            w.create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None),
        )
        .await;
    assert!(matches!(
        result,
        Err(CreateError::Job(JobError::Inference(
            InferenceError::Storage
        )))
    ));
    let id: Uuid = sqlx::query_scalar("SELECT id FROM async_jobs")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    let job = store::reload(&w.f.store, id).await.unwrap();
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM stored_files WHERE id=$1")
            .bind(job.work_file_id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert!(
        !deleted,
        "COMMIT reached PostgreSQL: the only work file must survive a lost acknowledgement"
    );
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    assert_eq!(w.reload(&job).await.state, "completed");
    assert_eq!(w.openai.executed(), ["one"]);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_file_rekey_does_not_block_the_batch_input_fk(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let (from, backend): (String, String) = sqlx::query_as(
        "SELECT encryption_key_id,backend FROM stored_files WHERE deleted_at IS NULL",
    )
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    let mut rekey = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(
        crate::filestore::rekey::claim_next_file(&mut rekey, &from, &backend, Uuid::nil())
            .await
            .unwrap()
            .is_some()
    );
    let created = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        w.create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None),
    )
    .await;
    rekey.rollback().await.unwrap();
    created
        .expect("batch must not wait for an object-store rewrap")
        .unwrap();
    consistent(&w.f).await;
}

async fn make_unknown(w: &World, job: &JobRow) {
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(job.execution_id).execute(&w.f.store.pool).await.unwrap();
    assert_eq!(reconcile_expired(&w.f.store, 100).await.unwrap(), 1);
    assert_eq!(envelope(w, job).await, "unknown");
}

#[dbtest::test]
async fn a_native_batch_with_an_unknown_envelope_finishes_and_keeps_its_hold(pool: PgPool) {
    let (w, job) = created(pool, true, None).await;
    make_unknown(&w, &job).await;
    w.poll().await;
    w.openai.push_batch(BatchStatus::Completed);
    w.openai.script.lock().unwrap().results = vec![
        json!({"custom_id":"l0","ok":true,"input":10,"output":5}),
        json!({"custom_id":"l1","ok":true,"input":10,"output":5}),
    ];
    w.poll().await;
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    assert_eq!(reservation(&w.f, job.execution_id).await.0, "unknown");
    assert_eq!(reservation(&w.f, job.execution_id).await.2, None);
    assert_eq!(w.openai.calls(), ["submit", "retrieve", "delete"]);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_resumed_batch_with_an_unknown_envelope_keeps_its_finished_lines(pool: PgPool) {
    let (w, job) = created(pool, false, None).await;
    w.jobs
        .soft_loss
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let open = w.reload(&job).await;
    assert_eq!(open.state, "in_progress");
    assert_eq!(open.request_completed, Some(1));
    assert_eq!(w.openai.executed().len(), 1);
    make_unknown(&w, &job).await;
    let held = reservation(&w.f, job.execution_id).await;
    let log = crate::log_capture_test::Capture::start();
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "failed");
    assert_eq!(job.error_code.as_deref(), Some("hold_unavailable"));
    assert_eq!(job.request_completed, Some(1));
    assert_eq!(
        w.openai.executed().len(),
        1,
        "finished lines never run again"
    );
    assert_eq!(
        w.read(w.f.principal.workspace_id, job.output_file_id)
            .await
            .len(),
        1
    );
    let errors = w.read(w.f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["error"]["code"], "batch_hold_unavailable");
    assert_eq!(reservation(&w.f, job.execution_id).await, held);
    let logs = log.text();
    assert!(logs.contains("batch budget hold unavailable"), "{logs}");
    assert!(logs.contains(&job.id.to_string()), "{logs}");
    consistent(&w.f).await;
}

async fn stopped_unknown(pool: PgPool, cancelled: bool) {
    let w = world(pool, false, false, 1).await;
    if !cancelled {
        // Age at INSERT, preserving the immutable identity fields thereafter.
        sqlx::raw_sql("CREATE FUNCTION age_job_640() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.created_at=now()-interval '25 hours'; RETURN NEW; END $$; CREATE TRIGGER age_job_640 BEFORE INSERT ON async_jobs FOR EACH ROW EXECUTE FUNCTION age_job_640()")
            .execute(&w.f.store.pool).await.unwrap();
    }
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    make_unknown(&w, &job).await;
    let held = reservation(&w.f, job.execution_id).await;
    if cancelled {
        w.jobs
            .cancel_batch(&w.f.principal, &client_id("batch_", job.id))
            .await
            .unwrap();
    }
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, if cancelled { "cancelled" } else { "expired" });
    assert_ne!(job.error_code.as_deref(), Some("hold_unavailable"));
    assert!(job.settled_at.is_some());
    assert!(w.openai.executed().is_empty());
    let errors = w.read(w.f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["error"]["code"],
        if cancelled {
            "batch_cancelled"
        } else {
            "batch_expired"
        }
    );
    assert_eq!(reservation(&w.f, job.execution_id).await, held);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn cancellation_precedes_hold_unavailable_and_keeps_the_unknown_hold(pool: PgPool) {
    stopped_unknown(pool, true).await;
}

#[dbtest::test]
async fn expiry_precedes_hold_unavailable_and_keeps_the_unknown_hold(pool: PgPool) {
    stopped_unknown(pool, false).await;
}

#[dbtest::test]
async fn a_terminal_batch_with_a_settled_envelope_can_be_reclaimed(pool: PgPool) {
    let (w, job) = created(pool, false, None).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_settled_640() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.settled_at IS NOT NULL THEN RAISE EXCEPTION 'settled marker refused'; END IF; RETURN NEW; END $$; CREATE TRIGGER refuse_settled_640 BEFORE UPDATE ON async_jobs FOR EACH ROW EXECUTE FUNCTION refuse_settled_640()")
        .execute(&w.f.store.pool).await.unwrap();
    assert!(Runner::new(w.jobs.clone()).run_next().await.is_err());
    let finished = w.reload(&job).await;
    assert_eq!(finished.state, "completed");
    assert!(finished.settled_at.is_none());
    let amounts = reservation(&w.f, job.execution_id).await;
    assert_eq!(amounts.0, "settled");
    let ledger_before: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    // The envelope committed before the failed job marker. Retry only the
    // settlement marker and cleanup, without rerunning or charging any line.
    sqlx::raw_sql(
        "DROP TRIGGER refuse_settled_640 ON async_jobs; DROP FUNCTION refuse_settled_640()",
    )
    .execute(&w.f.store.pool)
    .await
    .unwrap();
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let resumed = w.reload(&job).await;
    assert!(resumed.settled_at.is_some());
    assert_eq!(
        (resumed.state, resumed.output_file_id, resumed.error_file_id),
        (
            finished.state,
            finished.output_file_id,
            finished.error_file_id
        )
    );
    assert_eq!(reservation(&w.f, job.execution_id).await, amounts);
    assert_eq!(w.openai.executed().len(), 2);
    let ledger_after: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(ledger_after, ledger_before);
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_batch_in_global_admission_mode_inserts_its_job_and_final_lease(pool: PgPool) {
    let mut w = world(pool, false, false, 1).await;
    w.jobs.store = w
        .jobs
        .store
        .clone()
        .with_admission_mode(crate::governance::locks::AdmissionMode::Global);
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    lease_covers_the_job(&w, &job).await;
    assert_eq!(admission_rowcounts(&w.f).await, (1, 1, 1, 1));
    consistent(&w.f).await;
}

#[dbtest::test]
async fn a_failed_batch_commit_keeps_work_until_its_outcome_is_known(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_job_commit_640() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'commit refused'; END $$; CREATE CONSTRAINT TRIGGER refuse_job_commit_640 AFTER INSERT ON async_jobs DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION refuse_job_commit_640()")
        .execute(&w.f.store.pool).await.unwrap();
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let before = live_files(&w).await;
    let handle = crate::db::diagnostics::Handle::new();
    let result = crate::db::diagnostics::scope(
        handle.clone(),
        w.create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None),
    )
    .await;
    assert!(matches!(
        result,
        Err(CreateError::Job(JobError::Inference(
            InferenceError::Storage
        )))
    ));
    assert_eq!(
        crate::db::accounting_failure::tests::labels(&handle.signals()),
        [("governance_storage", "other_database")]
    );
    assert_eq!(admission_rowcounts(&w.f).await, (0, 0, 0, 0));
    // A server rejection is safe to retain too: do not infer rollback from a
    // COMMIT error. An unreferenced private file expires at its retention time.
    assert_eq!(live_files(&w).await, before + 1);
    consistent(&w.f).await;
}

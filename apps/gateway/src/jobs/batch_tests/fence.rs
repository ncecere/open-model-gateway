//! A gateway-run batch's claim is a fence: once another runner has taken a
//! batch over, the runner that lost it writes nothing more to it.
use super::*;
use crate::jobs::fence::{FenceError, Renewed};

/// `world` with one batch of `lines` (chat completions, one line each).
async fn batch(pool: PgPool, lines: &[(&str, &str)]) -> (World, JobRow) {
    let w = world(pool, false, false, 1).await;
    let lines: Vec<Value> = lines
        .iter()
        .map(|(id, text)| chat_line(id, "company/smart", text))
        .collect();
    let file = w.upload(&w.f.principal, &lines).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    (w, job)
}

/// Run the next claimable batch on a new runner (a process of its own).
fn start(w: &World) -> tokio::task::JoinHandle<JobResult<bool>> {
    let runner = Runner::new(w.jobs.clone());
    tokio::spawn(async move { runner.run_next().await })
}

/// The runner that holds the batch stops renewing: its lease lapses.
async fn lapse(w: &World, job: &JobRow) {
    sqlx::query("UPDATE async_jobs SET runner_lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

async fn count(w: &World, sql: &str, job: &JobRow) -> i64 {
    sqlx::query_scalar(sql)
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap()
}

/// Live segment files of the batch.
async fn segments(w: &World, job: &JobRow) -> i64 {
    count(w, "SELECT count(*) FROM batch_segments s JOIN stored_files f ON f.id=s.file_id WHERE s.job_id=$1 AND f.deleted_at IS NULL", job).await
}

/// Stored files of the batch's workspace that are not deleted.
async fn live_files(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE workspace_id=$1 AND deleted_at IS NULL",
    )
    .bind(w.f.principal.workspace_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap()
}

async fn envelope(w: &World, job: &JobRow) -> String {
    reservation(&w.f, job.execution_id).await.0
}

/// The old runner's provider call returns after the new runner finished the
/// batch: its line is already `interrupted`, counted and reported unavailable.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_cannot_change_a_batch_the_new_owner_finished(pool: PgPool) {
    let log = crate::log_capture_test::Capture::start();
    let (w, job) = batch(pool, &[("a", "hold-a")]).await;
    let held = w.openai.gate("hold-a");
    let old = start(&w);
    held.entered.notified().await;
    lapse(&w, &job).await;
    // The new owner interrupts the line and finishes the batch.
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    assert_eq!(
        (
            done.request_total,
            done.request_completed,
            done.request_failed
        ),
        (Some(1), Some(0), Some(1))
    );
    let errors = w.read(w.f.principal.workspace_id, done.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["error"]["code"], "result_unavailable");
    let files = live_files(&w).await;

    // The old runner's call completes (and is charged): it may write nothing.
    held.release.notify_one();
    let old = old.await.unwrap();
    let after = w.reload(&job).await;
    assert_eq!(
        (after.request_completed, after.request_failed),
        (Some(0), Some(1)),
        "a line is counted once, and never beyond the total"
    );
    assert_eq!(
        (
            after.state.as_str(),
            after.output_file_id,
            after.error_file_id
        ),
        ("completed", done.output_file_id, done.error_file_id)
    );
    assert_eq!(segments(&w, &job).await, 0, "no result segment recorded");
    assert_eq!(live_files(&w).await, files, "no duplicate output files");
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT state,error_code FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, ("interrupted".into(), Some("interrupted".into())));
    // The attempt's reservation still settles at what the provider charged,
    // so the batch's cost and its result file agree: charged, unavailable.
    let attempt: (String, Option<i64>) = sqlx::query_as("SELECT r.state,r.actual_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(attempt, ("settled".into(), Some(15)));
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (15, 0));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    // The refused outcome is logged once, with ids only, so the charged line
    // can be attributed.
    let execution: Uuid =
        sqlx::query_scalar("SELECT execution_id FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    let text = log.text();
    let logged: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(&execution.to_string()))
        .collect();
    assert_eq!(logged.len(), 1, "{text}");
    assert!(
        logged[0].contains("batch line outcome not recorded"),
        "{}",
        logged[0]
    );
    consistent(&w.f).await;
}

/// The old runner returns while the new owner still runs: it must not finish
/// or settle the batch, and the owner's own results must all be recorded.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_neither_finalizes_it_nor_costs_the_owner_a_result(
    pool: PgPool,
) {
    let (w, job) = batch(pool, &[("a", "hold-a"), ("b", "hold-b")]).await;
    let (held_a, held_b) = (w.openai.gate("hold-a"), w.openai.gate("hold-b"));
    let old = start(&w);
    held_a.entered.notified().await;
    lapse(&w, &job).await;
    let owner = start(&w);
    held_b.entered.notified().await;
    assert_eq!(w.reload(&job).await.request_failed, Some(1));

    held_a.release.notify_one();
    let old = old.await.unwrap();
    let running = w.reload(&job).await;
    assert_eq!(
        running.state, "in_progress",
        "the old runner finalized the batch"
    );
    assert_eq!(
        envelope(&w, &job).await,
        "pending",
        "the old runner closed the envelope"
    );
    assert_eq!(segments(&w, &job).await, 0);
    assert_eq!(
        (running.request_completed, running.request_failed),
        (Some(0), Some(1))
    );

    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let out = w.read(w.f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1, "the owner's result was lost");
    assert_eq!(out[0]["custom_id"], "b");
    let errors = w.read(w.f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["custom_id"], "a");
    assert_eq!(errors[0]["error"]["code"], "result_unavailable");
    assert_eq!(envelope(&w, &job).await, "settled");
    consistent(&w.f).await;
}

/// A runner that lost the batch starts no further line, even one nobody
/// claimed yet.
#[dbtest::test]
async fn a_runner_that_lost_the_batch_claims_and_runs_no_further_line(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "hold-a"), ("b", "hold-b"), ("c", "plain-c")]).await;
    let (held_a, held_b) = (w.openai.gate("hold-a"), w.openai.gate("hold-b"));
    let old = start(&w);
    held_a.entered.notified().await;
    lapse(&w, &job).await;
    let owner = start(&w);
    held_b.entered.notified().await;

    held_a.release.notify_one();
    let old = old.await.unwrap();
    assert_eq!(
        w.openai.executed(),
        ["hold-a", "hold-b"],
        "the old runner started a line after it lost the batch"
    );
    assert_eq!(
        count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await,
        2
    );

    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(
        matches!(old, Ok(true)),
        "the old runner stops quietly: {old:?}"
    );
    assert_eq!(w.openai.executed(), ["hold-a", "hold-b", "plain-c"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(1))
    );
    consistent(&w.f).await;
}

use crate::{
    governance::batch::{LineContext, admit_line},
    inference::{
        repository::{ExecutionStart, InferenceRepository},
        workload::{OutputReservation, WorkloadAdmission},
    },
    jobs::{fence::BatchFence, store::claim_gateway},
};

fn stale<T>(result: Result<T, FenceError>) -> bool {
    matches!(result, Err(FenceError::Stale))
}

/// Claim the batch as a new runner (lease already lapsed).
async fn claim(w: &World, job: &JobRow) -> BatchFence {
    lapse(w, job).await;
    let (_, fence) = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap()
        .pop()
        .expect("the batch is claimable");
    fence
}

#[dbtest::test]
async fn every_claim_of_a_batch_moves_its_epoch_on(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let first = claim(&w, &job).await;
    let second = claim(&w, &job).await;
    assert_eq!((first.epoch, second.epoch), (1, 2));
    // The same process claiming again is a new claim too: the earlier run of
    // it is fenced, never confused with the new one.
    lapse(&w, &job).await;
    let again = claim_gateway(&w.f.store, second.runner, 60.0, 1, &[])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .1;
    assert_eq!((again.runner, again.epoch), (second.runner, 3));
    assert_eq!(
        first.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Fenced
    );
    assert_eq!(
        second.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Fenced
    );
    assert_eq!(
        again.renew(&w.f.store, 60.0).await.unwrap(),
        Renewed::Held { cancel: false }
    );
    // A run in progress is not claimed again by its own process.
    lapse(&w, &job).await;
    let none = claim_gateway(&w.f.store, again.runner, 60.0, 1, &[job.id])
        .await
        .unwrap();
    assert!(none.is_empty());
    // Epochs never move back.
    let back = sqlx::query("UPDATE async_jobs SET runner_epoch=1 WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await;
    assert!(back.is_err());
}

#[dbtest::test]
async fn a_stale_claim_writes_nothing_and_a_current_one_everything(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain")]).await;
    let old = claim(&w, &job).await;
    assert!(
        old.claim_line(&w.f.store, job.workspace_id, 0, Uuid::new_v4())
            .await
            .unwrap()
    );
    let new = claim(&w, &job).await;
    // The takeover interrupts the line the old claim left running.
    assert_eq!(new.interrupt_running(&w.f.store).await.unwrap(), 1);
    let s = &w.f.store;
    assert!(stale(
        old.claim_line(s, job.workspace_id, 1, Uuid::new_v4()).await
    ));
    assert!(stale(old.retry_line(s, 0, 1, Uuid::new_v4()).await));
    assert!(stale(
        old.finish_line(s, 0, true, Some(200), None, true).await
    ));
    assert!(stale(old.progress(s, 1, 0).await));
    assert!(stale(old.interrupt_running(s).await));
    assert!(stale(
        old.insert_segment(s, job.workspace_id, Uuid::new_v4(), &[0])
            .await
    ));
    assert!(stale(old.set_status(s, "finalizing").await));
    assert!(stale(old.release(s).await));
    let done = crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: None,
        error_file: None,
        counts: None,
    };
    assert!(stale(old.finish_batch(s, &done).await));
    assert_eq!(
        crate::governance::batch::close_gateway_batch(s, job.execution_id, 1, &old)
            .await
            .unwrap(),
        crate::governance::batch::Closed::Fenced
    );
    assert_eq!(envelope(&w, &job).await, "pending");
    let row = w.reload(&job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.request_completed,
            row.request_failed
        ),
        ("queued", Some(0), Some(1)),
        "only the takeover's own interruption is counted"
    );
    // The current claim's line outcome is counted once.
    assert!(
        new.claim_line(s, job.workspace_id, 1, Uuid::new_v4())
            .await
            .unwrap()
    );
    assert!(
        new.finish_line(s, 1, true, Some(200), None, true)
            .await
            .unwrap()
    );
    assert!(
        !new.finish_line(s, 1, true, Some(200), None, true)
            .await
            .unwrap()
    );
    assert_eq!(w.reload(&job).await.request_completed, Some(1));
}

/// A process that predates the epoch (or a stale writer) took the next
/// segment number: the owner takes the one after it and keeps flushing.
#[dbtest::test]
async fn a_taken_segment_number_never_blocks_the_owners_later_flushes(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    let file = |n: u8| {
        let w = &w;
        async move {
            let stream: crate::filestore::ByteStream =
                Box::pin(futures_util::stream::once(async move {
                    Ok(axum::body::Bytes::from(vec![n]))
                }));
            w.files
                .create(
                    NewFile {
                        created_by_api_key_id: Some(w.f.principal.key_id),
                        quota: QuotaMode::CountOnly,
                        ..NewFile::new(Purpose::BatchOutput, Some(w.f.principal.workspace_id))
                    },
                    stream,
                )
                .await
                .unwrap()
                .id
        }
    };
    let ws = job.workspace_id;
    sqlx::query(
        "INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) VALUES($1,$2,0,$3,1)",
    )
    .bind(job.id)
    .bind(ws)
    .bind(file(0).await)
    .execute(&w.f.store.pool)
    .await
    .unwrap();
    let mut seqs = vec![];
    for n in 1..=3u8 {
        seqs.push(
            fence
                .insert_segment(&w.f.store, ws, file(n).await, &[0])
                .await
                .unwrap(),
        );
    }
    assert_eq!(seqs, [1, 2, 3]);
}

/// A stale runner is refused at admission: no execution, no reservation, no
/// provider call, and the current claim admits the same line.
#[dbtest::test]
async fn a_stale_claim_admits_no_line(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let principal = job.principal();
    let deployment = InferenceRepository::deployments(&w.f.store, &principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let admit = |fence: BatchFence| {
        let (store, principal, deployment) = (w.f.store.clone(), principal, deployment.clone());
        let envelope = job.execution_id;
        async move {
            let id = Uuid::new_v4();
            admit_line(
                &store,
                LineContext { envelope, fence },
                &ExecutionStart {
                    id,
                    root_request_id: id,
                    attempt_number: 1,
                    principal,
                    deployment_id: deployment.id,
                    provider: deployment.provider.clone(),
                    model: "company/smart".into(),
                    streamed: false,
                    upstream_model: Some(deployment.upstream_model.clone()),
                    client: Default::default(),
                },
                &WorkloadAdmission {
                    kind: crate::inference::types::WorkloadKind::Generation,
                    output: OutputReservation::Requested(Some(10)),
                    unit_ceilings: Default::default(),
                },
                60,
                &deployment,
                None,
                50,
                None,
            )
            .await
        }
    };
    assert_eq!(admit(old).await.unwrap_err(), InferenceError::Configuration);
    let lines = |w: &World| {
        let (store, job) = (w.f.store.clone(), job.id);
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM inference_executions WHERE batch_job_id=$1",
            )
            .bind(job)
            .fetch_one(&store.pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(lines(&w).await, 0);
    admit(new).await.unwrap();
    assert_eq!(lines(&w).await, 1);
    consistent(&w.f).await;
}

/// A process that predates the epoch claims with the runner id only (its
/// statement, verbatim): the claim it replaces is fenced all the same, by the
/// runner id, and the epoch does not move.
#[dbtest::test]
async fn a_claim_by_a_process_without_the_epoch_still_fences_the_old_claim(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let ours = claim(&w, &job).await;
    lapse(&w, &job).await;
    let old = Uuid::new_v4();
    let claimed = sqlx::query("UPDATE async_jobs SET runner_id=$1,runner_lease_until=clock_timestamp()+make_interval(secs=>$2) WHERE id IN(SELECT id FROM async_jobs WHERE batch_mode='gateway' AND settled_at IS NULL AND (runner_lease_until IS NULL OR runner_lease_until<clock_timestamp()) ORDER BY created_at,id LIMIT $3 FOR UPDATE SKIP LOCKED)")
        .bind(old)
        .bind(60.0_f64)
        .bind(1_i64)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(claimed.rows_affected(), 1);
    assert_eq!(w.reload(&job).await.request_total, job.request_total);
    let epoch: i64 = sqlx::query_scalar("SELECT runner_epoch FROM async_jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        epoch, ours.epoch,
        "the older statement does not know the epoch"
    );
    assert_eq!(ours.renew(&w.f.store, 60.0).await.unwrap(), Renewed::Fenced);
    assert!(matches!(
        ours.claim_line(&w.f.store, job.workspace_id, 0, Uuid::new_v4())
            .await,
        Err(FenceError::Stale)
    ));
}

/// The core of the fence: a claim skips a batch while a runner write is in
/// flight and takes it after, and then the earlier claim is refused.
async fn claim_skips_a_write_in_flight(pool: PgPool, hold: crate::jobs::fence::Hold) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    lapse(&w, &job).await;
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(fence.hold(&mut tx, hold).await.unwrap());
    let skipped = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap();
    assert!(
        skipped.is_empty(),
        "the claim waited for or overtook a write"
    );
    // A write that only reads and inserts does not queue the owner's own
    // renewal (`KEY SHARE` and the renewal's `NO KEY UPDATE` do not conflict).
    if hold == crate::jobs::fence::Hold::Rows {
        assert_eq!(
            fence.renew(&w.f.store, 0.001).await.unwrap(),
            Renewed::Held { cancel: false }
        );
    }
    tx.commit().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let taken = claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(
        !fence.hold(&mut tx, hold).await.unwrap(),
        "the old claim still holds"
    );
}
#[dbtest::test]
async fn a_claim_skips_a_batch_with_a_row_hold_in_flight(pool: PgPool) {
    claim_skips_a_write_in_flight(pool, crate::jobs::fence::Hold::Rows).await;
}
#[dbtest::test]
async fn a_claim_skips_a_batch_with_an_update_hold_in_flight(pool: PgPool) {
    claim_skips_a_write_in_flight(pool, crate::jobs::fence::Hold::Update).await;
}

/// A fenced transaction that stops talking is ended by the server, which
/// releases the batch for a claim.
#[dbtest::test]
async fn a_fenced_transaction_carries_an_idle_timeout(pool: PgPool) {
    use crate::jobs::fence::{Hold, IDLE_IN_TRANSACTION};
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let fence = claim(&w, &job).await;
    for hold in [Hold::Rows, Hold::Update] {
        let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
        assert!(fence.hold(&mut tx, hold).await.unwrap());
        let timeout: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(timeout, IDLE_IN_TRANSACTION);
        tx.rollback().await.unwrap();
    }
    // The server enforces it: a held row is released when the session sits idle.
    let mut tx = crate::db::begin(&w.f.store.pool).await.unwrap();
    assert!(fence.hold(&mut tx, Hold::Update).await.unwrap());
    sqlx::query("SET LOCAL idle_in_transaction_session_timeout='200ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(sqlx::query("SELECT 1").execute(&mut *tx).await.is_err());
    lapse(&w, &job).await;
    assert_eq!(
        claim_gateway(&w.f.store, Uuid::new_v4(), 60.0, 1, &[])
            .await
            .unwrap()
            .len(),
        1
    );
}

/// A run that loses the batch between creating its result files and recording
/// the batch finished removes those files; the owner finalises alone.
#[dbtest::test]
async fn a_takeover_between_the_result_files_and_finishing_leaves_no_duplicate_files(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    w.jobs.finalize_hold.arm();
    let old = start(&w);
    w.jobs.finalize_hold.reached().await;
    lapse(&w, &job).await;
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    let files = live_files(&w).await;

    w.jobs.finalize_hold.release();
    assert!(matches!(old.await.unwrap(), Ok(true)));
    assert_eq!(
        live_files(&w).await,
        files - 1,
        "the old run's output file was removed"
    );
    let after = w.reload(&job).await;
    assert_eq!(after.output_file_id, done.output_file_id);
    assert!(!file_deleted(&w.f, after.output_file_id).await);
    assert_eq!(
        w.read(w.f.principal.workspace_id, after.output_file_id)
            .await
            .len(),
        1
    );
    assert_eq!(
        (after.request_completed, after.request_failed),
        (Some(1), Some(0))
    );
    consistent(&w.f).await;
}

/// A line recorded `succeeded` whose result was still in the old run's
/// buffer when the batch was taken over has no stored result: it is reported
/// unavailable and counted failed, so counters and error file agree.
#[dbtest::test]
async fn a_result_still_buffered_at_takeover_is_counted_with_the_error_file(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "hold-b")]).await;
    let held = w.openai.gate("hold-b");
    let old = start(&w);
    held.entered.notified().await;
    // Line a finished before b started: succeeded, its record buffered.
    let state: String =
        sqlx::query_scalar("SELECT state FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, "succeeded");
    assert_eq!(segments(&w, &job).await, 0);
    lapse(&w, &job).await;
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let done = w.reload(&job).await;
    let errors = w.read(w.f.principal.workspace_id, done.error_file_id).await;
    assert_eq!(done.output_file_id, None);
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e["error"]["code"] == "result_unavailable")
    );
    assert_eq!(
        (
            done.request_total,
            done.request_completed,
            done.request_failed
        ),
        (Some(2), Some(0), Some(2)),
        "a line with no stored result is not counted completed"
    );
    held.release.notify_one();
    assert!(matches!(old.await.unwrap(), Ok(true)));
    assert_eq!(segments(&w, &job).await, 0);
    consistent(&w.f).await;
}

/// Every statement the scheduler and the job row accept from a claim that
/// ended is refused or does nothing; the current claim's go through.
#[dbtest::test]
async fn scheduler_and_job_row_writes_follow_the_claim(pool: PgPool) {
    use crate::jobs::schedule::{Claim, Pause, Scheduler, WaitRow};
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain")]).await;
    let s = &w.f.store;
    let scheduler = Scheduler::new(s.clone(), None);
    let route = w.f.deployment;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let ws = job.workspace_id;
    let row = |n| WaitRow {
        deployment: route,
        waiting_lines: n,
        reason: Some(Pause::Concurrency),
        ready: true,
    };
    // Ended claim.
    assert!(stale(
        scheduler.claim(&old, ws, 0, Uuid::new_v4(), route, 2).await
    ));
    assert!(stale(
        scheduler
            .claim_retry(&old, 0, 1, Uuid::new_v4(), route, 2)
            .await
    ));
    assert!(stale(
        scheduler.publish_waits(&old, ws, &[row(1)], true).await
    ));
    assert!(stale(scheduler.clear_waits(&old).await));
    old.mark_running(s).await.unwrap();
    old.mark_settled(s).await.unwrap();
    assert_eq!(
        count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await,
        0
    );
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        0
    );
    assert_eq!(
        w.reload(&job).await.state,
        "queued",
        "the ended claim marked it running"
    );
    // Current claim.
    assert_eq!(
        scheduler
            .claim(&new, ws, 0, Uuid::new_v4(), route, 2)
            .await
            .unwrap(),
        Claim::Claimed
    );
    scheduler
        .publish_waits(&new, ws, &[row(3)], true)
        .await
        .unwrap();
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        1
    );
    assert_eq!(
        scheduler
            .claim_retry(&new, 0, 1, Uuid::new_v4(), route, 2)
            .await
            .unwrap(),
        Some(false),
        "a running line is not a failed one: nothing to retry"
    );
    new.mark_running(s).await.unwrap();
    assert_eq!(w.reload(&job).await.state, "in_progress");
    new.progress(s, 1, 2).await.unwrap();
    new.set_status(s, "finalizing").await.unwrap();
    let row = w.reload(&job).await;
    assert_eq!(
        (
            row.request_completed,
            row.request_failed,
            row.upstream_status.as_deref()
        ),
        (Some(1), Some(2), Some("finalizing"))
    );
    scheduler.clear_waits(&new).await.unwrap();
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM batch_route_waits WHERE job_id=$1",
            &job
        )
        .await,
        0
    );
    // Settling follows finishing.
    old.mark_settled(s).await.unwrap();
    let done = crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: None,
        error_file: None,
        counts: None,
    };
    let finished = new.finish_batch(s, &done).await.unwrap();
    assert_eq!(finished.state, "completed");
    old.mark_settled(s).await.unwrap();
    assert!(
        w.reload(&job).await.settled_at.is_none(),
        "the ended claim settled it"
    );
    new.mark_settled(s).await.unwrap();
    assert!(w.reload(&job).await.settled_at.is_some());
    // Renewal reports a settled batch, not a lost claim; release is fenced.
    assert_eq!(new.renew(s, 60.0).await.unwrap(), Renewed::Settled);
    new.release(s).await.unwrap();
    assert!(stale(old.release(s).await));
}

/// The first failed renewal makes the writer store what it holds, before a
/// takeover can refuse it; a writer whose claim ended stores nothing.
#[dbtest::test]
async fn a_failed_renewal_stores_the_buffered_results_at_once(pool: PgPool) {
    use crate::jobs::runner::SegmentProbe;
    let (w, job) = batch(pool, &[("a", "plain"), ("b", "plain")]).await;
    let old = claim(&w, &job).await;
    for line in 0..2 {
        assert!(
            old.claim_line(&w.f.store, job.workspace_id, line, Uuid::new_v4())
                .await
                .unwrap()
        );
        assert!(
            old.finish_line(&w.f.store, line, true, Some(200), None, true)
                .await
                .unwrap()
        );
    }
    let probe = SegmentProbe::start(w.f.store.clone(), w.files.clone(), job.clone(), old);
    probe.finished(0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        segments(&w, &job).await,
        0,
        "nothing flushes before the age or size threshold"
    );
    probe.doubt();
    for _ in 0..50 {
        if segments(&w, &job).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(segments(&w, &job).await, 1, "the doubt flushed the buffer");
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT segment FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(0));
    // The batch is taken over: the next flush is refused and stores nothing.
    let _new = claim(&w, &job).await;
    probe.finished(1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    probe.doubt();
    for _ in 0..50 {
        if probe.fenced() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(probe.fenced());
    probe.close().await;
    assert_eq!(segments(&w, &job).await, 1);
}

/// A run that only doubts its lease (soft loss) never ends the batch: it
/// finishes its in-flight lines, releases, and the batch stays open with the
/// unstarted lines pending for the next claim.
#[dbtest::test]
async fn a_soft_lost_run_leaves_the_batch_open_and_a_second_runner_completes_it(pool: PgPool) {
    let (w, job) = batch(
        pool,
        &[("a", "plain-a"), ("b", "plain-b"), ("c", "plain-c")],
    )
    .await;
    w.jobs
        .soft_loss
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(start(&w).await.unwrap(), Ok(true)));
    let open = w.reload(&job).await;
    assert_eq!(open.state, "in_progress", "a soft-lost run ended the batch");
    assert_eq!(
        (open.output_file_id, open.error_file_id, open.settled_at),
        (None, None, None)
    );
    assert_eq!(envelope(&w, &job).await, "pending");
    let started = count(&w, "SELECT count(*) FROM batch_lines WHERE job_id=$1", &job).await;
    assert!(
        (1..3).contains(&started),
        "unstarted lines stay pending: {started}"
    );
    let lease: Option<gateway_datetime_compat::UtcInstant> =
        sqlx::query_scalar("SELECT runner_lease_until FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert!(lease.is_none(), "the run gave the batch up");

    // The next claim runs the rest and ends the batch.
    assert!(matches!(start(&w).await.unwrap(), Ok(true)));
    let done = w.reload(&job).await;
    assert_eq!(done.state, "completed");
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(3), Some(0))
    );
    let out = w
        .read(w.f.principal.workspace_id, done.output_file_id)
        .await;
    assert_eq!(out.len(), 3);
    assert_eq!(w.openai.executed().len(), 3, "every line exactly once");
    consistent(&w.f).await;
}

/// Failed `finish_batch` statements keep the result files only when the
/// server may still commit them.
#[test]
fn only_a_connection_failure_leaves_the_outcome_unknown() {
    use crate::jobs::fence::outcome_unknown;
    assert!(outcome_unknown(&sqlx::Error::Io(std::io::Error::other(
        "reset"
    ))));
    assert!(outcome_unknown(&sqlx::Error::WorkerCrashed));
    assert!(!outcome_unknown(&sqlx::Error::PoolTimedOut));
    assert!(!outcome_unknown(&sqlx::Error::PoolClosed));
}

/// A server error is certain: nothing was applied (a file id that does not
/// exist violates a foreign key).
#[dbtest::test]
async fn a_rejected_finish_is_a_certain_failure_and_a_refused_one_is_stale(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let old = claim(&w, &job).await;
    let new = claim(&w, &job).await;
    let done = |file: Option<Uuid>| crate::jobs::store::Finished {
        state: JobState::Completed,
        error_code: None,
        output_file: file,
        error_file: None,
        counts: None,
    };
    assert!(matches!(
        new.finish_batch(&w.f.store, &done(Some(Uuid::new_v4())))
            .await,
        Err(FenceError::Storage)
    ));
    assert!(matches!(
        old.finish_batch(&w.f.store, &done(None)).await,
        Err(FenceError::Stale)
    ));
    assert_eq!(w.reload(&job).await.state, "queued");
}

//! A gateway-run batch's claim is a fence: once another runner has taken a
//! batch over, the runner that lost it writes nothing more to it.
use super::*;
use crate::jobs::fence::FenceError;

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
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_runner_that_lost_the_batch_cannot_change_a_batch_the_new_owner_finished(pool: PgPool) {
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
    consistent(&w.f).await;
}

/// The old runner returns while the new owner still runs: it must not finish
/// or settle the batch, and the owner's own results must all be recorded.
#[sqlx::test(migrations = "./enterprise_migrations")]
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
#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
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
    assert_eq!(first.renew(&w.f.store, 60.0).await.unwrap(), None);
    assert_eq!(second.renew(&w.f.store, 60.0).await.unwrap(), None);
    assert_eq!(again.renew(&w.f.store, 60.0).await.unwrap(), Some(false));
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

#[sqlx::test(migrations = "./enterprise_migrations")]
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
#[sqlx::test(migrations = "./enterprise_migrations")]
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
#[sqlx::test(migrations = "./enterprise_migrations")]
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

/// A process that predates the epoch claims with the runner id only: the
/// claim it replaces is fenced all the same, by the runner id.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_claim_by_a_process_without_the_epoch_still_fences_the_old_claim(pool: PgPool) {
    let (w, job) = batch(pool, &[("a", "plain")]).await;
    let ours = claim(&w, &job).await;
    lapse(&w, &job).await;
    sqlx::query("UPDATE async_jobs SET runner_id=$2,runner_lease_until=clock_timestamp()+interval '60 seconds' WHERE id=$1")
        .bind(job.id)
        .bind(Uuid::new_v4())
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(ours.renew(&w.f.store, 60.0).await.unwrap(), None);
    assert!(matches!(
        ours.claim_line(&w.f.store, job.workspace_id, 0, Uuid::new_v4())
            .await,
        Err(FenceError::Stale)
    ));
}

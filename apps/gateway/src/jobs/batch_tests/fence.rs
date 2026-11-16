//! A gateway-run batch's claim is a fence: once another runner has taken a
//! batch over, the runner that lost it writes nothing more to it.
use super::*;

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
    sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE workspace_id=$1 AND deleted_at IS NULL")
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
        (done.request_total, done.request_completed, done.request_failed),
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
        (after.state.as_str(), after.output_file_id, after.error_file_id),
        ("completed", done.output_file_id, done.error_file_id)
    );
    assert_eq!(segments(&w, &job).await, 0, "no result segment recorded");
    assert_eq!(live_files(&w).await, files, "no duplicate output files");
    let state: (String, Option<String>) = sqlx::query_as("SELECT state,error_code FROM batch_lines WHERE job_id=$1 AND line_no=0")
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
    assert!(matches!(old, Ok(true)), "the old runner stops quietly: {old:?}");
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
    assert_eq!(running.state, "in_progress", "the old runner finalized the batch");
    assert_eq!(envelope(&w, &job).await, "pending", "the old runner closed the envelope");
    assert_eq!(segments(&w, &job).await, 0);
    assert_eq!(
        (running.request_completed, running.request_failed),
        (Some(0), Some(1))
    );

    held_b.release.notify_one();
    assert!(matches!(owner.await.unwrap(), Ok(true)));
    assert!(matches!(old, Ok(true)), "the old runner stops quietly: {old:?}");
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
    assert!(matches!(old, Ok(true)), "the old runner stops quietly: {old:?}");
    assert_eq!(w.openai.executed(), ["hold-a", "hold-b", "plain-c"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(1))
    );
    consistent(&w.f).await;
}

//! Bounded background poller (`serve`, runtime role): every interval it
//! claims at most [`BATCH`] due, unsettled jobs (`FOR UPDATE SKIP LOCKED`,
//! so several gateway processes never poll the same job concurrently),
//! refreshes each from its provider with a bounded deadline, applies the
//! forward-only state machine and settles terminal jobs idempotently. Errors
//! back off per job; jobs past their poll deadline are left to lease
//! reconciliation (the hold is retained as unknown).
use std::time::Duration;

use super::*;

/// Jobs claimed per tick.
pub const BATCH: i64 = 32;
/// Deadline of one job refresh (an output-file scan has its own deadline).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60 + 60);

/// Start the poller when an interval is configured.
pub fn start(jobs: Jobs) -> Option<tokio::task::JoinHandle<()>> {
    let every = jobs.limits.poll_interval?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match jobs.poll_once().await {
                Ok(n) if n > 0 => tracing::debug!(jobs = n, "async jobs polled"),
                Ok(_) => {}
                Err(_) => tracing::warn!("async job poll incomplete; retrying next interval"),
            }
        }
    }))
}

impl Jobs {
    /// One poll pass; returns how many jobs were refreshed successfully.
    pub async fn poll_once(&self) -> Result<usize, InferenceError> {
        let interval = self.poll_seconds();
        let due = store::claim_due(&self.store, BATCH, interval)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut done = 0;
        for job in due {
            let id = job.id;
            let refreshed = tokio::time::timeout(REFRESH_TIMEOUT, self.poll_job(job)).await;
            match refreshed {
                Ok(Ok(())) => done += 1,
                _ => {
                    tracing::warn!(job_id = %id, "async job refresh failed; backing off");
                    let _ = store::poll_failed(&self.store, id, interval).await;
                }
            }
        }
        Ok(done)
    }

    async fn poll_job(&self, job: JobRow) -> JobResult<()> {
        match job.job_kind() {
            Some(JobKind::Video) => {
                self.refresh_video(job).await?;
            }
            Some(JobKind::Batch) => {
                self.refresh_batch(job, true).await?;
            }
            None => return Err(InferenceError::Storage.into()),
        }
        Ok(())
    }
}

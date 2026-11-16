//! Lease renewal of a running batch.
//!
//! The task renews every [`RENEW`] until the run ends. A renewal the fence
//! refuses ends the run's work (the batch belongs to another claim). A
//! renewal that fails keeps the task going: once [`RENEW_TRUST`] has passed
//! since the last success the lease may have ended, so the run stops starting
//! lines and just finishes the ones it has; later successes do not restart
//! dispatch. Every failed renewal asks the segment writer to store what it
//! holds.
use std::future::Future;

use tokio::time::Instant;

use super::*;
use crate::jobs::fence::Renewed;

/// A run whose renewals keep failing stops starting lines this long after
/// the last success, while the database lease can still be running.
pub(super) const RENEW_TRUST: Duration =
    Duration::from_secs(LEASE_SECONDS as u64 - RENEW.as_secs());

/// One renewal gets this long: a hung connection is a failed renewal, not a
/// silence the trust window never sees.
pub(super) const RENEW_TIMEOUT: Duration = Duration::from_secs(RENEW.as_secs() * 2 / 3);

/// What one renewal attempt means for the run.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Held {
        cancel: bool,
    },
    /// The batch is settled: nothing is left to renew.
    Done,
    /// The claim ended.
    Fenced,
    Failed {
        stop_dispatch: bool,
    },
}

/// When the lease was last known to be held.
pub(super) struct Trust {
    renewed: Instant,
}
impl Trust {
    pub(super) fn new(at: Instant) -> Self {
        Self { renewed: at }
    }
    /// `attempt` is when the renewal request was sent: the database term it
    /// extended started no earlier.
    pub(super) fn step(
        &mut self,
        attempt: Instant,
        now: Instant,
        outcome: Result<Renewed, ()>,
    ) -> Step {
        match outcome {
            Ok(Renewed::Held { cancel }) => {
                self.renewed = attempt;
                Step::Held { cancel }
            }
            Ok(Renewed::Settled) => Step::Done,
            Ok(Renewed::Fenced) => Step::Fenced,
            Err(()) => Step::Failed {
                stop_dispatch: now.duration_since(self.renewed) >= RENEW_TRUST,
            },
        }
    }
}

/// Renew `renew` every [`RENEW`] for the run's `state`.
pub(super) fn spawn<F, Fut>(state: Arc<RunState>, mut renew: F) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Renewed, ()>> + Send,
{
    tokio::spawn(async move {
        let mut trust = Trust::new(Instant::now());
        loop {
            let attempt = Instant::now();
            let outcome = match tokio::time::timeout(RENEW_TIMEOUT, renew()).await {
                Ok(outcome) => outcome,
                Err(_) => Err(()),
            };
            match trust.step(attempt, Instant::now(), outcome) {
                Step::Held { cancel } => {
                    if cancel {
                        state.cancel.store(true, Ordering::SeqCst);
                    }
                }
                Step::Done => return,
                Step::Fenced => {
                    state.lose();
                    return;
                }
                Step::Failed { stop_dispatch } => {
                    tracing::warn!(job_id = %state.fence.job, "batch lease renewal failed");
                    state.doubt.notify_one();
                    if stop_dispatch {
                        state.lost.store(true, Ordering::SeqCst);
                    }
                }
            }
            tokio::time::sleep(RENEW).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn state() -> Arc<RunState> {
        Arc::new(RunState::new(BatchFence {
            job: Uuid::new_v4(),
            runner: Uuid::new_v4(),
            epoch: 1,
        }))
    }

    #[test]
    fn the_trust_window_follows_the_lease_constants() {
        assert_eq!(RENEW_TRUST + RENEW, Duration::from_secs_f64(LEASE_SECONDS));
    }

    #[test]
    fn dispatch_stops_only_once_the_last_success_is_older_than_the_trust_window() {
        let t0 = Instant::now();
        let mut trust = Trust::new(t0);
        let early = trust.step(t0 + RENEW, t0 + RENEW, Err(()));
        assert_eq!(
            early,
            Step::Failed {
                stop_dispatch: false
            }
        );
        let late = trust.step(t0 + RENEW_TRUST, t0 + RENEW_TRUST, Err(()));
        assert_eq!(
            late,
            Step::Failed {
                stop_dispatch: true
            }
        );
        // A success restarts the window.
        let ok = trust.step(
            t0 + RENEW_TRUST,
            t0 + RENEW_TRUST,
            Ok(Renewed::Held { cancel: false }),
        );
        assert_eq!(ok, Step::Held { cancel: false });
        let again = trust.step(t0 + RENEW_TRUST + RENEW, t0 + RENEW_TRUST + RENEW, Err(()));
        assert_eq!(
            again,
            Step::Failed {
                stop_dispatch: false
            }
        );
    }

    #[test]
    fn a_settled_batch_ends_the_task_quietly_and_a_refused_renewal_does_not() {
        let t0 = Instant::now();
        let mut trust = Trust::new(t0);
        assert_eq!(trust.step(t0, t0, Ok(Renewed::Settled)), Step::Done);
        assert_eq!(trust.step(t0, t0, Ok(Renewed::Fenced)), Step::Fenced);
    }

    /// Renewals fail for the whole trust window and then work again: the run
    /// stops starting lines, keeps renewing, and is not fenced.
    #[tokio::test(start_paused = true)]
    async fn a_failing_database_stops_dispatch_but_not_renewal() {
        let state = state();
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = (RENEW_TRUST.as_secs() / RENEW.as_secs()) as usize + 1;
        let task = {
            let calls = calls.clone();
            spawn(state.clone(), move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < failing {
                        Err(())
                    } else {
                        Ok(Renewed::Held { cancel: false })
                    }
                }
            })
        };
        tokio::time::sleep(RENEW_TRUST + Duration::from_secs(1)).await;
        assert!(state.lost.load(Ordering::SeqCst), "dispatch stopped");
        assert!(!state.fenced.load(Ordering::SeqCst));
        let seen = calls.load(Ordering::SeqCst);
        tokio::time::sleep(RENEW * 3).await;
        assert!(
            calls.load(Ordering::SeqCst) >= seen + 2,
            "renewal went on after the trust window"
        );
        assert!(!task.is_finished());
        assert!(
            !state.fenced.load(Ordering::SeqCst),
            "recovery is not a fence"
        );
        task.abort();
    }

    /// A renewal that never returns is a failed renewal: it asks for the
    /// buffers to be stored at once and ends dispatch after the trust window.
    #[tokio::test(start_paused = true)]
    async fn a_hung_renewal_counts_as_failed() {
        let state = state();
        let task = spawn(state.clone(), || {
            std::future::pending::<Result<Renewed, ()>>()
        });
        tokio::time::timeout(RENEW, state.doubt.notified())
            .await
            .expect("the first hung renewal sends the doubt notification");
        assert!(
            !state.lost.load(Ordering::SeqCst),
            "not yet past the trust window"
        );
        tokio::time::sleep(RENEW_TRUST + RENEW + RENEW_TIMEOUT).await;
        assert!(state.lost.load(Ordering::SeqCst), "dispatch stopped");
        assert!(!state.fenced.load(Ordering::SeqCst));
        assert!(!task.is_finished(), "it keeps trying");
        task.abort();
    }

    #[test]
    fn a_renewal_times_out_before_the_next_one_is_due() {
        assert!(RENEW_TIMEOUT < RENEW && !RENEW_TIMEOUT.is_zero());
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_renewal_ends_the_task_and_fences_the_run() {
        let state = state();
        let task = spawn(state.clone(), || async { Ok(Renewed::Fenced) });
        task.await.unwrap();
        assert!(state.fenced.load(Ordering::SeqCst) && state.lost.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn a_settled_batch_is_not_reported_as_lost() {
        let state = state();
        let task = spawn(state.clone(), || async { Ok(Renewed::Settled) });
        task.await.unwrap();
        assert!(!state.lost.load(Ordering::SeqCst));
    }
}

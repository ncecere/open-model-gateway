//! A future that is polled in place by its owner and, if the owner is dropped
//! before it finishes, is finished on a spawned task instead.
//!
//! Invariants:
//! - On the normal path the future is polled by the owner's own task: no
//!   spawn, no extra wake-up, one allocation for the pinned box.
//! - The future has exactly one owner at any time: the wrapper until the
//!   future returns `Ready` or the wrapper is dropped, then at most one
//!   spawned task. It is never polled concurrently and never cloned, so its
//!   side effects happen at most once.
//! - Dropping after `Ready` spawns nothing: the future is gone.
//! - Dropping before the first poll hands the whole future over, and it runs
//!   to its end on the spawned task. The wrapper cannot tell "never polled"
//!   from "polled and pending"; a caller that must not run the future at all
//!   after a drop must not construct the wrapper until it is about to poll it.
//!   (`ExecutionGuard::settle_to_completion` builds it and awaits it in the
//!   same poll, so it is never dropped unpolled.)
//! - A future that panics while polled is never resumed: it is taken out of
//!   the wrapper for the duration of each poll, so the unwind finds nothing to
//!   hand over.
//! - Without a Tokio runtime on the dropping thread, an unfinished future is
//!   dropped like any other value (no panic, nothing spawned).
//! - The spawned task's output is discarded; whatever must be observed
//!   (metrics, logs) is done inside the future.
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub(super) struct HandOff<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    future: Option<Pin<Box<F>>>,
}
impl<F> HandOff<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    pub(super) fn new(future: F) -> Self {
        Self {
            future: Some(Box::pin(future)),
        }
    }
}
impl<F> Future for HandOff<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let mut future = self.future.take().expect("polled after completion");
        match future.as_mut().poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending => {
                self.future = Some(future);
                Poll::Pending
            }
        }
    }
}
impl<F> Drop for HandOff<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn drop(&mut self) {
        if let Some(future) = self.future.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                future.await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        panic::AssertUnwindSafe,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    /// Pends `pending` times (waking itself each time), then counts one
    /// completion. `polls` counts every poll and `dropped` every drop.
    struct Counter {
        pending: usize,
        completed: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    #[derive(Clone, Default)]
    struct Probe {
        completed: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Probe {
        fn future(&self, pending: usize) -> Counter {
            Counter {
                pending,
                completed: self.completed.clone(),
                polls: self.polls.clone(),
                dropped: self.dropped.clone(),
            }
        }
        fn completed(&self) -> usize {
            self.completed.load(Ordering::SeqCst)
        }
        fn polls(&self) -> usize {
            self.polls.load(Ordering::SeqCst)
        }
        fn dropped(&self) -> usize {
            self.dropped.load(Ordering::SeqCst)
        }
        /// Lets spawned tasks run to their end, then checks nothing more happens.
        async fn settle(&self) {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    impl Future for Counter {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.pending > 0 {
                self.pending -= 1;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(())
        }
    }
    impl Drop for Counter {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll_once<F: Future + Send + 'static>(future: &mut HandOff<F>) -> Poll<F::Output>
    where
        F::Output: Send + 'static,
    {
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        Pin::new(future).poll(&mut cx)
    }

    #[tokio::test]
    async fn a_wrapper_dropped_before_its_first_poll_runs_the_future_once_to_its_end() {
        let probe = Probe::default();
        drop(HandOff::new(probe.future(3)));
        probe.settle().await;
        assert_eq!(probe.completed(), 1);
        assert_eq!(probe.polls(), 4);
        assert_eq!(probe.dropped(), 1);
    }

    #[tokio::test]
    async fn a_wrapper_dropped_while_pending_finishes_on_the_spawned_task_once() {
        let probe = Probe::default();
        let mut wrapper = HandOff::new(probe.future(3));
        assert!(poll_once(&mut wrapper).is_pending());
        assert_eq!((probe.completed(), probe.polls()), (0, 1));
        drop(wrapper);
        probe.settle().await;
        assert_eq!(probe.completed(), 1);
        assert_eq!(probe.polls(), 4, "the polls continue; none is repeated");
        assert_eq!(probe.dropped(), 1);
    }

    #[tokio::test]
    async fn a_wrapper_dropped_after_ready_spawns_nothing_and_runs_nothing_twice() {
        let probe = Probe::default();
        let alive = || {
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks()
        };
        let before = alive();
        let mut wrapper = HandOff::new(probe.future(2));
        assert!(poll_once(&mut wrapper).is_pending());
        assert!(poll_once(&mut wrapper).is_pending());
        assert!(poll_once(&mut wrapper).is_ready());
        assert_eq!((probe.completed(), probe.dropped()), (1, 1));
        drop(wrapper);
        probe.settle().await;
        assert_eq!(probe.completed(), 1);
        assert_eq!(probe.polls(), 3);
        assert_eq!(alive(), before, "no task was spawned");
    }

    #[tokio::test]
    async fn awaiting_to_the_end_in_place_needs_no_spawn() {
        let probe = Probe::default();
        let before = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        HandOff::new(probe.future(5)).await;
        assert_eq!(
            (probe.completed(), probe.polls(), probe.dropped()),
            (1, 6, 1)
        );
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            before
        );
    }

    #[test]
    fn a_wrapper_dropped_on_a_thread_without_a_runtime_drops_the_future() {
        let probe = Probe::default();
        let mut wrapper = HandOff::new(probe.future(3));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert!(poll_once(&mut wrapper).is_pending());
            });
        });
        // This thread has no runtime either.
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(wrapper);
        assert_eq!((probe.completed(), probe.dropped()), (0, 1));
        assert_eq!(probe.polls(), 1);
    }

    #[test]
    fn a_wrapper_never_polled_and_dropped_without_a_runtime_drops_the_future() {
        let probe = Probe::default();
        drop(HandOff::new(probe.future(0)));
        assert_eq!(
            (probe.polls(), probe.completed(), probe.dropped()),
            (0, 0, 1)
        );
    }

    #[tokio::test]
    async fn a_future_that_panics_while_polled_is_not_resumed_by_the_drop() {
        struct Panics(Arc<AtomicUsize>);
        impl Future for Panics {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("polled")
            }
        }
        let polls = Arc::new(AtomicUsize::new(0));
        let mut wrapper = HandOff::new(Panics(polls.clone()));
        // A runtime is current, so a resumed future would be spawned.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| poll_once(&mut wrapper)));
        assert!(outcome.is_err());
        drop(wrapper);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 1, "never polled again");
    }
}

//! Opt-in, test-only native accounting evidence. Never an exporter or error formatter.
//! Snapshots are bounded, nonblocking and point-in-time: pending work is not completion proof.
use serde::Serialize;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    PoolAcquireAndBegin,
    Setup,
    ScopeAudit,
    GovernanceStorage,
    ExplicitSettlement,
    CancellationSettlement,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Success,
    Error,
    Cancelled,
    Timeout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqlxKind {
    Database,
    PoolClosed,
    PoolTimedOut,
    Io,
    Tls,
    Protocol,
    WorkerCrashed,
    Other,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Event {
    pub stage: Stage,
    pub outcome: Outcome,
    pub duration_ns: u64,
    pub sqlx_error: Option<SqlxKind>,
    pub sqlstate: Option<String>,
}
#[derive(Debug, Serialize)]
pub(crate) struct Snapshot {
    pub events: Vec<Event>,
    pub pending: usize,
    pub dropped: usize,
    pub unavailable: bool,
}
/// A production accounting-failure signal (`db::accounting_failure`), mirrored
/// to the request that owns this capture. Never part of a [`Snapshot`].
pub(crate) type Signal = (
    super::accounting_failure::Stage,
    super::accounting_failure::Cause,
);
struct Inner {
    events: Mutex<Vec<Event>>,
    signals: Mutex<Vec<Signal>>,
    pending: AtomicUsize,
    dropped: AtomicUsize,
    writers: AtomicUsize,
    version: AtomicUsize,
}
/// Writers never wait. A snapshot overlapping any mutation is explicitly unavailable.
struct Mutation<'a>(&'a Inner);
impl<'a> Mutation<'a> {
    fn new(inner: &'a Inner) -> Self {
        inner.writers.fetch_add(1, Ordering::SeqCst);
        Self(inner)
    }
}
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        self.0.version.fetch_add(1, Ordering::SeqCst);
        self.0.writers.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
pub(crate) struct Handle(Arc<Inner>);
impl Handle {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            events: Mutex::new(Vec::with_capacity(CAPACITY)),
            signals: Mutex::new(Vec::new()),
            pending: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            version: AtomicUsize::new(0),
        }))
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_after_copy(|| {})
    }
    // Private deterministic interleaving seam: the copy's lock is released before the callback.
    fn snapshot_after_copy(&self, after_copy: impl FnOnce()) -> Snapshot {
        let version = self.0.version.load(Ordering::SeqCst);
        let writing = self.0.writers.load(Ordering::SeqCst) != 0;
        let events = self.0.events.try_lock().ok().map(|events| events.clone());
        after_copy();
        let pending = self.0.pending.load(Ordering::Relaxed);
        let dropped = self.0.dropped.load(Ordering::Relaxed);
        let changed = self.0.writers.load(Ordering::SeqCst) != 0
            || self.0.version.load(Ordering::SeqCst) != version;
        Snapshot {
            unavailable: events.is_none() || writing || changed,
            events: events.unwrap_or_default(),
            pending,
            dropped,
        }
    }
    /// The production signals recorded while this capture was the current one.
    pub fn signals(&self) -> Vec<Signal> {
        self.0
            .signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(super) fn signal(&self, signal: Signal) {
        let mut signals = self.0.signals.lock().unwrap_or_else(|e| e.into_inner());
        if signals.len() < CAPACITY {
            signals.push(signal);
        }
    }
    fn push(&self, event: Event) {
        let _mutation = Mutation::new(&self.0);
        if let Ok(mut events) = self.0.events.try_lock()
            && events.len() < CAPACITY
        {
            events.push(event);
            return;
        }
        self.0.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

tokio::task_local! { static CAPTURE: Option<Handle>; }

pub(crate) fn current() -> Option<Handle> {
    CAPTURE.try_with(Clone::clone).ok().flatten()
}
pub(crate) async fn scope<F: Future>(handle: Handle, future: F) -> F::Output {
    CAPTURE.scope(Some(handle), future).await
}
struct Pending(Handle);
impl Pending {
    fn new(handle: Handle) -> Self {
        {
            let _mutation = Mutation::new(&handle.0);
            handle.0.pending.fetch_add(1, Ordering::Relaxed);
        }
        Self(handle)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        let _mutation = Mutation::new(&self.0.0);
        self.0.0.pending.fetch_sub(1, Ordering::Relaxed);
    }
}
/// Deliberately NOT async: capture and register before spawning, even if never polled.
/// Unrelated tasks receive no ambient context. No observer allocation when disabled.
pub(crate) fn inherit<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let handle = current();
    let pending = handle.clone().map(Pending::new);
    async move {
        let _pending = pending;
        CAPTURE.scope(handle, future).await
    }
}

struct Operation {
    pending: Pending,
    stage: Stage,
    started: Instant,
    finished: bool,
}
impl Operation {
    fn new(handle: Handle, stage: Stage) -> Self {
        Self {
            pending: Pending::new(handle),
            stage,
            started: Instant::now(),
            finished: false,
        }
    }
    fn event(&self, outcome: Outcome, error: Option<&sqlx::Error>) -> Event {
        let (sqlx_error, sqlstate) = error.map(error_fields).unwrap_or((None, None));
        Event {
            stage: self.stage,
            outcome,
            duration_ns: self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            sqlx_error,
            sqlstate,
        }
    }
    fn finish(mut self, outcome: Outcome, error: Option<&sqlx::Error>) {
        self.pending.0.push(self.event(outcome, error));
        self.finished = true;
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished {
            self.pending.0.push(self.event(Outcome::Cancelled, None));
        }
    }
}
fn error_fields(error: &sqlx::Error) -> (Option<SqlxKind>, Option<String>) {
    // One classifier (#389): the production signal's, projected to the driver kind.
    use super::accounting_failure::{Cause, classify, sqlstate};
    let kind = match classify(error) {
        Cause::LockTimeout
        | Cause::StatementTimeout
        | Cause::BackendTerminated
        | Cause::Deadlock
        | Cause::ReadOnlyTransaction
        | Cause::OtherDatabase => SqlxKind::Database,
        Cause::PoolClosed => SqlxKind::PoolClosed,
        Cause::PoolTimeout => SqlxKind::PoolTimedOut,
        Cause::Io => SqlxKind::Io,
        Cause::Tls => SqlxKind::Tls,
        Cause::Protocol => SqlxKind::Protocol,
        Cause::WorkerCrashed => SqlxKind::WorkerCrashed,
        Cause::Other
        | Cause::DeadlineInQueue
        | Cause::DeadlineAcquiringConnection
        | Cause::DeadlineInDatabase => SqlxKind::Other,
    };
    (Some(kind), sqlstate(error).map(|code| code.into_owned()))
}
pub(crate) fn observe_sqlx(stage: Stage, error: &sqlx::Error) {
    if let Some(handle) = current() {
        Operation::new(handle, stage).finish(Outcome::Error, Some(error));
    }
}
pub(crate) async fn sql<F, T>(stage: Stage, future: F) -> Result<T, sqlx::Error>
where
    F: Future<Output = Result<T, sqlx::Error>>,
{
    let Some(handle) = current() else {
        return future.await;
    };
    let operation = Operation::new(handle, stage);
    let result = future.await;
    match &result {
        Ok(_) => operation.finish(Outcome::Success, None),
        Err(error) => operation.finish(Outcome::Error, Some(error)),
    }
    result
}

/// Observes the unchanged timeout future; it never constructs or extends a deadline.
/// Construction registers pending work before a detached cancellation task can run.
pub(crate) fn settlement<F>(
    handle: Option<Handle>,
    stage: Stage,
    future: F,
) -> impl Future<Output = F::Output>
where
    F: Future<
        Output = Result<
            Result<(), crate::inference::error::InferenceError>,
            tokio::time::error::Elapsed,
        >,
    >,
{
    let observation = handle.map(|handle| {
        let operation = Operation::new(handle.clone(), stage);
        (handle, operation)
    });
    async move {
        let Some((handle, operation)) = observation else {
            // An unobserved guard must not adopt a later poller's unrelated recorder.
            return CAPTURE.scope(None, future).await;
        };
        let result = scope(handle, future).await;
        let outcome = match &result {
            Ok(Ok(())) => Outcome::Success,
            Ok(Err(_)) => Outcome::Error,
            Err(_) => Outcome::Timeout,
        };
        operation.finish(outcome, None);
        result
    }
}

#[cfg(test)]
mod tests;

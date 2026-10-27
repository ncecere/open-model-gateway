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
struct Inner {
    events: Mutex<Vec<Event>>,
    pending: AtomicUsize,
    dropped: AtomicUsize,
}
#[derive(Clone)]
pub(crate) struct Handle(Arc<Inner>);
impl Handle {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            events: Mutex::new(Vec::with_capacity(CAPACITY)),
            pending: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }))
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_after_copy(|| {})
    }
    // Private deterministic interleaving seam: the copy's lock is released before the callback.
    fn snapshot_after_copy(&self, after_copy: impl FnOnce()) -> Snapshot {
        let events = self.0.events.try_lock().ok().map(|events| events.clone());
        after_copy();
        Snapshot {
            unavailable: events.is_none(),
            events: events.unwrap_or_default(),
            pending: self.0.pending.load(Ordering::Relaxed),
            dropped: self.0.dropped.load(Ordering::Relaxed),
        }
    }
    fn push(&self, event: Event) {
        if let Ok(mut events) = self.0.events.try_lock()
            && events.len() < CAPACITY
        {
            events.push(event);
            return;
        }
        self.0.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

tokio::task_local! { static CAPTURE: Handle; }

pub(crate) fn current() -> Option<Handle> {
    CAPTURE.try_with(Clone::clone).ok()
}
pub(crate) async fn scope<F: Future>(handle: Handle, future: F) -> F::Output {
    CAPTURE.scope(handle, future).await
}
struct Pending(Handle);
impl Pending {
    fn new(handle: Handle) -> Self {
        handle.0.pending.fetch_add(1, Ordering::Relaxed);
        Self(handle)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
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
        match handle {
            Some(handle) => scope(handle, future).await,
            None => future.await,
        }
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
    let kind = match error {
        sqlx::Error::Database(_) => SqlxKind::Database,
        sqlx::Error::PoolClosed => SqlxKind::PoolClosed,
        sqlx::Error::PoolTimedOut => SqlxKind::PoolTimedOut,
        sqlx::Error::Io(_) => SqlxKind::Io,
        sqlx::Error::Tls(_) => SqlxKind::Tls,
        sqlx::Error::Protocol(_) => SqlxKind::Protocol,
        sqlx::Error::WorkerCrashed => SqlxKind::WorkerCrashed,
        _ => SqlxKind::Other,
    };
    let code = error.as_database_error().and_then(|error| error.code());
    let sqlstate = code
        .filter(|code| {
            code.len() == 5
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
        .map(|code| code.into_owned());
    (Some(kind), sqlstate)
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
            return future.await;
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

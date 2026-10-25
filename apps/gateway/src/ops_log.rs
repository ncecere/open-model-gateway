//! Optional OTLP operational logs (#294): typed events only, disabled by
//! default. No tracing-subscriber bridge, schema migration or `OTEL_*`
//! setting is read here. [`OpsEvent`] is a closed, exhaustively-encoded
//! enum: every field is either a fixed, approved enum or a bounded integer
//! (the limiter's suppressed-count attribute); there is no generic
//! attribute map, `String` payload, formatting or deserialization, so a
//! caller cannot inject free text, a request id, a prompt or a collector
//! response into the wire body. [`LogTraceId`] is the one opaque exception:
//! only generation finalization may mint it, from already shared-admission
//! proven provenance (never a public string/UUID constructor).
//!
//! Wire mapping (opentelemetry-proto v1.9.0 logs/v1/logs.proto and
//! collector/logs/v1/logs_service.proto): resourceLogs -> scopeLogs ->
//! logRecords, scope `omg.ops`. Severity numbers follow the OTLP table
//! (INFO=9, WARN=13, ERROR=17); timestamps and the optional hexadecimal
//! `traceId` follow the same JSON conventions as traces and metrics.
//!
//! One bounded worker per process: a 1,024-record `try_send` queue, records
//! capped at 512 bytes, batches of at most 100 records/64 KiB every 5
//! seconds (missed ticks skipped, never replayed), one immediate final batch
//! on shutdown. Producers (including `egress::denied`, lease renewal,
//! background jobs, the configuration listener and inference/job
//! finalization) only perform bounded atomic operations and `try_send`;
//! encoding and network I/O stay on the worker, off every hot path. A
//! per-variant limiter admits at most one event every 30 seconds (burst
//! one); the next admitted event of that variant carries the saturating
//! suppressed count. Severity filtering happens before limiting, so a
//! floor-filtered event never touches the limiter or the queue.
//!
//! The logs transport's own egress resolver and client never emit
//! `OpsEvent`s (`egress::Policy::without_ops_events`): a refusal of the
//! logs destination cannot feed the very queue it would need to deliver
//! through. Ordinary egress metrics and the stderr warning are unaffected.
//! Audit-exporter transitions are excluded entirely; audit stays on its own
//! pipeline (URL comparison cannot detect a collector alias, so audit and
//! operational logs need separately routed credentials/pipelines).
use std::{
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::otlp::{Delivery, fixed_enum};

fixed_enum!(DropReason {
    QueueFull => "queue_full", RecordLimit => "record_limit", BodyLimit => "body_limit",
    Delivery => "delivery", Shutdown => "shutdown"
});
fixed_enum!(Signal { Traces => "traces", Metrics => "metrics" });
fixed_enum!(BackgroundFailure { Error => "error", Timeout => "timeout" });
fixed_enum!(Finalizer {
    GenerationFinish => "generation_finish", GenerationCancel => "generation_cancel",
    RealtimeFinish => "realtime_finish", RealtimeCancel => "realtime_cancel",
    JobFail => "job_fail", JobCancel => "job_cancel"
});
fixed_enum!(Job {
    LockWaitPrune => "lock_wait_prune", RatePrune => "rate_prune",
    LoginAttemptPrune => "login_attempt_prune", SessionPrune => "session_prune",
    StorageUsage => "storage_usage", Compaction => "compaction", Partitions => "partitions",
    Rollups => "rollups", HistoryVerify => "history_verify", Lifecycle => "lifecycle",
    FileSweep => "file_sweep"
});
/// Shared `Delivery` failures plus `Rejected` (a partial-success collector
/// acknowledgement also degrades the exporter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFailure {
    HttpStatus,
    Transport,
    Timeout,
    Acknowledgement,
    Rejected,
}
impl ExportFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HttpStatus => "http_status",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Acknowledgement => "acknowledgement",
            Self::Rejected => "rejected",
        }
    }
    /// `Delivery::Accepted` never reaches here in practice (callers only
    /// convert a failed exchange); map it defensively rather than panic.
    pub(crate) fn from_delivery(delivery: Delivery) -> Self {
        match delivery {
            Delivery::HttpStatus => Self::HttpStatus,
            Delivery::Transport => Self::Transport,
            Delivery::Timeout => Self::Timeout,
            Delivery::Acknowledgement | Delivery::Accepted => Self::Acknowledgement,
        }
    }
}

/// `info|warn|error`, the OTLP SeverityNumber a floor compares against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Error,
}
impl Severity {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
    fn number(self) -> u8 {
        match self {
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
        }
    }
    fn text(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Opaque proof that a trace id came from existing shared-admission-proven
/// `log_trace_id` provenance (#98). No crate-wide raw constructor: only code
/// inside `crate::inference` (where that provenance is computed, right
/// after admission commits) may mint one, from the value provenance already
/// computed; no arbitrary string or UUID is accepted elsewhere. `as_str` is
/// read-only and does not let a caller forge a new value from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTraceId(String);
impl LogTraceId {
    pub(in crate::inference) fn from_provenance(hex: String) -> Self {
        Self(hex)
    }
    /// Test fixtures only: compiled out of every production build, so this
    /// is never a production forgery path.
    #[cfg(test)]
    pub(crate) fn for_test(hex: &str) -> Self {
        Self(hex.to_owned())
    }
    /// Read-only: the existing hexadecimal value, for stderr diagnostics and
    /// the wire `traceId` field. Never lets a caller mint a new value.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Seven closed variants (#294). Every field is a fixed, approved enum or
/// (via the limiter, not a variant field) a bounded integer; there is no
/// `String`, formatting, arbitrary attribute or deserialization path.
///
/// ```compile_fail
/// // A free-text body/attribute never compiles: no variant accepts a String.
/// let _ = open_model_gateway::ops_log::OpsEvent::ConfigListenerDown("not allowed".to_string());
/// ```
/// ```compile_fail
/// // LogTraceId has no public constructor: a caller cannot forge correlation.
/// let _ = open_model_gateway::ops_log::LogTraceId("forged-trace-id".to_string());
/// ```
/// ```compile_fail
/// // Unknown fields never compile against the closed variant shape.
/// let _ = open_model_gateway::ops_log::OpsEvent::EgressRefused {
///     purpose: open_model_gateway::egress::Purpose::Provider,
///     host: "collector.invalid",
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpsEvent {
    ExporterDegraded {
        signal: Signal,
        failure: ExportFailure,
    },
    ExporterRecovered {
        signal: Signal,
    },
    EgressRefused {
        purpose: crate::egress::Purpose,
    },
    LeaseLost {
        lease: crate::leases::Lease,
    },
    ConfigListenerDown,
    FinalizationNeedsReconciliation {
        finalizer: Finalizer,
        trace: Option<LogTraceId>,
    },
    BackgroundJobFailed {
        job: Job,
        failure: BackgroundFailure,
    },
}
impl OpsEvent {
    pub(crate) const COUNT: usize = 7;
    /// Fixed metric-label slugs, indexed exactly like [`OpsEvent::index`]; the
    /// precreated `gateway_otlp_logs_suppressed_total{event}` series.
    pub(crate) const SLUGS: [&'static str; Self::COUNT] = [
        "exporter_degraded",
        "exporter_recovered",
        "egress_refused",
        "lease_lost",
        "config_listener_down",
        "finalization_needs_reconciliation",
        "background_job_failed",
    ];
    fn index(&self) -> usize {
        match self {
            Self::ExporterDegraded { .. } => 0,
            Self::ExporterRecovered { .. } => 1,
            Self::EgressRefused { .. } => 2,
            Self::LeaseLost { .. } => 3,
            Self::ConfigListenerDown => 4,
            Self::FinalizationNeedsReconciliation { .. } => 5,
            Self::BackgroundJobFailed { .. } => 6,
        }
    }
    fn severity(&self) -> Severity {
        match self {
            Self::ExporterRecovered { .. } => Severity::Info,
            Self::FinalizationNeedsReconciliation { .. } => Severity::Error,
            _ => Severity::Warn,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::ExporterDegraded { .. } => "omg.ops.exporter_degraded",
            Self::ExporterRecovered { .. } => "omg.ops.exporter_recovered",
            Self::EgressRefused { .. } => "omg.ops.egress_refused",
            Self::LeaseLost { .. } => "omg.ops.lease_lost",
            Self::ConfigListenerDown => "omg.ops.config_listener_down",
            Self::FinalizationNeedsReconciliation { .. } => {
                "omg.ops.finalization_needs_reconciliation"
            }
            Self::BackgroundJobFailed { .. } => "omg.ops.background_job_failed",
        }
    }
    fn body(&self) -> &'static str {
        match self {
            Self::ExporterDegraded { .. } => "Exporter degraded",
            Self::ExporterRecovered { .. } => "Exporter recovered",
            Self::EgressRefused { .. } => "Egress refused",
            Self::LeaseLost { .. } => "Lease lost",
            Self::ConfigListenerDown => "Configuration listener unavailable",
            Self::FinalizationNeedsReconciliation { .. } => "Finalization requires reconciliation",
            Self::BackgroundJobFailed { .. } => "Background job failed",
        }
    }
    fn trace(&self) -> Option<&LogTraceId> {
        match self {
            Self::FinalizationNeedsReconciliation { trace, .. } => trace.as_ref(),
            _ => None,
        }
    }
    fn attributes(&self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::ExporterDegraded { signal, failure } => {
                vec![("signal", signal.as_str()), ("failure", failure.as_str())]
            }
            Self::ExporterRecovered { signal } => vec![("signal", signal.as_str())],
            Self::EgressRefused { purpose } => vec![("purpose", purpose.as_str())],
            Self::LeaseLost { lease } => vec![("lease", lease.as_str())],
            Self::ConfigListenerDown => vec![],
            Self::FinalizationNeedsReconciliation { finalizer, .. } => {
                vec![("finalizer", finalizer.as_str())]
            }
            Self::BackgroundJobFailed { job, failure } => {
                vec![("job", job.as_str()), ("failure", failure.as_str())]
            }
        }
    }
}

/// One queued occurrence: captured once at admission time, never recomputed.
struct Queued {
    event: OpsEvent,
    at: u64,
    suppressed: u64,
}

#[derive(Default)]
struct Limiter {
    /// Milliseconds (offset, never zero) since process start of the last
    /// admitted event per variant; zero means "never admitted".
    last: [AtomicU64; OpsEvent::COUNT],
    suppressed: [AtomicU64; OpsEvent::COUNT],
}
/// Burst one, refill every 30 seconds, independent of severity filtering
/// (which runs first and never touches the limiter).
const REFILL: Duration = Duration::from_millis(30_000);
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
/// Offset so "never admitted" (0) is never produced by elapsed time itself.
fn now_ms() -> u64 {
    ORIGIN.elapsed().as_millis() as u64 + 86_400_000
}
impl Limiter {
    /// `Some(suppressed_count)` to admit (and reset that count to zero), or
    /// `None` to suppress (and count it). A compare-exchange claims the
    /// admission atomically: concurrent callers in the same refill window
    /// can never all observe "ready" and all admit (a burst beyond one), and
    /// the suppressed-count swap belongs only to whichever caller actually
    /// won the admission.
    fn admit(&self, index: usize) -> Option<u64> {
        let now = now_ms();
        loop {
            let last = self.last[index].load(Ordering::Acquire);
            if last != 0 && now.saturating_sub(last) < REFILL.as_millis() as u64 {
                self.suppressed[index].fetch_add(1, Ordering::AcqRel);
                return None;
            }
            if self.last[index]
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(self.suppressed[index].swap(0, Ordering::AcqRel));
            }
            // Lost the race to another admitting caller: re-read `last` (it
            // now reflects the winner) rather than admit a second time.
        }
    }
}

struct Shared {
    /// `u8::MAX`: disabled (no Runtime started). Otherwise the configured
    /// floor's severity number; an event below it is dropped before the
    /// limiter, uncounted (not a `DropReason`, like disabled Traffic families).
    threshold: AtomicU8,
    limiter: Limiter,
    /// A reader-writer lock, never a plain `Mutex`: `emit` (every request
    /// and background-job path) only ever takes a non-blocking `try_read`,
    /// so any number of concurrent producers can read the sender without
    /// contending each other; only `configure` (Runtime start/shutdown,
    /// never a hot path) takes the rare blocking write lock.
    sender: std::sync::RwLock<Option<mpsc::Sender<Queued>>>,
}
const DISABLED: u8 = u8::MAX;
static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    threshold: AtomicU8::new(DISABLED),
    limiter: Limiter::default(),
    sender: std::sync::RwLock::new(None),
});

/// Runtime-only: attach (enabled) or detach (disabled/shutdown) the process
/// singleton. Detaching first closes admission before any drain.
fn configure(sender: Option<mpsc::Sender<Queued>>, min_severity: Option<Severity>) {
    *SHARED.sender.write().unwrap_or_else(|e| e.into_inner()) = sender;
    SHARED.threshold.store(
        min_severity.map_or(DISABLED, Severity::number),
        Ordering::Release,
    );
}

/// The only production entry point. Bounded atomic operations, a
/// non-blocking read and `try_send` only: no encoding, allocation beyond one
/// small struct, or network I/O happens on the caller's thread, and this
/// never blocks a request or background-job path.
pub fn emit(event: OpsEvent) {
    let threshold = SHARED.threshold.load(Ordering::Acquire);
    if threshold == DISABLED || event.severity().number() < threshold {
        return;
    }
    let index = event.index();
    let Some(suppressed) = SHARED.limiter.admit(index) else {
        crate::metrics::METRICS.otlp_logs_suppressed(index, 1);
        return;
    };
    // try_read never blocks: a momentary contention with the rare `configure`
    // writer (Runtime start/shutdown) is counted exactly like a closed queue,
    // never awaited.
    let Ok(guard) = SHARED.sender.try_read() else {
        crate::metrics::METRICS.otlp_logs_drop(DropReason::Shutdown, 1);
        return;
    };
    let sender = guard.clone();
    drop(guard);
    let Some(sender) = sender else {
        // Enabled but detached mid-emit (a shutdown race): never panics or
        // blocks; counted the same as a closed queue.
        crate::metrics::METRICS.otlp_logs_drop(DropReason::Shutdown, 1);
        return;
    };
    let queued = Queued {
        event,
        at: crate::telemetry::now_unix_nanos(),
        suppressed,
    };
    if let Err(error) = sender.try_send(queued) {
        let reason = match error {
            mpsc::error::TrySendError::Full(_) => DropReason::QueueFull,
            mpsc::error::TrySendError::Closed(_) => DropReason::Shutdown,
        };
        crate::metrics::METRICS.otlp_logs_drop(reason, 1);
    }
}

const MAX_RECORD: usize = 512;
const MAX_BATCH: usize = 100;
const MAX_BODY: usize = 64 * 1024;
const QUEUE: usize = 1024;
const BATCH_INTERVAL: Duration = Duration::from_secs(5);
const EXCHANGE: Duration = Duration::from_secs(2);
const DRAIN: Duration = Duration::from_secs(3);

fn log_record(event: &OpsEvent, at: u64, suppressed: u64) -> Value {
    let mut attributes: Vec<Value> = event
        .attributes()
        .into_iter()
        .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
        .collect();
    attributes.push(json!({"key": "event.name", "value": {"stringValue": event.name()}}));
    if suppressed > 0 {
        attributes.push(json!({
            "key": "omg.ops.suppressed_count",
            "value": {"intValue": suppressed.to_string()}
        }));
    }
    let mut record = json!({
        "timeUnixNano": at.to_string(),
        "observedTimeUnixNano": at.to_string(),
        "severityNumber": event.severity().number(),
        "severityText": event.severity().text(),
        "body": {"stringValue": event.body()},
        "attributes": attributes,
    });
    if let Some(trace) = event.trace() {
        record["traceId"] = json!(trace.as_str());
    }
    record
}

fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("finite, bounded JSON values serialize")
        .len()
}

struct Exporter {
    url: reqwest::Url,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    environment: crate::config::Environment,
    instance: uuid::Uuid,
}
impl Exporter {
    fn envelope(&self, records: &[Value]) -> Value {
        json!({"resourceLogs":[{
            "resource": {"attributes": crate::otlp::resource_attributes(self.environment, self.instance)},
            "scopeLogs": [{
                "scope": {"name": "omg.ops", "version": env!("CARGO_PKG_VERSION")},
                "logRecords": records
            }]
        }]})
    }
    fn body(&self, records: &[Value]) -> Vec<u8> {
        serde_json::to_vec(&self.envelope(records)).expect("bounded JSON values serialize")
    }
    async fn send(&self, body: Vec<u8>, count: usize) -> Result<u64, Delivery> {
        crate::otlp::post(
            &self.client,
            &self.url,
            &self.headers,
            body,
            count,
            "rejectedLogRecords",
            EXCHANGE,
        )
        .await
    }
}

/// Owns the queue receiver and how many records are mid-exchange right now.
/// Dropping it (a normal return, *or* a cancelled/aborted task) accounts
/// every record that was never explicitly counted: the `in_flight` ones
/// bracketing the one cancellable `.await` in [`send_batch`], plus whatever
/// is still queued. Cancellation (a forced shutdown abort) can therefore
/// never lose a record uncounted.
struct Pending {
    receiver: mpsc::Receiver<Queued>,
    in_flight: usize,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.receiver.close();
        let mut n = self.in_flight as u64;
        while self.receiver.try_recv().is_ok() {
            n += 1;
        }
        if n > 0 {
            crate::metrics::METRICS.otlp_logs_drop(DropReason::Shutdown, n);
        }
    }
}

/// One immediate batch of whatever is currently queued (never a retry/spool):
/// at most [`MAX_BATCH`] records within [`MAX_BODY`], oversized or
/// already-malformed records dropped and counted, never fabricated.
/// `pending.in_flight` brackets the exchange so a cancellation mid-`.await`
/// is still accounted by [`Pending::drop`], never silently lost.
async fn send_batch(exporter: &Exporter, pending: &mut Pending) {
    let overhead = json_size(&exporter.envelope(&[]));
    let mut records = Vec::new();
    let mut size = overhead;
    let mut count = 0;
    while count < MAX_BATCH {
        let Ok(queued) = pending.receiver.try_recv() else {
            break;
        };
        let record = log_record(&queued.event, queued.at, queued.suppressed);
        let bytes = json_size(&record);
        if bytes > MAX_RECORD {
            crate::metrics::METRICS.otlp_logs_drop(DropReason::RecordLimit, 1);
            continue;
        }
        let extra = bytes + usize::from(!records.is_empty());
        if size + extra > MAX_BODY {
            crate::metrics::METRICS.otlp_logs_drop(DropReason::BodyLimit, 1);
            break;
        }
        size += extra;
        records.push(record);
        count += 1;
    }
    if records.is_empty() {
        return;
    }
    let sent = records.len();
    let body = exporter.body(&records);
    debug_assert!(body.len() <= MAX_BODY);
    pending.in_flight = sent;
    let result = exporter.send(body, sent).await;
    pending.in_flight = 0;
    match result {
        Ok(rejected) => {
            crate::metrics::METRICS.otlp_logs_push(Delivery::Accepted);
            crate::metrics::METRICS.otlp_logs_rejected(rejected);
        }
        Err(outcome) => {
            crate::metrics::METRICS.otlp_logs_push(outcome);
            crate::metrics::METRICS.otlp_logs_drop(DropReason::Delivery, sent as u64);
        }
    }
}

async fn worker(
    exporter: Exporter,
    receiver: mpsc::Receiver<Queued>,
    interval: Duration,
    mut stop: oneshot::Receiver<()>,
) {
    let mut pending = Pending {
        receiver,
        in_flight: 0,
    };
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = tick.tick() => {}
        }
        send_batch(&exporter, &mut pending).await;
    }
    // Shutdown: one immediate final batch of whatever is already queued.
    // Pending::drop (below, at end of scope) accounts anything still left,
    // including a cancelled/aborted in-flight exchange.
    send_batch(&exporter, &mut pending).await;
}

/// Owns one worker and the process-wide `emit` attachment. Disabled (no
/// URL) initializes nothing: no task, socket or process identity draw.
pub struct Runtime {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Runtime {
    pub fn start(
        config: crate::config::LogsExportConfig,
        environment: crate::config::Environment,
    ) -> Option<Self> {
        let (url, client) = config.endpoint?;
        let instance = (*crate::otlp::INSTANCE).ok()?;
        let exporter = Exporter {
            url,
            client,
            headers: config.headers,
            environment,
            instance,
        };
        let (sender, receiver) = mpsc::channel(QUEUE);
        configure(Some(sender), Some(config.min_severity));
        let (stop, stopping) = oneshot::channel();
        let task = tokio::spawn(worker(exporter, receiver, BATCH_INTERVAL, stopping));
        Some(Self {
            stop: Some(stop),
            task,
        })
    }
    pub async fn shutdown(mut self) {
        // Close admission before any drain: no new event enqueues once
        // shutdown begins, matching the shared traces/metrics/logs budget.
        configure(None, None);
        let deadline = tokio::time::Instant::now() + DRAIN;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout_at(deadline, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        configure(None, None);
        self.task.abort();
    }
}

#[cfg(test)]
/// Serializes tests that attach/detach the process-wide singleton
/// (`configure`/`emit`); white-box encoder/worker tests never need it.
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
#[cfg(test)]
#[path = "ops_log_tests.rs"]
mod tests;

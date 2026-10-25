//! Prometheus/OpenMetrics instrumentation (docs/operations.md).
//!
//! Served only by the optional, separate `GATEWAY_METRICS_ADDR` listener, never
//! on the public port. Labels are low cardinality by construction: route
//! templates (never raw paths), status codes, provider kinds, configured public
//! model names (admission-validated, bounded below), outcome/error codes and
//! limit scopes. Never workspace, key, user or request identifiers, and never
//! prompt/response data.
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus_client::{
    encoding::text::encode,
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
    registry::Registry,
};

use crate::{inference::error::InferenceError, store::Store};

type L0 = [(&'static str, String); 0];
type L1 = [(&'static str, String); 1];
type L2 = [(&'static str, String); 2];
type L3 = [(&'static str, String); 3];
type Histograms<L> = Family<L, Histogram, fn() -> Histogram>;

/// Distinct values admitted per dynamic label before folding into `other`.
const MAX_DYNAMIC_VALUES: usize = 200;
const MAX_LABEL_CHARS: usize = 96;
/// Reservation gauges are refreshed at most this often, however often scraped.
const RESERVATION_REFRESH: Duration = Duration::from_secs(15);

// Pending work is bounded by its admission window; terminal failures must
// remain visible for a full observation window after completion/cleanup.
const ALERT_DELIVERY_COUNTS: &str = "SELECT recent.*,pending.* FROM (SELECT count(*) FILTER(WHERE retrying),count(*) FILTER(WHERE exhausted>0),count(*) FILTER(WHERE status IN ('failed','partial')),count(*) FILTER(WHERE status='suppressed') FROM alert_deliveries WHERE event_id IS NOT NULL AND ((status='pending' AND created_at>clock_timestamp()-interval '24 hours') OR (status<>'pending' AND completed_at>clock_timestamp()-interval '24 hours'))) recent CROSS JOIN (SELECT count(DISTINCT bucket_id),floor(greatest(0,coalesce(extract(epoch FROM clock_timestamp()-min(created_at)),0)))::bigint FROM alert_deliveries WHERE status='pending') pending";

pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

fn request_buckets() -> Histogram {
    // 5 ms .. ~82 s
    Histogram::new(exponential_buckets(0.005, 2.0, 15))
}
fn upstream_buckets() -> Histogram {
    // 10 ms .. ~164 s
    Histogram::new(exponential_buckets(0.01, 2.0, 15))
}
fn phase_buckets() -> Histogram {
    // 100 µs .. ~52 s: admission/settlement phases are sub-millisecond when
    // uncontended and seconds when queued behind the installation lock.
    Histogram::new(exponential_buckets(0.0001, 2.0, 20))
}

/// Phases of one durable admission (`governance::admit*`), in order. The
/// scale design's future scoped protocol renames `limits` to `lock_rows`.
pub const ADMISSION_PHASES: [&str; 8] = [
    "queue", "connect", "locks", "read", "limits", "write", "commit", "total",
];
/// Phases of one terminal settlement (`governance::finish*`), in order.
pub const SETTLEMENT_PHASES: [&str; 7] = [
    "queue", "connect", "locks", "read", "write", "commit", "total",
];

/// Wall-clock time per phase of one admission or settlement transaction.
/// `phase` closes the span since the previous mark; phases never reached
/// (early return) are simply not observed. `total` is always observed.
pub(crate) struct PhaseTimer {
    started: Instant,
    mark: Instant,
    spans: Vec<(&'static str, Duration)>,
}
impl PhaseTimer {
    pub(crate) fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            mark: now,
            spans: Vec::with_capacity(8),
        }
    }
    pub(crate) fn phase(&mut self, name: &'static str) {
        let now = Instant::now();
        self.spans.push((name, now - self.mark));
        self.mark = now;
    }
    fn observe(self, family: &Histograms<L2>, outcome: &'static str) {
        let total = self.started.elapsed();
        for (phase, elapsed) in self
            .spans
            .into_iter()
            .chain(std::iter::once(("total", total)))
        {
            family
                .get_or_create(&[("phase", phase.to_owned()), ("outcome", outcome.to_owned())])
                .observe(elapsed.as_secs_f64());
        }
    }
}

/// An in-progress scoped lock acquisition ([`Metrics::scope_lock_wait`]).
pub(crate) struct ScopeLockWait {
    metrics: &'static Metrics,
    lock: &'static str,
    path: &'static str,
    started: Instant,
}
impl ScopeLockWait {
    /// Time waited so far.
    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}
impl Drop for ScopeLockWait {
    fn drop(&mut self) {
        let path = [("path", self.path.to_owned())];
        self.metrics.scope_lock_waiting.get_or_create(&path).dec();
        self.metrics
            .scope_lock_wait
            .get_or_create(&[
                ("lock", self.lock.to_owned()),
                ("path", self.path.to_owned()),
            ])
            .observe(self.started.elapsed().as_secs_f64());
    }
}

/// Outcome label of an admission: `admitted`, `denied` (a limit, budget or
/// unresolved-usage denial, also counted by `gateway_admission_denials_total`),
/// `error` (storage/database failure) or `rejected` (any other refusal, such as
/// a model that is no longer available or an unbounded price).
pub(crate) fn admission_outcome(result: &Result<(), InferenceError>) -> &'static str {
    match result {
        Ok(()) => "admitted",
        Err(InferenceError::Storage) => "error",
        Err(error) if denial_scope(*error).is_some() => "denied",
        Err(_) => "rejected",
    }
}

/// Limit scope label of an admission denial; `None` for other errors.
fn denial_scope(error: InferenceError) -> Option<&'static str> {
    use crate::inference::error::LimitScope;
    let scope = |s: LimitScope| match s {
        LimitScope::Workspace => "workspace",
        LimitScope::ApiKey => "api_key",
    };
    match error {
        InferenceError::Busy => Some("policy"),
        InferenceError::BudgetExceeded(s)
        | InferenceError::UnresolvedUsage(s)
        | InferenceError::TokenReservationExceedsLimit(s)
        | InferenceError::JobLimitExceeded(s) => Some(scope(s)),
        _ => None,
    }
}

pub struct Metrics {
    registry: Registry,
    http_requests: Family<L3, Counter>,
    http_duration: Histograms<L2>,
    attempts: Family<L3, Counter>,
    attempt_errors: Family<L2, Counter>,
    upstream_latency: Histograms<L2>,
    time_to_first_token: Histograms<L2>,
    tokens: Family<L3, Counter>,
    settlements: Family<L1, Counter>,
    denials: Family<L2, Counter>,
    admission_phases: Histograms<L2>,
    settlement_phases: Histograms<L2>,
    reservations: Family<L1, Gauge>,
    alert_runs: Family<L1, Counter>,
    alert_rule_failures: Counter,
    alert_delivery_attempts: Family<L1, Counter>,
    alert_delivery_queue: Family<L1, Gauge>,
    alert_webhook_attempts: Family<L1, Counter>,
    alert_webhook_queue: Family<L1, Gauge>,
    alert_webhook_oldest: Family<L0, Gauge>,
    alert_pending_buckets: Family<L0, Gauge>,
    alert_oldest_pending_age: Family<L0, Gauge>,
    deadlock_retries: Family<L1, Counter>,
    scope_lock_wait: Histograms<L2>,
    scope_lock_waiting: Family<L1, Gauge>,
    pool_connections: Family<L1, Gauge>,
    pool_max: Gauge,
    collection_errors: Family<L1, Counter>,
    file_store_ops: Family<L3, Counter>,
    file_store_bytes: Family<L2, Counter>,
    batches: Family<L2, Counter>,
    batch_lines: Family<L3, Counter>,
    batch_queue: Family<L1, Gauge>,
    batch_workers: Family<L1, Gauge>,
    batch_route_lines: Family<L2, Gauge>,
    batch_paused_routes: Family<L1, Gauge>,
    batch_route_pauses: Family<L1, Counter>,
    batch_route_providers: Mutex<HashSet<String>>,
    cache_lookups: Family<L2, Counter>,
    config_changes: Family<L1, Counter>,
    config_poll_failures: Counter,
    auth_throttled: Family<L2, Counter>,
    key_issuance_refused: Family<L2, Counter>,
    egress_denied: Family<L1, Counter>,
    audit_export_deliveries: Family<L1, Counter>,
    audit_export_rejected_records: Counter,
    trace_dropped_spans: [Counter; crate::telemetry::DropReason::COUNT],
    trace_deliveries: [Counter; crate::telemetry::Delivery::COUNT],
    trace_rejected_spans: Counter,
    otlp_metrics_pushes: [Counter; crate::otlp::Delivery::COUNT],
    otlp_metrics_dropped: [Counter; crate::metrics_export::DropReason::COUNT],
    otlp_metrics_rejected_points: Counter,
    audit_export_queued: Gauge,
    audit_export_oldest:
        prometheus_client::metrics::gauge::Gauge<f64, std::sync::atomic::AtomicU64>,
    config_listener: Gauge,
    leases_held: Family<L1, Gauge>,
    lease_terms: Family<L1, Counter>,
    background_runs: Family<L2, Counter>,
    report_outcomes: Family<L1, Counter>,
    report_delivery_attempts: Family<L1, Counter>,
    report_delivery_queue: Family<L1, Gauge>,
    report_backlog: Family<L0, Gauge>,
    report_backlog_capped: Family<L0, Gauge>,
    report_oldest_due_lag: Family<L0, Gauge>,
    partition_months: Family<L1, Gauge>,
    history_findings: Gauge,
    rollup_hours: Family<L1, Counter>,
    providers: Mutex<HashSet<String>>,
    models: Mutex<HashSet<String>>,
    reservations_refreshed: Mutex<Option<Instant>>,
}

pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// One of a fixed set, so a client cannot mint label values.
fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

fn bounded(seen: &Mutex<HashSet<String>>, value: &str) -> String {
    let clean = value.chars().count() <= MAX_LABEL_CHARS
        && !value.is_empty()
        && value.chars().all(|c| !c.is_control());
    if !clean {
        return "other".into();
    }
    let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.contains(value) || seen.len() < MAX_DYNAMIC_VALUES {
        seen.insert(value.to_owned());
        value.to_owned()
    } else {
        "other".into()
    }
}

/// Labels of an admitted attempt, read back from the durable execution row.
pub struct AttemptLabels {
    pub provider: String,
    pub model: String,
}

/// Terminal accounting of an attempt (see `governance::finish_with_telemetry`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Exact cost known and recorded.
    Settled,
    /// Cost unknown: the hold (floor) is retained.
    Unknown,
    /// Finalization failed; the reservation stays pending until reconciliation.
    Held,
}
impl Settlement {
    fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Unknown => "unknown",
            Self::Held => "held",
        }
    }
}

pub struct AttemptObservation<'a> {
    pub labels: &'a AttemptLabels,
    pub outcome: &'static str,
    pub error: Option<InferenceError>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub generation_ms: Option<u64>,
    pub time_to_first_token_ms: Option<u64>,
}

impl Metrics {
    fn new() -> Self {
        let mut registry = Registry::with_prefix("gateway");
        // Pre-create every trace label once. Recording increments plain atomic
        // counters, never takes a Family's lock or allocates label strings.
        let trace_drops = Family::<L1, Counter>::default();
        let trace_dropped_spans = std::array::from_fn(|i| {
            trace_drops
                .get_or_create(&[(
                    "reason",
                    crate::telemetry::DropReason::ALL[i].as_str().to_owned(),
                )])
                .clone()
        });
        let trace_exchanges = Family::<L1, Counter>::default();
        let trace_deliveries = std::array::from_fn(|i| {
            trace_exchanges
                .get_or_create(&[(
                    "outcome",
                    crate::telemetry::Delivery::ALL[i].as_str().to_owned(),
                )])
                .clone()
        });
        let metric_pushes = Family::<L1, Counter>::default();
        let otlp_metrics_pushes = std::array::from_fn(|i| {
            metric_pushes
                .get_or_create(&[("outcome", crate::otlp::Delivery::ALL[i].as_str().to_owned())])
                .clone()
        });
        let metric_drops = Family::<L1, Counter>::default();
        let otlp_metrics_dropped = std::array::from_fn(|i| {
            metric_drops
                .get_or_create(&[(
                    "reason",
                    crate::metrics_export::DropReason::ALL[i]
                        .as_str()
                        .to_owned(),
                )])
                .clone()
        });
        let metrics = Self {
            http_requests: Family::default(),
            http_duration: Family::new_with_constructor(request_buckets),
            attempts: Family::default(),
            attempt_errors: Family::default(),
            upstream_latency: Family::new_with_constructor(upstream_buckets),
            time_to_first_token: Family::new_with_constructor(upstream_buckets),
            tokens: Family::default(),
            settlements: Family::default(),
            denials: Family::default(),
            admission_phases: Family::new_with_constructor(phase_buckets),
            settlement_phases: Family::new_with_constructor(phase_buckets),
            reservations: Family::default(),
            alert_runs: Family::default(),
            alert_rule_failures: Counter::default(),
            alert_delivery_attempts: Family::default(),
            alert_delivery_queue: Family::default(),
            alert_webhook_attempts: Family::default(),
            alert_webhook_queue: Family::default(),
            alert_webhook_oldest: Family::default(),
            alert_pending_buckets: Family::default(),
            alert_oldest_pending_age: Family::default(),
            deadlock_retries: Family::default(),
            scope_lock_wait: Family::new_with_constructor(phase_buckets),
            scope_lock_waiting: Family::default(),
            pool_connections: Family::default(),
            pool_max: Gauge::default(),
            collection_errors: Family::default(),
            file_store_ops: Family::default(),
            file_store_bytes: Family::default(),
            batches: Family::default(),
            batch_lines: Family::default(),
            batch_queue: Family::default(),
            batch_workers: Family::default(),
            batch_route_lines: Family::default(),
            batch_paused_routes: Family::default(),
            batch_route_pauses: Family::default(),
            batch_route_providers: Mutex::default(),
            cache_lookups: Family::default(),
            config_changes: Family::default(),
            config_poll_failures: Counter::default(),
            auth_throttled: Family::default(),
            key_issuance_refused: Family::default(),
            egress_denied: Family::default(),
            audit_export_deliveries: Family::default(),
            audit_export_rejected_records: Counter::default(),
            trace_dropped_spans,
            trace_deliveries,
            trace_rejected_spans: Counter::default(),
            otlp_metrics_pushes,
            otlp_metrics_dropped,
            otlp_metrics_rejected_points: Counter::default(),
            audit_export_queued: Gauge::default(),
            audit_export_oldest: Default::default(),
            config_listener: Gauge::default(),
            leases_held: Family::default(),
            lease_terms: Family::default(),
            background_runs: Family::default(),
            report_outcomes: Family::default(),
            report_delivery_attempts: Family::default(),
            report_delivery_queue: Family::default(),
            report_backlog: Family::default(),
            report_backlog_capped: Family::default(),
            report_oldest_due_lag: Family::default(),
            partition_months: Family::default(),
            history_findings: Gauge::default(),
            rollup_hours: Family::default(),
            providers: Mutex::default(),
            models: Mutex::default(),
            reservations_refreshed: Mutex::default(),
            registry: Registry::default(),
        };
        // A labelled gauge rather than an OpenMetrics `info` type, so the
        // exposition also parses as classic Prometheus text.
        let build = Family::<L1, Gauge>::default();
        build
            .get_or_create(&[("version", env!("CARGO_PKG_VERSION").to_owned())])
            .set(1);
        registry.register("build_info", "Build information (always 1)", build);
        registry.register(
            "http_requests",
            "HTTP responses by method, route template and status",
            metrics.http_requests.clone(),
        );
        registry.register(
            "http_request_duration_seconds",
            "Time until response headers (streams: time to first byte) by method and route template",
            metrics.http_duration.clone(),
        );
        registry.register(
            "inference_attempts",
            "Finished upstream attempts by provider kind, public model and outcome",
            metrics.attempts.clone(),
        );
        registry.register(
            "inference_attempt_errors",
            "Failed upstream attempts by provider kind and safe error code",
            metrics.attempt_errors.clone(),
        );
        registry.register(
            "upstream_duration_seconds",
            "Upstream dispatch to end of response, by provider kind and public model",
            metrics.upstream_latency.clone(),
        );
        registry.register(
            "upstream_time_to_first_token_seconds",
            "Streams: upstream dispatch to first content delta",
            metrics.time_to_first_token.clone(),
        );
        registry.register(
            "inference_tokens",
            "Provider-reported tokens by provider kind, public model and direction (unreported usage is not counted)",
            metrics.tokens.clone(),
        );
        registry.register(
            "settlements",
            "Attempt settlements: settled (exact), unknown (hold retained), held (finalization failed; pending reconciliation)",
            metrics.settlements.clone(),
        );
        registry.register(
            "admission_denials",
            "Admission denials by error code and limit scope",
            metrics.denials.clone(),
        );
        registry.register(
            "admission_seconds",
            "Durable admission transaction time by phase (queue, connect, locks, read, limits, write, commit, total) and outcome (admitted, denied, rejected, error)",
            metrics.admission_phases.clone(),
        );
        registry.register(
            "settlement_seconds",
            "Terminal settlement transaction time by phase (queue, connect, locks, read, write, commit, total) and outcome (settled, unknown, replay, conflict, error)",
            metrics.settlement_phases.clone(),
        );
        registry.register(
            "reservations_held",
            "Reservations still holding budget: pending (in flight/lease) and unknown (unresolved cost)",
            metrics.reservations.clone(),
        );
        registry.register(
            "alert_evaluations",
            "Alert evaluator runs by result (ok, skipped, failed)",
            metrics.alert_runs.clone(),
        );
        registry.register(
            "alert_rule_failures",
            "Individual alert rules that failed during an evaluation",
            metrics.alert_rule_failures.clone(),
        );
        registry.register(
            "alert_webhook_attempts",
            "Webhook attempt outcomes (fixed labels only)",
            metrics.alert_webhook_attempts.clone(),
        );
        registry.register(
            "alert_webhook_deliveries",
            "Active webhook queue and terminal outcomes completed in the last 24 hours",
            metrics.alert_webhook_queue.clone(),
        );
        registry.register(
            "alert_webhook_oldest_waiting_seconds",
            "Age of oldest generation-matching waiting webhook",
            metrics.alert_webhook_oldest.clone(),
        );
        registry.register(
            "alert_email_attempts",
            "Recipient attempt outcomes (no recipient labels)",
            metrics.alert_delivery_attempts.clone(),
        );
        registry.register(
            "alert_email_deliveries",
            "Pending deliveries created or terminal failures/suppressions completed in the last 24 hours",
            metrics.alert_delivery_queue.clone(),
        );
        registry.register(
            "lock_deadlock_retries",
            "Governance transactions re-run after the database aborted them as a deadlock victim (SQLSTATE 40P01), by path",
            metrics.deadlock_retries.clone(),
        );
        registry.register(
            "scope_lock_wait_seconds",
            "Time to acquire scoped admission locks, by lock (authority: workspace type/workspace/user/key advisory locks; rows: the workspace and key totals and counter rows that serialize one scope) and path (admission, settlement). No scope identifiers; a single hot workspace or key shows as a rising rows/admission tail",
            metrics.scope_lock_wait.clone(),
        );
        registry.register(
            "scope_lock_waiting",
            "Governance transactions of this replica waiting for scoped locks right now, by path (admission, settlement)",
            metrics.scope_lock_waiting.clone(),
        );
        registry.register(
            "db_pool_connections",
            "Database pool connections by state (idle, in_use)",
            metrics.pool_connections.clone(),
        );
        registry.register(
            "db_pool_max_connections",
            "Configured database pool maximum",
            metrics.pool_max.clone(),
        );
        registry.register(
            "metrics_collection_errors",
            "Scrape-time collectors that failed (values keep their last reading)",
            metrics.collection_errors.clone(),
        );
        registry.register(
            "file_store_operations",
            "File store operations by backend (local, s3), operation and outcome (ok or a safe error code)",
            metrics.file_store_ops.clone(),
        );
        registry.register(
            "file_store_bytes",
            "Plaintext bytes written (put) and read to the end (get) by backend",
            metrics.file_store_bytes.clone(),
        );
        registry.register(
            "batches",
            "Batches by mode (native, gateway) and event (created, submitted, or the final state)",
            metrics.batches.clone(),
        );
        registry.register(
            "batch_lines",
            "Batch lines processed by mode, provider kind and outcome (completed, failed)",
            metrics.batch_lines.clone(),
        );
        registry.register(
            "batch_queue_depth",
            "Unfinished batches by mode",
            metrics.batch_queue.clone(),
        );
        registry.register(
            "batch_workers",
            "Gateway-run batch line workers of this process: capacity and busy",
            metrics.batch_workers.clone(),
        );
        registry.register(
            "batch_route_lines",
            "Gateway-run batch lines by route provider kind and state (waiting for capacity, running); installation-wide",
            metrics.batch_route_lines.clone(),
        );
        registry.register(
            "batch_paused_routes",
            "Routes with batch lines waiting, by pause reason; installation-wide",
            metrics.batch_paused_routes.clone(),
        );
        registry.register(
            "batch_route_pauses",
            "Times this process saw a route's batch gate close, by reason",
            metrics.batch_route_pauses.clone(),
        );
        registry.register(
            "cache_lookups",
            "Per-replica cache lookups by cache (keys, candidates, routes, health, prices) and result (hit, miss, bypass: versions unconfirmed or caching off)",
            metrics.cache_lookups.clone(),
        );
        registry.register(
            "config_changes",
            "Configuration version changes this replica observed, by topic (access, catalog, keys, policy, settings)",
            metrics.config_changes.clone(),
        );
        registry.register(
            "audit_export_deliveries",
            "Audit export requests by outcome (delivered, failed); skipped counts events no longer exportable",
            metrics.audit_export_deliveries.clone(),
        );
        registry.register(
            "audit_export_rejected_records",
            "Audit events an OTLP collector refused for good (not retried; receivers see the seq gap)",
            metrics.audit_export_rejected_records.clone(),
        );
        registry.register(
            "trace_dropped_spans",
            "Trace spans dropped locally, by fixed reason",
            trace_drops,
        );
        registry.register(
            "trace_deliveries",
            "Trace export exchanges, by fixed outcome (no retries)",
            trace_exchanges,
        );
        registry.register(
            "trace_rejected_spans",
            "Trace spans rejected by the collector partialSuccess acknowledgement",
            metrics.trace_rejected_spans.clone(),
        );
        registry.register(
            "otlp_metrics_pushes",
            "Metrics export exchanges, by fixed outcome (no retries)",
            metric_pushes,
        );
        registry.register(
            "otlp_metrics_dropped",
            "Metrics export points dropped locally, by fixed reason",
            metric_drops,
        );
        registry.register(
            "otlp_metrics_rejected_points",
            "Metrics points rejected by the collector partialSuccess acknowledgement",
            metrics.otlp_metrics_rejected_points.clone(),
        );
        registry.register(
            "audit_export_queued",
            "Audit events queued for export, not yet accepted by the receiver",
            metrics.audit_export_queued.clone(),
        );
        registry.register(
            "audit_export_oldest_seconds",
            "Age of the oldest audit event queued for export (0 when none)",
            metrics.audit_export_oldest.clone(),
        );
        registry.register(
            "egress_denied",
            "Outbound connections refused by the egress policy, by purpose (provider, oidc, storage, catalog, smtp, audit)",
            metrics.egress_denied.clone(),
        );
        registry.register(
            "auth_throttled",
            "Sign-in steps refused by abuse controls, by step (login, callback) and reason (client_rate, pending_cap)",
            metrics.auth_throttled.clone(),
        );
        registry.register(
            "key_issuance_refused",
            "API key creations and rotations refused by the issuance budget (#253), by scope (holder, workspace) and operation (create, rotate). No workspace or user identifiers",
            metrics.key_issuance_refused.clone(),
        );
        registry.register(
            "config_poll_failures",
            "Failed configuration version polls (caches are bypassed after 3 s without a successful poll)",
            metrics.config_poll_failures.clone(),
        );
        registry.register(
            "config_listener_up",
            "Whether this replica's configuration LISTEN connection is up (1) or not (0)",
            metrics.config_listener.clone(),
        );
        registry.register(
            "work_leases_held",
            "Background work leases this replica holds (1) or not (0), by lease",
            metrics.leases_held.clone(),
        );
        registry.register(
            "work_lease_terms",
            "Lease terms this replica started (acquisitions and takeovers), by lease",
            metrics.lease_terms.clone(),
        );
        registry.register(
            "background_runs",
            "Singleton background job runs on this replica by job and result (ok, failed, fenced: the lease term ended)",
            metrics.background_runs.clone(),
        );
        for category in crate::scheduled_reports::Outcome::ALL {
            metrics
                .report_outcomes
                .get_or_create(&[("category", category.as_str().to_owned())])
                .inc_by(0);
        }
        registry.register(
            "report_generation_outcomes",
            "Scheduled report generation outcomes, fixed categories only; no scope identifiers",
            metrics.report_outcomes.clone(),
        );
        registry.register(
            "report_email_attempts",
            "Report recipient send outcomes; fixed categories only",
            metrics.report_delivery_attempts.clone(),
        );
        registry.register("report_email_recipients", "Recipient states in the latest 1024 report deliveries created within 24 hours; ineligible means authorization suppression", metrics.report_delivery_queue.clone());
        registry.register(
            "report_due_backlog",
            "Active due schedules, saturated at 1024; includes uninitialized calendars",
            metrics.report_backlog.clone(),
        );
        registry.register(
            "report_due_backlog_capped",
            "Whether due backlog exceeds the 1024-schedule bounded sample",
            metrics.report_backlog_capped.clone(),
        );
        registry.register(
            "report_oldest_due_lag_seconds",
            "Oldest initialized active due schedule lag at the scheduler's last bounded check",
            metrics.report_oldest_due_lag.clone(),
        );
        registry.register(
            "history_partition_months_ahead",
            "Future UTC months covered by partitions, by history table (the partitions job's last check; alert below 2)",
            metrics.partition_months.clone(),
        );
        registry.register(
            "history_verify_findings",
            "Orphaned or scope-mismatched history references found by the history_verify job's last run on this replica (0034; alert on any)",
            metrics.history_findings.clone(),
        );
        registry.register(
            "usage_rollup_hours",
            "Hours the rollups job (re)computed on this replica, by reason (new, changed)",
            metrics.rollup_hours.clone(),
        );
        // Empty-label families can be cleared on replicas that do not hold
        // the metrics lease, rather than exporting stale data.
        registry.register(
            "alert_delivery_pending_buckets",
            "Distinct pending delivery buckets, including overdue work awaiting cleanup",
            metrics.alert_pending_buckets.clone(),
        );
        registry.register(
            "alert_delivery_oldest_pending_age_seconds",
            "Age in seconds of the oldest pending delivery, or zero for an empty queue",
            metrics.alert_oldest_pending_age.clone(),
        );
        Self {
            registry,
            ..metrics
        }
    }

    pub(crate) fn set_partition_months(&self, table: &'static str, months: i64) {
        self.partition_months
            .get_or_create(&[("table", table.to_owned())])
            .set(months);
    }

    pub(crate) fn set_history_findings(&self, findings: i64) {
        self.history_findings.set(findings);
    }

    pub(crate) fn observe_rollup_hours(&self, reason: &'static str, n: u64) {
        if n > 0 {
            self.rollup_hours
                .get_or_create(&[("reason", reason.to_owned())])
                .inc_by(n);
        }
    }

    /// One cache lookup (`cache` and `result` from fixed sets).
    pub(crate) fn observe_cache(&self, cache: &'static str, result: &'static str) {
        self.observe_cache_n(cache, result, 1);
    }

    pub(crate) fn observe_cache_n(&self, cache: &'static str, result: &'static str, n: u64) {
        if n > 0 {
            self.cache_lookups
                .get_or_create(&[("cache", cache.to_owned()), ("result", result.to_owned())])
                .inc_by(n);
        }
    }

    /// Lookups so far of one cache and result (tests, load reports).
    pub fn cache_lookups(&self, cache: &str, result: &str) -> u64 {
        self.cache_lookups
            .get_or_create(&[("cache", cache.to_owned()), ("result", result.to_owned())])
            .get()
    }

    pub(crate) fn observe_config_change(&self, topic: &'static str) {
        self.config_changes
            .get_or_create(&[("topic", topic.to_owned())])
            .inc();
    }

    /// Fixed outcomes shared with the trace transport; never a destination or error string.
    pub fn otlp_metrics_push(&self, outcome: crate::metrics_export::Delivery) {
        self.otlp_metrics_pushes[outcome as usize].inc();
    }

    pub fn otlp_metrics_drop(&self, reason: crate::metrics_export::DropReason, n: u64) {
        self.otlp_metrics_dropped[reason as usize].inc_by(n);
    }

    pub fn otlp_metrics_rejected(&self, n: u64) {
        self.otlp_metrics_rejected_points.inc_by(n);
    }

    pub(crate) fn trace_drop(&self, reason: crate::telemetry::DropReason, n: u64) {
        self.trace_dropped_spans[reason as usize].inc_by(n);
    }

    pub(crate) fn trace_delivery(&self, outcome: crate::telemetry::Delivery) {
        self.trace_deliveries[outcome as usize].inc();
    }

    pub(crate) fn trace_rejected(&self, n: u64) {
        self.trace_rejected_spans.inc_by(n);
    }

    pub(crate) fn audit_export_delivery(&self, outcome: &'static str) {
        self.audit_export_deliveries
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    pub(crate) fn audit_export_skipped(&self, events: u64) {
        self.audit_export_deliveries
            .get_or_create(&[("outcome", "skipped".to_owned())])
            .inc_by(events);
    }

    /// Records an OTLP collector refused permanently (#108): delivered, not
    /// retried, so they count here instead.
    pub(crate) fn audit_export_rejected(&self, records: u64) {
        self.audit_export_rejected_records.inc_by(records);
    }

    /// Refused records counted so far (tests).
    #[cfg(test)]
    pub(crate) fn audit_export_rejected_count(&self) -> u64 {
        self.audit_export_rejected_records.get()
    }

    pub(crate) fn audit_export_backlog(&self, queued: i64, oldest_seconds: f64) {
        self.audit_export_queued.set(queued);
        self.audit_export_oldest.set(oldest_seconds);
    }

    pub(crate) fn egress_denied(&self, purpose: &'static str) {
        self.egress_denied
            .get_or_create(&[("purpose", purpose.to_owned())])
            .inc();
    }

    /// Refusals so far for one purpose (tests).
    pub fn egress_denied_count(&self, purpose: &str) -> u64 {
        self.egress_denied
            .get_or_create(&[("purpose", purpose.to_owned())])
            .get()
    }

    pub(crate) fn auth_throttled(&self, endpoint: &'static str, reason: &'static str) {
        self.auth_throttled
            .get_or_create(&[
                ("endpoint", endpoint.to_owned()),
                ("reason", reason.to_owned()),
            ])
            .inc();
    }

    /// Refusals so far of one sign-in step and reason (tests).
    pub fn auth_throttled_count(&self, endpoint: &str, reason: &str) -> u64 {
        self.auth_throttled
            .get_or_create(&[
                ("endpoint", endpoint.to_owned()),
                ("reason", reason.to_owned()),
            ])
            .get()
    }

    pub(crate) fn key_issuance_refused(&self, scope: &'static str, operation: &'static str) {
        self.key_issuance_refused
            .get_or_create(&[
                ("scope", scope.to_owned()),
                ("operation", operation.to_owned()),
            ])
            .inc();
    }

    /// Issuance refusals so far for one scope and operation (tests).
    pub fn key_issuance_refused_count(&self, scope: &str, operation: &str) -> u64 {
        self.key_issuance_refused
            .get_or_create(&[
                ("scope", scope.to_owned()),
                ("operation", operation.to_owned()),
            ])
            .get()
    }

    pub(crate) fn observe_config_poll_failure(&self) {
        self.config_poll_failures.inc();
    }

    pub(crate) fn set_config_listener(&self, up: bool) {
        self.config_listener.set(i64::from(up));
    }

    pub(crate) fn set_lease_held(&self, lease: &'static str, held: bool) {
        self.leases_held
            .get_or_create(&[("lease", lease.to_owned())])
            .set(i64::from(held));
    }

    pub(crate) fn observe_lease_term(&self, lease: &'static str) {
        self.lease_terms
            .get_or_create(&[("lease", lease.to_owned())])
            .inc();
    }

    pub(crate) fn observe_report_delivery(&self, outcome: &'static str, n: u64) {
        self.report_delivery_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc_by(n);
    }
    pub(crate) fn observe_report_outcome(&self, category: crate::scheduled_reports::Outcome) {
        self.report_outcomes
            .get_or_create(&[("category", category.as_str().to_owned())])
            .inc();
    }
    pub(crate) fn set_report_queue(&self, backlog: i64, capped: bool, lag: i64) {
        self.report_backlog.get_or_create(&[]).set(backlog);
        self.report_backlog_capped
            .get_or_create(&[])
            .set(i64::from(capped));
        self.report_oldest_due_lag.get_or_create(&[]).set(lag);
    }
    pub(crate) fn clear_report_queue(&self) {
        self.report_backlog.clear();
        self.report_backlog_capped.clear();
        self.report_oldest_due_lag.clear();
    }
    pub(crate) fn observe_background_run(&self, job: &'static str, result: &'static str) {
        self.background_runs
            .get_or_create(&[("job", job.to_owned()), ("result", result.to_owned())])
            .inc();
    }

    pub fn observe_http(
        &self,
        method: &axum::http::Method,
        route: &str,
        status: u16,
        elapsed: Duration,
    ) {
        let method = method_label(method).to_owned();
        self.http_requests
            .get_or_create(&[
                ("method", method.clone()),
                ("route", route.to_owned()),
                ("status", status.to_string()),
            ])
            .inc();
        self.http_duration
            .get_or_create(&[("method", method), ("route", route.to_owned())])
            .observe(elapsed.as_secs_f64());
    }

    pub fn labels(&self, provider: &str, model: &str) -> AttemptLabels {
        AttemptLabels {
            provider: bounded(&self.providers, provider),
            model: bounded(&self.models, model),
        }
    }

    pub fn observe_attempt(&self, a: AttemptObservation<'_>) {
        let (provider, model) = (a.labels.provider.clone(), a.labels.model.clone());
        self.attempts
            .get_or_create(&[
                ("provider", provider.clone()),
                ("model", model.clone()),
                ("outcome", a.outcome.to_owned()),
            ])
            .inc();
        if let Some(error) = a.error {
            self.attempt_errors
                .get_or_create(&[
                    ("provider", provider.clone()),
                    ("code", error.code().to_owned()),
                ])
                .inc();
        }
        let pm = [("provider", provider.clone()), ("model", model.clone())];
        if let Some(ms) = a.generation_ms {
            self.upstream_latency
                .get_or_create(&pm)
                .observe(ms as f64 / 1000.0);
        }
        if let Some(ms) = a.time_to_first_token_ms {
            self.time_to_first_token
                .get_or_create(&pm)
                .observe(ms as f64 / 1000.0);
        }
        for (direction, n) in [("input", a.input_tokens), ("output", a.output_tokens)] {
            if let Some(n) = n.filter(|n| *n > 0) {
                self.tokens
                    .get_or_create(&[
                        ("provider", provider.clone()),
                        ("model", model.clone()),
                        ("direction", direction.to_owned()),
                    ])
                    .inc_by(n as u64);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn settlement_count(&self, settlement: Settlement) -> u64 {
        self.settlements
            .get_or_create(&[("outcome", settlement.as_str().to_owned())])
            .get()
    }

    pub fn observe_settlement(&self, settlement: Settlement) {
        self.settlements
            .get_or_create(&[("outcome", settlement.as_str().to_owned())])
            .inc();
    }

    /// Phase timings of one admission transaction (`gateway_admission_seconds`).
    pub(crate) fn observe_admission_phases(&self, timer: PhaseTimer, outcome: &'static str) {
        timer.observe(&self.admission_phases, outcome);
    }

    /// A governance transaction re-run after a deadlock (`path`: admission,
    /// settlement or reconciliation).
    pub(crate) fn observe_deadlock_retry(&self, path: &'static str) {
        self.deadlock_retries
            .get_or_create(&[("path", path.to_owned())])
            .inc();
    }

    /// A scoped lock acquisition is starting on `path`; the returned guard
    /// keeps `gateway_scope_lock_waiting{path}` raised until it is dropped
    /// (also on error or cancellation) and then observes the wait.
    pub(crate) fn scope_lock_wait(
        &'static self,
        lock: &'static str,
        path: &'static str,
    ) -> ScopeLockWait {
        self.scope_lock_waiting
            .get_or_create(&[("path", path.to_owned())])
            .inc();
        ScopeLockWait {
            metrics: self,
            lock,
            path,
            started: Instant::now(),
        }
    }

    /// Scrape-time or background collector failure (`collector` from a fixed set).
    pub(crate) fn observe_collection_error(&self, collector: &'static str) {
        self.collection_error(collector);
    }

    /// Phase timings of one settlement transaction (`gateway_settlement_seconds`).
    pub(crate) fn observe_settlement_phases(&self, timer: PhaseTimer, outcome: &'static str) {
        timer.observe(&self.settlement_phases, outcome);
    }

    /// Observations so far of one admission phase and outcome (tests).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn admission_phase_count(&self, phase: &str, outcome: &str) -> u64 {
        self.histogram_count("admission_seconds", phase, outcome)
    }

    /// Observations so far of one settlement phase and outcome (tests).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn settlement_phase_count(&self, phase: &str, outcome: &str) -> u64 {
        self.histogram_count("settlement_seconds", phase, outcome)
    }

    /// Reads `_count` from the exposition (the histogram's own accessor is
    /// test-only upstream); absent series read 0.
    #[cfg(any(test, feature = "integration-tests"))]
    fn histogram_count(&self, metric: &str, phase: &str, outcome: &str) -> u64 {
        let mut out = String::new();
        if encode(&mut out, &self.registry).is_err() {
            return 0;
        }
        let prefix = format!("gateway_{metric}_count{{phase=\"{phase}\",outcome=\"{outcome}\"}} ");
        out.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    /// Admission denials only: rate/concurrency, budget, unresolved usage and
    /// oversize token reservations. Other admission errors are not denials.
    pub fn observe_admission_error(&self, error: InferenceError) {
        let Some(scope) = denial_scope(error) else {
            return;
        };
        self.denials
            .get_or_create(&[
                ("code", error.code().to_owned()),
                ("scope", scope.to_owned()),
            ])
            .inc();
    }

    /// Gateway-local concurrency limit (`GATEWAY_MAX_CONCURRENT_REQUESTS`).
    pub fn observe_capacity_denial(&self) {
        self.denials
            .get_or_create(&[
                ("code", InferenceError::Busy.code().to_owned()),
                ("scope", "gateway_capacity".to_owned()),
            ])
            .inc();
    }

    pub(crate) fn observe_alert_webhook(&self, outcome: &'static str) {
        self.alert_webhook_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    pub fn observe_alert_delivery(&self, outcome: &'static str) {
        self.alert_delivery_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    pub fn observe_alert_run(&self, result: &'static str, failed_rules: usize) {
        self.alert_runs
            .get_or_create(&[("result", result.to_owned())])
            .inc();
        self.alert_rule_failures.inc_by(failed_rules as u64);
    }

    /// File store operation outcome. All labels come from fixed sets: backend
    /// kind, operation name and `FileStoreError::code()`.
    pub fn observe_file_store(
        &self,
        backend: &'static str,
        op: &'static str,
        error: Option<&'static str>,
    ) {
        self.file_store_ops
            .get_or_create(&[
                ("backend", backend.to_owned()),
                ("op", op.to_owned()),
                ("outcome", error.unwrap_or("ok").to_owned()),
            ])
            .inc();
    }

    pub fn observe_file_store_bytes(&self, backend: &'static str, op: &'static str, bytes: u64) {
        self.file_store_bytes
            .get_or_create(&[("backend", backend.to_owned()), ("op", op.to_owned())])
            .inc_by(bytes);
    }

    pub fn observe_batch_created(&self, mode: &'static str) {
        self.batches
            .get_or_create(&[("mode", mode.to_owned()), ("event", "created".to_owned())])
            .inc();
    }
    pub fn observe_batch_submitted(&self, _provider: &str) {
        self.batches
            .get_or_create(&[
                ("mode", "native".to_owned()),
                ("event", "submitted".to_owned()),
            ])
            .inc();
    }
    /// `state` is a terminal job state (fixed set).
    pub fn observe_batch_finished(&self, mode: &'static str, state: &'static str) {
        self.batches
            .get_or_create(&[("mode", mode.to_owned()), ("event", state.to_owned())])
            .inc();
    }
    pub fn observe_batch_lines(
        &self,
        mode: &'static str,
        provider: &str,
        completed: u64,
        failed: u64,
    ) {
        let provider = bounded(&self.providers, provider);
        for (outcome, n) in [("completed", completed), ("failed", failed)] {
            if n > 0 {
                self.batch_lines
                    .get_or_create(&[
                        ("mode", mode.to_owned()),
                        ("provider", provider.clone()),
                        ("outcome", outcome.to_owned()),
                    ])
                    .inc_by(n);
            }
        }
    }
    pub fn set_batch_queue(&self, mode: &'static str, unfinished: u64) {
        self.batch_queue
            .get_or_create(&[("mode", mode.to_owned())])
            .set(unfinished.min(i64::MAX as u64) as i64);
    }
    /// Batch lines waiting and running per provider kind (`provider`,
    /// waiting, running); kinds no longer present read 0.
    pub fn set_batch_route_lines(&self, rows: &[(String, u64, u64)]) {
        let mut seen = self
            .batch_route_providers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for provider in seen.iter() {
            for state in ["waiting", "running"] {
                self.batch_route_lines
                    .get_or_create(&[("provider", provider.clone()), ("state", state.to_owned())])
                    .set(0);
            }
        }
        for (provider, waiting, running) in rows {
            let provider = bounded(&self.providers, provider);
            seen.insert(provider.clone());
            for (state, n) in [("waiting", waiting), ("running", running)] {
                self.batch_route_lines
                    .get_or_create(&[("provider", provider.clone()), ("state", state.to_owned())])
                    .set((*n).min(i64::MAX as u64) as i64);
            }
        }
    }
    /// Routes paused per reason (fixed set); absent reasons read 0.
    pub fn set_batch_paused_routes(&self, rows: &[(&'static str, u64)]) {
        for reason in [
            "outside_window",
            "live_traffic",
            "server_busy",
            "metrics_unavailable",
            "concurrency",
            "fair_share",
            "workers",
            "rate_limited",
        ] {
            let n = rows.iter().find(|r| r.0 == reason).map_or(0, |r| r.1);
            self.batch_paused_routes
                .get_or_create(&[("reason", reason.to_owned())])
                .set(n.min(i64::MAX as u64) as i64);
        }
    }
    /// A route's batch gate closed (`reason` from a fixed set).
    pub fn observe_batch_route_pause(&self, reason: &'static str) {
        self.batch_route_pauses
            .get_or_create(&[("reason", reason.to_owned())])
            .inc();
    }
    pub fn set_batch_workers(&self, capacity: u64, busy: u64) {
        self.batch_workers
            .get_or_create(&[("state", "capacity".to_owned())])
            .set(capacity as i64);
        self.batch_workers
            .get_or_create(&[("state", "busy".to_owned())])
            .set(busy as i64);
    }

    fn collection_error(&self, collector: &'static str) {
        self.collection_errors
            .get_or_create(&[("collector", collector.to_owned())])
            .inc();
    }

    async fn refresh(&self, store: &Store) {
        let pool = &store.pool;
        let size = i64::from(pool.size());
        let idle = pool.num_idle() as i64;
        self.pool_connections
            .get_or_create(&[("state", "idle".to_owned())])
            .set(idle);
        self.pool_connections
            .get_or_create(&[("state", "in_use".to_owned())])
            .set((size - idle).max(0));
        self.pool_max
            .set(i64::from(pool.options().get_max_connections()));
        // Installation-wide gauge: with leased background work (serve) only
        // the `metrics` lease holder exports it, so summing over replicas
        // does not multiply it.
        if let Some(leases) = store.leases()
            && leases.held(crate::leases::Lease::Metrics).is_none()
        {
            self.reservations.clear();
            self.alert_delivery_queue.clear();
            self.report_delivery_queue.clear();
            self.alert_webhook_queue.clear();
            self.alert_webhook_oldest.clear();
            self.alert_pending_buckets.clear();
            self.alert_oldest_pending_age.clear();
            *self
                .reservations_refreshed
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        {
            let mut refreshed = self
                .reservations_refreshed
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if refreshed.is_some_and(|at| at.elapsed() < RESERVATION_REFRESH) {
                return;
            }
            *refreshed = Some(Instant::now());
        }
        match tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(ALERT_DELIVERY_COUNTS)
                .fetch_one(pool),
        )
        .await
        {
            Ok(Ok((retrying, exhausted, failed, suppressed, buckets, oldest_age))) => {
                self.alert_pending_buckets.get_or_create(&[]).set(buckets);
                self.alert_oldest_pending_age
                    .get_or_create(&[])
                    .set(oldest_age);
                for (state, count) in [
                    ("retrying", retrying),
                    ("exhausted", exhausted),
                    ("failed", failed),
                    ("suppressed", suppressed),
                ] {
                    self.alert_delivery_queue
                        .get_or_create(&[("state", state.to_owned())])
                        .set(count);
                }
            }
            _ => self.collection_error("alert_email"),
        }
        match tokio::time::timeout(Duration::from_secs(2), sqlx::query_as::<_,(String,i64)>("SELECT q.state,count(*) FROM (SELECT id FROM alert_deliveries WHERE report_run_id IS NOT NULL AND created_at>clock_timestamp()-interval '24 hours' ORDER BY created_at DESC,id LIMIT 1024) d JOIN alert_recipient_outbox q ON q.delivery_id=d.id GROUP BY q.state").fetch_all(pool)).await {
            Ok(Ok(rows)) => {
                for state in ["waiting","leased","sent","failed","ineligible","exhausted","not_configured"] {
                    self.report_delivery_queue.get_or_create(&[("state",state.to_owned())]).set(rows.iter().find(|(s,_)|s==state).map_or(0,|(_,n)|*n));
                }
            }
            _ => self.collection_error("report_email"),
        }
        match tokio::time::timeout(Duration::from_secs(2), sqlx::query_as::<_, (String,i64,i64)>("SELECT s.state,count(q.id)::bigint,coalesce(max(greatest(0,extract(epoch FROM clock_timestamp()-q.created_at))) FILTER(WHERE s.state='waiting'),0)::bigint FROM unnest(ARRAY['waiting','leased','sent','failed','exhausted','ineligible']) s(state) LEFT JOIN (alert_webhook_outbox q JOIN alert_webhook_destinations d ON d.id=q.destination_id) ON q.state=s.state AND (q.state<>'waiting' OR q.destination_generation=d.enable_generation) AND (q.state IN ('waiting','leased') OR q.completed_at>clock_timestamp()-interval '24 hours') GROUP BY s.state").fetch_all(pool)).await {
            Ok(Ok(rows)) => {
                for (state,count,age) in rows {
                    if state=="waiting" { self.alert_webhook_oldest.get_or_create(&[]).set(age); }
                    self.alert_webhook_queue.get_or_create(&[("state",state)]).set(count);
                }
            }
            _ => self.collection_error("alert_webhook"),
        }
        // The workspace lifetime totals rows (0015) count every pending and
        // unknown reservation exactly (each belongs to one workspace): one row
        // per workspace through the 0026 window index, not a scan of history.
        let counted = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (i64, i64)>(crate::governance::totals::RESERVATION_COUNTS)
                .fetch_one(pool),
        )
        .await;
        match counted {
            Ok(Ok((pending, unknown))) => {
                for (state, n) in [("pending", pending), ("unknown", unknown)] {
                    self.reservations
                        .get_or_create(&[("state", state.to_owned())])
                        .set(n);
                }
            }
            _ => self.collection_error("reservations"),
        }
    }

    /// OpenMetrics text exposition, refreshing scrape-time gauges first.
    pub async fn render(&self, store: Option<&Store>) -> String {
        if let Some(store) = store {
            self.refresh(store).await;
        }
        let mut out = String::new();
        if encode(&mut out, &self.registry).is_err() {
            out.clear();
        }
        out
    }
}

/// The gateway-local concurrency permit was unavailable.
pub(crate) fn capacity_denied() -> InferenceError {
    METRICS.observe_capacity_denial();
    InferenceError::Busy
}

pub(crate) fn observe_admission(result: &Result<(), InferenceError>) {
    if let Err(error) = result {
        METRICS.observe_admission_error(*error);
    }
}

/// The metrics-only router: `GET /metrics`; everything else is 404 (no SPA).
pub fn router(store: Store) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let store = store.clone();
                async move {
                    let body = METRICS.render(Some(&store)).await;
                    let mut response = (StatusCode::OK, body).into_response();
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    response
                }
            }),
        )
        .fallback(|| async { not_found() })
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// `GATEWAY_METRICS_ADDR`: unset or empty disables metrics. It must differ
/// from the public listener; a non-loopback bind is allowed (e.g. a private
/// container network) but the endpoint is unauthenticated.
pub fn listen_from(value: Option<&str>, public: SocketAddr) -> anyhow::Result<Option<SocketAddr>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let address: SocketAddr = value
        .parse()
        .map_err(|_| anyhow::anyhow!("GATEWAY_METRICS_ADDR must be an IP address and port"))?;
    anyhow::ensure!(
        address.port() != 0 && address.port() != public.port(),
        "GATEWAY_METRICS_ADDR must use a different, nonzero port than GATEWAY_LISTEN"
    );
    Ok(Some(address))
}

#[cfg(test)]
#[path = "metrics_characterization_tests.rs"]
pub(crate) mod characterization_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, HttpBody, to_bytes};
    use tower::ServiceExt;

    #[test]
    fn scheduled_reports_metrics_are_system_and_identifier_free() {
        let m = Metrics::new();
        m.observe_report_outcome(crate::scheduled_reports::Outcome::Timeout);
        m.set_report_queue(1024, true, 42);
        let mut out = String::new();
        encode(&mut out, &m.registry).unwrap();
        assert!(out.contains("gateway_report_generation_outcomes_total{category=\"timeout\"} 1"));
        assert!(out.contains("gateway_report_due_backlog{} 1024"));
        assert!(out.contains("gateway_report_due_backlog_capped{} 1"));
        assert!(out.contains("gateway_report_oldest_due_lag_seconds{} 42"));
    }
    #[test]
    fn listener_is_optional_and_never_the_public_port() {
        let public: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(listen_from(None, public).unwrap(), None);
        assert_eq!(listen_from(Some(" "), public).unwrap(), None);
        assert_eq!(
            listen_from(Some("127.0.0.1:9464"), public).unwrap(),
            Some("127.0.0.1:9464".parse().unwrap())
        );
        for bad in [
            "127.0.0.1:8080",
            "0.0.0.0:8080",
            "127.0.0.1:0",
            "metrics",
            ":9464",
        ] {
            assert!(listen_from(Some(bad), public).is_err(), "{bad}");
        }
    }

    #[test]
    fn dynamic_labels_are_bounded_and_sanitized() {
        let seen = Mutex::new(HashSet::new());
        for i in 0..MAX_DYNAMIC_VALUES {
            assert_eq!(bounded(&seen, &format!("m{i}")), format!("m{i}"));
        }
        assert_eq!(bounded(&seen, "m0"), "m0");
        assert_eq!(bounded(&seen, "one-too-many"), "other");
        let fresh = Mutex::new(HashSet::new());
        assert_eq!(bounded(&fresh, ""), "other");
        assert_eq!(bounded(&fresh, "bad\nvalue"), "other");
        assert_eq!(bounded(&fresh, &"x".repeat(MAX_LABEL_CHARS + 1)), "other");
    }

    #[test]
    fn only_denials_are_counted_as_denials() {
        let m = Metrics::new();
        m.observe_admission_error(InferenceError::ModelUnavailable);
        m.observe_admission_error(InferenceError::Storage);
        m.observe_admission_error(InferenceError::Busy);
        m.observe_admission_error(InferenceError::BudgetExceeded(
            crate::inference::error::LimitScope::Workspace,
        ));
        m.observe_capacity_denial();
        let mut out = String::new();
        encode(&mut out, &m.registry).unwrap();
        assert!(out.contains(
            r#"gateway_admission_denials_total{code="rate_limit_error",scope="policy"} 1"#
        ));
        assert!(out.contains(
            r#"gateway_admission_denials_total{code="budget_exceeded",scope="workspace"} 1"#
        ));
        assert!(out.contains(
            r#"gateway_admission_denials_total{code="rate_limit_error",scope="gateway_capacity"} 1"#
        ));
        assert!(!out.contains("model_not_found"));
        assert!(!out.contains("accounting_unavailable"));
    }

    #[test]
    fn phase_timer_observes_reached_phases_and_total_with_outcome() {
        let m = Metrics::new();
        let mut timer = PhaseTimer::start();
        timer.phase("queue");
        timer.phase("connect");
        timer.phase("locks");
        // An early return after `locks`: later phases are not observed.
        m.observe_admission_phases(timer, "error");
        let mut timer = PhaseTimer::start();
        for phase in &ADMISSION_PHASES[..7] {
            timer.phase(phase);
        }
        m.observe_admission_phases(timer, "admitted");
        let mut timer = PhaseTimer::start();
        for phase in &SETTLEMENT_PHASES[..6] {
            timer.phase(phase);
        }
        m.observe_settlement_phases(timer, "settled");
        assert_eq!(m.admission_phase_count("locks", "error"), 1);
        assert_eq!(m.admission_phase_count("total", "error"), 1);
        assert_eq!(m.admission_phase_count("read", "error"), 0);
        for phase in ADMISSION_PHASES {
            assert_eq!(m.admission_phase_count(phase, "admitted"), 1, "{phase}");
        }
        for phase in SETTLEMENT_PHASES {
            assert_eq!(m.settlement_phase_count(phase, "settled"), 1, "{phase}");
        }
        let mut out = String::new();
        encode(&mut out, &m.registry).unwrap();
        for expected in [
            "# TYPE gateway_admission_seconds histogram",
            "# TYPE gateway_settlement_seconds histogram",
            r#"gateway_admission_seconds_bucket{le="0.0001",phase="locks",outcome="error"}"#,
            r#"gateway_admission_seconds_count{phase="total",outcome="admitted"} 1"#,
            r#"gateway_settlement_seconds_count{phase="commit",outcome="settled"} 1"#,
        ] {
            assert!(out.contains(expected), "missing {expected} in\n{out}");
        }
    }

    #[test]
    fn scope_lock_waits_are_observed_by_lock_and_path_without_identifiers() {
        let waiting = |path: &str| {
            METRICS
                .scope_lock_waiting
                .get_or_create(&[("path", path.to_owned())])
                .get()
        };
        let before = waiting("settlement");
        {
            let wait = METRICS.scope_lock_wait("rows", "settlement");
            assert_eq!(waiting("settlement"), before + 1);
            assert!(wait.elapsed() < Duration::from_secs(5));
        }
        assert_eq!(waiting("settlement"), before);
        drop(METRICS.scope_lock_wait("authority", "admission"));
        let mut out = String::new();
        encode(&mut out, &METRICS.registry).unwrap();
        for expected in [
            "# TYPE gateway_scope_lock_wait_seconds histogram",
            r#"gateway_scope_lock_wait_seconds_count{lock="rows",path="settlement"}"#,
            r#"gateway_scope_lock_wait_seconds_count{lock="authority",path="admission"}"#,
            r#"gateway_scope_lock_waiting{path="settlement"}"#,
        ] {
            assert!(out.contains(expected), "missing {expected}");
        }
        // Two labels from fixed sets only: no workspace or key ids.
        for line in out.lines().filter(|l| l.starts_with("gateway_scope_lock")) {
            assert!(
                !line.contains("workspace=") && !line.contains("key="),
                "{line}"
            );
        }
    }

    #[test]
    fn admission_outcomes_are_a_fixed_set() {
        use crate::inference::error::LimitScope;
        assert_eq!(admission_outcome(&Ok(())), "admitted");
        assert_eq!(admission_outcome(&Err(InferenceError::Storage)), "error");
        for denial in [
            InferenceError::Busy,
            InferenceError::BudgetExceeded(LimitScope::Workspace),
            InferenceError::UnresolvedUsage(LimitScope::Workspace),
            InferenceError::TokenReservationExceedsLimit(LimitScope::ApiKey),
            InferenceError::JobLimitExceeded(LimitScope::Workspace),
        ] {
            assert_eq!(admission_outcome(&Err(denial)), "denied");
        }
        for other in [
            InferenceError::ModelUnavailable,
            InferenceError::Configuration,
            InferenceError::PriceUnbounded,
        ] {
            assert_eq!(admission_outcome(&Err(other)), "rejected");
        }
    }

    #[tokio::test]
    async fn exposition_has_expected_families_and_no_identifiers() {
        let m = Metrics::new();
        m.observe_http(
            &axum::http::Method::POST,
            "/v1/chat/completions",
            200,
            Duration::from_millis(12),
        );
        let labels = m.labels("openai", "company/smart");
        m.observe_attempt(AttemptObservation {
            labels: &labels,
            outcome: "succeeded",
            error: None,
            input_tokens: Some(7),
            output_tokens: Some(2),
            generation_ms: Some(40),
            time_to_first_token_ms: Some(10),
        });
        m.observe_attempt(AttemptObservation {
            labels: &labels,
            outcome: "failed",
            error: Some(InferenceError::UpstreamUnavailable),
            input_tokens: None,
            output_tokens: None,
            generation_ms: None,
            time_to_first_token_ms: None,
        });
        m.observe_settlement(Settlement::Settled);
        m.observe_settlement(Settlement::Unknown);
        m.observe_alert_run("ok", 2);
        m.observe_alert_delivery("transient_failure");
        m.observe_file_store("s3", "put", None);
        m.observe_file_store("local", "get", Some("integrity"));
        m.observe_file_store_bytes("s3", "put", 42);
        let out = m.render(None).await;
        for expected in [
            r#"gateway_build_info{version=""#,
            r#"gateway_http_requests_total{method="POST",route="/v1/chat/completions",status="200"} 1"#,
            r#"gateway_http_request_duration_seconds_bucket{le="0.02",method="POST",route="/v1/chat/completions"} 1"#,
            r#"gateway_inference_attempts_total{provider="openai",model="company/smart",outcome="succeeded"} 1"#,
            r#"gateway_inference_attempt_errors_total{provider="openai",code="upstream_unavailable"} 1"#,
            r#"gateway_inference_tokens_total{provider="openai",model="company/smart",direction="input"} 7"#,
            r#"gateway_inference_tokens_total{provider="openai",model="company/smart",direction="output"} 2"#,
            r#"gateway_upstream_duration_seconds_count{provider="openai",model="company/smart"} 1"#,
            r#"gateway_upstream_time_to_first_token_seconds_count{provider="openai",model="company/smart"} 1"#,
            r#"gateway_settlements_total{outcome="settled"} 1"#,
            r#"gateway_settlements_total{outcome="unknown"} 1"#,
            r#"gateway_alert_evaluations_total{result="ok"} 1"#,
            "gateway_alert_rule_failures_total 2",
            r#"gateway_alert_email_attempts_total{outcome="transient_failure"} 1"#,
            r#"gateway_file_store_operations_total{backend="s3",op="put",outcome="ok"} 1"#,
            r#"gateway_file_store_operations_total{backend="local",op="get",outcome="integrity"} 1"#,
            r#"gateway_file_store_bytes_total{backend="s3",op="put"} 42"#,
            "# EOF",
        ] {
            assert!(out.contains(expected), "missing {expected} in\n{out}");
        }
    }

    #[cfg(feature = "integration-tests")]
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn fair_dispatch_metrics_include_overdue_pending_and_no_workspace_labels(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let ws = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Metrics','team')")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
        let rule = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO alert_rules(id,scope,workspace_id,kind,name,budget_layers,thresholds) VALUES($1,'workspace',$2,'budget_threshold','Metrics',ARRAY['local'],ARRAY[100])")
            .bind(rule).bind(ws).execute(&pool).await.unwrap();
        sqlx::query("WITH events AS (INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) SELECT gen_random_uuid(),$1,'budget_threshold',i::text,100,'warning',$2,'Metrics' FROM generate_series(1,2) i RETURNING id) INSERT INTO alert_deliveries(id,event_id,transition,created_at) SELECT gen_random_uuid(),id,'fired',clock_timestamp()-interval '1 hour' FROM events")
            .bind(rule).bind(ws).execute(&pool).await.unwrap();
        sqlx::raw_sql("WITH event AS (INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin','user',100,'critical','Metrics') RETURNING id) INSERT INTO alert_deliveries(id,event_id,transition,created_at) SELECT gen_random_uuid(),id,'fired',clock_timestamp()-interval '25 hours' FROM event")
            .execute(&pool).await.unwrap();
        // Owner-only clock travel: admission now deliberately overwrites
        // caller timestamps. Preserve the original overdue-backlog fixture.
        sqlx::raw_sql("ALTER TABLE alert_deliveries DISABLE TRIGGER alert_delivery_generation_immutable; UPDATE alert_deliveries SET created_at=clock_timestamp()-CASE WHEN bucket_id='00000000-0000-0000-0000-000000000000' THEN interval '25 hours' ELSE interval '1 hour' END; ALTER TABLE alert_deliveries ENABLE TRIGGER alert_delivery_generation_immutable").execute(&pool).await.unwrap();
        let m = Metrics::new();
        let scrape = m.render(Some(&store)).await;
        assert!(
            scrape.contains("gateway_alert_delivery_pending_buckets{} 2"),
            "{scrape}"
        );
        let age = scrape
            .lines()
            .find_map(|line| {
                line.strip_prefix("gateway_alert_delivery_oldest_pending_age_seconds{} ")
            })
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!((90000..90010).contains(&age));
        assert!(!scrape.contains(&ws.to_string()));
        let unleased = store
            .clone()
            .with_leases(std::sync::Arc::new(crate::leases::Leases::new()));
        let scrape = m.render(Some(&unleased)).await;
        assert!(
            !scrape
                .lines()
                .any(|line| line.starts_with("gateway_alert_delivery_pending_buckets{"))
        );
        assert!(
            !scrape
                .lines()
                .any(|line| line.starts_with("gateway_alert_delivery_oldest_pending_age_seconds{"))
        );
        crate::alerts::deliver_pending(&store, 25).await;
        let scrape = Metrics::new().render(Some(&store)).await;
        assert!(scrape.contains("gateway_alert_delivery_pending_buckets{} 0"));
        assert!(scrape.contains("gateway_alert_delivery_oldest_pending_age_seconds{} 0"));
    }

    #[cfg(feature = "integration-tests")]
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn expired_outbox_cleanup_remains_visible_past_failure_alert_window(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let event = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES($1,'scim_last_admin','scim_last_admin','user',100,'critical','Test')")
            .bind(event).execute(&pool).await.unwrap();
        let initialized = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition,created_at,initialized) VALUES($1,$2,'fired',clock_timestamp()-interval '25 hours',false),(gen_random_uuid(),$2,'resolved',clock_timestamp()-interval '25 hours',false)")
            .bind(initialized).bind(event).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint) VALUES($1,sha256('test'::bytea))")
            .bind(initialized).execute(&pool).await.unwrap();
        sqlx::query("UPDATE alert_deliveries SET initialized=true WHERE id=$1")
            .bind(initialized)
            .execute(&pool)
            .await
            .unwrap();
        // Owner-only clock travel for the cleanup scenario.
        sqlx::raw_sql("ALTER TABLE alert_deliveries DISABLE TRIGGER alert_delivery_generation_immutable; UPDATE alert_deliveries SET created_at=clock_timestamp()-interval '25 hours'; ALTER TABLE alert_deliveries ENABLE TRIGGER alert_delivery_generation_immutable").execute(&pool).await.unwrap();
        let before = Metrics::new().render(Some(&store)).await;
        assert!(before.contains("gateway_alert_email_deliveries{state=\"retrying\"} 0"));
        crate::alerts::deliver_pending(&store, 25).await;
        let outcomes: Vec<(String, i32, i32, bool)> = sqlx::query_as("SELECT status,exhausted,attempt_count,completed_at IS NOT NULL FROM alert_deliveries ORDER BY initialized")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(
            outcomes,
            vec![("failed".into(), 0, 0, true), ("failed".into(), 1, 0, true)]
        );
        // Cleanup made no network attempt; never inflate its attempt counter.
        {
            let scrape = Metrics::new().render(Some(&store)).await;
            assert!(
                scrape.contains("gateway_alert_email_deliveries{state=\"failed\"} 2"),
                "recent cleanup failures must survive the 15m alert window"
            );
            assert!(scrape.contains("gateway_alert_email_deliveries{state=\"exhausted\"} 1"));
            assert!(
                !scrape.contains("gateway_alert_email_attempts_total{"),
                "cleanup is not an SMTP attempt"
            );
        }
        // Evaluate the same scrape query at later clocks without rewriting
        // immutable terminal rows or sleeping through the alert's 15m window.
        for (age, expected) in [
            ("16 minutes", (0_i64, 1_i64, 2_i64)),
            ("25 hours", (0, 0, 0)),
        ] {
            let later = ALERT_DELIVERY_COUNTS.replace(
                "clock_timestamp()",
                &format!("(clock_timestamp()+interval '{age}')"),
            );
            let counts: (i64, i64, i64, i64, i64, i64) =
                sqlx::query_as(&later).fetch_one(&pool).await.unwrap();
            assert_eq!((counts.0, counts.1, counts.2), expected);
            assert_eq!((counts.3, counts.4, counts.5), (0, 0, 0));
        }
    }

    #[cfg(feature = "integration-tests")]
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn alert_generation_suppressed_metric_is_bounded_to_last_24h(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        sqlx::raw_sql("WITH events AS (INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) SELECT gen_random_uuid(),'scim_last_admin','scim_last_admin',i::text,1,'critical','Metrics' FROM generate_series(1,101) i RETURNING id) INSERT INTO alert_deliveries(id,event_id,transition) SELECT gen_random_uuid(),id,'fired' FROM events").execute(&pool).await.unwrap();
        let scrape = Metrics::new().render(Some(&store)).await;
        assert!(
            scrape.contains("gateway_alert_email_deliveries{state=\"suppressed\"} 1"),
            "{scrape}"
        );
        let later = ALERT_DELIVERY_COUNTS.replace(
            "clock_timestamp()",
            "(clock_timestamp()+interval '25 hours')",
        );
        let counts: (i64, i64, i64, i64, i64, i64) =
            sqlx::query_as(&later).fetch_one(&pool).await.unwrap();
        assert_eq!(counts.3, 0);
    }

    #[tokio::test]
    async fn metrics_router_serves_only_metrics() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let app = router(Store::new(pool));
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], CONTENT_TYPE);
        // The process-wide registry includes every route exercised by parallel
        // tests. A complete route matrix can legitimately exceed 1 MiB; that
        // is not an endpoint limit. Bound the read by this locally generated
        // String body's exact length instead, retaining the full-content checks.
        let length = usize::try_from(response.body().size_hint().exact().unwrap()).unwrap();
        assert!(length > 0);
        let body = to_bytes(response.into_body(), length).await.unwrap();
        assert_eq!(body.len(), length);
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("gateway_db_pool_max_connections"));
        assert!(text.ends_with("# EOF\n"));
        for path in [
            "/",
            "/health/ready",
            "/v1/models",
            "/metrics/x",
            "/index.html",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
}

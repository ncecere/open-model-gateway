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

mod admission;
mod alerts;
mod collection;
mod jobs;
mod operations;
mod registry;
mod requests;

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

// Shared with the retained-history EXPLAIN harness: measure the exact collector SQL.
// Separate branches can use the existing waiting/leased partial indexes and
// 0062's completed_at-leading terminal index. statement_timestamp() is a stable
// range bound; volatile clock_timestamp() would filter retained history instead.
// One statement retains one snapshot/deadline and always emits all six states.
// Only waiting needs the destination join: its live generation must match;
// the foreign key already guarantees destination existence for the other states.
const ALERT_WEBHOOK_COUNTS: &str = r#"
WITH counts AS (
    SELECT 'waiting'::text AS state,count(*)::bigint AS n,
        coalesce(max(greatest(0,extract(epoch FROM statement_timestamp()-q.created_at))),0)::bigint AS age
    FROM alert_webhook_outbox q
    JOIN alert_webhook_destinations d ON d.id=q.destination_id
    WHERE q.state='waiting' AND q.destination_generation=d.enable_generation
    UNION ALL
    SELECT 'leased',count(*)::bigint,0::bigint
    FROM alert_webhook_outbox WHERE state='leased'
    UNION ALL
    SELECT state,count(*)::bigint,0::bigint
    FROM alert_webhook_outbox
    WHERE state IN ('sent','failed','exhausted','ineligible')
        AND completed_at>statement_timestamp()-interval '24 hours'
    GROUP BY state
)
SELECT s.state,coalesce(c.n,0)::bigint,coalesce(c.age,0)::bigint
FROM unnest(ARRAY['waiting','leased','sent','failed','exhausted','ineligible']) s(state)
LEFT JOIN counts c ON c.state=s.state
"#;

#[cfg(all(test, feature = "integration-tests"))]
#[path = "metrics/webhook_tests.rs"]
mod webhook_tests;

pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

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
    accounting_failures: Family<L2, Counter>,
    denials: Family<L2, Counter>,
    admission_phases: Histograms<L2>,
    settlement_phases: Histograms<L2>,
    reservations: Family<L1, Gauge>,
    alert_runs: Family<L1, Counter>,
    alert_rule_failures: Counter,
    aged_hold_alerts: Family<L1, Counter>,
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
    otlp_logs_pushes: [Counter; crate::otlp::Delivery::COUNT],
    otlp_logs_dropped: [Counter; crate::ops_log::DropReason::COUNT],
    otlp_logs_rejected_records: Counter,
    otlp_logs_suppressed: [Counter; crate::ops_log::OpsEvent::COUNT],
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
        m.observe_report_delivery("permanent_failure", 1);
        for state in ["exhausted", "ineligible"] {
            m.report_delivery_queue
                .get_or_create(&[("state", state.into())])
                .set(1);
        }
        let mut out = String::new();
        encode(&mut out, &m.registry).unwrap();
        assert!(out.contains("gateway_report_generation_outcomes_total{category=\"timeout\"} 1"));
        assert!(out.contains("gateway_report_due_backlog{} 1024"));
        assert!(out.contains("gateway_report_due_backlog_capped{} 1"));
        assert!(out.contains("gateway_report_oldest_due_lag_seconds{} 42"));
        assert!(
            out.contains("gateway_report_email_attempts_total{outcome=\"permanent_failure\"} 1")
        );
        for state in ["exhausted", "ineligible"] {
            assert!(out.contains(&format!(
                "gateway_report_email_recipients{{state=\"{state}\"}} 1"
            )));
        }
        let report_families: Vec<_> = include_str!("metrics_families.golden")
            .lines()
            .filter(|line| line.starts_with("gateway_report_"))
            .collect();
        assert_eq!(report_families.len(), 6);
        assert!(report_families.iter().all(|line| line.ends_with(" System")));
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

    /// #389: registered on the existing endpoint; no series until an
    /// accounting operation fails; label values only from the closed enums.
    #[tokio::test]
    async fn accounting_failures_are_exported_only_after_a_failure_with_closed_labels() {
        use crate::db::accounting_failure::{Cause, Stage, tests::exported_series};
        let m = Metrics::new();
        m.observe_settlement(Settlement::Settled);
        assert!(
            !m.render(None).await.contains("gateway_accounting_failures"),
            "no series while accounting succeeds"
        );
        m.observe_accounting_failure(Stage::GovernanceStorage, Cause::LockTimeout);
        m.observe_accounting_failure(Stage::GovernanceStorage, Cause::LockTimeout);
        m.observe_accounting_failure(Stage::ExplicitSettlement, Cause::DeadlineInDatabase);
        let out = m.render(None).await;
        assert!(out.contains("# TYPE gateway_accounting_failures counter"));
        for line in [
            r#"gateway_accounting_failures_total{stage="governance_storage",cause="lock_timeout"} 2"#,
            r#"gateway_accounting_failures_total{stage="explicit_settlement",cause="deadline_in_database"} 1"#,
        ] {
            assert!(out.contains(line), "missing: {line}");
        }
        assert_eq!(exported_series(&out).len(), 2);
        assert_eq!(
            m.accounting_failure_count(Stage::GovernanceStorage, Cause::LockTimeout),
            2
        );
        // Every value the two enums can render, and nothing else: the type bound.
        for stage in Stage::ALL {
            for cause in Cause::ALL {
                m.observe_accounting_failure(stage, cause);
            }
        }
        assert_eq!(exported_series(&m.render(None).await).len(), 11 * 16);
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

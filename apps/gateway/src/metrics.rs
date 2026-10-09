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

type L1 = [(&'static str, String); 1];
type L2 = [(&'static str, String); 2];
type L3 = [(&'static str, String); 3];
type Histograms<L> = Family<L, Histogram, fn() -> Histogram>;

/// Distinct values admitted per dynamic label before folding into `other`.
const MAX_DYNAMIC_VALUES: usize = 200;
const MAX_LABEL_CHARS: usize = 96;
/// Reservation gauges are refreshed at most this often, however often scraped.
const RESERVATION_REFRESH: Duration = Duration::from_secs(15);

pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

fn request_buckets() -> Histogram {
    // 5 ms .. ~82 s
    Histogram::new(exponential_buckets(0.005, 2.0, 15))
}
fn upstream_buckets() -> Histogram {
    // 10 ms .. ~164 s
    Histogram::new(exponential_buckets(0.01, 2.0, 15))
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
    reservations: Family<L1, Gauge>,
    alert_runs: Family<L1, Counter>,
    alert_rule_failures: Counter,
    pool_connections: Family<L1, Gauge>,
    pool_max: Gauge,
    collection_errors: Family<L1, Counter>,
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
            reservations: Family::default(),
            alert_runs: Family::default(),
            alert_rule_failures: Counter::default(),
            pool_connections: Family::default(),
            pool_max: Gauge::default(),
            collection_errors: Family::default(),
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
        Self {
            registry,
            ..metrics
        }
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

    pub fn observe_settlement(&self, settlement: Settlement) {
        self.settlements
            .get_or_create(&[("outcome", settlement.as_str().to_owned())])
            .inc();
    }

    /// Admission denials only: rate/concurrency, budget, unresolved usage and
    /// oversize token reservations. Other admission errors are not denials.
    pub fn observe_admission_error(&self, error: InferenceError) {
        use crate::inference::error::LimitScope;
        let scope = |s: LimitScope| match s {
            LimitScope::Installation => "installation",
            LimitScope::Workspace => "workspace",
            LimitScope::ApiKey => "api_key",
        };
        let scope = match error {
            InferenceError::Busy => "policy",
            InferenceError::BudgetExceeded(s)
            | InferenceError::UnresolvedUsage(s)
            | InferenceError::TokenReservationExceedsLimit(s) => scope(s),
            _ => return,
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

    pub fn observe_alert_run(&self, result: &'static str, failed_rules: usize) {
        self.alert_runs
            .get_or_create(&[("result", result.to_owned())])
            .inc();
        self.alert_rule_failures.inc_by(failed_rules as u64);
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
        let counted = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (String, i64)>(
                "SELECT state,count(*) FROM governance_reservations WHERE state IN ('pending','unknown') GROUP BY state",
            )
            .fetch_all(pool),
        )
        .await;
        match counted {
            Ok(Ok(rows)) => {
                for state in ["pending", "unknown"] {
                    let n = rows.iter().find(|(s, _)| s == state).map_or(0, |(_, n)| *n);
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
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

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
            "# EOF",
        ] {
            assert!(out.contains(expected), "missing {expected} in\n{out}");
        }
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
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
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

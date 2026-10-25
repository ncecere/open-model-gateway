//! Scrape-derived snapshots, OTLP/HTTP JSON encoding and a single bounded worker.
//!
//! Wire mapping: opentelemetry-proto v1.9.0 metrics/v1/metrics.proto and
//! collector/metrics/v1/metrics_service.proto. Int64/uint64 fields are decimal
//! strings, enums are integers, histogram buckets are NON-cumulative counts.
//! The static table is both the privacy boundary and the units/type inventory.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
    time::Duration,
};

use serde_json::{Value, json};

pub use crate::otlp::Delivery;
use crate::{config::Environment, otlp::fixed_enum};

fixed_enum!(DropReason {
    SeriesLimit => "series_limit", BodyLimit => "body_limit", Label => "label",
    Unclassified => "unclassified", InvalidValue => "invalid_value"
});

// Ordering is export priority: System health must not be starved by Traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    System,
    Traffic,
    Excluded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}
impl Kind {
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
    fn field(self) -> &'static str {
        match self {
            Self::Counter => "sum",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}
struct Classification {
    // OpenMetrics family name: counter samples add `_total`, OTLP does not.
    name: &'static str,
    class: Class,
    kind: Kind,
    labels: &'static [&'static str],
    unit: &'static str,
}
macro_rules! classes {
    ($($name:literal => ($class:ident, $kind:ident, $unit:literal, [$($label:literal),*])),+ $(,)?) => {
        const CLASSES: &[Classification] = &[$(Classification {
            name: $name, class: Class::$class, kind: Kind::$kind, unit: $unit, labels: &[$($label),*]
        }),+];
    };
}
// Alphabetical, matching metrics_families.golden. No name-prefix heuristics:
// adding an instrument requires an explicit privacy decision here.
classes! {
    "gateway_admission_denials" => (Traffic, Counter, "1", ["code", "scope"]),
    "gateway_admission_seconds" => (Traffic, Histogram, "s", ["outcome", "phase"]),
    "gateway_aged_hold_alerts" => (System, Counter, "1", ["result"]),
    "gateway_alert_delivery_oldest_pending_age_seconds" => (Traffic, Gauge, "s", []),
    "gateway_alert_delivery_pending_buckets" => (Traffic, Gauge, "1", []),
    "gateway_alert_email_attempts" => (Traffic, Counter, "1", ["outcome"]),
    "gateway_alert_email_deliveries" => (Traffic, Gauge, "1", ["state"]),
    "gateway_alert_evaluations" => (System, Counter, "1", ["result"]),
    "gateway_alert_rule_failures" => (System, Counter, "1", []),
    "gateway_alert_webhook_attempts" => (Traffic, Counter, "1", ["outcome"]),
    "gateway_alert_webhook_deliveries" => (Traffic, Gauge, "1", ["state"]),
    "gateway_alert_webhook_oldest_waiting_seconds" => (Traffic, Gauge, "s", []),
    "gateway_audit_export_deliveries" => (System, Counter, "1", ["outcome"]),
    "gateway_audit_export_oldest_seconds" => (System, Gauge, "s", []),
    "gateway_audit_export_queued" => (System, Gauge, "1", []),
    "gateway_audit_export_rejected_records" => (System, Counter, "1", []),
    "gateway_auth_throttled" => (Traffic, Counter, "1", ["endpoint", "reason"]),
    "gateway_background_runs" => (System, Counter, "1", ["job", "result"]),
    "gateway_batch_lines" => (Traffic, Counter, "1", ["mode", "outcome", "provider"]),
    "gateway_batch_paused_routes" => (Traffic, Gauge, "1", ["reason"]),
    "gateway_batch_queue_depth" => (Traffic, Gauge, "1", ["mode"]),
    "gateway_batch_route_lines" => (Traffic, Gauge, "1", ["provider", "state"]),
    "gateway_batch_route_pauses" => (Traffic, Counter, "1", ["reason"]),
    "gateway_batch_workers" => (Traffic, Gauge, "1", ["state"]),
    "gateway_batches" => (Traffic, Counter, "1", ["event", "mode"]),
    "gateway_build_info" => (Excluded, Gauge, "1", ["version"]),
    "gateway_cache_lookups" => (Traffic, Counter, "1", ["cache", "result"]),
    "gateway_config_changes" => (System, Counter, "1", ["topic"]),
    "gateway_config_listener_up" => (System, Gauge, "1", []),
    "gateway_config_poll_failures" => (System, Counter, "1", []),
    "gateway_db_pool_connections" => (System, Gauge, "1", ["state"]),
    "gateway_db_pool_max_connections" => (System, Gauge, "1", []),
    "gateway_egress_denied" => (System, Counter, "1", ["purpose"]),
    "gateway_file_store_bytes" => (Traffic, Counter, "By", ["backend", "op"]),
    "gateway_file_store_operations" => (Traffic, Counter, "1", ["backend", "op", "outcome"]),
    "gateway_history_partition_months_ahead" => (System, Gauge, "mo", ["table"]),
    "gateway_history_verify_findings" => (System, Gauge, "1", []),
    "gateway_http_request_duration_seconds" => (Traffic, Histogram, "s", ["method", "route"]),
    "gateway_http_requests" => (Traffic, Counter, "1", ["method", "route", "status"]),
    "gateway_inference_attempt_errors" => (Traffic, Counter, "1", ["code", "provider"]),
    "gateway_inference_attempts" => (Traffic, Counter, "1", ["model", "outcome", "provider"]),
    "gateway_inference_tokens" => (Traffic, Counter, "1", ["direction", "model", "provider"]),
    "gateway_key_issuance_refused" => (Traffic, Counter, "1", ["operation", "scope"]),
    "gateway_lock_deadlock_retries" => (System, Counter, "1", ["path"]),
    "gateway_metrics_collection_errors" => (System, Counter, "1", ["collector"]),
    "gateway_otlp_metrics_dropped" => (System, Counter, "1", ["reason"]),
    "gateway_otlp_metrics_pushes" => (System, Counter, "1", ["outcome"]),
    "gateway_otlp_metrics_rejected_points" => (System, Counter, "1", []),
    "gateway_report_due_backlog" => (System, Gauge, "1", []),
    "gateway_report_due_backlog_capped" => (System, Gauge, "1", []),
    "gateway_report_generation_outcomes" => (System, Counter, "1", ["category"]),
    "gateway_report_oldest_due_lag_seconds" => (System, Gauge, "s", []),
    "gateway_reservations_held" => (Traffic, Gauge, "1", ["state"]),
    "gateway_scope_lock_wait_seconds" => (Traffic, Histogram, "s", ["lock", "path"]),
    "gateway_scope_lock_waiting" => (Traffic, Gauge, "1", ["path"]),
    "gateway_settlement_seconds" => (Traffic, Histogram, "s", ["outcome", "phase"]),
    "gateway_settlements" => (Traffic, Counter, "1", ["outcome"]),
    "gateway_trace_deliveries" => (System, Counter, "1", ["outcome"]),
    "gateway_trace_dropped_spans" => (System, Counter, "1", ["reason"]),
    "gateway_trace_rejected_spans" => (System, Counter, "1", []),
    "gateway_upstream_duration_seconds" => (Traffic, Histogram, "s", ["model", "provider"]),
    "gateway_upstream_time_to_first_token_seconds" => (Traffic, Histogram, "s", ["model", "provider"]),
    "gateway_usage_rollup_hours" => (System, Counter, "h", ["reason"]),
    "gateway_work_lease_terms" => (System, Counter, "1", ["lease"]),
    "gateway_work_leases_held" => (System, Gauge, "1", ["lease"]),
}
const PATH_FAMILIES: &[&str] = &[
    "gateway_scope_lock_waiting",
    "gateway_scope_lock_wait_seconds",
    "gateway_lock_deadlock_retries",
];
const MAX_BODY: usize = 1024 * 1024;
const MAX_SERIES: usize = 300;
const MAX_POINTS: usize = 2000;
type Labels = Vec<(String, String)>;

/// Counts refer to logical data points (a histogram is one point), not bytes.
/// Disabled traffic and explicitly excluded families are not drops.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Drops([u64; DropReason::COUNT]);
impl Drops {
    pub fn get(&self, reason: DropReason) -> u64 {
        self.0[reason as usize]
    }
    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }
    fn add(&mut self, reason: DropReason) {
        self.0[reason as usize] += 1;
    }
    /// Record after encoding, so these counters appear in the NEXT snapshot.
    pub fn record(&self, metrics: &crate::metrics::Metrics) {
        for reason in DropReason::ALL {
            metrics.otlp_metrics_drop(*reason, self.get(*reason));
        }
    }
}

fn labels_allowed(class: &Classification, labels: &Labels) -> bool {
    labels.iter().all(|(key, value)| {
        class.labels.contains(&key.as_str())
            && value.chars().count() <= 96
            && !value.chars().any(char::is_control)
            && (key != "path"
                || (PATH_FAMILIES.contains(&class.name)
                    && ["admission", "settlement", "reconciliation"].contains(&value.as_str())))
    })
}

#[derive(Default)]
struct RawPoint {
    scalar: Option<String>,
    count: Option<u64>,
    sum: Option<f64>,
    buckets: Vec<(f64, u64)>,
    invalid: bool,
}
impl RawPoint {
    fn insert(&mut self, suffix: &str, value: &str, bound: Option<&str>) {
        let ok = match suffix {
            "" | "_total" => self.scalar.replace(value.into()).is_none(),
            "_count" => integer_count(value).is_some_and(|n| self.count.replace(n).is_none()),
            "_sum" => value
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .is_some_and(|n| self.sum.replace(n).is_none()),
            "_bucket" => {
                let bound = bound.and_then(|b| {
                    if b == "+Inf" {
                        Some(f64::INFINITY)
                    } else {
                        b.parse::<f64>().ok().filter(|n| n.is_finite())
                    }
                });
                match (bound, integer_count(value)) {
                    (Some(bound), Some(count))
                        if self.buckets.len() < 128
                            && !self.buckets.iter().any(|(b, _)| *b == bound) =>
                    {
                        self.buckets.push((bound, count));
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        self.invalid |= !ok;
    }
    fn finish(mut self, kind: Kind) -> Option<Value> {
        if self.invalid {
            return None;
        }
        match kind {
            Kind::Counter => {
                let n = self.scalar?.parse::<i64>().ok().filter(|n| *n >= 0)?;
                Some(json!({"asInt": n.to_string()}))
            }
            Kind::Gauge => {
                let value = self.scalar?;
                if let Ok(n) = value.parse::<i64>() {
                    Some(json!({"asInt": n.to_string()}))
                } else {
                    value
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .map(|n| json!({"asDouble":n}))
                }
            }
            Kind::Histogram => {
                let count = self.count?;
                let sum = self.sum?;
                self.buckets.sort_by(|(a, _), (b, _)| a.total_cmp(b));
                if self.buckets.last().copied() != Some((f64::INFINITY, count)) {
                    return None;
                }
                let mut previous = 0;
                let mut counts = Vec::new();
                let mut bounds = Vec::new();
                for (bound, cumulative) in self.buckets {
                    counts.push(cumulative.checked_sub(previous)?.to_string());
                    previous = cumulative;
                    if bound.is_finite() {
                        bounds.push(bound);
                    }
                }
                Some(
                    json!({"count":count.to_string(),"sum":sum,"explicitBounds":bounds,"bucketCounts":counts}),
                )
            }
        }
    }
}
fn integer_count(value: &str) -> Option<u64> {
    // Parse the original decimal text, never round through f64.
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

/// One synchronous parse of the very same `METRICS.render(Some(&store))` text
/// used by Prometheus. No DB collection of its own; lease-holder omissions are
/// retained. Metadata cannot override the static type/privacy table.
pub struct Snapshot {
    points: BTreeMap<(Class, usize), Vec<(Labels, Value)>>,
    dropped: Drops,
}
impl Snapshot {
    pub fn parse(text: &str, traffic: bool) -> Self {
        let mut dropped = Drops::default();
        let mut raw = BTreeMap::<(Class, usize), BTreeMap<Labels, RawPoint>>::new();
        let mut invalid_labels = BTreeSet::new();
        for line in text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let name = line.split(['{', ' ', '\t']).next().unwrap_or_default();
            let Some((index, class, suffix)) = identify(name) else {
                dropped.add(DropReason::Unclassified);
                continue;
            };
            // Skip disabled/excluded families before parsing or validating labels:
            // they cannot enter the snapshot and must not count as drops.
            if class.class == Class::Excluded || (class.class == Class::Traffic && !traffic) {
                continue;
            }
            let (mut labels, value) = match sample(&line[name.len()..]) {
                Ok(sample) => sample,
                Err(reason) => {
                    dropped.add(reason);
                    continue;
                }
            };
            let bound = if suffix == "_bucket" {
                labels
                    .iter()
                    .position(|(key, _)| key == "le")
                    .map(|i| labels.remove(i).1)
            } else {
                None
            };
            if !labels_allowed(class, &labels) {
                // The synthetic `le` was removed above: every component of
                // one rejected histogram contributes exactly one series drop.
                if invalid_labels.insert((index, labels)) {
                    dropped.add(DropReason::Label);
                }
                continue;
            }
            labels.sort();
            raw.entry((class.class, index))
                .or_default()
                .entry(labels)
                .or_default()
                .insert(suffix, value, bound.as_deref());
        }
        let mut points = BTreeMap::new();
        let mut total = 0;
        for (key @ (_, index), series) in raw {
            let mut accepted = Vec::new();
            for (labels, point) in series {
                let Some(value) = point.finish(CLASSES[index].kind) else {
                    dropped.add(DropReason::InvalidValue);
                    continue;
                };
                if accepted.len() >= MAX_SERIES || total >= MAX_POINTS {
                    dropped.add(DropReason::SeriesLimit);
                    continue;
                }
                accepted.push((labels, value));
                total += 1;
            }
            if !accepted.is_empty() {
                points.insert(key, accepted);
            }
        }
        Self { points, dropped }
    }
}
fn identify(name: &str) -> Option<(usize, &'static Classification, &'static str)> {
    CLASSES.iter().enumerate().find_map(|(i, class)| {
        let suffix = name.strip_prefix(class.name)?;
        let valid = match class.kind {
            Kind::Counter => suffix == "_total",
            Kind::Gauge => suffix.is_empty(),
            Kind::Histogram => ["_bucket", "_sum", "_count"].contains(&suffix),
        };
        if !valid {
            return None;
        }
        let suffix = match suffix {
            "_total" => "_total",
            "_bucket" => "_bucket",
            "_sum" => "_sum",
            "_count" => "_count",
            _ => "",
        };
        Some((i, class, suffix))
    })
}

/// Strict OpenMetrics label grammar, including escaped quotes, slashes and
/// newlines. Bad syntax is refused rather than guessed; slicing is only at
/// ASCII boundaries. No regex, unchecked indexing or numeric unwraps.
fn sample(mut rest: &str) -> Result<(Labels, &str), DropReason> {
    let mut labels = BTreeMap::new();
    if let Some(inner) = rest.strip_prefix('{') {
        rest = inner;
        if let Some(inner) = rest.strip_prefix('}') {
            rest = inner;
        } else {
            loop {
                let (key, after) = rest.split_once('=').ok_or(DropReason::Label)?;
                if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    return Err(DropReason::Label);
                }
                let after = after.strip_prefix('"').ok_or(DropReason::Label)?;
                let mut value = String::new();
                let mut chars = after.char_indices();
                let end = loop {
                    match chars.next() {
                        Some((i, '"')) => break i + 1,
                        Some((_, '\\')) => value.push(match chars.next() {
                            Some((_, 'n')) => '\n',
                            Some((_, '"')) => '"',
                            Some((_, '\\')) => '\\',
                            _ => return Err(DropReason::Label),
                        }),
                        Some((_, c)) => value.push(c),
                        None => return Err(DropReason::Label),
                    }
                };
                if labels.insert(key.into(), value).is_some() {
                    return Err(DropReason::Label);
                }
                rest = &after[end..];
                if let Some(inner) = rest.strip_prefix('}') {
                    rest = inner;
                    break;
                }
                rest = rest.strip_prefix(',').ok_or(DropReason::Label)?;
            }
        }
    }
    if !rest.starts_with([' ', '\t']) {
        return Err(DropReason::InvalidValue);
    }
    let value = rest.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(DropReason::InvalidValue);
    }
    Ok((labels.into_iter().collect(), value))
}

// One id for the entire process, across encoder instances. Cache failure too:
// export stays quietly disabled, never a predictable fallback or another draw.
static INSTANCE: LazyLock<Result<uuid::Uuid, crate::entropy::EntropyUnavailable>> =
    LazyLock::new(|| {
        Ok(uuid::Builder::from_random_bytes(crate::entropy::bytes_quiet::<16>()?).into_uuid())
    });

pub struct Encoder {
    environment: Environment,
    start: u64,
    instance: uuid::Uuid,
}
pub struct Encoded {
    pub body: Vec<u8>,
    pub dropped: Drops,
    pub points: usize,
}
impl Encoder {
    /// The caller captures process start, never a per-push timestamp. Failure
    /// quietly disables export. Only service.instance.id may carry this UUID.
    pub fn new(
        environment: Environment,
        process_start_unix_nano: u64,
    ) -> Result<Self, crate::entropy::EntropyUnavailable> {
        Ok(Self {
            environment,
            start: process_start_unix_nano,
            instance: (*INSTANCE)?,
        })
    }
    pub fn encode(&self, snapshot: Snapshot, now_unix_nano: u64) -> Encoded {
        self.encode_with_limit(snapshot, now_unix_nano, MAX_BODY)
    }
    fn envelope(&self, metrics: Vec<Value>) -> Value {
        json!({"resourceMetrics":[{
            "resource":{"attributes":[
                {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
                {"key":"service.version","value":{"stringValue":env!("CARGO_PKG_VERSION")}},
                {"key":"deployment.environment.name","value":{"stringValue":self.environment.as_str()}},
                {"key":"service.instance.id","value":{"stringValue":self.instance.to_string()}}
            ]},
            "scopeMetrics":[{"scope":{"name":"omg.metrics","version":env!("CARGO_PKG_VERSION")},"metrics":metrics}]
        }]})
    }
    fn encode_with_limit(&self, snapshot: Snapshot, now: u64, limit: usize) -> Encoded {
        let mut dropped = snapshot.dropped;
        let mut metrics = Vec::new();
        let mut size = json_size(&self.envelope(Vec::new()));
        let mut points = 0;
        let mut full = false;
        for ((_, index), series) in snapshot.points {
            let class = &CLASSES[index];
            let field = class.kind.field();
            let mut metric = json!({"name":class.name,"unit":class.unit,field:{"dataPoints":[]}});
            if class.kind != Kind::Gauge {
                metric[field]["aggregationTemporality"] = json!(2);
            }
            if class.kind == Kind::Counter {
                metric[field]["isMonotonic"] = json!(true);
            }
            let overhead = json_size(&metric) + usize::from(!metrics.is_empty());
            let mut data = Vec::new();
            for (labels, mut point) in series {
                point["attributes"] = json!(
                    labels
                        .into_iter()
                        .map(|(key, value)| json!({"key":key,"value":{"stringValue":value}}))
                        .collect::<Vec<_>>()
                );
                point["startTimeUnixNano"] = json!(self.start.to_string());
                point["timeUnixNano"] = json!(now.to_string());
                let extra = json_size(&point) + if data.is_empty() { overhead } else { 1 };
                // Retain a System-first, sorted prefix. Once full, every remaining point
                // is a body drop, irrespective of a later point's size.
                if full || size + extra > limit {
                    full = true;
                    dropped.add(DropReason::BodyLimit);
                    continue;
                }
                size += extra;
                points += 1;
                data.push(point);
            }
            if !data.is_empty() {
                metric[field]["dataPoints"] = json!(data);
                metrics.push(metric);
            }
        }
        let body = serde_json::to_vec(&self.envelope(metrics))
            .expect("finite, bounded JSON values serialize");
        debug_assert_eq!(body.len(), size);
        Encoded {
            body,
            dropped,
            points,
        }
    }
}
fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("finite JSON values serialize")
        .len()
}

const EXCHANGE: Duration = Duration::from_secs(5);
const DRAIN: Duration = Duration::from_secs(3);

/// Owns one task, with one snapshot/body/exchange and no queue or retry.
/// Dropping it (including a cancelled shutdown) aborts all network work.
pub struct Runtime {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Runtime {
    pub fn start(
        config: crate::config::MetricsExportConfig,
        environment: Environment,
        process_start_unix_nano: u64,
        store: crate::store::Store,
    ) -> Option<Self> {
        // Disabled must not initialize INSTANCE, draw entropy or spawn a task.
        let (url, client) = config.endpoint?;
        let encoder = Encoder::new(environment, process_start_unix_nano).ok()?;
        let exporter = Exporter {
            url,
            client,
            headers: config.headers,
            encoder,
            traffic: config.traffic,
        };
        let (stop, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(worker(exporter, store, config.interval, receiver));
        Some(Self {
            stop: Some(stop),
            task,
        })
    }

    pub async fn shutdown(mut self) {
        // ONE total budget: cancel any periodic collection/exchange, then
        // attempt one fresh final snapshot. Never give each phase another 3s.
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
        self.task.abort();
    }
}

struct Exporter {
    url: reqwest::Url,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    encoder: Encoder,
    traffic: bool,
}
impl Exporter {
    async fn push(&self, store: &crate::store::Store, outage: &mut bool) {
        let encoded = {
            let text = crate::metrics::METRICS.render(Some(store)).await;
            self.encoder.encode(
                Snapshot::parse(&text, self.traffic),
                crate::telemetry::now_unix_nanos(),
            )
        };
        // Render has returned: no metrics lock or DB connection crosses HTTP.
        let result = crate::otlp::post(
            &self.client,
            &self.url,
            &self.headers,
            encoded.body,
            encoded.points,
            "rejectedDataPoints",
            EXCHANGE,
        )
        .await;
        // Self-observations are visible only in the NEXT snapshot, never a
        // recursive export. Outcomes/drops are already precreated at zero.
        encoded.dropped.record(&crate::metrics::METRICS);
        let outcome = match result {
            Ok(rejected) => {
                crate::metrics::METRICS.otlp_metrics_rejected(rejected);
                Delivery::Accepted
            }
            Err(outcome) => outcome,
        };
        crate::metrics::METRICS.otlp_metrics_push(outcome);
        let failed = outcome != Delivery::Accepted;
        if failed != *outage {
            // Fixed enum only: no destination, headers, errors or server text.
            tracing::warn!(outcome = outcome.as_str(), "metrics exporter state changed");
        }
        *outage = failed;
    }
}

async fn worker(
    exporter: Exporter,
    store: crate::store::Store,
    interval: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut outage = false;
    let mut next = tokio::time::Instant::now() + interval;
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = tokio::time::sleep_until(next) => {}
        }
        let started = tokio::time::Instant::now();
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = exporter.push(&store, &mut outage) => {}
        }
        next = started + interval;
        let now = tokio::time::Instant::now();
        if next <= now {
            // Missed ticks are discarded, not immediately replayed in bursts.
            next = now + interval;
        }
    }
    // The owning Runtime bounds collection AND HTTP with its single deadline.
    exporter.push(&store, &mut outage).await;
}

#[cfg(test)]
#[path = "metrics_export_tests.rs"]
mod tests;

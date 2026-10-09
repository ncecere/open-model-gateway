//! Capacity-aware scheduling of gateway-run batch lines (0022; see
//! docs/batches.md#scheduling-on-self-hosted-models). Native provider
//! batches are unaffected.
//!
//! A line starts on its route only when every gate is open:
//!
//! - **Time window** (optional): allowed days and local hours in an IANA
//!   time zone; outside it no new line starts.
//! - **Live traffic** (optional): fewer than N live (non-batch) requests to
//!   the route are in flight, from the gateway's durable executions and
//!   their reservation leases (every process sees the same count).
//! - **Server load** (optional): a vLLM-compatible Prometheus `/metrics` on an
//!   approved local origin, fetched with the approval's pinned client,
//!   bounded and cached for a few seconds. A failed or incomplete reading is
//!   treated as busy (fail closed) and surfaced as `metrics_unavailable`.
//! - **Concurrency and fair share**: claimed under a per-route transaction
//!   lock. The route runs at most `max_concurrency` batch lines (all batches,
//!   all processes) and the next slot goes to the waiting batch whose
//!   workspace, then batch, runs the fewest lines on the route, least
//!   recently served first, so one huge batch cannot starve the others.
//!
//! Claims stay exactly-once: inserting the line's row is the claim. Nothing
//! is retried implicitly; running lines always finish when a gate closes.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::store::JobRow;
use crate::{
    inference::error::InferenceError, providers::local::endpoints::ApprovedEndpoints, store::Store,
};

/// Batch lines at once per route when it has no settings.
pub const DEFAULT_MAX_CONCURRENCY: i32 = 2;
/// Profiles whose servers accept vLLM's `priority` request field.
pub const PRIORITY_PROFILES: [&str; 2] = ["vllm", "openai_compatible"];
/// Demand rows older than this are ignored (their runner stopped).
const WAIT_FRESH_SECONDS: i32 = 10;
/// Upper bound of a metrics response.
const METRICS_MAX_BYTES: usize = 1024 * 1024;

/// Why a route (or a batch on it) is not starting lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pause {
    OutsideWindow,
    LiveTraffic,
    ServerBusy,
    /// The server load signal could not be read (fail closed).
    MetricsUnavailable,
    /// The route runs its maximum of batch lines.
    Concurrency,
    /// Another batch's turn on the route.
    FairShare,
    /// This process's batch workers are busy.
    Workers,
    /// The provider answered 429: this batch backs off.
    RateLimited,
}
impl Pause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutsideWindow => "outside_window",
            Self::LiveTraffic => "live_traffic",
            Self::ServerBusy => "server_busy",
            Self::MetricsUnavailable => "metrics_unavailable",
            Self::Concurrency => "concurrency",
            Self::FairShare => "fair_share",
            Self::Workers => "workers",
            Self::RateLimited => "rate_limited",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::OutsideWindow,
            Self::LiveTraffic,
            Self::ServerBusy,
            Self::MetricsUnavailable,
            Self::Concurrency,
            Self::FairShare,
            Self::Workers,
            Self::RateLimited,
        ]
        .into_iter()
        .find(|p| p.as_str() == s)
    }
    /// A legitimate wait (configured gates or capacity): the batch stall
    /// alert's clock does not run. An unreadable load signal is not one.
    pub fn legitimate(self) -> bool {
        self != Self::MetricsUnavailable
    }
    /// A pause of the whole route (as opposed to one batch's turn).
    pub fn route_wide(self) -> bool {
        matches!(
            self,
            Self::OutsideWindow | Self::LiveTraffic | Self::ServerBusy | Self::MetricsUnavailable
        )
    }
}

// ------------------------------------------------------------- Settings ----

/// Server load thresholds: pause while any reading is above its limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsGate {
    pub url: String,
    #[serde(default)]
    pub max_waiting: Option<i32>,
    #[serde(default)]
    pub max_running: Option<i32>,
    #[serde(default)]
    pub max_kv_cache_percent: Option<i16>,
}

/// Allowed days and local hours (`HH:MM`) in an IANA time zone. `days` are
/// the days a span starts; `end <= start` spans midnight; `end == start` is
/// the whole day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeWindow {
    pub timezone: String,
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

/// Batch scheduling settings of one route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSettings {
    pub max_concurrency: i32,
    #[serde(default)]
    pub yield_live_threshold: Option<i32>,
    #[serde(default)]
    pub metrics: Option<MetricsGate>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub window: Option<TimeWindow>,
}
impl Default for RouteSettings {
    fn default() -> Self {
        Self {
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            yield_live_threshold: None,
            metrics: None,
            priority: None,
            window: None,
        }
    }
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// `HH:MM` → minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !(h.bytes().chain(m.bytes())).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}
fn hhmm(minutes: i16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// A window ready to evaluate.
#[derive(Clone, Debug)]
pub struct CompiledWindow {
    tz: jiff::tz::TimeZone,
    days: u8,
    start: u16,
    end: u16,
}
impl CompiledWindow {
    /// Whether a line may start at `at`.
    pub fn allows(&self, at: DateTime<Utc>) -> bool {
        let Ok(ts) = jiff::Timestamp::from_second(at.timestamp()) else {
            return false;
        };
        let local = ts.to_zoned(self.tz.clone());
        let day = local.weekday().to_monday_zero_offset() as u8;
        let minute = local.hour() as u16 * 60 + local.minute() as u16;
        let on = |d: u8| self.days & (1 << d) != 0;
        let previous = (day + 6) % 7;
        match self.start.cmp(&self.end) {
            std::cmp::Ordering::Equal => on(day),
            std::cmp::Ordering::Less => on(day) && minute >= self.start && minute < self.end,
            std::cmp::Ordering::Greater => {
                (on(day) && minute >= self.start) || (on(previous) && minute < self.end)
            }
        }
    }
}
impl TimeWindow {
    /// Validate and compile (unknown zone, day or time: `None`).
    pub fn compile(&self) -> Option<CompiledWindow> {
        if self.timezone.is_empty() || self.timezone.len() > 64 || self.days.is_empty() {
            return None;
        }
        let mut days = 0u8;
        for d in &self.days {
            let i = DAYS.iter().position(|x| x == d)?;
            if days & (1 << i) != 0 {
                return None;
            }
            days |= 1 << i;
        }
        let tz = jiff::tz::TimeZone::get(&self.timezone).ok()?;
        Some(CompiledWindow {
            tz,
            days,
            start: parse_hhmm(&self.start)?,
            end: parse_hhmm(&self.end)?,
        })
    }
    fn bitmask(&self) -> Option<i16> {
        self.compile().map(|c| i16::from(c.days))
    }
}

impl RouteSettings {
    /// Validate the settings for a route of `provider`. `approvals` checks
    /// the metrics URL (approved local origin only). Returns a fixed message.
    pub fn validate(
        &self,
        provider: &str,
        approvals: &ApprovedEndpoints,
    ) -> Result<(), &'static str> {
        if !(1..=256).contains(&self.max_concurrency) {
            return Err("Batch lines at once must be 1–256.");
        }
        if self
            .yield_live_threshold
            .is_some_and(|n| !(1..=100_000).contains(&n))
        {
            return Err("The live-traffic threshold must be 1–100000 requests.");
        }
        if let Some(m) = &self.metrics {
            if m.max_waiting.is_none()
                && m.max_running.is_none()
                && m.max_kv_cache_percent.is_none()
            {
                return Err("Set at least one server load threshold.");
            }
            if m.max_waiting.is_some_and(|n| !(0..=100_000).contains(&n))
                || m.max_running.is_some_and(|n| !(0..=100_000).contains(&n))
                || m.max_kv_cache_percent
                    .is_some_and(|n| !(1..=100).contains(&n))
            {
                return Err("A server load threshold is out of range.");
            }
            if approvals.metrics_endpoint(&m.url).is_err() {
                return Err(
                    "The metrics URL must be /metrics on an approved local endpoint's origin.",
                );
            }
        }
        if let Some(p) = self.priority {
            if !PRIORITY_PROFILES.contains(&provider) {
                return Err("Priority hints need a vLLM-compatible route.");
            }
            if !(1..=1_000_000).contains(&p) {
                return Err("The priority must be 1–1000000 (lower runs earlier in vLLM).");
            }
        }
        if self.window.as_ref().is_some_and(|w| w.compile().is_none()) {
            return Err("The time window needs a known IANA time zone, days and HH:MM times.");
        }
        Ok(())
    }
}

/// The stored settings row.
#[derive(sqlx::FromRow)]
struct SettingsRow {
    max_concurrency: i32,
    yield_live_threshold: Option<i32>,
    metrics_url: Option<String>,
    metrics_max_waiting: Option<i32>,
    metrics_max_running: Option<i32>,
    metrics_max_kv_cache_percent: Option<i16>,
    priority: Option<i32>,
    window_timezone: Option<String>,
    window_days: Option<i16>,
    window_start_minute: Option<i16>,
    window_end_minute: Option<i16>,
}
impl From<SettingsRow> for RouteSettings {
    fn from(r: SettingsRow) -> Self {
        let window = match (
            r.window_timezone,
            r.window_days,
            r.window_start_minute,
            r.window_end_minute,
        ) {
            (Some(timezone), Some(days), Some(start), Some(end)) => Some(TimeWindow {
                timezone,
                days: DAYS
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| days & (1 << i) != 0)
                    .map(|(_, d)| (*d).to_owned())
                    .collect(),
                start: hhmm(start),
                end: hhmm(end),
            }),
            _ => None,
        };
        Self {
            max_concurrency: r.max_concurrency,
            yield_live_threshold: r.yield_live_threshold,
            metrics: r.metrics_url.map(|url| MetricsGate {
                url,
                max_waiting: r.metrics_max_waiting,
                max_running: r.metrics_max_running,
                max_kv_cache_percent: r.metrics_max_kv_cache_percent,
            }),
            priority: r.priority,
            window,
        }
    }
}

/// A route's settings (defaults when it has none).
pub async fn load_settings<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    deployment: Uuid,
) -> Result<RouteSettings, sqlx::Error> {
    let row: Option<SettingsRow> = sqlx::query_as("SELECT max_concurrency,yield_live_threshold,metrics_url,metrics_max_waiting,metrics_max_running,metrics_max_kv_cache_percent,priority,window_timezone,window_days,window_start_minute,window_end_minute FROM deployment_batch_scheduling WHERE deployment_id=$1")
        .bind(deployment)
        .fetch_optional(executor)
        .await?;
    Ok(row.map(RouteSettings::from).unwrap_or_default())
}

/// Store validated settings (an upsert).
pub async fn save_settings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deployment: Uuid,
    s: &RouteSettings,
    user: Uuid,
) -> Result<(), sqlx::Error> {
    let w = s.window.as_ref();
    let m = s.metrics.as_ref();
    sqlx::query("INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,yield_live_threshold,metrics_url,metrics_max_waiting,metrics_max_running,metrics_max_kv_cache_percent,priority,window_timezone,window_days,window_start_minute,window_end_minute,updated_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(deployment_id) DO UPDATE SET max_concurrency=excluded.max_concurrency,yield_live_threshold=excluded.yield_live_threshold,metrics_url=excluded.metrics_url,metrics_max_waiting=excluded.metrics_max_waiting,metrics_max_running=excluded.metrics_max_running,metrics_max_kv_cache_percent=excluded.metrics_max_kv_cache_percent,priority=excluded.priority,window_timezone=excluded.window_timezone,window_days=excluded.window_days,window_start_minute=excluded.window_start_minute,window_end_minute=excluded.window_end_minute,updated_at=clock_timestamp(),updated_by=excluded.updated_by")
        .bind(deployment)
        .bind(s.max_concurrency)
        .bind(s.yield_live_threshold)
        .bind(m.map(|m| m.url.as_str()))
        .bind(m.and_then(|m| m.max_waiting))
        .bind(m.and_then(|m| m.max_running))
        .bind(m.and_then(|m| m.max_kv_cache_percent))
        .bind(s.priority)
        .bind(w.map(|w| w.timezone.as_str()))
        .bind(w.and_then(TimeWindow::bitmask))
        .bind(w.and_then(|w| parse_hhmm(&w.start)).map(|n| n as i16))
        .bind(w.and_then(|w| parse_hhmm(&w.end)).map(|n| n as i16))
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// -------------------------------------------------------- Server metrics ----

/// One reading of a vLLM-compatible `/metrics` (`None`: not exported).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// `vllm:num_requests_waiting`, summed over engines.
    pub waiting: Option<u64>,
    /// `vllm:num_requests_running`, summed over engines.
    pub running: Option<u64>,
    /// `vllm:kv_cache_usage_perc` (or the older `vllm:gpu_cache_usage_perc`),
    /// a 0–1 fraction, as the maximum over engines in permille.
    pub kv_cache_permille: Option<u32>,
}

/// Parse Prometheus text exposition for the gauges above. Other series,
/// comments and malformed lines are ignored; values must be finite and ≥ 0.
pub fn parse_metrics(text: &str) -> Reading {
    let mut r = Reading::default();
    let (mut waiting, mut running) = (None::<f64>, None::<f64>);
    let mut kv = None::<f64>;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name_end = line.find(['{', ' ', '\t']).unwrap_or(line.len());
        let name = &line[..name_end];
        let rest = &line[name_end..];
        let rest = if rest.starts_with('{') {
            // Skip the label set, honoring quoted values and escapes.
            let (mut quoted, mut escaped, mut close) = (false, false, None);
            for (i, c) in rest.char_indices().skip(1) {
                match c {
                    _ if escaped => escaped = false,
                    '\\' if quoted => escaped = true,
                    '"' => quoted = !quoted,
                    '}' if !quoted => {
                        close = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            match close {
                Some(i) => &rest[i + 1..],
                None => continue,
            }
        } else {
            rest
        };
        let Some(value) = rest.split_whitespace().next() else {
            continue;
        };
        let Ok(value) = value.parse::<f64>() else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        match name {
            "vllm:num_requests_waiting" => *waiting.get_or_insert(0.0) += value,
            "vllm:num_requests_running" => *running.get_or_insert(0.0) += value,
            "vllm:kv_cache_usage_perc" | "vllm:gpu_cache_usage_perc" => {
                kv = Some(kv.map_or(value, |k: f64| k.max(value)));
            }
            _ => {}
        }
    }
    let count = |v: f64| v.round().min(u64::MAX as f64) as u64;
    r.waiting = waiting.map(count);
    r.running = running.map(count);
    r.kv_cache_permille = kv.map(|k| (k.min(1.0) * 1000.0).round() as u32);
    r
}

impl MetricsGate {
    /// Whether the server is busy. A threshold whose metric is missing makes
    /// the reading unusable (`Err`): the gate fails closed.
    pub fn busy(&self, r: &Reading) -> Result<bool, &'static str> {
        let above = |limit: Option<i32>, value: Option<u64>| match limit {
            None => Ok(false),
            Some(n) => value.map(|v| v > n.max(0) as u64).ok_or("missing_metric"),
        };
        let kv = match self.max_kv_cache_percent {
            None => false,
            Some(p) => r.kv_cache_permille.ok_or("missing_metric")? > (p.max(0) as u32) * 10,
        };
        Ok(above(self.max_waiting, r.waiting)? || above(self.max_running, r.running)? || kv)
    }
}

/// Fetch one reading: bounded time and size, the approval's pinned client.
async fn fetch_reading(
    approvals: Option<&ApprovedEndpoints>,
    url: &str,
    timeout: Duration,
) -> Result<Reading, &'static str> {
    let (client, url) = approvals
        .ok_or("not_approved")?
        .metrics_endpoint(url)
        .map_err(|_| "not_approved")?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut response = tokio::time::timeout_at(
        deadline,
        client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/plain")
            .send(),
    )
    .await
    .map_err(|_| "timeout")?
    .map_err(|_| "unreachable")?;
    if !response.status().is_success() {
        return Err("http_status");
    }
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Err(_) => return Err("timeout"),
            Ok(Err(_)) => return Err("unreachable"),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => {
                if body.len() + chunk.len() > METRICS_MAX_BYTES {
                    return Err("too_large");
                }
                body.extend_from_slice(&chunk);
            }
        }
    }
    let text = std::str::from_utf8(&body).map_err(|_| "invalid_body")?;
    Ok(parse_metrics(text))
}

// ------------------------------------------------------------- Scheduler ----

/// Cache lifetimes (tests shorten them).
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub settings: Duration,
    pub live: Duration,
    pub metrics: Duration,
    pub metrics_timeout: Duration,
    /// Minimum interval between signal writes of one route (unless the
    /// pause reason changed).
    pub signal: Duration,
}
impl Default for Timings {
    fn default() -> Self {
        Self {
            settings: Duration::from_secs(5),
            live: Duration::from_secs(1),
            metrics: Duration::from_secs(5),
            metrics_timeout: Duration::from_secs(2),
            signal: Duration::from_secs(5),
        }
    }
}

/// A route's settings with its compiled window.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub settings: RouteSettings,
    window: Option<CompiledWindow>,
}

/// The gates of one route right now.
#[derive(Clone, Debug)]
pub struct Gate {
    pub settings: Arc<Compiled>,
    pub pause: Option<Pause>,
}

type MetricsSlot = Arc<tokio::sync::Mutex<Option<(Instant, Result<Reading, &'static str>)>>>;

/// One process's view of route gates (cached; the claim itself is durable).
pub struct Scheduler {
    store: Store,
    approvals: Option<Arc<ApprovedEndpoints>>,
    pub(crate) timings: Timings,
    settings: Mutex<HashMap<Uuid, (Instant, Arc<Compiled>)>>,
    live: Mutex<HashMap<Uuid, (Instant, i64)>>,
    metrics: Mutex<HashMap<String, MetricsSlot>>,
    signals: Mutex<HashMap<Uuid, (Instant, Option<Pause>)>>,
}

/// Outcome of a claim attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Claimed,
    /// The line was already claimed (never run it again).
    Taken,
    /// The route runs its maximum.
    Full,
    /// Another batch's turn.
    Wait,
}

/// One batch's demand on one route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitRow {
    pub deployment: Uuid,
    pub waiting_lines: i32,
    pub reason: Option<Pause>,
    pub ready: bool,
}

/// The fair-share order of fresh demand on route `$1` (`ready` only when
/// `READY`): workspace running lines, workspace last served, batch running
/// lines, batch last served, waiting since.
fn order_sql(ready_only: bool) -> String {
    format!(
        "WITH w AS (SELECT job_id,workspace_id,last_claim_at,since FROM batch_route_waits WHERE deployment_id=$1 {} AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 jr AS (SELECT job_id,count(*) n FROM batch_lines WHERE deployment_id=$1 AND state='running' GROUP BY job_id),
 wr AS (SELECT workspace_id,count(*) n FROM batch_lines WHERE deployment_id=$1 AND state='running' GROUP BY workspace_id),
 x AS (SELECT w.job_id,w.last_claim_at,w.since,coalesce(jr.n,0) jn,coalesce(wr.n,0) wn,max(w.last_claim_at) OVER (PARTITION BY w.workspace_id) wl FROM w LEFT JOIN jr ON jr.job_id=w.job_id LEFT JOIN wr ON wr.workspace_id=w.workspace_id)
 SELECT job_id FROM x ORDER BY wn,wl NULLS FIRST,jn,last_claim_at NULLS FIRST,since,job_id LIMIT 1",
        if ready_only { "AND ready" } else { "" }
    )
}

impl Scheduler {
    pub fn new(store: Store, approvals: Option<Arc<ApprovedEndpoints>>) -> Self {
        Self {
            store,
            approvals,
            timings: Timings::default(),
            settings: Mutex::default(),
            live: Mutex::default(),
            metrics: Mutex::default(),
            signals: Mutex::default(),
        }
    }
    pub fn with_timings(mut self, timings: Timings) -> Self {
        self.timings = timings;
        self
    }

    async fn settings(&self, deployment: Uuid) -> Result<Arc<Compiled>, sqlx::Error> {
        if let Some((at, s)) = self.settings.lock().expect("settings").get(&deployment)
            && at.elapsed() < self.timings.settings
        {
            return Ok(s.clone());
        }
        let settings = load_settings(&self.store.pool, deployment).await?;
        let compiled = Arc::new(Compiled {
            window: settings.window.as_ref().and_then(TimeWindow::compile),
            settings,
        });
        self.settings
            .lock()
            .expect("settings")
            .insert(deployment, (Instant::now(), compiled.clone()));
        Ok(compiled)
    }

    /// Live (non-batch) requests to the route in flight: started executions
    /// whose reservation lease is still running (all processes).
    async fn live_in_flight(&self, deployment: Uuid) -> Result<i64, sqlx::Error> {
        if let Some((at, n)) = self.live.lock().expect("live").get(&deployment)
            && at.elapsed() < self.timings.live
        {
            return Ok(*n);
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.deployment_id=$1 AND e.state='started' AND e.batch_job_id IS NULL AND e.workload_kind NOT IN('batches','videos') AND r.state='pending' AND r.lease_expires_at>clock_timestamp()")
            .bind(deployment)
            .fetch_one(&self.store.pool)
            .await?;
        self.live
            .lock()
            .expect("live")
            .insert(deployment, (Instant::now(), n));
        Ok(n)
    }

    async fn reading(&self, url: &str) -> (Instant, Result<Reading, &'static str>) {
        let slot = self
            .metrics
            .lock()
            .expect("metrics")
            .entry(url.to_owned())
            .or_default()
            .clone();
        // One fetch per URL at a time; concurrent callers share it.
        let mut slot = slot.lock().await;
        if let Some((at, r)) = *slot
            && at.elapsed() < self.timings.metrics
        {
            return (at, r);
        }
        let r = fetch_reading(self.approvals.as_deref(), url, self.timings.metrics_timeout).await;
        let at = Instant::now();
        *slot = Some((at, r));
        (at, r)
    }

    /// Evaluate the route's gates (window, live traffic, server load) and
    /// record the result for Admin and metrics.
    pub async fn gate(&self, deployment: Uuid) -> Result<Gate, InferenceError> {
        let compiled = self
            .settings(deployment)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let s = &compiled.settings;
        let mut live = None;
        let mut reading: Option<Result<Reading, &'static str>> = None;
        // Cheapest first: the window, then live traffic, then the server.
        let mut pause = compiled
            .window
            .as_ref()
            .is_some_and(|w| !w.allows(Utc::now()))
            .then_some(Pause::OutsideWindow);
        if pause.is_none()
            && let Some(threshold) = s.yield_live_threshold
        {
            let n = self
                .live_in_flight(deployment)
                .await
                .map_err(|_| InferenceError::Storage)?;
            live = Some(n);
            pause = (n >= i64::from(threshold)).then_some(Pause::LiveTraffic);
        }
        if pause.is_none()
            && let Some(m) = &s.metrics
        {
            let (_, r) = self.reading(&m.url).await;
            reading = Some(r);
            pause = match r.and_then(|r| m.busy(&r)) {
                Ok(false) => None,
                Ok(true) => Some(Pause::ServerBusy),
                Err(_) => Some(Pause::MetricsUnavailable),
            };
        }
        self.record_signal(deployment, pause, live, reading).await;
        Ok(Gate {
            settings: compiled,
            pause,
        })
    }

    /// Record the route's last evaluation (throttled per process).
    async fn record_signal(
        &self,
        deployment: Uuid,
        pause: Option<Pause>,
        live: Option<i64>,
        reading: Option<Result<Reading, &'static str>>,
    ) {
        {
            let mut signals = self.signals.lock().expect("signals");
            let previous = signals.get(&deployment).copied();
            if let Some((at, p)) = previous
                && p == pause
                && at.elapsed() < self.timings.signal
            {
                return;
            }
            if let Some(p) = pause.filter(|p| previous.is_none_or(|(_, q)| q != Some(*p))) {
                crate::metrics::METRICS.observe_batch_route_pause(p.as_str());
            }
            signals.insert(deployment, (Instant::now(), pause));
        }
        let (ok, waiting, running, kv, error) = match reading {
            None => (None, None, None, None, None),
            Some(Ok(r)) => (
                Some(true),
                r.waiting.map(|n| n.min(i64::MAX as u64) as i64),
                r.running.map(|n| n.min(i64::MAX as u64) as i64),
                r.kv_cache_permille.map(|n| n as i32),
                None,
            ),
            Some(Err(code)) => (Some(false), None, None, None, Some(code)),
        };
        // A reading that misses a configured metric is still a failed reading.
        let (ok, error) = if pause == Some(Pause::MetricsUnavailable) {
            (Some(false), error.or(Some("missing_metric")))
        } else {
            (ok, error)
        };
        let result = sqlx::query("INSERT INTO deployment_batch_signals(deployment_id,paused_reason,checked_at,live_in_flight,metrics_checked_at,metrics_ok,metrics_waiting,metrics_running,metrics_kv_cache_permille,metrics_error) VALUES($1,$2,clock_timestamp(),$3,CASE WHEN $4::boolean IS NULL THEN NULL ELSE clock_timestamp() END,$4,$5,$6,$7,$8) ON CONFLICT(deployment_id) DO UPDATE SET paused_reason=excluded.paused_reason,checked_at=excluded.checked_at,live_in_flight=coalesce(excluded.live_in_flight,deployment_batch_signals.live_in_flight),metrics_checked_at=coalesce(excluded.metrics_checked_at,deployment_batch_signals.metrics_checked_at),metrics_ok=coalesce(excluded.metrics_ok,deployment_batch_signals.metrics_ok),metrics_waiting=CASE WHEN excluded.metrics_ok IS NULL THEN deployment_batch_signals.metrics_waiting ELSE excluded.metrics_waiting END,metrics_running=CASE WHEN excluded.metrics_ok IS NULL THEN deployment_batch_signals.metrics_running ELSE excluded.metrics_running END,metrics_kv_cache_permille=CASE WHEN excluded.metrics_ok IS NULL THEN deployment_batch_signals.metrics_kv_cache_permille ELSE excluded.metrics_kv_cache_permille END,metrics_error=CASE WHEN excluded.metrics_ok IS NULL THEN deployment_batch_signals.metrics_error ELSE excluded.metrics_error END")
            .bind(deployment)
            .bind(pause.filter(|p| p.route_wide()).map(Pause::as_str))
            .bind(live.map(|n| n.clamp(0, i32::MAX as i64) as i32))
            .bind(ok)
            .bind(waiting)
            .bind(running)
            .bind(kv)
            .bind(error)
            .execute(&self.store.pool)
            .await;
        if result.is_err() {
            tracing::warn!(deployment_id = %deployment, "batch route signal not recorded");
        }
    }

    /// Batch lines running on `deployment` (all batches, all processes).
    pub async fn running_lines(&self, deployment: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running'",
        )
        .bind(deployment)
        .fetch_one(&self.store.pool)
        .await
    }

    /// Claim line `line` of `job` on `deployment` when the route has room and
    /// it is this batch's turn. Serialized per route by a transaction lock;
    /// inserting the line row is the (exactly-once) claim.
    pub async fn claim(
        &self,
        job: &JobRow,
        line: i32,
        execution: Uuid,
        deployment: Uuid,
        max_concurrency: i32,
    ) -> Result<Claim, sqlx::Error> {
        let mut tx = self.store.pool.begin().await?;
        lock_route(&mut tx, deployment).await?;
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running'",
        )
        .bind(deployment)
        .fetch_one(&mut *tx)
        .await?;
        if running >= i64::from(max_concurrency) {
            tx.commit().await?;
            return Ok(Claim::Full);
        }
        sqlx::query("INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,ready) VALUES($1,$2,$3,1,true) ON CONFLICT(deployment_id,job_id) DO UPDATE SET ready=true,updated_at=clock_timestamp()")
            .bind(job.id)
            .bind(job.workspace_id)
            .bind(deployment)
            .execute(&mut *tx)
            .await?;
        let first: Option<Uuid> = sqlx::query_scalar(&order_sql(true))
            .bind(deployment)
            .fetch_optional(&mut *tx)
            .await?;
        if first != Some(job.id) {
            tx.commit().await?;
            return Ok(Claim::Wait);
        }
        let inserted = sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id,deployment_id) VALUES($1,$2,$3,'running',$4,$5) ON CONFLICT DO NOTHING")
            .bind(job.id)
            .bind(job.workspace_id)
            .bind(line)
            .bind(execution)
            .bind(deployment)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if inserted != 1 {
            tx.commit().await?;
            return Ok(Claim::Taken);
        }
        sqlx::query("UPDATE batch_route_waits SET last_claim_at=clock_timestamp() WHERE deployment_id=$1 AND job_id=$2")
            .bind(deployment)
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Claim::Claimed)
    }

    /// An explicit retry of a failed line on its route (a new attempt),
    /// within the route's concurrency. `None`: the route is full.
    pub async fn claim_retry(
        &self,
        job: Uuid,
        line: i32,
        attempts: i16,
        execution: Uuid,
        deployment: Uuid,
        max_concurrency: i32,
    ) -> Result<Option<bool>, sqlx::Error> {
        let mut tx = self.store.pool.begin().await?;
        lock_route(&mut tx, deployment).await?;
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running'",
        )
        .bind(deployment)
        .fetch_one(&mut *tx)
        .await?;
        if running >= i64::from(max_concurrency) {
            tx.commit().await?;
            return Ok(None);
        }
        let claimed = sqlx::query("UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=$4,status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=$1 AND line_no=$2 AND state='failed' AND attempts=$3 AND segment IS NULL")
            .bind(job).bind(line).bind(attempts).bind(execution).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(Some(claimed))
    }

    /// Replace this batch's demand rows (heartbeat). `legit` records that
    /// the batch is legitimately waiting (stall alerts pause their clock).
    pub async fn publish_waits(
        &self,
        job: &JobRow,
        rows: &[WaitRow],
        legit: bool,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.store.pool.begin().await?;
        let ids: Vec<Uuid> = rows.iter().map(|r| r.deployment).collect();
        let lines: Vec<i32> = rows
            .iter()
            .map(|r| r.waiting_lines.clamp(0, 50_000))
            .collect();
        let reasons: Vec<Option<&str>> = rows.iter().map(|r| r.reason.map(Pause::as_str)).collect();
        let ready: Vec<bool> = rows.iter().map(|r| r.ready).collect();
        sqlx::query("DELETE FROM batch_route_waits WHERE job_id=$1 AND deployment_id<>ALL($2)")
            .bind(job.id)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,reason,ready) SELECT $1,$2,d,n,r,y FROM unnest($3::uuid[],$4::integer[],$5::text[],$6::boolean[]) AS u(d,n,r,y) ON CONFLICT(deployment_id,job_id) DO UPDATE SET waiting_lines=excluded.waiting_lines,reason=excluded.reason,ready=excluded.ready,updated_at=clock_timestamp()")
            .bind(job.id)
            .bind(job.workspace_id)
            .bind(&ids)
            .bind(&lines)
            .bind(&reasons)
            .bind(&ready)
            .execute(&mut *tx)
            .await?;
        if legit {
            sqlx::query("UPDATE async_jobs SET last_waited_at=clock_timestamp() WHERE id=$1 AND state IN('queued','in_progress')")
                .bind(job.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Remove a finished batch's demand.
    pub async fn clear_waits(&self, job: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM batch_route_waits WHERE job_id=$1")
            .bind(job)
            .execute(&self.store.pool)
            .await?;
        Ok(())
    }
}

async fn lock_route(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deployment: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('omg_batch_route:'||$1::text,0))")
        .bind(deployment)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// ------------------------------------------------------------ Visibility ----

/// Admin › route: queue depth, batch lines running, the pause reason and
/// the last server metrics reading.
pub async fn route_status<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    deployment: Uuid,
) -> Result<Value, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT jsonb_build_object(
 'queued_lines',(SELECT coalesce(sum(waiting_lines),0) FROM batch_route_waits WHERE deployment_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 'waiting_batches',(SELECT count(*) FROM batch_route_waits WHERE deployment_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 'running_lines',(SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running'),
 'paused_reason',s.paused_reason,'checked_at',s.checked_at,'live_in_flight',s.live_in_flight,
 'metrics',CASE WHEN s.metrics_checked_at IS NULL THEN NULL ELSE jsonb_build_object('checked_at',s.metrics_checked_at,'ok',s.metrics_ok,'waiting',s.metrics_waiting,'running',s.metrics_running,'kv_cache_permille',s.metrics_kv_cache_permille,'error',s.metrics_error) END)
 FROM (SELECT 1) one LEFT JOIN deployment_batch_signals s ON s.deployment_id=$1"))
        .bind(deployment)
        .fetch_one(executor)
        .await
}

/// A batch's waits per route (model name, reason, lines waiting and running,
/// queue position and length on the route). Metadata only.
pub async fn batch_waits<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    job: Uuid,
) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(&format!("WITH mine AS (SELECT deployment_id FROM batch_route_waits WHERE job_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 w AS (SELECT b.* FROM batch_route_waits b WHERE b.deployment_id IN (SELECT deployment_id FROM mine) AND b.updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 jr AS (SELECT deployment_id,job_id,count(*) n FROM batch_lines WHERE state='running' AND deployment_id IN (SELECT deployment_id FROM mine) GROUP BY 1,2),
 wr AS (SELECT deployment_id,workspace_id,count(*) n FROM batch_lines WHERE state='running' AND deployment_id IN (SELECT deployment_id FROM mine) GROUP BY 1,2),
 x AS (SELECT w.*,coalesce(jr.n,0) jn,coalesce(wr.n,0) wn,max(w.last_claim_at) OVER (PARTITION BY w.deployment_id,w.workspace_id) wl FROM w LEFT JOIN jr ON jr.deployment_id=w.deployment_id AND jr.job_id=w.job_id LEFT JOIN wr ON wr.deployment_id=w.deployment_id AND wr.workspace_id=w.workspace_id),
 r AS (SELECT x.*,row_number() OVER (PARTITION BY deployment_id ORDER BY wn,wl NULLS FIRST,jn,last_claim_at NULLS FIRST,since,job_id) pos,count(*) OVER (PARTITION BY deployment_id) queue FROM x)
 SELECT jsonb_build_object('model',m.public_name,'reason',r.reason,'waiting_lines',r.waiting_lines,'running_lines',r.jn,'position',r.pos,'queue',r.queue,'since',r.since)
 FROM r JOIN deployments d ON d.id=r.deployment_id JOIN models m ON m.id=d.model_id WHERE r.job_id=$1 ORDER BY m.public_name,r.deployment_id"))
        .bind(job)
        .fetch_all(executor)
        .await
}

/// Installation-wide gauges: batch lines waiting and running per provider
/// kind, and routes paused per reason.
pub async fn observe_metrics(store: &Store) -> Result<(), sqlx::Error> {
    let lines: Vec<(String, i64, i64)> = sqlx::query_as(&format!("WITH w AS (SELECT p.provider,sum(b.waiting_lines)::bigint n FROM batch_route_waits b JOIN deployments d ON d.id=b.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE b.updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS}) GROUP BY 1),
 r AS (SELECT p.provider,count(*) n FROM batch_lines l JOIN deployments d ON d.id=l.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE l.state='running' AND l.deployment_id IS NOT NULL GROUP BY 1)
 SELECT coalesce(w.provider,r.provider),coalesce(w.n,0),coalesce(r.n,0) FROM w FULL JOIN r ON r.provider=w.provider"))
        .fetch_all(&store.pool)
        .await?;
    let paused: Vec<(String, i64)> = sqlx::query_as(&format!("SELECT reason,count(DISTINCT deployment_id) FROM batch_route_waits WHERE reason IS NOT NULL AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS}) GROUP BY 1"))
        .fetch_all(&store.pool)
        .await?;
    crate::metrics::METRICS.set_batch_route_lines(
        &lines
            .into_iter()
            .map(|(p, w, r)| (p, w.max(0) as u64, r.max(0) as u64))
            .collect::<Vec<_>>(),
    );
    crate::metrics::METRICS.set_batch_paused_routes(
        &paused
            .into_iter()
            .filter_map(|(r, n)| Pause::parse(&r).map(|p| (p.as_str(), n.max(0) as u64)))
            .collect::<Vec<_>>(),
    );
    Ok(())
}

/// The settings document of the management API (settings, defaults and
/// what the route supports).
pub fn settings_document(settings: &RouteSettings, provider: &str, status: Value) -> Value {
    json!({
        "settings": settings,
        "defaults": RouteSettings::default(),
        "priority_supported": PRIORITY_PROFILES.contains(&provider),
        "status": status,
    })
}

#[cfg(test)]
mod tests;

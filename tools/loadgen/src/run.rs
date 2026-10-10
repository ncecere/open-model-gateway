//! Open-loop load generation against one or more gateway replicas.
//!
//! Requests are scheduled at a fixed arrival rate (request `i` at
//! `start + i / rate`) whether or not earlier requests have finished, so a
//! slow gateway accumulates in-flight requests instead of silently lowering
//! the offered load (no coordinated omission). Latency is measured from the
//! scheduled time. Every prompt carries `nonce:<run>-<i>`; the gateway's
//! `x-request-id` (its `root_request_id`) is recorded per request so that
//! upstream calls (from the mock) and durable reservations (from the
//! database) can be matched to client requests exactly.
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    keys,
    prom::{self, Sample, Summary},
    reader::{self, ReaderStats},
    stats::{Percentiles, percentiles},
    verify::{self, DatabaseCheck},
};

#[derive(Clone, Debug, Serialize)]
pub struct RunConfig {
    /// Gateway base URLs (round-robin), without a trailing `/v1`.
    pub targets: Vec<String>,
    /// Offered arrival rate, requests per second.
    pub rate: f64,
    pub duration_s: f64,
    /// Leading seconds whose requests are sent and verified but excluded from
    /// latency statistics; gateway metrics are scraped at its end.
    pub warmup_s: f64,
    /// Fraction of streamed requests (deterministic per request).
    pub stream_ratio: f64,
    #[serde(skip)]
    pub key_seed: String,
    /// Keys used: indexes `key_offset .. key_offset + keys` of the seeded set.
    pub keys: u64,
    pub key_offset: u64,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_s: f64,
    /// Requests in flight beyond this are not sent (`client_saturated`).
    pub max_in_flight: usize,
    pub rng_seed: u64,
    /// Mock upstream admin base URL (`/__mock/calls`) for nonce matching.
    pub mock_url: Option<String>,
    /// Gateway metrics endpoints (`.../metrics`), one per replica.
    pub metrics_urls: Vec<String>,
    #[serde(skip)]
    pub database_url: Option<String>,
    pub label: Option<String>,
    /// Concurrent closed-loop management readers (reports, usage, logs,
    /// `/me`, `/v1/models`); 0 disables them. See [`crate::reader`].
    pub readers: usize,
    /// Report window of the readers, in days ending tomorrow (UTC).
    pub reader_days: u32,
}

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub index: u64,
    pub nonce: String,
    pub target: usize,
    pub stream: bool,
    pub warmup: bool,
    /// HTTP status; 0 when no response was received.
    pub status: u16,
    /// 200 and a complete body (`[DONE]` for streams).
    pub ok: bool,
    pub request_id: Option<Uuid>,
    pub error_code: Option<String>,
    /// Scheduled time to end of response.
    pub latency_us: u64,
    /// Sent to end of response.
    pub service_us: u64,
    /// Sent to first body byte (streams) or response headers.
    pub ttfb_us: u64,
    /// Scheduled to sent.
    pub lag_us: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub scheduled: u64,
    pub measured: u64,
    pub sent: u64,
    pub ok: u64,
    pub incomplete: u64,
    pub by_status: BTreeMap<String, u64>,
    pub by_error_code: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub struct Latencies {
    pub all: Percentiles,
    pub stream: Percentiles,
    pub non_stream: Percentiles,
}

#[derive(Debug, Serialize)]
pub struct TargetStats {
    pub target: String,
    pub sent: u64,
    pub ok: u64,
    pub latency: Percentiles,
}

#[derive(Debug, Default, Serialize)]
pub struct GatewayMetrics {
    /// `gateway_admission_seconds` deltas by `phase=..,outcome=..`.
    pub admission: BTreeMap<String, Summary>,
    pub settlement: BTreeMap<String, Summary>,
    pub admission_denials: BTreeMap<String, f64>,
    pub settlements: BTreeMap<String, f64>,
    /// `gateway_cache_lookups_total` deltas by `cache=..,result=..` (P4).
    pub cache_lookups: BTreeMap<String, f64>,
    /// `gateway_background_runs_total` deltas by `job=..,result=..` (P5).
    pub background_runs: BTreeMap<String, f64>,
    pub scrape_errors: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct UpstreamCheck {
    /// Mock calls carrying this run's nonce prefix.
    pub calls: u64,
    pub ok_requests_matched: u64,
    pub ok_requests_without_upstream_call: u64,
    /// Nonces seen more than once upstream (an implicit retry).
    pub duplicate_upstream_calls: u64,
    /// Upstream calls for requests the client saw denied (status 429).
    pub upstream_calls_for_denied_requests: u64,
    /// Upstream calls whose nonce the client never sent.
    pub upstream_calls_without_client_request: u64,
    pub upstream_errors: u64,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// The mock's configuration (latency, TTFT, tokens, error rate).
    pub mock_config: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub run_id: String,
    pub started_at_unix: u64,
    pub config: RunConfig,
    pub elapsed_s: f64,
    /// Requests actually sent per second over the scheduling window.
    pub offered_rate: f64,
    /// Successful measured requests per second over the measured window.
    pub throughput_ok_per_s: f64,
    pub totals: Totals,
    pub latency_ms: Latencies,
    pub ttfb_ms: Percentiles,
    pub schedule_lag_ms: Percentiles,
    /// Client service time minus the mock's own service time (successful
    /// requests with exactly one matched upstream call).
    pub gateway_overhead_ms: Option<Percentiles>,
    pub per_target: Vec<TargetStats>,
    pub gateway: Option<GatewayMetrics>,
    pub upstream: Option<UpstreamCheck>,
    pub database: Option<DatabaseCheck>,
    /// Concurrent management readers (`--readers`), when enabled.
    pub reader: Option<ReaderStats>,
    pub violations: Vec<String>,
    /// Per scheduled second (from `started_at_unix`): requests, failures
    /// and client latency percentiles, to locate latency spikes in time
    /// (for example background jobs on every replica).
    pub timeline: Vec<TimelinePoint>,
}

#[derive(Debug, Serialize)]
pub struct TimelinePoint {
    pub second: u64,
    pub requests: usize,
    pub failed: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Group requests by the second they were scheduled in.
pub fn timeline(records: &[RequestRecord], interval_s: f64) -> Vec<TimelinePoint> {
    let mut seconds: BTreeMap<u64, (Vec<u64>, usize)> = BTreeMap::new();
    for r in records {
        let second = (r.index as f64 * interval_s).floor() as u64;
        let entry = seconds.entry(second).or_default();
        entry.0.push(r.latency_us);
        if !r.ok {
            entry.1 += 1;
        }
    }
    seconds
        .into_iter()
        .map(|(second, (latencies, failed))| {
            let p = crate::stats::percentiles(latencies);
            TimelinePoint {
                second,
                requests: p.count,
                failed,
                p50_ms: p.p50_ms,
                p99_ms: p.p99_ms,
                max_ms: p.max_ms,
            }
        })
        .collect()
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic (key index, stream) for request `i`.
pub fn plan(config: &RunConfig, i: u64) -> (u64, bool) {
    let a = splitmix(config.rng_seed ^ i.wrapping_mul(0x2545_F491_4F6C_DD1D));
    let b = splitmix(a);
    let key = config.key_offset + a % config.keys.max(1);
    let stream = (b % 1_000_000) < (config.stream_ratio * 1_000_000.0) as u64;
    (key, stream)
}

fn micros(d: Duration) -> u64 {
    d.as_micros().min(u128::from(u64::MAX)) as u64
}

struct Request {
    index: u64,
    nonce: String,
    target: usize,
    url: String,
    token: Arc<str>,
    stream: bool,
    warmup: bool,
    scheduled: Instant,
}

async fn execute(
    client: reqwest::Client,
    request: Request,
    model: Arc<str>,
    max_tokens: u32,
    timeout: Duration,
) -> RequestRecord {
    let sent = Instant::now();
    let mut record = RequestRecord {
        index: request.index,
        nonce: request.nonce.clone(),
        target: request.target,
        stream: request.stream,
        warmup: request.warmup,
        status: 0,
        ok: false,
        request_id: None,
        error_code: None,
        latency_us: 0,
        service_us: 0,
        ttfb_us: 0,
        lag_us: micros(sent.saturating_duration_since(request.scheduled)),
    };
    let body = json!({
        "model": &*model,
        "max_completion_tokens": max_tokens,
        "stream": request.stream,
        "messages": [{"role": "user", "content": format!("Synthetic load test request nonce:{}. Reply briefly.", request.nonce)}],
    });
    let work = async {
        let response = client
            .post(&request.url)
            .bearer_auth(&*request.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        record.status = status;
        record.request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v).ok());
        record.ttfb_us = micros(sent.elapsed());
        if status == 200 && request.stream {
            let mut chunks = response.bytes_stream();
            let mut window: Vec<u8> = Vec::new();
            let mut first = true;
            let mut done = false;
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk?;
                if first && !chunk.is_empty() {
                    record.ttfb_us = micros(sent.elapsed());
                    first = false;
                }
                window.extend_from_slice(&chunk);
                if window.windows(12).any(|w| w == b"data: [DONE]") {
                    done = true;
                }
                if window.len() > 64 {
                    window.drain(..window.len() - 64);
                }
            }
            record.ok = done;
            if !done {
                record.error_code = Some("stream_incomplete".into());
            }
        } else {
            let bytes = response.bytes().await?;
            if status == 200 {
                record.ok =
                    serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["choices"].is_array());
                if !record.ok {
                    record.error_code = Some("invalid_response".into());
                }
            } else {
                record.error_code = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
                    v["error"]["code"]
                        .as_str()
                        .or_else(|| v["error"]["type"].as_str())
                        .map(|c| c.chars().take(64).collect())
                });
            }
        }
        Ok::<_, reqwest::Error>(())
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            record.error_code = Some(
                if e.is_connect() {
                    "client_connect_error"
                } else {
                    "client_transport_error"
                }
                .into(),
            );
            record.ok = false;
        }
        Err(_) => {
            record.error_code = Some("client_timeout".into());
            record.ok = false;
        }
    }
    let end = Instant::now();
    record.service_us = micros(end - sent);
    record.latency_us = micros(end.saturating_duration_since(request.scheduled));
    record
}

async fn scrape(client: &reqwest::Client, urls: &[String]) -> (Vec<Vec<Sample>>, u64) {
    let mut out = Vec::new();
    let mut errors = 0;
    for url in urls {
        let text = match client.get(url).send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            _ => {
                errors += 1;
                String::new()
            }
        };
        out.push(prom::parse(&text));
    }
    (out, errors)
}

pub async fn run(config: RunConfig) -> anyhow::Result<Report> {
    anyhow::ensure!(
        !config.targets.is_empty(),
        "at least one --target is required"
    );
    anyhow::ensure!(
        config.rate > 0.0 && config.rate <= 100_000.0,
        "rate must be in (0, 100000]"
    );
    anyhow::ensure!(
        config.duration_s > 0.0 && config.warmup_s >= 0.0 && config.warmup_s < config.duration_s,
        "duration must exceed the warmup"
    );
    anyhow::ensure!(config.keys > 0, "at least one key is required");
    anyhow::ensure!(
        (0.0..=1.0).contains(&config.stream_ratio),
        "stream ratio must be between 0 and 1"
    );
    let run_id = format!("r{}", &Uuid::new_v4().simple().to_string()[..10]);
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(config.max_in_flight)
        .pool_idle_timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("HTTP client")?;
    let database = match &config.database_url {
        Some(url) => Some(verify::Database::connect(url).await?),
        None => None,
    };
    let db_start = match &database {
        Some(db) => Some(db.now().await?),
        None => None,
    };
    let tokens: Vec<Arc<str>> = (config.key_offset..config.key_offset + config.keys)
        .map(|i| Arc::from(keys::token(&config.key_seed, i)))
        .collect();
    let urls: Vec<String> = config
        .targets
        .iter()
        .map(|t| format!("{}/v1/chat/completions", t.trim_end_matches('/')))
        .collect();
    let model: Arc<str> = Arc::from(config.model.as_str());
    let timeout = Duration::from_secs_f64(config.timeout_s);
    let total = (config.rate * config.duration_s).round() as u64;
    let warmup_requests = (config.rate * config.warmup_s).round() as u64;
    let interval = 1.0 / config.rate;
    let in_flight = Arc::new(AtomicUsize::new(0));

    let (mut before, mut scrape_errors) = (Vec::new(), 0);
    if warmup_requests == 0 {
        (before, scrape_errors) = scrape(&client, &config.metrics_urls).await;
    }
    let mut set = tokio::task::JoinSet::new();
    let mut saturated = Vec::new();
    let start = Instant::now() + Duration::from_millis(50);
    let mut measured_start = start;
    let mut readers = tokio::task::JoinSet::new();
    if config.readers > 0 {
        let cycle = reader::endpoints(&config.key_seed, config.reader_days);
        let session = keys::session_token(&config.key_seed, "reader");
        let key = keys::token(&config.key_seed, 0);
        let until = start + Duration::from_secs_f64(config.duration_s);
        for r in 0..config.readers {
            // Spread readers over the cycle so every endpoint is always in use.
            let offset = r * cycle.len() / config.readers;
            readers.spawn(reader::reader(
                client.clone(),
                config.targets.clone(),
                cycle.clone(),
                session.clone(),
                key.clone(),
                offset,
                until,
                timeout,
            ));
        }
    }
    for i in 0..total {
        let scheduled = start + Duration::from_secs_f64(i as f64 * interval);
        if i == warmup_requests && warmup_requests > 0 {
            // Requests up to here are warmup; metrics deltas start now.
            tokio::time::sleep_until(scheduled.into()).await;
            (before, scrape_errors) = scrape(&client, &config.metrics_urls).await;
            measured_start = scheduled;
        }
        tokio::time::sleep_until(scheduled.into()).await;
        let (key, stream) = plan(&config, i);
        let target = (i % urls.len() as u64) as usize;
        let nonce = format!("{run_id}-{i}");
        let warmup = i < warmup_requests;
        if in_flight.load(Ordering::Relaxed) >= config.max_in_flight {
            saturated.push(RequestRecord {
                index: i,
                nonce,
                target,
                stream,
                warmup,
                status: 0,
                ok: false,
                request_id: None,
                error_code: Some("client_saturated".into()),
                latency_us: 0,
                service_us: 0,
                ttfb_us: 0,
                lag_us: 0,
            });
            continue;
        }
        in_flight.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            index: i,
            nonce,
            target,
            url: urls[target].clone(),
            token: tokens[(key - config.key_offset) as usize].clone(),
            stream,
            warmup,
            scheduled,
        };
        let (client, model, in_flight) = (client.clone(), model.clone(), in_flight.clone());
        let max_tokens = config.max_tokens;
        set.spawn(async move {
            let record = execute(client, request, model, max_tokens, timeout).await;
            in_flight.fetch_sub(1, Ordering::Relaxed);
            record
        });
    }
    let scheduled_end = Instant::now();
    let mut records = saturated;
    while let Some(record) = set.join_next().await {
        records.push(record.context("request task")?);
    }
    let mut reader_samples = Vec::new();
    while let Some(samples) = readers.join_next().await {
        reader_samples.extend(samples.context("reader task")?);
    }
    let elapsed = start.elapsed();
    let (after, after_errors) = scrape(&client, &config.metrics_urls).await;
    records.sort_by_key(|r| r.index);

    let mut report = summarize(
        &config,
        &records,
        run_id,
        started_at_unix,
        elapsed,
        scheduled_end.saturating_duration_since(start),
        measured_start,
        start,
    );
    report.timeline = timeline(&records, interval);
    if config.readers > 0 {
        report.reader = Some(reader::summarize(
            config.readers,
            config.reader_days,
            &reader_samples,
            measured_start,
            scheduled_end.saturating_duration_since(measured_start),
        ));
    }
    if !config.metrics_urls.is_empty() {
        let admission = prom::histogram_deltas(&before, &after, "gateway_admission_seconds");
        let settlement = prom::histogram_deltas(&before, &after, "gateway_settlement_seconds");
        report.gateway = Some(GatewayMetrics {
            admission: admission
                .into_iter()
                .filter(|(_, h)| h.count > 0.0)
                .map(|(k, h)| (k, h.summary()))
                .collect(),
            settlement: settlement
                .into_iter()
                .filter(|(_, h)| h.count > 0.0)
                .map(|(k, h)| (k, h.summary()))
                .collect(),
            admission_denials: prom::counter_deltas(&before, &after, "gateway_admission_denials"),
            settlements: prom::counter_deltas(&before, &after, "gateway_settlements"),
            cache_lookups: prom::counter_deltas(&before, &after, "gateway_cache_lookups"),
            background_runs: prom::counter_deltas(&before, &after, "gateway_background_runs"),
            scrape_errors: scrape_errors + after_errors,
        });
    }
    if let Some(mock) = &config.mock_url {
        let upstream = upstream_check(&client, mock, &report.run_id, &records).await?;
        report.gateway_overhead_ms = Some(upstream.1);
        report.upstream = Some(upstream.0);
    }
    if let (Some(db), Some(since)) = (&database, db_start) {
        let expected = report
            .upstream
            .as_ref()
            .and_then(|u| u.prompt_tokens.zip(u.completion_tokens))
            .map(|(p, c)| (p as i64, c.min(u64::from(config.max_tokens)) as i64));
        report.database = Some(db.check(&records, since, expected).await?);
    }
    report.violations = violations(&report);
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    config: &RunConfig,
    records: &[RequestRecord],
    run_id: String,
    started_at_unix: u64,
    elapsed: Duration,
    scheduling: Duration,
    measured_start: Instant,
    start: Instant,
) -> Report {
    let mut totals = Totals {
        scheduled: records.len() as u64,
        ..Totals::default()
    };
    let measured: Vec<&RequestRecord> = records.iter().filter(|r| !r.warmup).collect();
    totals.measured = measured.len() as u64;
    for r in records {
        if r.error_code.as_deref() != Some("client_saturated") {
            totals.sent += 1;
        }
        *totals.by_status.entry(r.status.to_string()).or_default() += 1;
        if let Some(code) = &r.error_code {
            *totals.by_error_code.entry(code.clone()).or_default() += 1;
        }
        if r.ok {
            totals.ok += 1;
        } else if r.status == 200 {
            totals.incomplete += 1;
        }
    }
    let ok: Vec<&&RequestRecord> = measured.iter().filter(|r| r.ok).collect();
    let lat = |filter: &dyn Fn(&RequestRecord) -> bool| {
        percentiles(
            ok.iter()
                .filter(|r| filter(r))
                .map(|r| r.latency_us)
                .collect(),
        )
    };
    let measured_window = elapsed
        .saturating_sub(measured_start.saturating_duration_since(start))
        .as_secs_f64()
        .max(1e-9);
    let per_target = config
        .targets
        .iter()
        .enumerate()
        .map(|(i, target)| TargetStats {
            target: target.clone(),
            sent: measured.iter().filter(|r| r.target == i).count() as u64,
            ok: ok.iter().filter(|r| r.target == i).count() as u64,
            latency: percentiles(
                ok.iter()
                    .filter(|r| r.target == i)
                    .map(|r| r.latency_us)
                    .collect(),
            ),
        })
        .collect();
    Report {
        run_id,
        started_at_unix,
        config: config.clone(),
        elapsed_s: (elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        offered_rate: (totals.sent as f64 / scheduling.as_secs_f64().max(1e-9) * 10.0).round()
            / 10.0,
        throughput_ok_per_s: (ok.len() as f64 / measured_window * 10.0).round() / 10.0,
        latency_ms: Latencies {
            all: lat(&|_| true),
            stream: lat(&|r| r.stream),
            non_stream: lat(&|r| !r.stream),
        },
        ttfb_ms: percentiles(ok.iter().filter(|r| r.stream).map(|r| r.ttfb_us).collect()),
        schedule_lag_ms: percentiles(
            measured
                .iter()
                .filter(|r| r.status != 0 || r.error_code.as_deref() != Some("client_saturated"))
                .map(|r| r.lag_us)
                .collect(),
        ),
        gateway_overhead_ms: None,
        per_target,
        gateway: None,
        upstream: None,
        database: None,
        reader: None,
        totals,
        violations: Vec::new(),
        timeline: Vec::new(),
    }
}

#[derive(serde::Deserialize)]
struct MockCall {
    nonce: Option<String>,
    status: u16,
    service_us: u64,
}

async fn upstream_check(
    client: &reqwest::Client,
    mock: &str,
    run_id: &str,
    records: &[RequestRecord],
) -> anyhow::Result<(UpstreamCheck, Percentiles)> {
    let base = mock.trim_end_matches('/');
    let calls: Vec<MockCall> = client
        .get(format!("{base}/__mock/calls"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("mock upstream calls")?
        .json()
        .await
        .context("mock upstream calls JSON")?;
    let stats: Value = client
        .get(format!("{base}/__mock/stats"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("mock upstream stats")?
        .json()
        .await?;
    let prefix = format!("{run_id}-");
    let mut by_nonce: HashMap<&str, Vec<&MockCall>> = HashMap::new();
    let mut check = UpstreamCheck {
        prompt_tokens: stats["config"]["prompt_tokens"].as_u64(),
        completion_tokens: stats["config"]["completion_tokens"].as_u64(),
        mock_config: Some(stats["config"].clone()),
        ..UpstreamCheck::default()
    };
    for call in &calls {
        if let Some(nonce) = call.nonce.as_deref().filter(|n| n.starts_with(&prefix)) {
            check.calls += 1;
            if call.status != 200 {
                check.upstream_errors += 1;
            }
            by_nonce.entry(nonce).or_default().push(call);
        }
    }
    let sent: HashMap<&str, &RequestRecord> =
        records.iter().map(|r| (r.nonce.as_str(), r)).collect();
    let mut overhead = Vec::new();
    for (nonce, calls) in &by_nonce {
        if calls.len() > 1 {
            check.duplicate_upstream_calls += calls.len() as u64 - 1;
        }
        match sent.get(nonce) {
            None => check.upstream_calls_without_client_request += calls.len() as u64,
            Some(r) if r.status == 429 => {
                check.upstream_calls_for_denied_requests += calls.len() as u64
            }
            _ => {}
        }
    }
    for r in records.iter().filter(|r| r.ok) {
        match by_nonce.get(r.nonce.as_str()) {
            Some(calls) if calls.len() == 1 => {
                check.ok_requests_matched += 1;
                if !r.warmup {
                    overhead.push(r.service_us.saturating_sub(calls[0].service_us));
                }
            }
            Some(_) => check.ok_requests_matched += 1,
            None => check.ok_requests_without_upstream_call += 1,
        }
    }
    Ok((check, percentiles(overhead)))
}

fn violations(report: &Report) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(u) = &report.upstream {
        for (name, n) in [
            (
                "ok requests without an upstream call",
                u.ok_requests_without_upstream_call,
            ),
            ("duplicate upstream calls", u.duplicate_upstream_calls),
            (
                "upstream calls for denied requests",
                u.upstream_calls_for_denied_requests,
            ),
            (
                "upstream calls without a client request",
                u.upstream_calls_without_client_request,
            ),
        ] {
            if n > 0 {
                v.push(format!("{name}: {n}"));
            }
        }
    }
    if let Some(db) = &report.database {
        v.extend(db.violations.iter().cloned());
    }
    v
}

impl Report {
    /// One human-readable line per headline number (stderr).
    pub fn headline(&self) -> String {
        let mut out = format!(
            "{} {}: offered {:.1}/s, ok {:.1}/s, {} sent, {} ok, statuses {:?}, codes {:?}\n  latency ms p50/p95/p99 {:.1}/{:.1}/{:.1} (stream {:.1}/{:.1}/{:.1}, ttfb p50 {:.1})",
            self.run_id,
            self.config.label.as_deref().unwrap_or(""),
            self.offered_rate,
            self.throughput_ok_per_s,
            self.totals.sent,
            self.totals.ok,
            self.totals.by_status,
            self.totals.by_error_code,
            self.latency_ms.all.p50_ms,
            self.latency_ms.all.p95_ms,
            self.latency_ms.all.p99_ms,
            self.latency_ms.stream.p50_ms,
            self.latency_ms.stream.p95_ms,
            self.latency_ms.stream.p99_ms,
            self.ttfb_ms.p50_ms,
        );
        if let Some(o) = &self.gateway_overhead_ms {
            out += &format!(
                "\n  gateway overhead ms p50/p95/p99 {:.1}/{:.1}/{:.1}",
                o.p50_ms, o.p95_ms, o.p99_ms
            );
        }
        if let Some(g) = &self.gateway {
            for (name, family) in [("admission", &g.admission), ("settlement", &g.settlement)] {
                for (series, s) in family {
                    if series.starts_with("phase=total") || series.starts_with("phase=locks") {
                        out += &format!(
                            "\n  {name} {series}: n={} p50/p95/p99 {:.2}/{:.2}/{:.2} ms",
                            s.count, s.p50_ms, s.p95_ms, s.p99_ms
                        );
                    }
                }
            }
        }
        if let Some(r) = &self.reader {
            out += &format!(
                "\n  readers {}: {} requests, {} ok ({:.1}/s), statuses {:?}, ok latency ms p50/p95/p99 {:.1}/{:.1}/{:.1}",
                r.readers,
                r.requests,
                r.ok,
                r.ok_per_s,
                r.by_status,
                r.latency.p50_ms,
                r.latency.p95_ms,
                r.latency.p99_ms
            );
        }
        if self.violations.is_empty() {
            out += "\n  invariants: ok";
        } else {
            out += &format!("\n  VIOLATIONS: {:?}", self.violations);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RunConfig {
        RunConfig {
            targets: vec!["http://a".into()],
            rate: 10.0,
            duration_s: 1.0,
            warmup_s: 0.0,
            stream_ratio: 0.5,
            key_seed: "s".into(),
            keys: 100,
            key_offset: 10,
            model: "m".into(),
            max_tokens: 8,
            timeout_s: 5.0,
            max_in_flight: 10,
            rng_seed: 7,
            mock_url: None,
            metrics_urls: vec![],
            database_url: None,
            label: None,
            readers: 0,
            reader_days: 7,
        }
    }

    #[test]
    fn plan_is_deterministic_and_spread() {
        let c = config();
        let plans: Vec<_> = (0..10_000).map(|i| plan(&c, i)).collect();
        assert_eq!(plans, (0..10_000).map(|i| plan(&c, i)).collect::<Vec<_>>());
        assert!(plans.iter().all(|(k, _)| (10..110).contains(k)));
        let streams = plans.iter().filter(|(_, s)| *s).count();
        assert!((4500..5500).contains(&streams), "{streams}");
        let distinct: std::collections::HashSet<_> = plans.iter().map(|(k, _)| k).collect();
        assert_eq!(distinct.len(), 100);
    }
}

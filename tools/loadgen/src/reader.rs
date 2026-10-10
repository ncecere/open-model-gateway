//! Concurrent management readers (`loadgen run --readers N`): closed-loop
//! report, usage, logs and `/me` reads alongside the inference load, as a
//! dashboard-heavy installation would issue them (scale plan P1).
//!
//! Each reader repeatedly GETs the next endpoint of a fixed cycle with the
//! seeded reader's browser session (user 1: a platform Auditor that owns
//! shared workspace 1 and personal workspace 1; see `seed.sql`) and
//! `/v1/models` with key 0. Latency is measured per endpoint. Readers stop
//! issuing new requests when the inference schedule ends; samples that start
//! during the warmup are excluded like inference requests.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    keys,
    stats::{Percentiles, percentiles},
};

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub name: &'static str,
    pub path: String,
    /// Bearer inference key instead of the browser session.
    pub bearer: bool,
}

/// `YYYY-MM-DD` of a day number since 1970-01-01 (proleptic Gregorian).
pub fn civil_date(days: i64) -> String {
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The endpoint cycle: platform cost report, usage overview and request log,
/// the owned shared workspace's usage overview and request log, `/me` pages
/// and `/v1/models`, over the last `days` UTC days (through tomorrow).
pub fn endpoints(seed: &str, days: u32) -> Vec<Endpoint> {
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| (d.as_secs() / 86_400) as i64);
    let (start, end) = (
        civil_date(today + 1 - i64::from(days.clamp(1, 93))),
        civil_date(today + 1),
    );
    let ws = keys::seed_id(seed, "shared:1");
    let range = format!("start_date={start}&end_date={end}");
    let e = |name, path: String| Endpoint {
        name,
        path,
        bearer: false,
    };
    vec![
        e(
            "platform_cost_report",
            format!("/api/v1/platform/cost-report?{range}"),
        ),
        e(
            "platform_usage_overview",
            format!("/api/v1/platform/usage/overview?{range}"),
        ),
        e(
            "platform_logs_requests",
            format!("/api/v1/platform/logs/requests?{range}&limit=50"),
        ),
        e(
            "workspace_usage_overview",
            format!("/api/v1/workspaces/{ws}/usage/overview?{range}"),
        ),
        e(
            "workspace_requests",
            format!("/api/v1/workspaces/{ws}/requests?{range}&limit=50"),
        ),
        e("me_summary", "/api/v1/me/summary".into()),
        e("me", "/api/v1/me".into()),
        Endpoint {
            name: "v1_models",
            path: "/v1/models".into(),
            bearer: true,
        },
    ]
}

#[derive(Debug, Default, Serialize)]
pub struct EndpointStats {
    pub requests: u64,
    pub ok: u64,
    pub by_status: BTreeMap<String, u64>,
    /// Successful responses only.
    pub latency: Percentiles,
}

#[derive(Debug, Default, Serialize)]
pub struct ReaderStats {
    pub readers: usize,
    pub days: u32,
    pub requests: u64,
    pub ok: u64,
    pub ok_per_s: f64,
    pub by_status: BTreeMap<String, u64>,
    /// Successful responses across every endpoint.
    pub latency: Percentiles,
    pub endpoints: BTreeMap<String, EndpointStats>,
}

pub struct Sample {
    pub endpoint: &'static str,
    /// HTTP status, 0 for a transport error or timeout.
    pub status: u16,
    pub latency_us: u64,
    pub started: Instant,
}

/// One reader: issue `cycle[offset..]` round-robin until `until`.
#[allow(clippy::too_many_arguments)]
pub async fn reader(
    client: reqwest::Client,
    targets: Vec<String>,
    cycle: Vec<Endpoint>,
    session: String,
    key: String,
    offset: usize,
    until: Instant,
    timeout: Duration,
) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut i = offset;
    while Instant::now() < until {
        let endpoint = &cycle[i % cycle.len()];
        let target = &targets[i % targets.len()];
        i += 1;
        let url = format!("{}{}", target.trim_end_matches('/'), endpoint.path);
        let request = client.get(&url);
        let request = if endpoint.bearer {
            request.bearer_auth(&key)
        } else {
            request.header(reqwest::header::COOKIE, format!("omg_session={session}"))
        };
        let started = Instant::now();
        let status = match tokio::time::timeout(timeout, async {
            let response = request.send().await?;
            let status = response.status().as_u16();
            response.bytes().await?;
            Ok::<_, reqwest::Error>(status)
        })
        .await
        {
            Ok(Ok(status)) => status,
            _ => 0,
        };
        out.push(Sample {
            endpoint: endpoint.name,
            status,
            latency_us: started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            started,
        });
    }
    out
}

pub fn summarize(
    readers: usize,
    days: u32,
    samples: &[Sample],
    measured_start: Instant,
    window: Duration,
) -> ReaderStats {
    let mut stats = ReaderStats {
        readers,
        days,
        ..ReaderStats::default()
    };
    let measured: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.started >= measured_start)
        .collect();
    let mut latencies: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for s in &measured {
        let e = stats.endpoints.entry(s.endpoint.to_owned()).or_default();
        e.requests += 1;
        *e.by_status.entry(s.status.to_string()).or_default() += 1;
        *stats.by_status.entry(s.status.to_string()).or_default() += 1;
        stats.requests += 1;
        if s.status == 200 {
            e.ok += 1;
            stats.ok += 1;
            latencies.entry(s.endpoint).or_default().push(s.latency_us);
        }
    }
    stats.latency = percentiles(latencies.values().flatten().copied().collect());
    for (name, values) in latencies {
        if let Some(e) = stats.endpoints.get_mut(name) {
            e.latency = percentiles(values);
        }
    }
    stats.ok_per_s = (stats.ok as f64 / window.as_secs_f64().max(1e-9) * 10.0).round() / 10.0;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
        assert_eq!(civil_date(20_735), "2026-10-09");
        assert_eq!(civil_date(-1), "1969-12-31");
    }

    #[test]
    fn cycle_covers_reports_usage_logs_and_me() {
        let cycle = endpoints("s1", 7);
        assert_eq!(cycle.len(), 8);
        assert!(cycle.iter().filter(|e| e.bearer).count() == 1);
        assert!(cycle[0].path.contains("start_date="));
    }

    #[test]
    fn summary_excludes_warmup_and_failures_from_latency() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(1);
        let s = |endpoint, status, ms: u64, started| Sample {
            endpoint,
            status,
            latency_us: ms * 1000,
            started,
        };
        let samples = vec![
            s("a", 200, 900, t0),
            s("a", 200, 10, later),
            s("a", 503, 10_000, later),
            s("b", 200, 30, later),
        ];
        let stats = summarize(2, 7, &samples, later, Duration::from_secs(10));
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.ok, 2);
        assert_eq!(stats.latency.max_ms, 30.0);
        assert_eq!(stats.endpoints["a"].by_status["503"], 1);
        assert_eq!(stats.ok_per_s, 0.2);
    }
}

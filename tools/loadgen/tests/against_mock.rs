//! The generator against the mock upstream standing in for a gateway: open-loop
//! pacing, round-robin targets, nonce matching and error accounting.
use std::sync::Arc;

use omg_loadgen::run::{RunConfig, run};
use omg_mock_upstream::{Config, Mock, serve};

async fn spawn(config: Config) -> (String, Arc<Mock>) {
    let mock = Mock::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = mock.clone();
    tokio::spawn(async move { serve(listener, served, std::future::pending()).await });
    (base, mock)
}

fn config(targets: Vec<String>, mock: &str) -> RunConfig {
    RunConfig {
        targets,
        rate: 200.0,
        duration_s: 1.0,
        warmup_s: 0.25,
        stream_ratio: 0.5,
        key_seed: "test".into(),
        keys: 10,
        key_offset: 0,
        model: "loadtest/chat".into(),
        max_tokens: 3,
        timeout_s: 10.0,
        max_in_flight: 1000,
        rng_seed: 1,
        mock_url: Some(mock.to_owned()),
        metrics_urls: vec![],
        database_url: None,
        label: Some("mock".into()),
    }
}

fn fast() -> Config {
    Config {
        latency_ms: 20,
        ttft_ms: 5,
        inter_token_ms: 2,
        completion_tokens: 4,
        ..Config::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_loop_round_robin_and_every_request_matches_one_upstream_call() {
    let (a, mock) = spawn(fast()).await;
    // A second "replica": the same mock under another URL form.
    let b = a.replace("127.0.0.1", "localhost");
    let report = run(config(vec![a.clone(), b], &a)).await.unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.totals.scheduled, 200);
    assert_eq!(report.totals.sent, 200);
    assert_eq!(report.totals.ok, 200);
    assert_eq!(report.totals.measured, 150);
    assert_eq!(report.per_target[0].sent, 75);
    assert_eq!(report.per_target[1].sent, 75);
    // Open loop: 200 requests at 200/s take about one second, not 200 x 20 ms.
    assert!(report.elapsed_s < 2.0, "{}", report.elapsed_s);
    assert!(report.offered_rate > 150.0, "{}", report.offered_rate);
    let upstream = report.upstream.as_ref().unwrap();
    assert_eq!(upstream.calls, 200);
    assert_eq!(upstream.ok_requests_matched, 200);
    assert_eq!(upstream.prompt_tokens, Some(12));
    let overhead = report.gateway_overhead_ms.as_ref().unwrap();
    assert_eq!(overhead.count, 150);
    // The mock is the "gateway" here, so the overhead is only client/HTTP time.
    assert!(overhead.p50_ms < report.latency_ms.all.p50_ms);
    assert!(report.latency_ms.non_stream.p50_ms >= 20.0);
    assert!(report.latency_ms.stream.count > 40 && report.latency_ms.non_stream.count > 40);
    assert!(report.ttfb_ms.p50_ms < report.latency_ms.stream.p50_ms);
    assert_eq!(mock.calls().len(), 200);
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["config"].get("key_seed").is_none());
    assert!(json["latency_ms"]["all"]["p99_ms"].is_number());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_errors_and_saturation_are_counted_not_hidden() {
    let (a, _) = spawn(Config {
        error_rate: 0.25,
        error_status: 500,
        ..fast()
    })
    .await;
    let report = run(config(vec![a.clone()], &a)).await.unwrap();
    let errors = report.totals.by_status.get("500").copied().unwrap_or(0);
    assert!(errors > 20 && errors < 90, "{errors}");
    assert_eq!(report.totals.ok + errors, 200);
    assert_eq!(
        report
            .totals
            .by_error_code
            .get("mock_injected_error")
            .copied(),
        Some(errors)
    );
    assert_eq!(report.upstream.as_ref().unwrap().upstream_errors, errors);
    assert!(report.violations.is_empty(), "{:?}", report.violations);

    // A slow target with a tiny in-flight cap: excess arrivals are recorded
    // as `client_saturated` instead of slowing the arrival rate.
    let (slow, _) = spawn(Config {
        latency_ms: 500,
        ..fast()
    })
    .await;
    let mut c = config(vec![slow.clone()], &slow);
    c.stream_ratio = 0.0;
    c.max_in_flight = 5;
    c.warmup_s = 0.0;
    let report = run(c).await.unwrap();
    let saturated = report.totals.by_error_code["client_saturated"];
    assert!(saturated > 150, "{saturated}");
    assert_eq!(report.totals.sent + saturated, 200);
}

#[tokio::test]
async fn bad_configurations_are_refused() {
    let mut c = config(vec!["http://127.0.0.1:9".into()], "http://127.0.0.1:9");
    c.rate = 0.0;
    assert!(run(c.clone()).await.is_err());
    c.rate = 10.0;
    c.warmup_s = 2.0;
    assert!(run(c.clone()).await.is_err());
    c.warmup_s = 0.0;
    c.targets.clear();
    assert!(run(c).await.is_err());
}

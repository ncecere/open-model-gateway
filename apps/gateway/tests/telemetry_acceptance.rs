#![cfg(feature = "integration-tests")]
//! #98 PR 3 acceptance: end-to-end collector acceptance for the bounded
//! OTLP trace exporter (PR 1, #259) and its interactive-generation spans
//! (PR 2, #266), against a REAL running gateway (`http::router_with_engine`
//! served on a real loopback TCP listener, exactly like
//! `tests/sdk_retries.rs`/`tests/runtime_privileges.rs`) and a local mock
//! OTLP collector (a real loopback HTTP server receiving `/v1/traces` JSON,
//! development mode only). No paid providers: a mock upstream adapter only.
//!
//! The span-set/parentage mechanics and the live eligibility decision are
//! already proven without a collector by `inference::telemetry_tests`
//! (fake repository) and `governance::telemetry_tests`
//! (real-PostgreSQL, no exporter). This file's job is the remaining gap:
//! what a real collector actually receives on the wire, and that the
//! collector's own health (closed, slow, or a refused destination) never
//! touches the inference hot path. The refused-destination startup case
//! (production mode, no collector involved) is `tests/startup.rs`'s
//! `a_refused_trace_collector_destination_fails_real_startup_without_echoing_it`.

use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use open_model_gateway::{
    bootstrap::{self, DevelopmentKeys},
    config::{Environment, TraceConfig},
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    metrics,
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
    telemetry,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

/// Always succeeds, non-streaming. Span-set/parentage, failover and
/// timeout mechanics are already covered without a collector elsewhere;
/// this fixture only needs to produce one clean, admitted, settled
/// request so the real exporter has real spans to send.
struct FixtureAdapter;
#[async_trait]
impl ProviderAdapter for FixtureAdapter {
    fn id(&self) -> &'static str {
        "telemetry_fixture"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    async fn execute(
        &self,
        target: &Deployment,
        _request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        assert_eq!(target.upstream_model, "fixture-upstream-id");
        Ok(ProviderOutput::Complete(ChatResponse {
            content: Some("hello".into()),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: Some(11),
                output_tokens: Some(13),
                ..Default::default()
            },
        }))
    }
}

/// One collector session: an axum server on a real loopback TCP listener
/// receiving `POST /v1/traces` JSON (development mode's plain HTTP OTLP
/// transport), capturing every parsed `resourceSpans` batch it received.
struct Collector {
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Collector {
    async fn start(reply_status: StatusCode, reply_body: &'static str, delay: Duration) -> Self {
        type Seen = Arc<Mutex<Vec<Value>>>;
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route(
                "/v1/traces",
                post(
                    move |State(seen): State<Seen>, Json(body): Json<Value>| async move {
                        seen.lock().unwrap().push(body);
                        tokio::time::sleep(delay).await;
                        (
                            reply_status,
                            [("content-type", "application/json")],
                            reply_body,
                        )
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, seen, task }
    }
    fn batches(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
    fn spans(&self) -> Vec<Value> {
        self.batches()
            .iter()
            .flat_map(|batch| {
                batch["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Seeds development keys and points the demo catalog's one model/
/// deployment at [`FixtureAdapter`], once per test database. Separate from
/// [`Gateway::for_trace_config`] so a test can rebuild the app/engine/
/// runtime against a different collector destination mid-test without
/// re-seeding (`bootstrap::seed` is a one-time operation per database).
async fn seed(pool: &PgPool) -> DevelopmentKeys {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='telemetry_fixture', enabled=true")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true, upstream_model='fixture-upstream-id'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,10000,10000,1 FROM deployments")
        .execute(pool).await.unwrap();
    keys
}

/// A real gateway and a real, separate metrics listener, both served on
/// loopback TCP, wired to the same optional trace `Recorder` the real
/// `serve` command builds (`Runtime::start` -> `Recorder` on both
/// `Store::with_traces` and `Engine::with_telemetry`, main.rs).
struct Gateway {
    app_addr: std::net::SocketAddr,
    metrics_addr: std::net::SocketAddr,
    runtime: Option<telemetry::Runtime>,
    _app: tokio::task::JoinHandle<()>,
    _metrics: tokio::task::JoinHandle<()>,
}
impl Drop for Gateway {
    // Stop the served app and metrics listener with the test, like Collector.
    fn drop(&mut self) {
        self._app.abort();
        self._metrics.abort();
    }
}
impl Gateway {
    async fn for_trace_config(pool: &PgPool, trace_url: Option<&str>, sample_ratio: f64) -> Self {
        let store = Store::new(pool.clone());
        let values = trace_url.map(|url| {
            [
                (TraceConfig::URL.to_owned(), url.to_owned()),
                (TraceConfig::RATIO.to_owned(), sample_ratio.to_string()),
            ]
        });
        let trace_config = TraceConfig::from_lookup(
            |key| {
                values
                    .as_ref()
                    .and_then(|pairs| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
            },
            Environment::Development,
        )
        .unwrap();
        let runtime = telemetry::Runtime::start(trace_config);
        let recorder = runtime.as_ref().map(telemetry::Runtime::recorder);

        let store = store.with_traces(recorder.clone());
        let mut registry = ProviderRegistry::default();
        registry.register(Arc::new(FixtureAdapter)).unwrap();
        let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default())
            .unwrap()
            .with_telemetry(recorder);

        let app = http::router_with_engine(store.clone(), None, engine);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_addr = listener.local_addr().unwrap();
        let app_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let metrics_app = metrics::router(store);
        let metrics_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metrics_addr = metrics_listener.local_addr().unwrap();
        let metrics_task = tokio::spawn(async move {
            let _ = axum::serve(metrics_listener, metrics_app).await;
        });

        Self {
            app_addr,
            metrics_addr,
            runtime,
            _app: app_task,
            _metrics: metrics_task,
        }
    }

    /// Sends one chat completion and returns (status, response JSON,
    /// `x-request-id` as a `Uuid` - the same id the engine uses as the
    /// request's `root_request_id`, and so the same id `telemetry::trace_id`
    /// and the Logs API's own `trace_id` field (`management::logs::with_trace_id`)
    /// both derive from for this one request).
    async fn chat(&self, key: &str) -> (StatusCode, Value, Uuid) {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{}/v1/chat/completions", self.app_addr))
            .header("authorization", format!("Bearer {key}"))
            .json(&json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],"stream":false}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let request_id = Uuid::parse_str(
            response
                .headers()
                .get("x-request-id")
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let body: Value = response.json().await.unwrap();
        (status, body, request_id)
    }

    /// The value of the OpenMetrics line matching both `name` and `label`
    /// (e.g. `label = "reason=\"delivery\""`), or 0 if the line is absent.
    ///
    /// The registry is process-wide (`metrics::METRICS`), shared by every test
    /// in this binary. Callers compare before/after deltas, which is sound only
    /// while no two concurrently running tests read the same label.
    async fn metric_value(&self, name: &str, label: &str) -> f64 {
        reqwest::Client::new()
            .get(format!("http://{}/metrics", self.metrics_addr))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .lines()
            .find(|line| line.starts_with(name) && line.contains(label))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    }

    async fn shutdown(mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown().await;
        }
    }
}

/// Requests per timing sample.
const SAMPLES: usize = 5;

/// Headroom a fault request gets over the same-run healthy baseline. The
/// hanging collector's whole exchange takes 2 s, so a request held up by an
/// export exchange overshoots this and fails, while an ordinary host stall
/// (scheduler pause, loaded CI machine) stays inside it.
const FAULT_HEADROOM: Duration = Duration::from_millis(1500);

/// The wall-clock time of each of [`SAMPLES`] consecutive chat requests, in
/// request order (each request must succeed). Every timing is kept: the
/// caller bounds each one, so a request that blocks only sometimes (the
/// first after the fault begins, or the one that flushes a batch) is not
/// hidden behind faster neighbours.
async fn timed_chats(gateway: &Gateway, key: &str) -> Vec<Duration> {
    let mut timings = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = std::time::Instant::now();
        let (status, body, _) = gateway.chat(key).await;
        let elapsed = started.elapsed();
        assert_eq!(status, StatusCode::OK, "{body}");
        timings.push(elapsed);
    }
    timings
}

/// Asserts that every request in `timings` finished within
/// [`FAULT_HEADROOM`] of `baseline`, the first one included, and prints all
/// timings on failure.
fn assert_no_request_slowed(fault: &str, timings: &[Duration], baseline: Duration) {
    let bound = baseline + FAULT_HEADROOM;
    for (index, elapsed) in timings.iter().enumerate() {
        assert!(
            *elapsed < bound,
            "{fault} must never slow an inference request: request {} of {} took {elapsed:?}, bound {bound:?} (same-run baseline {baseline:?} + {FAULT_HEADROOM:?}); all fault timings {timings:?}",
            index + 1,
            timings.len(),
        );
    }
}

/// The same-run reference for the outage timings below: the fastest of
/// [`SAMPLES`] requests against the same gateway wiring with tracing enabled,
/// a healthy collector and no fault. Whatever the host's load makes a
/// request cost right now, this measures it. The minimum is the right
/// reference here because an unrelated stall only ever inflates some
/// samples.
async fn healthy_baseline(pool: &PgPool, key: &str) -> Duration {
    let healthy = Collector::start(StatusCode::OK, "{}", Duration::ZERO).await;
    let gateway = Gateway::for_trace_config(pool, Some(&healthy.url), 1.0).await;
    let timings = timed_chats(&gateway, key).await;
    gateway.shutdown().await;
    timings.into_iter().min().expect("SAMPLES is non-zero")
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_shared_workspace_request_exports_the_expected_span_set_with_parentage_matching_the_logs_trace_id(
    pool: PgPool,
) {
    let keys = seed(&pool).await;
    let collector = Collector::start(StatusCode::OK, "{}", Duration::ZERO).await;
    let gateway = Gateway::for_trace_config(&pool, Some(&collector.url), 1.0).await;

    let (status, body, request_id) = gateway.chat(&keys.team_key.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The worker batches at most once per second; wait for real delivery
    // rather than racing it, bounded well under this test's own timeout.
    for _ in 0..50 {
        if !collector.spans().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    gateway.shutdown().await;

    let spans = collector.spans();
    assert!(!spans.is_empty(), "the collector received no spans at all");
    let names: std::collections::BTreeSet<&str> =
        spans.iter().map(|s| s["name"].as_str().unwrap()).collect();
    for required in [
        "inference.request",
        "routing",
        "admission",
        "upstream.attempt",
        "settlement",
    ] {
        assert!(
            names.contains(required),
            "missing span {required}: {names:?}"
        );
    }

    let expected_trace = hex::encode(telemetry::trace_id(request_id));
    // Every exported span shares this request's trace id (#98: derived
    // only from the server's own request id, never a client header) -
    // exactly the value the Logs API's `trace_id` field computes
    // (`management::logs::with_trace_id`, `telemetry::trace_id(root_request_id)`)
    // for this same shared-workspace row.
    for span in &spans {
        assert_eq!(span["traceId"], expected_trace, "{span}");
    }

    let root = spans
        .iter()
        .find(|s| s["name"] == "inference.request")
        .unwrap();
    assert!(
        root.get("parentSpanId").is_none(),
        "the root span must have no parent: {root}"
    );
    let root_id = root["spanId"].clone();
    for child_name in ["routing", "admission", "upstream.attempt", "settlement"] {
        let child = spans.iter().find(|s| s["name"] == child_name).unwrap();
        assert_eq!(
            child["parentSpanId"], root_id,
            "{child_name} must be a direct child of inference.request: {child}"
        );
    }
    // Admission's own sub-phase spans are children of the admission
    // wrapper span, not of the root directly.
    let admission_id = spans.iter().find(|s| s["name"] == "admission").unwrap()["spanId"].clone();
    let sub_phases: Vec<&Value> = spans
        .iter()
        .filter(|s| {
            s["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("admission."))
        })
        .collect();
    assert!(
        !sub_phases.is_empty(),
        "expected admission.* sub-phase spans: {spans:?}"
    );
    for phase in sub_phases {
        assert_eq!(phase["parentSpanId"], admission_id, "{phase}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_personal_workspace_request_exports_nothing(pool: PgPool) {
    let keys = seed(&pool).await;
    let collector = Collector::start(StatusCode::OK, "{}", Duration::ZERO).await;
    let gateway = Gateway::for_trace_config(&pool, Some(&collector.url), 1.0).await;

    let (status, body, _) = gateway.chat(&keys.personal_key.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A personal request's spans are dropped (`ineligible`) locally and
    // never reach the export queue, so there is nothing that could arrive
    // late; still wait out a full batching cycle before asserting.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    gateway.shutdown().await;

    assert!(
        collector.spans().is_empty(),
        "a personal-workspace request must export no spans: {:?}",
        collector.spans()
    );
}

/// A collector outage - first a closed port (nothing listening, refused
/// immediately), then a hanging collector (accepts the connection but
/// never replies within the exporter's 2 s whole-exchange timeout) - must
/// never fail or measurably slow the inference request, and every lost
/// span must still be counted (`gateway_trace_dropped_spans_total{reason="delivery"}`,
/// `gateway_trace_deliveries_total{outcome=...}`), never silently.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_collector_outage_never_fails_or_slows_inference_and_drops_are_counted(pool: PgPool) {
    let keys = seed(&pool).await;
    // Same run, same host load, no fault: what a request costs right now.
    let baseline = healthy_baseline(&pool, &keys.team_key.token).await;

    // Phase 1: closed port. Bind to get a free loopback port, then drop
    // the listener immediately so the address refuses every connection.
    let closed_addr = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let gateway =
        Gateway::for_trace_config(&pool, Some(&format!("http://{closed_addr}/v1/traces")), 1.0)
            .await;

    let dropped_before = gateway
        .metric_value("gateway_trace_dropped_spans_total", "reason=\"delivery\"")
        .await;
    let timings = timed_chats(&gateway, &keys.team_key.token).await;
    assert_no_request_slowed("a closed collector", &timings, baseline);

    let mut dropped_after = dropped_before;
    for _ in 0..50 {
        dropped_after = gateway
            .metric_value("gateway_trace_dropped_spans_total", "reason=\"delivery\"")
            .await;
        if dropped_after > dropped_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        dropped_after > dropped_before,
        "a closed-port export failure must be counted: before={dropped_before} after={dropped_after}"
    );
    let transport_deliveries = gateway
        .metric_value("gateway_trace_deliveries_total", "outcome=\"transport\"")
        .await;
    assert!(
        transport_deliveries >= 1.0,
        "a closed port must be counted as a transport delivery failure"
    );
    gateway.shutdown().await;

    // Phase 2: a hanging collector (accepts, never replies within 2 s).
    let hanging = Collector::start(StatusCode::OK, "{}", Duration::from_secs(5)).await;
    let gateway = Gateway::for_trace_config(&pool, Some(&hanging.url), 1.0).await;

    let timeout_deliveries_before = gateway
        .metric_value("gateway_trace_deliveries_total", "outcome=\"timeout\"")
        .await;
    let timings = timed_chats(&gateway, &keys.team_key.token).await;
    assert_no_request_slowed("a hanging collector", &timings, baseline);

    let mut timeout_deliveries_after = timeout_deliveries_before;
    // The exporter's own 2 s whole-exchange timeout plus one 1 s batch
    // tick: well under this loop's 5 s bound.
    for _ in 0..50 {
        timeout_deliveries_after = gateway
            .metric_value("gateway_trace_deliveries_total", "outcome=\"timeout\"")
            .await;
        if timeout_deliveries_after > timeout_deliveries_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        timeout_deliveries_after > timeout_deliveries_before,
        "a hanging collector must time out and be counted, not hang forever: before={timeout_deliveries_before} after={timeout_deliveries_after}"
    );
    gateway.shutdown().await;
}

/// Opt-in, truly external acceptance: same harness as the mock-collector
/// tests above, but `GATEWAY_TRACES_OTLP_URL` points at a real collector
/// outside this test binary (e.g. Grafana Tempo's OTLP ingest, port-forwarded
/// to loopback - docs/operations.md's "Live check against Tempo") instead of
/// the in-process [`Collector`]. Never runs in CI: `#[ignore]`, and it skips
/// cleanly (no failure) when `OMG_TRACE_LIVE_COLLECTOR_URL` is unset, the
/// same opt-in shape as `tests/openrouter_live.rs`/`tests/images_live.rs`.
/// Development mode's exporter only ever allows plain HTTP on loopback
/// (`GATEWAY_TRACES_OTLP_URL`'s own startup validation, `config.rs`), so any
/// non-loopback or non-HTTP value here is a hard, explicit refusal rather
/// than a silent upgrade or a confusing downstream failure. Prints exactly
/// one `LIVE trace_id=<32 hex>` line - the id an operator pastes into
/// Tempo - and nothing else sensitive (no tokens, keys or collector
/// response bodies).
#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "live external collector: set OMG_TRACE_LIVE_COLLECTOR_URL to a loopback http://127.0.0.1:<port>/v1/traces URL"]
async fn shared_workspace_trace_reaches_an_external_collector(pool: PgPool) {
    let Ok(url) = std::env::var("OMG_TRACE_LIVE_COLLECTOR_URL") else {
        eprintln!(
            "SKIP shared_workspace_trace_reaches_an_external_collector: OMG_TRACE_LIVE_COLLECTOR_URL is unset"
        );
        return;
    };
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with("/v1/traces"),
        "OMG_TRACE_LIVE_COLLECTOR_URL must be a loopback http://127.0.0.1:<port>/v1/traces URL - \
         development mode only allows plain HTTP on loopback for the trace exporter; got {url:?}"
    );

    let keys = seed(&pool).await;
    let gateway = Gateway::for_trace_config(&pool, Some(&url), 1.0).await;

    let accepted_before = gateway
        .metric_value("gateway_trace_deliveries_total", "outcome=\"accepted\"")
        .await;

    let (status, body, request_id) = gateway.chat(&keys.team_key.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The worker batches at most once per second; wait for real delivery
    // rather than racing it (~3s), bounded well under this test's own
    // timeout.
    let mut accepted_after = accepted_before;
    for _ in 0..30 {
        accepted_after = gateway
            .metric_value("gateway_trace_deliveries_total", "outcome=\"accepted\"")
            .await;
        if accepted_after >= accepted_before + 1.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    gateway.shutdown().await;

    assert!(
        accepted_after >= accepted_before + 1.0,
        "the external collector never accepted the exchange: before={accepted_before} after={accepted_after}"
    );

    eprintln!(
        "LIVE trace_id={}",
        hex::encode(telemetry::trace_id(request_id))
    );
}

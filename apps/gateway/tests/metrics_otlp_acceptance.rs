#![cfg(feature = "integration-tests")]
// Deliberately discard errors that might embed credentials or connection URLs.
#![allow(clippy::ok_expect)]
//! #99 slice C: the actual `serve` process, a fresh SQLx
//! enterprise database and a local HTTP collector. No inference/provider calls.
//!
//! The parent must explicitly select a DISPOSABLE PostgreSQL cluster through
//! DATABASE_URL; an explicit guard runs BEFORE SQLx's dotenv-capable setup.
//! SQLx creates/migrates a fresh database, never migrates the selected base
//! database (only its standard _sqlx_test bookkeeping). Children use only that
//! fresh pool's connection options (including its authentication/TLS profile),
//! not ambient PG* settings. As in
//! telemetry_acceptance.rs this uses the SQLx fixture owner, NOT runtime-role
//! acceptance: a parent-owned external harness must provision/login a dedicated
//! runtime role and apply reviewed grants on a disposable cluster. This test
//! never creates/alters roles, changes passwords or applies cluster-wide ACLs.
//!
//! startup.rs supplies the clean-environment subprocess pattern;
//! telemetry_acceptance.rs supplies the fresh-DB/local-collector pattern.
//! Transport hardening (#313) is a separate prerequisite, not copied here.

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, Uri},
};
use open_model_gateway::{
    bootstrap,
    config::{Environment, MetricsExportConfig},
    store::Store,
};
use serde_json::Value;
use sqlx::{ConnectOptions, PgPool, postgres::PgConnectOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, watch},
    task::JoinHandle,
};
use uuid::Uuid;

const BINARY: &str = env!("CARGO_BIN_EXE_open-model-gateway");
const BODY_LIMIT: usize = 1024 * 1024;
// Each served request logs one line; fault tests poll health while waiting.
const OUTPUT_LIMIT: usize = 256 * 1024;
const HEADER_SECRET: &str = "metrics-header-canary-99c";
const HEADER_NAME: &str = "x-metrics-canary-99c";
const SERVER_CANARY: &str = "collector-error-text-canary-99c";
const REQUEST_CANARY: &str = "request-header-canary-99c";
const BODY_CANARY: &str = "request-body-canary-99c";
const OTEL_CANARY: &str = "ignored-otel-canary-99c";

async fn loopback_listener() -> tokio::net::TcpListener {
    for _ in 0..16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind disposable loopback listener");
        if ![54339, 54349, 54369].contains(&listener.local_addr().unwrap().port()) {
            return listener;
        }
    }
    panic!("could not allocate a non-reserved fixture port");
}

struct Receipt {
    method: Method,
    uri: Uri,
    json_content_type: bool,
    authenticated: bool,
    raw: Bytes,
    arrived: tokio::time::Instant,
}

#[derive(Clone)]
struct CollectorState {
    receipts: mpsc::Sender<Receipt>,
    reply_allowed: watch::Receiver<bool>,
    overflow: Arc<AtomicBool>,
    failure: bool,
}

async fn collect(
    State(mut state): State<CollectorState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
) -> (StatusCode, [(&'static str, &'static str); 1], &'static str) {
    let receipt = Receipt {
        method,
        uri,
        json_content_type: headers
            .get("content-type")
            .is_some_and(|v| v == "application/json"),
        authenticated: headers
            .get("authorization")
            .is_some_and(|v| v == HEADER_SECRET)
            && headers.get(HEADER_NAME).is_some_and(|v| v == HEADER_SECRET),
        raw,
        arrived: tokio::time::Instant::now(),
    };
    if state.receipts.try_send(receipt).is_err() {
        state.overflow.store(true, Ordering::SeqCst);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            "{}",
        );
    }
    // Even an attempted collector health probe gets no successful response
    // until HTTP readiness has been proved. Bounded on failure/panic too.
    let allowed = tokio::time::timeout(
        Duration::from_secs(15),
        state.reply_allowed.wait_for(|allowed| *allowed),
    )
    .await
    .is_ok_and(|result| result.is_ok());
    (
        if state.failure {
            StatusCode::INTERNAL_SERVER_ERROR
        } else if allowed {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        [("content-type", "application/json")],
        if state.failure { SERVER_CANARY } else { "{}" },
    )
}

struct Collector {
    url: String,
    receipts: mpsc::Receiver<Receipt>,
    reply_allowed: watch::Sender<bool>,
    overflow: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl Collector {
    async fn start() -> Self {
        Self::with_failure(false).await
    }
    async fn with_failure(failure: bool) -> Self {
        let listener = loopback_listener().await;
        let url = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
        let (tx, receipts) = mpsc::channel(4);
        let (reply_allowed, rx) = watch::channel(false);
        let overflow = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            // Capture ALL methods/paths, so startup health probes cannot hide.
            .fallback(collect)
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .with_state(CollectorState {
                receipts: tx,
                reply_allowed: rx,
                overflow: overflow.clone(),
                failure,
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock collector serve");
        });
        Self {
            url,
            receipts,
            reply_allowed,
            overflow,
            task,
        }
    }
    async fn next(&mut self) -> Receipt {
        tokio::time::timeout(Duration::from_secs(17), self.receipts.recv())
            .await
            .expect("no periodic OTLP metrics receipt within one 10s interval plus headroom")
            .expect("mock collector stopped")
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Output = Arc<Mutex<Vec<u8>>>;

fn drain(mut pipe: impl AsyncRead + Unpin + Send + 'static) -> (Output, JoinHandle<()>) {
    let output: Output = Arc::default();
    let saved = output.clone();
    let task = tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let mut bytes = saved.lock().unwrap();
            let keep = n.min(OUTPUT_LIMIT.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..keep]);
            // Continue draining after the cap: no pipe backpressure or disk logs.
        }
    });
    (output, task)
}

struct Gateway {
    child: Child,
    addr: SocketAddr,
    scrape: Option<SocketAddr>,
    outputs: Vec<Output>,
    drains: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
impl Gateway {
    async fn spawn(database_url: &str, collector_url: &str, enabled: bool) -> Self {
        Self::spawn_with_scrape(database_url, collector_url, enabled, false).await
    }
    async fn spawn_with_scrape(
        database_url: &str,
        collector_url: &str,
        enabled: bool,
        scrape: bool,
    ) -> Self {
        let scrape_listener = if scrape {
            Some(loopback_listener().await)
        } else {
            None
        };
        let scrape = scrape_listener.as_ref().map(|l| l.local_addr().unwrap());
        let dir = tempfile::tempdir().expect("isolated subprocess directory");
        let empty_env = dir.path().join("empty.env");
        std::fs::write(&empty_env, "").expect("empty environment file");
        // The binary has no socket-activation API. Reserve an ephemeral address
        // until immediately before spawning, like the existing startup fixtures.
        let listener = loopback_listener().await;
        let addr = listener.local_addr().unwrap();
        let mut command = Command::new(BINARY);
        command
            .arg("serve")
            .env_clear()
            .current_dir(dir.path())
            .env("GATEWAY_ENV_FILE", empty_env)
            .env("GATEWAY_ENV", "development")
            .env("DATABASE_URL", database_url)
            .env("GATEWAY_LISTEN", addr.to_string())
            .env("GATEWAY_DATABASE_MAX_CONNECTIONS", "4")
            .env("TOKIO_WORKER_THREADS", "2")
            // These must NOT opt either process into any OTLP signal.
            .env("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", collector_url)
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", collector_url)
            .env("OTEL_EXPORTER_OTLP_HEADERS", OTEL_CANARY)
            .env("OTEL_RESOURCE_ATTRIBUTES", format!("user.id={OTEL_CANARY}"))
            .env("OTEL_SERVICE_NAME", OTEL_CANARY)
            .env(
                "METRICS_ACCEPTANCE_HEADERS",
                format!("Authorization={HEADER_SECRET},{HEADER_NAME}={HEADER_SECRET}"),
            )
            // Scrape is opt-in below, only for fault-isolation tests.
            // No traces URL or Traffic setting.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if enabled {
            command
                .env(MetricsExportConfig::URL, collector_url)
                .env(MetricsExportConfig::INTERVAL, "10")
                .env(MetricsExportConfig::HEADERS, "METRICS_ACCEPTANCE_HEADERS");
        }
        if let Some(scrape) = scrape {
            command.env("GATEWAY_METRICS_ADDR", scrape.to_string());
        }
        drop(scrape_listener);
        drop(listener);
        let mut child = command.spawn().expect("spawn gateway binary");
        let (stdout, stdout_task) = drain(child.stdout.take().unwrap());
        let (stderr, stderr_task) = drain(child.stderr.take().unwrap());
        Self {
            child,
            addr,
            scrape,
            outputs: vec![stdout, stderr],
            drains: vec![stdout_task, stderr_task],
            _dir: dir,
        }
    }

    async fn ready(&mut self, label: &str) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let ready = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if self.child.try_wait().expect("check child status").is_some() {
                    for task in &mut self.drains {
                        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
                    }
                    let refused = self.outputs.iter().any(|output| {
                        String::from_utf8_lossy(&output.lock().unwrap())
                            .contains("metrics export is not implemented in this release")
                    });
                    assert!(!refused,
                        "{label}: valid GATEWAY_METRICS_OTLP_URL still explicitly refuses startup; metrics runtime is not wired");
                    panic!("{label}: gateway exited before readiness (bounded output withheld)");
                }
                if let Ok(response) = client
                    .get(format!("http://{}/health/ready", self.addr))
                    .header("x-request-id", "9394f414-f91f-48d4-ab04-8f6448d5884d")
                    .header("x-privacy-canary", REQUEST_CANARY)
                    .body(BODY_CANARY)
                    .send()
                    .await
                    && response.status() == StatusCode::OK
                    && let Ok(body) = response.json::<Value>().await
                    && body["status"] == "ready"
                    && body["checks"]["database"] == "ok"
                    && body["checks"]["schema"] == "ok"
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await;
        assert!(
            ready.is_ok(),
            "{label}: gateway did not become ready while collector acknowledgements were withheld"
        );
    }

    async fn stop(&mut self) {
        tokio::time::timeout(Duration::from_secs(3), self.child.kill())
            .await
            .expect("bounded child cleanup")
            .expect("kill and reap child");
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        // Child::kill_on_drop is the panic/timeout safety net.
        for task in &self.drains {
            task.abort();
        }
    }
}

// Raw TCP deliberately sends no HTTP headers or body. Observe EOF/reset as
// well as accepts: process exit alone would not prove cancellation of sockets.
struct HangingCollector {
    url: String,
    connections: watch::Receiver<(usize, usize)>, // opened, closed
    task: JoinHandle<()>,
}
impl HangingCollector {
    async fn start() -> Self {
        let listener = loopback_listener().await;
        let url = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
        let (counts, connections) = watch::channel((0, 0));
        let task = tokio::spawn(async move {
            let mut sockets = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        counts.send_modify(|n| n.0 += 1);
                        let counts = counts.clone();
                        sockets.spawn(async move {
                            let mut bytes = [0u8; 4096];
                            let mut total = 0;
                            while let Ok(n) = stream.read(&mut bytes).await {
                                if n == 0 { break; }
                                total += n;
                                assert!(total <= BODY_LIMIT + 16 * 1024, "bounded HTTP request");
                            }
                            counts.send_modify(|n| n.1 += 1);
                        });
                    }
                    result = sockets.join_next(), if !sockets.is_empty() => {
                        result.unwrap().expect("hanging collector socket task");
                    }
                }
            }
        });
        Self {
            url,
            connections,
            task,
        }
    }
    async fn opened(&mut self, expected: usize) {
        tokio::time::timeout(
            Duration::from_secs(17),
            self.connections.wait_for(|n| n.0 >= expected),
        )
        .await
        .expect("expected hanging collector connection")
        .unwrap();
    }
    async fn all_closed(&mut self, expected: usize) {
        tokio::time::timeout(
            Duration::from_secs(1),
            self.connections.wait_for(|n| n.0 >= expected && n.0 == n.1),
        )
        .await
        .expect("gateway left detached collector sockets")
        .unwrap();
    }
}
impl Drop for HangingCollector {
    fn drop(&mut self) {
        self.task.abort();
    } // JoinSet also aborts every socket task.
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap()
}

impl Gateway {
    async fn assert_serving(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none());
        let client = http_client();
        for (path, status) in [("/health/ready", "ready"), ("/health/live", "ok")] {
            let response = client
                .get(format!("http://{}{path}", self.addr))
                .send()
                .await
                .expect("collector outage must not block HTTP");
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["status"], status);
        }
    }

    async fn wait_for_pushes(&mut self, expected_outcome: &str, count: u64) {
        let client = http_client();
        let scrape = self.scrape.expect("fault-isolation-only scrape listener");
        tokio::time::timeout(Duration::from_secs(32), async {
            loop {
                self.assert_serving().await;
                let response = client
                    .get(format!("http://{scrape}/metrics"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let text = response.text().await.unwrap();
                let mut seen = BTreeMap::new();
                for outcome in [
                    "accepted",
                    "http_status",
                    "transport",
                    "timeout",
                    "acknowledgement",
                ] {
                    let prefix =
                        format!("gateway_otlp_metrics_pushes_total{{outcome=\"{outcome}\"}} ");
                    let value = text
                        .lines()
                        .find_map(|line| line.strip_prefix(&prefix))
                        .expect("precreated fixed outcome series")
                        .parse::<u64>()
                        .unwrap();
                    if outcome != expected_outcome {
                        assert_eq!(value, 0, "unexpected exporter outcome");
                    }
                    seen.insert(outcome, value);
                }
                if seen[expected_outcome] >= count {
                    break;
                }
                // Pushes are 10s apart; frequent polling would only add request logs.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("periodic failure counter did not advance");
    }

    #[cfg(unix)]
    async fn terminate(&mut self) {
        let started = std::time::Instant::now();
        let pid = self.child.id().expect("running gateway").to_string();
        let signalled = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .unwrap();
        assert!(signalled.success());
        let status = tokio::time::timeout(Duration::from_secs(6), self.child.wait())
            .await
            .expect("SIGTERM exceeded 3s drain plus headroom")
            .expect("reap gateway");
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(
            status.success(),
            "SIGTERM must exit cleanly, not via forced kill"
        );
        for task in &mut self.drains {
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    fn assert_fixed_warning(&self, outcome: &str, collector_url: &str) {
        for output in &self.outputs {
            let bytes = output.lock().unwrap();
            assert!(
                bytes.len() < OUTPUT_LIMIT,
                "cannot verify truncated diagnostics"
            );
            let text = String::from_utf8_lossy(&bytes);
            for canary in [
                collector_url,
                collector_url.strip_suffix("/v1/metrics").unwrap(),
                HEADER_SECRET,
                HEADER_NAME,
                "METRICS_ACCEPTANCE_HEADERS",
                SERVER_CANARY,
            ] {
                assert!(
                    !text
                        .to_ascii_lowercase()
                        .contains(&canary.to_ascii_lowercase()),
                    "collector diagnostic leaked private material (withheld)"
                );
            }
            assert!(!text.to_ascii_lowercase().contains("authorization"));
        }
        let stderr = self.outputs[1].lock().unwrap();
        let events: Vec<Value> = String::from_utf8_lossy(&stderr)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("structured stderr"))
            .filter(|event| event["target"] == "open_model_gateway::metrics_export")
            .collect();
        assert_eq!(
            events.len(),
            1,
            "one outage transition, not one warning per failed push"
        );
        assert_eq!(events[0]["level"], "WARN");
        assert_eq!(
            events[0]["fields"],
            serde_json::json!({"message":"metrics exporter state changed", "outcome":outcome})
        );
    }
}

fn fresh_child_url(pool: &PgPool) -> String {
    let base: PgConnectOptions = std::env::var("DATABASE_URL")
        .ok()
        .unwrap()
        .parse()
        .ok()
        .unwrap();
    let options = pool.connect_options();
    assert!(options.get_database().is_some() && options.get_database() != base.get_database());
    let mut url = options.to_url_lossy();
    if let Some(value) = options.get_options() {
        url.query_pairs_mut().append_pair("options", value);
    }
    if let Some(value) = options.get_application_name() {
        url.query_pairs_mut().append_pair("application_name", value);
    }
    url.to_string()
}

async fn seed_canaries(pool: &PgPool) -> Vec<String> {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .ok()
        .flatten()
        .expect("seed fresh enterprise fixture");
    let mut canaries = vec![
        keys.installation_id.to_string(),
        keys.personal_workspace_id.to_string(),
        keys.team_workspace_id.to_string(),
        keys.personal_key.token,
        keys.team_key.token,
        "installation-canary-99c".into(),
        "user-canary-99c@example.invalid".into(),
        "workspace-canary-99c".into(),
        "model-canary-99c".into(),
        "provider-canary-99c".into(),
        "http://127.0.0.1:1/url-canary-99c".into(),
        REQUEST_CANARY.into(),
        BODY_CANARY.into(),
        HEADER_SECRET.into(),
        OTEL_CANARY.into(),
    ];
    for statement in [
        "UPDATE installation SET name='installation-canary-99c'",
        "UPDATE users SET email='user-canary-99c@example.invalid'",
        "UPDATE workspaces SET name='workspace-canary-99c'",
        "UPDATE models SET public_name='model-canary-99c'",
        "UPDATE deployments SET upstream_model='model-canary-99c'",
        "UPDATE provider_connections SET name='provider-canary-99c', provider='provider-canary-99c', endpoint='http://127.0.0.1:1/url-canary-99c'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .ok()
            .expect("seed privacy canary");
    }
    let user: Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(pool)
        .await
        .ok()
        .expect("fixture user");
    canaries.push(user.to_string());
    canaries
}

fn no_uuid(text: &str) {
    // Check substrings, not just whole values: prefixed/braced/compact UUIDs
    // are identities too. The sole exception is removed at its EXACT JSON path.
    for length in [32, 36] {
        assert!(
            !text.as_bytes().windows(length).any(|part| {
                std::str::from_utf8(part).is_ok_and(|s| Uuid::parse_str(s).is_ok())
            }),
            "UUID-shaped identity outside service.instance.id"
        );
    }
}
fn no_uuid_anywhere(value: &Value) {
    match value {
        Value::String(text) => no_uuid(text),
        Value::Array(values) => values.iter().for_each(no_uuid_anywhere),
        Value::Object(values) => {
            for (key, value) in values {
                no_uuid(key);
                no_uuid_anywhere(value);
            }
        }
        _ => {}
    }
}

fn attributes(value: &Value) -> BTreeMap<&str, &str> {
    let mut attributes = BTreeMap::new();
    for attribute in value.as_array().expect("OTLP attributes array") {
        let key = attribute["key"].as_str().expect("attribute key");
        let value = attribute["value"]
            .as_object()
            .expect("typed attribute value");
        assert_eq!(value.len(), 1, "only string attributes are permitted");
        let text = value["stringValue"].as_str().expect("string attribute");
        assert!(
            attributes.insert(key, text).is_none(),
            "duplicate attribute key"
        );
    }
    attributes
}

struct Identity {
    instance: Uuid,
    start: u64,
    time: u64,
    accepted: u64,
    arrived: tokio::time::Instant,
}

fn verify_receipt(receipt: Receipt, canaries: &[String], process_started_after: u64) -> Identity {
    assert_eq!(receipt.method, Method::POST);
    assert_eq!(receipt.uri.path(), "/v1/metrics");
    assert!(receipt.uri.query().is_none());
    assert!(receipt.json_content_type, "metrics must be OTLP/HTTP JSON");
    assert!(
        receipt.authenticated,
        "dedicated collector header must reach transport"
    );
    assert!(!receipt.raw.is_empty() && receipt.raw.len() <= BODY_LIMIT);
    let raw = std::str::from_utf8(&receipt.raw).expect("UTF-8 JSON");
    for canary in canaries {
        assert!(
            !raw.contains(canary),
            "privacy canary leaked into raw metrics body (value withheld)"
        );
    }
    let mut body: Value = serde_json::from_slice(&receipt.raw).expect("raw OTLP JSON");
    // Also catch JSON-escaped canaries rather than only literal wire strings.
    let decoded = body.to_string();
    for canary in canaries {
        assert!(
            !decoded.contains(canary),
            "encoded privacy canary leaked (value withheld)"
        );
    }
    let resources = body["resourceMetrics"].as_array().expect("resourceMetrics");
    assert_eq!(resources.len(), 1);
    let resource = &resources[0];
    let attrs = attributes(&resource["resource"]["attributes"]);
    assert_eq!(
        attrs.len(),
        4,
        "no installation/user/host resource attributes"
    );
    assert_eq!(attrs["service.name"], "open-model-gateway");
    assert_eq!(attrs["service.version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(attrs["deployment.environment.name"], "development");
    let instance = Uuid::parse_str(attrs["service.instance.id"]).expect("instance UUID");
    assert_eq!(instance.get_version_num(), 4);
    assert_eq!(instance.get_variant(), uuid::Variant::RFC4122);
    let mut raw_uuids = Vec::new();
    for width in [32, 36] {
        for part in raw.as_bytes().windows(width) {
            if let Ok(text) = std::str::from_utf8(part)
                && let Ok(id) = Uuid::parse_str(text)
            {
                raw_uuids.push(id);
            }
        }
    }
    assert!(
        raw_uuids == [instance],
        "exactly one UUID on the wire, at the checked resource attribute; no blanket UUID exemption"
    );
    let scopes = resource["scopeMetrics"].as_array().expect("scopeMetrics");
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0]["scope"]["name"], "omg.metrics");
    assert_eq!(scopes[0]["scope"]["version"], env!("CARGO_PKG_VERSION"));

    let golden: BTreeMap<_, _> = include_str!("../src/metrics_families.golden")
        .lines()
        .map(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            (columns[0], (columns[1], columns[2], columns[3]))
        })
        .collect();
    let metrics = scopes[0]["metrics"].as_array().expect("metrics array");
    assert!(!metrics.is_empty(), "not an empty envelope/trace payload");
    let mut names = BTreeSet::new();
    let mut start = None;
    let mut time = None;
    let mut accepted = None;
    for metric in metrics {
        let name = metric["name"].as_str().expect("metric name");
        assert!(names.insert(name), "duplicate metric family");
        let (kind, labels, class) = golden.get(name).expect("family must be in reviewed golden");
        assert_eq!(
            *class, "System",
            "Traffic is OFF by default; Excluded stays excluded"
        );
        assert!(
            metric.get("description").is_none(),
            "no free-form descriptions"
        );
        let field = match *kind {
            "counter" => "sum",
            "gauge" => "gauge",
            _ => panic!("unexpected System metric kind"),
        };
        let data = &metric[field];
        if *kind == "counter" {
            assert_eq!(data["aggregationTemporality"], 2);
            assert_eq!(data["isMonotonic"], true);
        }
        let points = data["dataPoints"].as_array().expect("data points");
        assert!(!points.is_empty());
        for point in points {
            let attrs = attributes(&point["attributes"]);
            let keys = attrs.keys().copied().collect::<Vec<_>>().join(",");
            assert_eq!(
                if keys.is_empty() { "-" } else { &keys },
                *labels,
                "reviewed label keys only"
            );
            for (key, value) in &attrs {
                assert!(value.chars().count() <= 96 && !value.chars().any(char::is_control));
                if *key == "path" {
                    assert_eq!(name, "gateway_lock_deadlock_retries");
                    assert!(["admission", "settlement", "reconciliation"].contains(value));
                }
            }
            let point_start = point["startTimeUnixNano"]
                .as_str()
                .expect("uint64 start string")
                .parse::<u64>()
                .unwrap();
            let point_time = point["timeUnixNano"]
                .as_str()
                .expect("uint64 time string")
                .parse::<u64>()
                .unwrap();
            assert!(point_start >= process_started_after && point_start <= point_time);
            assert_eq!(
                *start.get_or_insert(point_start),
                point_start,
                "one process start for all series"
            );
            assert_eq!(
                *time.get_or_insert(point_time),
                point_time,
                "one snapshot time"
            );
            if *kind == "counter" {
                let n = point["asInt"]
                    .as_str()
                    .expect("counter int64 decimal string")
                    .parse::<u64>()
                    .unwrap();
                if name == "gateway_otlp_metrics_pushes"
                    && attrs.get("outcome") == Some(&"accepted")
                {
                    accepted = Some(n);
                }
            }
            if name == "gateway_db_pool_max_connections" {
                assert_eq!(
                    point["asInt"], "4",
                    "must refresh using the actual serve pool, without a scrape listener"
                );
            }
        }
    }
    for required in [
        "gateway_db_pool_connections",
        "gateway_db_pool_max_connections",
        "gateway_otlp_metrics_pushes",
        "gateway_otlp_metrics_dropped",
        "gateway_otlp_metrics_rejected_points",
    ] {
        assert!(names.contains(required), "missing required system family");
    }
    let identity = Identity {
        instance,
        start: start.unwrap(),
        time: time.unwrap(),
        accepted: accepted.expect("precreated accepted outcome"),
        arrived: receipt.arrived,
    };
    // Exempt ONE resource value, never every string equal to the UUID and
    // never service.instance.id at another resource/scope/point path.
    let attrs = body["resourceMetrics"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap();
    let instance_attr = attrs
        .iter_mut()
        .find(|a| a["key"] == "service.instance.id")
        .unwrap();
    instance_attr["value"]["stringValue"] = Value::String("uuid-exempted-here-only".into());
    no_uuid_anywhere(&body);
    identity
}

macro_rules! database_checkpoint {
    ($name:ident, $checkpoint:ident) => {
        #[test]
        fn $name() {
            // The ordinary #[sqlx::test(pool)] setup reads dotenv BEFORE entering the
            // test body. Use exactly its public runner, but refuse an absent explicit
            // selection before it can create a DB on an implicitly discovered cluster.
            let selected = std::env::var("DATABASE_URL")
                .ok()
                .expect("explicit disposable-cluster DATABASE_URL required; no dotenv fallback");
            let url = reqwest::Url::parse(&selected)
                .ok()
                .expect("explicit PostgreSQL URL");
            assert!(
                !url.path().trim_start_matches('/').is_empty()
                    || url
                        .query_pairs()
                        .any(|(key, value)| key == "dbname" && !value.is_empty()),
                "DATABASE_URL must explicitly name the disposable base database"
            );
            let _: PgConnectOptions = selected
                .parse()
                .ok()
                .expect("valid explicit PostgreSQL connection options");
            static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./enterprise_migrations");
            let mut args =
                sqlx::testing::TestArgs::new(concat!(module_path!(), "::", stringify!($name)));
            args.migrator(&MIGRATOR);
            let checkpoint: fn(PgPool) -> _ = $checkpoint;
            sqlx::testing::TestFn::run_test(checkpoint, args);
        }
    };
}

database_checkpoint!(
    serve_exports_periodic_system_metrics_only_when_configured_without_a_scrape_listener,
    startup_and_periodic_checkpoint
);

#[cfg(unix)]
database_checkpoint!(
    serve_isolates_hanging_metrics_collector_and_bounds_sigterm,
    hanging_checkpoint
);
#[cfg(unix)]
database_checkpoint!(
    serve_isolates_metrics_collector_http_500,
    http_failure_checkpoint
);
#[cfg(unix)]
database_checkpoint!(
    serve_isolates_metrics_collector_connection_failure,
    refused_checkpoint
);
#[cfg(unix)]
database_checkpoint!(
    serve_shutdown_sends_exactly_one_final_metrics_snapshot,
    final_snapshot_checkpoint
);

#[cfg(unix)]
async fn hanging_checkpoint(pool: PgPool) {
    tokio::time::timeout(Duration::from_secs(55), async {
        seed_canaries(&pool).await;
        let database_url = fresh_child_url(&pool);
        let mut collector = HangingCollector::start().await;
        let mut gateway =
            Gateway::spawn_with_scrape(&database_url, &collector.url, true, true).await;
        gateway.ready("hanging collector").await;
        gateway.wait_for_pushes("timeout", 2).await;
        // Stop during the THIRD exchange, not in its idle interval. Shutdown
        // must cancel it and the fresh final exchange within ONE 3s budget.
        collector.opened(3).await;
        gateway.assert_serving().await;
        gateway.terminate().await;
        collector.all_closed(4).await;
        assert_eq!(
            *collector.connections.borrow(),
            (4, 4),
            "three periodic attempts and one cancelled final attempt; no detached sockets"
        );
        gateway.assert_fixed_warning("timeout", &collector.url);
    })
    .await
    .expect("bounded hanging-collector acceptance");
}

#[cfg(unix)]
async fn http_failure_checkpoint(pool: PgPool) {
    tokio::time::timeout(Duration::from_secs(45), async {
        seed_canaries(&pool).await;
        let mut collector = Collector::with_failure(true).await;
        collector.reply_allowed.send_replace(true);
        let mut gateway =
            Gateway::spawn_with_scrape(&fresh_child_url(&pool), &collector.url, true, true).await;
        gateway.ready("HTTP 500 collector").await;
        gateway.wait_for_pushes("http_status", 2).await;
        gateway.assert_serving().await;
        gateway.terminate().await;
        gateway.assert_fixed_warning("http_status", &collector.url);
        // The collector actually received authenticated, nonempty metrics,
        // not an unrelated client/probe responsible for the 500 counter.
        for _ in 0..3 {
            let receipt = collector
                .receipts
                .try_recv()
                .expect("two periodic and one final receipt");
            assert!(receipt.authenticated && !receipt.raw.is_empty());
            assert_eq!(receipt.method, Method::POST);
        }
        assert!(collector.receipts.try_recv().is_err());
        assert!(!collector.overflow.load(Ordering::SeqCst));
    })
    .await
    .expect("bounded HTTP-500 acceptance");
}

#[cfg(unix)]
async fn refused_checkpoint(pool: PgPool) {
    tokio::time::timeout(Duration::from_secs(45), async {
        seed_canaries(&pool).await;
        // A collector that accepts and immediately closes every connection.
        // (A bound but non-listening socket is platform-dependent: macOS lets
        // the connect attempt time out instead of refusing it.) The port stays
        // reserved for the whole test, so there is no ephemeral-port race.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
        let refused = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let mut gateway =
            Gateway::spawn_with_scrape(&fresh_child_url(&pool), &url, true, true).await;
        gateway.ready("refusing collector").await;
        gateway.wait_for_pushes("transport", 2).await;
        gateway.assert_serving().await;
        gateway.terminate().await;
        gateway.assert_fixed_warning("transport", &url);
        refused.abort();
    })
    .await
    .expect("bounded connection-refusal acceptance");
}

#[cfg(unix)]
async fn final_snapshot_checkpoint(pool: PgPool) {
    tokio::time::timeout(Duration::from_secs(45), async {
        let mut canaries = seed_canaries(&pool).await;
        let database_url = fresh_child_url(&pool);
        let mut collector = Collector::start().await;
        canaries.extend([database_url.clone(), collector.url.clone()]);
        let not_before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let mut gateway = Gateway::spawn(&database_url, &collector.url, true).await;
        gateway.ready("healthy shutdown collector").await;
        collector.reply_allowed.send_replace(true);
        let first = verify_receipt(collector.next().await, &canaries, not_before);
        let second = verify_receipt(collector.next().await, &canaries, not_before);
        assert_eq!(first.instance, second.instance);
        assert!(second.accepted > first.accepted);
        gateway.terminate().await;
        let final_receipt = collector
            .receipts
            .try_recv()
            .expect("exactly one final snapshot before process exit");
        let final_snapshot = verify_receipt(final_receipt, &canaries, not_before);
        assert_eq!(final_snapshot.instance, second.instance);
        assert_eq!(final_snapshot.start, second.start);
        assert!(final_snapshot.time > second.time);
        // SIGTERM may cancel the second exchange before its acknowledgement
        // is recorded, even though the collector has already received it.
        assert!(final_snapshot.accepted >= second.accepted);
        assert!(
            final_snapshot.arrived.duration_since(second.arrived) < Duration::from_secs(9),
            "final is shutdown-driven, not the next periodic tick"
        );
        // Child has been reaped and its pipes drained: all its sends have
        // completed. Check the full queue, not just the first receipt.
        assert!(matches!(
            collector.receipts.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(!collector.overflow.load(Ordering::SeqCst));
    })
    .await
    .expect("bounded final-snapshot acceptance");
}

async fn startup_and_periodic_checkpoint(pool: PgPool) {
    tokio::time::timeout(Duration::from_secs(65), async {
        // Never accidentally hand the explicitly selected base database to a
        // child. Preserve the SQLx-created pool's credentials/security options.
        let base: PgConnectOptions = std::env::var("DATABASE_URL")
            .ok().expect("parent must explicitly select a disposable PostgreSQL DATABASE_URL")
            .parse().ok().expect("valid explicit PostgreSQL connection options");
        let options = pool.connect_options();
        assert!(options.get_database().is_some());
        assert!(options.get_database() != base.get_database(), "child requires the fresh SQLx database, not the selected base database");
        // SQLx 0.8's lossy URL preserves effective credentials/socket/TLS but
        // omits startup options. Retain those too; never relax a selected role
        // or other connection security setting merely to launch the child.
        let mut child_url = options.to_url_lossy();
        if let Some(value) = options.get_options() {
            child_url.query_pairs_mut().append_pair("options", value);
        }
        if let Some(value) = options.get_application_name() {
            child_url.query_pairs_mut().append_pair("application_name", value);
        }
        let database_url = child_url.to_string();
        let mut canaries = seed_canaries(&pool).await;
        canaries.push(database_url.clone());
        let mut disabled_collector = Collector::start().await;
        let mut enabled_collector = Collector::start().await;
        canaries.extend([disabled_collector.url.clone(), enabled_collector.url.clone()]);

        let mut disabled = Gateway::spawn(&database_url, &disabled_collector.url, false).await;
        disabled.ready("URL-unset control").await;
        let not_before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        let mut enabled = Gateway::spawn(&database_url, &enabled_collector.url, true).await;
        canaries.extend([format!("http://{}", disabled.addr), format!("http://{}", enabled.addr)]);

        // Observe the disabled control throughout enabled startup AND two real
        // periodic receipts. This is receiver-driven, not a sleep/min-of-N
        // assertion; the control stays alive over the same periodic window.
        let (first, second) = tokio::select! {
            _ = disabled_collector.receipts.recv() => panic!("URL-unset control sent to the collector (including via ignored OTEL_* settings)"),
            receipts = async {
                enabled.ready("metrics enabled").await;
                // Readiness has succeeded without any collector acknowledgement.
                enabled_collector.reply_allowed.send_replace(true);
                let first = verify_receipt(enabled_collector.next().await, &canaries, not_before);
                let second = verify_receipt(enabled_collector.next().await, &canaries, not_before);
                (first, second)
            } => receipts,
        };
        assert_eq!(first.instance, second.instance, "stable per-process instance UUID");
        assert_eq!(first.start, second.start, "cumulative start must not reset per push");
        assert!(second.time > first.time, "two distinct periodic snapshots while serve is running");
        assert!(second.arrived.duration_since(first.arrived) >= Duration::from_secs(9), "10s periodic cadence, not duplicate workers or an immediate retry loop");
        assert!(second.accepted > first.accepted, "second snapshot observes the first acknowledged push");
        assert!(!disabled_collector.overflow.load(Ordering::SeqCst));
        assert!(!enabled_collector.overflow.load(Ordering::SeqCst));
        assert!(disabled.child.try_wait().unwrap().is_none(), "control remains running");
        assert!(enabled.child.try_wait().unwrap().is_none(), "periodic exports precede shutdown");
        disabled.stop().await;
        enabled.stop().await;
        assert!(matches!(disabled_collector.receipts.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "URL-unset control must have no queued send");
    }).await.expect("bounded metrics process acceptance checkpoint");
}

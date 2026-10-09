//! Capacity-aware batch scheduling (0022) on a disposable database, against
//! a mock, slow local OpenAI-compatible server (the vLLM profile, a normal
//! route) and a mock vLLM `/metrics` endpoint. Loopback only; no paid calls.
//!
//! Covers: the route concurrency cap, the priority hint (batch lines of
//! opted-in routes only), yielding to live traffic, server-busy and failed
//! metrics readings (fail closed), time windows and the stall alert, fair
//! sharing across workspaces, expiry with partial results and hold release,
//! and restart resume with two runners (exactly once).
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst},
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    batch::CreateBatch,
    runner::Runner,
    schedule::{self, MetricsGate, RouteSettings, Scheduler, TimeWindow, Timings},
    *,
};
use crate::{
    filestore::{FileStoreRuntime, NewFile, Purpose, QuotaMode, files::public_id},
    governance::tests::db::{Fixture, fixture},
    inference::{
        EngineLimits,
        types::{ChatRequest, Message, ProviderOutput, Role},
    },
    providers::{local::endpoints::ApprovedEndpoints, secrets::EnvSecrets},
};

/// The mock server: records request bodies, tracks requests in flight.
#[derive(Default)]
struct Server {
    bodies: Mutex<Vec<Value>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    delay_ms: AtomicU64,
    /// Prometheus text; `None` answers 500.
    metrics: Mutex<Option<String>>,
}
impl Server {
    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
    fn texts(&self) -> Vec<String> {
        self.bodies()
            .iter()
            .map(|b| b["messages"][0]["content"].as_str().unwrap().to_owned())
            .collect()
    }
    fn requests(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
    fn delay(&self, ms: u64) {
        self.delay_ms.store(ms, SeqCst);
    }
    fn set_metrics(&self, text: Option<&str>) {
        *self.metrics.lock().unwrap() = text.map(str::to_owned);
    }
}
async fn chat(State(s): State<Arc<Server>>, Json(body): Json<Value>) -> Json<Value> {
    let text = body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    s.bodies.lock().unwrap().push(body);
    let now = s.in_flight.fetch_add(1, SeqCst) + 1;
    s.max_in_flight.fetch_max(now, SeqCst);
    let delay = if text.starts_with("slow") {
        3000
    } else {
        s.delay_ms.load(SeqCst)
    };
    tokio::time::sleep(Duration::from_millis(delay)).await;
    s.in_flight.fetch_sub(1, SeqCst);
    let mut v = json!({"model":"google/gemma-4","choices":[{"index":0,"message":{"role":"assistant","content":format!("re: {text}")},"finish_reason":"stop"}]});
    if !text.contains("nousage") {
        v["usage"] = json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15});
    }
    Json(v)
}
async fn metrics(State(s): State<Arc<Server>>) -> Result<String, StatusCode> {
    s.metrics
        .lock()
        .unwrap()
        .clone()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

struct World {
    f: Fixture,
    jobs: Jobs,
    files: crate::filestore::FileStorage,
    server: Arc<Server>,
    port: u16,
    /// The self-hosted route (vLLM profile).
    route: Uuid,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for World {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn world(pool: PgPool) -> World {
    let f = fixture(pool).await;
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=true WHERE singleton")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let server = Arc::new(Server::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/metrics", get(metrics))
        .with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://127.0.0.1:{port}/v1");
    // A self-hosted model (Gemma on vLLM) added as a normal route.
    let (model, connection, route) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/gemma',ARRAY['chat_completions'])").bind(model).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'GPU box','vllm','none',$2,true)").bind(connection).bind(&base).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'google/gemma-4',true)").bind(route).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT id,$1,'direct' FROM workspaces").bind(model).execute(&f.store.pool).await.unwrap();
    // 1 µUSD per token; ceilings 100 in / 50 out.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,50,1)").bind(Uuid::new_v4()).bind(route).execute(&f.store.pool).await.unwrap();
    let mut registry = ProviderRegistry::default();
    for adapter in crate::providers::local::adapters(
        Arc::new(EnvSecrets::new([])),
        ApprovedEndpoints::for_test(&base),
    ) {
        registry.register(adapter).unwrap();
    }
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry.clone(),
        EngineLimits::default(),
    )
    .unwrap();
    let runtime = FileStoreRuntime::memory();
    let jobs = Jobs::with(
        f.store.clone(),
        registry,
        JobLimits {
            batch_workers: 16,
            batch_concurrency: 8,
            ..JobLimits::default()
        },
    )
    .with_files(Some(runtime.clone()))
    .with_engine(engine)
    .with_approvals(ApprovedEndpoints::for_test(&base));
    let files = crate::filestore::FileStorage::new(f.store.clone(), runtime);
    // Room for several batches at once in each workspace.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) SELECT id,10 FROM workspaces ON CONFLICT(workspace_id) DO UPDATE SET concurrent_jobs=10")
        .execute(&f.store.pool)
        .await
        .unwrap();
    World {
        f,
        jobs,
        files,
        server,
        port,
        route,
        task,
    }
}

fn line(id: &str, text: &str) -> Value {
    json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions","body":{"model":"company/gemma","messages":[{"role":"user","content":text}],"max_completion_tokens":10}})
}
fn lines(prefix: &str, n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| line(&format!("{prefix}{i}"), &format!("{prefix}{i}")))
        .collect()
}

/// Wait (bounded) until `$cond` holds.
macro_rules! eventually {
    ($cond:expr) => {{
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if $cond {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                stringify!($cond)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }};
}

impl World {
    fn metrics_url(&self) -> String {
        format!("http://127.0.0.1:{}/metrics", self.port)
    }
    /// A runner with fast polling and no gate caching (tests change gates).
    fn runner(&self) -> Runner {
        let mut r = Runner::new(self.jobs.clone());
        r.poll = Duration::from_millis(20);
        r.scheduler = Arc::new(
            Scheduler::new(self.f.store.clone(), self.jobs.approvals.clone()).with_timings(
                Timings {
                    settings: Duration::ZERO,
                    live: Duration::ZERO,
                    metrics: Duration::from_millis(50),
                    metrics_timeout: Duration::from_secs(2),
                    signal: Duration::ZERO,
                },
            ),
        );
        r
    }
    async fn settings(&self, s: RouteSettings) {
        let mut tx = self.f.store.pool.begin().await.unwrap();
        schedule::save_settings(&mut tx, self.route, &s, self.f.owner)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    async fn batch(&self, principal: crate::auth::Principal, lines: &[Value]) -> JobRow {
        let mut body = Vec::new();
        for l in lines {
            body.extend_from_slice(&serde_json::to_vec(l).unwrap());
            body.push(b'\n');
        }
        let stream: crate::filestore::ByteStream =
            Box::pin(futures_util::stream::once(async move {
                Ok(axum::body::Bytes::from(body))
            }));
        let stored = self
            .files
            .create(
                NewFile {
                    created_by_api_key_id: Some(principal.key_id),
                    quota: QuotaMode::CountOnly,
                    ..NewFile::new(Purpose::BatchInput, Some(principal.workspace_id))
                },
                stream,
            )
            .await
            .unwrap();
        let job = self
            .jobs
            .create_batch(
                principal,
                Uuid::new_v4(),
                CreateBatch {
                    input_file_id: public_id(stored.id),
                    endpoint: BatchEndpoint::ChatCompletions,
                    metadata: None,
                    completion_window_hours: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(job.mode(), Some(BatchMode::Gateway));
        job
    }
    async fn reload(&self, job: &JobRow) -> JobRow {
        store::reload(&self.f.store, job.id).await.unwrap()
    }
    async fn read(&self, workspace: Uuid, file: Option<Uuid>) -> Vec<Value> {
        let Some(file) = file else {
            return vec![];
        };
        let (_, mut stream) = self.files.open(file, Some(workspace)).await.unwrap();
        let mut all = Vec::new();
        while let Some(c) = stream.next().await {
            all.extend_from_slice(&c.unwrap());
        }
        all.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
    }
    /// The batch's wait reason on the route (`None`: not waiting).
    async fn reason(&self, job: &JobRow) -> Option<String> {
        schedule::batch_waits(&self.f.store.pool, job.id)
            .await
            .unwrap()
            .first()
            .and_then(|w| w["reason"].as_str().map(str::to_owned))
    }
    async fn status(&self) -> Value {
        schedule::route_status(&self.f.store.pool, self.route)
            .await
            .unwrap()
    }
    /// An interactive (live) request to the self-hosted route.
    fn live(&self, text: &str) -> tokio::task::JoinHandle<Result<ProviderOutput, InferenceError>> {
        let engine = self.jobs.engine.clone().unwrap();
        let principal = self.f.principal;
        let request = ChatRequest {
            model: "company/gemma".into(),
            messages: vec![Message {
                role: Role::User,
                content: Some(text.into()),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            tools: vec![],
            tool_choice: None,
            temperature: None,
            max_output_tokens: Some(10),
            stream: false,
        };
        tokio::spawn(async move { engine.execute(principal, request, Uuid::new_v4()).await })
    }
}
async fn consistent(f: &Fixture) {
    let report = crate::governance::totals::verify(&f.store).await.unwrap();
    assert!(report.consistent(), "{report:?}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn route_concurrency_cap_and_priority_hint_on_batch_lines_only(pool: PgPool) {
    let w = world(pool).await;
    w.server.delay(150);
    w.settings(RouteSettings {
        max_concurrency: 2,
        priority: Some(7),
        ..RouteSettings::default()
    })
    .await;
    let job = w.batch(w.f.principal, &lines("a", 6)).await;
    assert!(w.runner().run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.state.as_str(), job.request_completed),
        ("completed", Some(6))
    );
    assert_eq!(w.server.max_in_flight.load(SeqCst), 2, "at most 2 at once");
    // vLLM priority on every batch line of the opted-in route.
    assert!(w.server.bodies().iter().all(|b| b["priority"] == json!(7)));
    let routed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM batch_lines WHERE job_id=$1 AND deployment_id=$2")
            .bind(job.id)
            .bind(w.route)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(routed, 6, "every line records the route it was claimed on");
    // Interactive requests to the same route never carry it.
    w.server.delay(0);
    assert!(w.live("hello").await.unwrap().is_ok());
    assert!(w.server.bodies().last().unwrap().get("priority").is_none());
    // Without the setting, batch lines don't carry it either.
    w.settings(RouteSettings::default()).await;
    let job = w.batch(w.f.principal, &lines("b", 2)).await;
    assert!(w.runner().run_next().await.unwrap());
    assert_eq!(w.reload(&job).await.state, "completed");
    let bodies = w.server.bodies();
    assert!(
        bodies[bodies.len() - 2..]
            .iter()
            .all(|b| b.get("priority").is_none())
    );
    // The demand is cleared when the batch ends.
    assert!(
        schedule::batch_waits(&w.f.store.pool, job.id)
            .await
            .unwrap()
            .is_empty()
    );
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_lines_yield_to_live_traffic(pool: PgPool) {
    let w = world(pool).await;
    w.settings(RouteSettings {
        yield_live_threshold: Some(1),
        ..RouteSettings::default()
    })
    .await;
    w.server.delay(1500);
    let live = w.live("live");
    eventually!(w.server.in_flight.load(SeqCst) == 1);
    w.server.delay(10);
    let job = w.batch(w.f.team, &lines("b", 2)).await;
    let runner = w.runner();
    let run = tokio::spawn(async move { runner.run_next().await.unwrap() });
    eventually!(w.reason(&job).await.as_deref() == Some("live_traffic"));
    assert_eq!(
        w.server.requests(),
        1,
        "no batch line while live traffic runs"
    );
    let status = w.status().await;
    assert_eq!(status["paused_reason"], "live_traffic");
    assert_eq!(status["live_in_flight"], 1);
    assert_eq!(status["queued_lines"], 2);
    let waits = schedule::batch_waits(&w.f.store.pool, job.id)
        .await
        .unwrap();
    assert_eq!(
        (
            &waits[0]["model"],
            &waits[0]["position"],
            &waits[0]["queue"]
        ),
        (&json!("company/gemma"), &json!(1), &json!(1))
    );
    // The live request finishes; the batch proceeds.
    assert!(live.await.unwrap().is_ok());
    assert!(run.await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.state.as_str(), job.request_completed),
        ("completed", Some(2))
    );
    assert_eq!(w.server.texts()[0], "live");
    assert_eq!(w.server.requests(), 3);
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn server_metrics_busy_or_unreadable_pause_the_route(pool: PgPool) {
    let w = world(pool).await;
    let gate = MetricsGate {
        url: w.metrics_url(),
        max_waiting: Some(0),
        max_running: None,
        max_kv_cache_percent: Some(90),
    };
    w.server.set_metrics(Some(
        "# TYPE vllm:num_requests_waiting gauge\nvllm:num_requests_waiting{engine=\"0\",model_name=\"google/gemma-4\"} 3\nvllm:kv_cache_usage_perc{engine=\"0\",model_name=\"google/gemma-4\"} 0.2\n",
    ));
    w.settings(RouteSettings {
        metrics: Some(gate.clone()),
        ..RouteSettings::default()
    })
    .await;
    let job = w.batch(w.f.principal, &lines("m", 2)).await;
    let runner = w.runner();
    let run = tokio::spawn(async move { runner.run_next().await.unwrap() });
    eventually!(w.reason(&job).await.as_deref() == Some("server_busy"));
    let status = w.status().await;
    assert_eq!(status["paused_reason"], "server_busy");
    assert_eq!(status["metrics"]["ok"], true);
    assert_eq!(status["metrics"]["waiting"], 3);
    assert_eq!(status["metrics"]["kv_cache_permille"], 200);
    // The server stops answering: fail closed and say so.
    w.server.set_metrics(None);
    eventually!(w.reason(&job).await.as_deref() == Some("metrics_unavailable"));
    eventually!(w.status().await["metrics"]["error"] == "http_status");
    assert_eq!(w.status().await["metrics"]["ok"], false);
    // A URL that is not an approved origin is never fetched.
    w.settings(RouteSettings {
        metrics: Some(MetricsGate {
            url: "http://127.0.0.1:1/metrics".into(),
            ..gate.clone()
        }),
        ..RouteSettings::default()
    })
    .await;
    eventually!(w.status().await["metrics"]["error"] == "not_approved");
    // A cache above its limit is busy too.
    w.settings(RouteSettings {
        metrics: Some(gate.clone()),
        ..RouteSettings::default()
    })
    .await;
    w.server.set_metrics(Some(
        "vllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.95\n",
    ));
    eventually!(w.status().await["metrics"]["kv_cache_permille"] == 950);
    assert_eq!(w.reason(&job).await.as_deref(), Some("server_busy"));
    assert_eq!(w.server.requests(), 0);
    // An idle server: the lines run.
    w.server.set_metrics(Some(
        "vllm:num_requests_waiting 0\nvllm:num_requests_running 1\nvllm:kv_cache_usage_perc 0.5\n",
    ));
    assert!(run.await.unwrap());
    assert_eq!(w.reload(&job).await.state, "completed");
    assert_eq!(w.server.requests(), 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn time_windows_hold_lines_and_waiting_is_not_a_stall(pool: PgPool) {
    let w = world(pool).await;
    let at = |offset: i64| {
        (Utc::now() + chrono::TimeDelta::minutes(offset))
            .format("%H:%M")
            .to_string()
    };
    let every = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        .map(String::from)
        .to_vec();
    // A window that opens in two hours (UTC, every day).
    w.settings(RouteSettings {
        window: Some(TimeWindow {
            timezone: "UTC".into(),
            days: every.clone(),
            start: at(120),
            end: at(180),
        }),
        ..RouteSettings::default()
    })
    .await;
    sqlx::query("INSERT INTO alert_rules(id,scope,kind,name,window_minutes) VALUES($1,'installation','batch_stalled','Stalled',5)")
        .bind(Uuid::new_v4())
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let job = w.batch(w.f.team, &lines("w", 2)).await;
    let runner = w.runner();
    let run = tokio::spawn(async move { runner.run_next().await.unwrap() });
    eventually!(w.reason(&job).await.as_deref() == Some("outside_window"));
    assert_eq!(w.status().await["paused_reason"], "outside_window");
    let waited: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_waited_at FROM async_jobs WHERE id=$1")
            .bind(job.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert!(waited.is_some(), "the runner records a legitimate wait");
    // No progress for an hour, but waiting for its window: not stalled.
    sqlx::query("UPDATE async_jobs SET in_progress_at=now()-interval '1 hour',last_progress_at=now()-interval '1 hour' WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    crate::alerts::evaluate_once(&w.f.store).await.unwrap();
    let stalled: i64 =
        sqlx::query_scalar("SELECT count(*) FROM alert_events WHERE kind='batch_stalled'")
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(stalled, 0);
    assert_eq!(w.server.requests(), 0);
    // The window opens (an overnight-capable span around now).
    w.settings(RouteSettings {
        window: Some(TimeWindow {
            timezone: "UTC".into(),
            days: every,
            start: at(-60),
            end: at(60),
        }),
        ..RouteSettings::default()
    })
    .await;
    assert!(run.await.unwrap());
    assert_eq!(w.reload(&job).await.state, "completed");
    assert_eq!(w.server.requests(), 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn fair_share_across_workspaces(pool: PgPool) {
    let w = world(pool).await;
    w.server.delay(40);
    w.settings(RouteSettings {
        max_concurrency: 1,
        ..RouteSettings::default()
    })
    .await;
    // A huge batch first, then a small one from another workspace.
    let big = w.batch(w.f.principal, &lines("p", 8)).await;
    let small = w.batch(w.f.team, &lines("t", 2)).await;
    let runner = w.runner();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        runner.tick().await.unwrap();
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM async_jobs WHERE id=ANY($1) AND settled_at IS NULL",
        )
        .bind(vec![big.id, small.id])
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        if open == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "batches did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let order = w.server.texts();
    assert_eq!(order.len(), 10, "{order:?}");
    let last_small = order.iter().rposition(|t| t.starts_with('t')).unwrap();
    assert!(last_small <= 4, "the small batch was starved: {order:?}");
    assert_eq!(w.server.max_in_flight.load(SeqCst), 1);
    for job in [&big, &small] {
        assert_eq!(w.reload(job).await.state, "completed");
    }
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn expiry_keeps_partial_results_and_releases_unrun_holds(pool: PgPool) {
    let w = world(pool).await;
    w.settings(RouteSettings {
        max_concurrency: 1,
        ..RouteSettings::default()
    })
    .await;
    let job = w
        .batch(
            w.f.principal,
            &[
                line("e0", "slow nousage"),
                line("e1", "e1"),
                line("e2", "e2"),
                line("e3", "e3"),
            ],
        )
        .await;
    // The 24 h window ends 1.5 s from now (moved back as owner).
    sqlx::query("ALTER TABLE async_jobs DISABLE TRIGGER async_jobs_guard")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET created_at=now()-interval '24 hours'+interval '1500 milliseconds' WHERE id=$1")
        .bind(job.id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE async_jobs ENABLE TRIGGER async_jobs_guard")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    assert!(w.runner().run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "expired");
    // The running line finished; the others never started.
    assert_eq!(w.server.texts(), ["slow nousage"]);
    let out = w.read(job.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["custom_id"], "e0");
    let errors = w.read(job.workspace_id, job.error_file_id).await;
    let mut codes: Vec<(String, String)> = errors
        .iter()
        .map(|e| {
            (
                e["custom_id"].as_str().unwrap().to_owned(),
                e["error"]["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    codes.sort();
    assert_eq!(
        codes,
        ["e1", "e2", "e3"].map(|id| (id.to_owned(), "batch_expired".to_owned()))
    );
    // The line without usage keeps its hold; the envelope released the rest.
    let lines: Vec<(String, Option<i64>)> = sqlx::query_as("SELECT r.state,r.held_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1")
        .bind(job.id)
        .fetch_all(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1, "lines that never ran have no attempt");
    assert_eq!(lines[0].0, "unknown");
    assert_eq!(lines[0].1, Some(110), "unknown usage keeps the line's hold");
    let envelope: (String, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(job.execution_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(envelope, ("settled".to_owned(), Some(0)));
    let v = w
        .jobs
        .get_batch(&w.f.principal, &types::client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["status"], "expired");
    assert_eq!(v["completion_window"], "24h");
    consistent(&w.f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn two_runners_resume_and_share_a_route_exactly_once(pool: PgPool) {
    let w = world(pool).await;
    w.server.delay(60);
    w.settings(RouteSettings {
        max_concurrency: 2,
        ..RouteSettings::default()
    })
    .await;
    let a = w.batch(w.f.principal, &lines("a", 5)).await;
    let b = w.batch(w.f.team, &lines("b", 5)).await;
    // A crashed runner left a's line 0 running on the route; its lease ended.
    sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id,deployment_id) VALUES($1,$2,0,'running',$3,$4)")
        .bind(a.id).bind(a.workspace_id).bind(Uuid::new_v4()).bind(w.route)
        .execute(&w.f.store.pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET runner_id=$2,runner_lease_until=now()-interval '1 second',state='in_progress' WHERE id=$1")
        .bind(a.id).bind(Uuid::new_v4()).execute(&w.f.store.pool).await.unwrap();
    // Two processes, each with its own scheduler, share the route's cap.
    let work = |r: Runner| async move { while r.run_next().await.unwrap() {} };
    tokio::join!(work(w.runner()), work(w.runner()));
    let mut texts = w.server.texts();
    texts.sort();
    assert_eq!(
        texts,
        ["a1", "a2", "a3", "a4", "b0", "b1", "b2", "b3", "b4"],
        "every line exactly once; the interrupted line never again"
    );
    assert!(w.server.max_in_flight.load(SeqCst) <= 2);
    let interrupted: String =
        sqlx::query_scalar("SELECT state FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(a.id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
    assert_eq!(interrupted, "interrupted");
    for (job, done) in [(&a, (Some(4), Some(1))), (&b, (Some(5), Some(0)))] {
        let job = w.reload(job).await;
        assert_eq!(job.state, "completed");
        assert_eq!((job.request_completed, job.request_failed), done);
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn longer_completion_windows_run_gateway_side(pool: PgPool) {
    let w = world(pool).await;
    let mut body = serde_json::to_vec(&line("x", "x")).unwrap();
    body.push(b'\n');
    let stored = w
        .files
        .create(
            NewFile {
                created_by_api_key_id: Some(w.f.principal.key_id),
                quota: QuotaMode::CountOnly,
                ..NewFile::new(Purpose::BatchInput, Some(w.f.principal.workspace_id))
            },
            Box::pin(futures_util::stream::once(async move {
                Ok(axum::body::Bytes::from(body))
            })),
        )
        .await
        .unwrap();
    let create = |hours| CreateBatch {
        input_file_id: public_id(stored.id),
        endpoint: BatchEndpoint::ChatCompletions,
        metadata: None,
        completion_window_hours: Some(hours),
    };
    assert!(matches!(
        w.jobs
            .create_batch(w.f.principal, Uuid::new_v4(), create(36))
            .await,
        Err(batch::CreateError::Job(JobError::Invalid(
            "invalid_completion_window",
            _
        )))
    ));
    let job = w
        .jobs
        .create_batch(w.f.principal, Uuid::new_v4(), create(72))
        .await
        .unwrap();
    assert_eq!(job.completion_window_hours, Some(72));
    let v = w
        .jobs
        .get_batch(&w.f.principal, &types::client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["completion_window"], "72h");
    assert_eq!(
        v["expires_at"],
        json!((job.created_at + chrono::TimeDelta::hours(72)).timestamp())
    );
    // The envelope is held for the whole window (plus finalization).
    let lease: DateTime<Utc> = sqlx::query_scalar(
        "SELECT lease_expires_at FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(job.execution_id)
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert!(lease > job.created_at + chrono::TimeDelta::hours(74));
}
